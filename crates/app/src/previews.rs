//! Thumbnails and waveforms for the widgets ([`aiframes_ui::MediaPreview`]).
//!
//! Requests never block: a miss queues a job for a small worker pool (most recent request first, so
//! what is on screen loads first; hover-scrub prefetches wait behind it) and returns a stand-in: the
//! same moment at another size, or the loaded frame nearest in time within the caller's tolerance,
//! never a far-away frame. The worker uploads the texture and requests a repaint. Textures are
//! LRU-limited by bytes.
//!
//! Sizes: thumbnails are decoded at one of [`THUMB_SIZES`] (physical pixels), the smallest at least
//! as tall as the box they fill, so they stay sharp on HiDPI screens and in large bin tiles, and are
//! sampled with mipmapped linear filtering so smaller boxes do not alias.
//!
//! Posters: the bin shows a representative frame per file, not a black one (fade-ins, slates): the
//! first candidate (10 %, 25 %, 50 %, 75 %, 2 % of the duration) that is not near-black or flat.

use aiframes_core::{MediaAsset, MediaKind};
use aiframes_media::{MediaPool, Waveform};
use eframe::egui::{self, TextureFilter, TextureHandle, TextureOptions};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

/// Thumbnail heights decoded, in physical pixels.
pub const THUMB_SIZES: [u32; 4] = [54, 108, 216, 432];
/// Thumbnail height for callers that do not say how big they draw it.
pub const THUMB_HEIGHT: u32 = 108;
/// Bytes of thumbnail textures kept (least recently used go first).
const TEXTURE_BUDGET: usize = 160 << 20;
/// Pending thumbnail jobs kept (older requests are forgotten and re-requested when visible again).
const MAX_QUEUE: usize = 384;
/// Pending low-priority (prefetch) jobs kept.
const MAX_LOW_QUEUE: usize = 256;
const THUMB_WORKERS: usize = 4;
/// Poster candidates, as fractions of the duration.
const POSTER_AT: [f64; 5] = [0.1, 0.25, 0.5, 0.75, 0.02];
/// Posters are judged at this size (cheap; the chosen moment is then loaded at the tile's size).
const POSTER_PROBE: u32 = 54;
/// The on-disk thumbnail cache is pruned to this at startup (oldest files first).
const DISK_BUDGET: u64 = 256 << 20;
/// Mipmapped linear sampling: sharp when drawn at the decoded size, no aliasing when smaller.
const THUMB_TEXTURE: TextureOptions = TextureOptions::LINEAR.with_mipmap_mode(Some(TextureFilter::Linear));

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Absolute path of a media asset relative to a project folder (same rule as the core document).
pub fn media_path(dir: &Path, asset: &MediaAsset) -> PathBuf {
    let p = Path::new(&asset.path);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    let mut out = PathBuf::new();
    for c in dir.join(p).components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Thumbnail time key: tenths of a second (the media pool's thumbnail grid).
pub fn time_key(t: f64) -> i64 {
    if t.is_finite() {
        (t.max(0.0) * 10.0).round() as i64
    } else {
        0
    }
}

/// The decoded height for a box `px` physical pixels tall: the smallest size at least that tall
/// (within 5 %: a 219 px tile uses 216 px rather than twice the pixels).
pub fn size_for(px: f32) -> u32 {
    let px = if px.is_finite() { px.max(1.0) } else { THUMB_HEIGHT as f32 };
    THUMB_SIZES.iter().copied().find(|s| *s as f32 >= px * 0.95).unwrap_or(THUMB_SIZES[THUMB_SIZES.len() - 1])
}

/// True for frames that make a poor poster: near-black (fade-ins, black leaders) or flat (one colour).
pub fn is_blank(rgba: &[u8]) -> bool {
    let n = rgba.len() / 4;
    if n == 0 {
        return true;
    }
    let (mut sum, mut sq) = (0f64, 0f64);
    for px in rgba.as_chunks::<4>().0 {
        let y = 0.2126 * px[0] as f64 + 0.7152 * px[1] as f64 + 0.0722 * px[2] as f64;
        sum += y;
        sq += y * y;
    }
    let mean = sum / n as f64;
    let sd = (sq / n as f64 - mean * mean).max(0.0).sqrt();
    mean < 20.0 || sd < 4.0
}

enum Thumb {
    Loading,
    /// Texture, last use (tick), bytes.
    Ready(TextureHandle, u64, usize),
    Failed,
}

enum Wave {
    Loading,
    Ready(Arc<Waveform>),
    Failed,
}

#[derive(Clone, Copy, PartialEq)]
enum Poster {
    Searching,
    /// The chosen moment (source seconds).
    Ready(f64),
}

#[derive(Clone, PartialEq)]
enum Job {
    Thumb { path: PathBuf, size: u32, key: i64 },
    Poster { path: PathBuf, duration: f64 },
}

/// Per file: per decoded size, thumbnails by time key.
type Sizes = BTreeMap<u32, BTreeMap<i64, Thumb>>;

#[derive(Default)]
struct State {
    thumbs: HashMap<PathBuf, Sizes>,
    /// Bytes of ready textures.
    bytes: usize,
    ready: usize,
    posters: HashMap<PathBuf, Poster>,
    /// Files no picture could be made of (offline, unreadable).
    failed: HashMap<PathBuf, bool>,
    waves: HashMap<PathBuf, Wave>,
    queue: VecDeque<Job>,
    /// Prefetches (hover-scrub strips): served only when `queue` is empty.
    low: VecDeque<Job>,
    wave_queue: VecDeque<PathBuf>,
    tick: u64,
    stop: bool,
}

impl State {
    fn map(&mut self, path: &Path, size: u32) -> &mut BTreeMap<i64, Thumb> {
        self.thumbs.entry(path.to_path_buf()).or_default().entry(size).or_default()
    }

    /// Ready texture at exactly (size, key), marked as used.
    fn hit(&mut self, path: &Path, size: u32, key: i64) -> Option<egui::TextureId> {
        self.tick += 1;
        let tick = self.tick;
        match self.thumbs.get_mut(path)?.get_mut(&size)?.get_mut(&key)? {
            Thumb::Ready(tex, used, _) => {
                *used = tick;
                Some(tex.id())
            }
            _ => None,
        }
    }

    /// Queues a thumbnail job unless it is loading, loaded or failed. Returns true when queued.
    fn request(&mut self, path: &Path, size: u32, key: i64, low: bool) -> bool {
        let map = self.map(path, size);
        if map.contains_key(&key) {
            return false;
        }
        map.insert(key, Thumb::Loading);
        let job = Job::Thumb { path: path.to_path_buf(), size, key };
        let (q, max) = if low { (&mut self.low, MAX_LOW_QUEUE) } else { (&mut self.queue, MAX_QUEUE) };
        q.push_back(job);
        let mut dropped = Vec::new();
        while q.len() > max {
            if let Some(j) = q.pop_front() {
                dropped.push(j);
            }
        }
        for j in dropped {
            if let Job::Thumb { path, size, key } = j {
                if let Some(m) = self.thumbs.get_mut(&path).and_then(|s| s.get_mut(&size)) {
                    if matches!(m.get(&key), Some(Thumb::Loading)) {
                        m.remove(&key);
                    }
                }
            }
        }
        true
    }

    /// A stand-in while (size, key) loads: the same moment at another size, else the loaded frame
    /// nearest in time within `near_keys` (any size, the requested size preferred on ties).
    fn stand_in(&self, path: &Path, size: u32, key: i64, near_keys: i64) -> Option<egui::TextureId> {
        let sizes = self.thumbs.get(path)?;
        // Same moment, closest size (a larger one first: it only gets sharper).
        let mut same: Vec<(u32, egui::TextureId)> = sizes.iter().filter_map(|(s, m)| if let Some(Thumb::Ready(t, _, _)) = m.get(&key) { Some((*s, t.id())) } else { None }).collect();
        same.sort_by_key(|(s, _)| (if *s >= size { 0 } else { 1 }, s.abs_diff(size)));
        if let Some((_, t)) = same.first() {
            return Some(*t);
        }
        if near_keys <= 0 {
            return None;
        }
        let mut best: Option<(i64, u32, egui::TextureId)> = None;
        for (s, m) in sizes {
            let before = m.range(..key).rev().find_map(|(k, t)| if let Thumb::Ready(tex, _, _) = t { Some((*k, tex.id())) } else { None });
            let after = m.range(key..).find_map(|(k, t)| if let Thumb::Ready(tex, _, _) = t { Some((*k, tex.id())) } else { None });
            for (k, tex) in before.into_iter().chain(after) {
                let d = (k - key).abs();
                if d > near_keys {
                    continue;
                }
                let better = match best {
                    None => true,
                    Some((bd, bs, _)) => d < bd || (d == bd && *s == size && bs != size),
                };
                if better {
                    best = Some((d, *s, tex));
                }
            }
        }
        best.map(|b| b.2)
    }
}

struct Inner {
    pool: Arc<MediaPool>,
    ctx: egui::Context,
    st: Mutex<State>,
    cv: Condvar,
    /// While playing, only one thumbnail decodes at a time (playback gets the CPU and decoders).
    throttle: std::sync::atomic::AtomicBool,
}

pub struct Previews {
    inner: Arc<Inner>,
    dir: std::cell::RefCell<PathBuf>,
}

impl Previews {
    pub fn new(ctx: egui::Context, pool: Arc<MediaPool>) -> Previews {
        let inner = Arc::new(Inner { pool, ctx, st: Mutex::new(State::default()), cv: Condvar::new(), throttle: Default::default() });
        for n in 0..THUMB_WORKERS {
            let i = inner.clone();
            let _ = std::thread::Builder::new().name(format!("aiframes-thumbs-{n}")).spawn(move || thumb_worker(i, n));
        }
        let i = inner.clone();
        let _ = std::thread::Builder::new().name("aiframes-waves".into()).spawn(move || wave_worker(i));
        // Larger thumbnails for HiDPI screens take more disk: keep the thumbnail cache bounded.
        let pool = inner.pool.clone();
        let _ = std::thread::Builder::new().name("aiframes-thumb-prune".into()).spawn(move || pool.prune_thumbnail_cache(DISK_BUDGET));
        Previews { inner, dir: std::cell::RefCell::new(PathBuf::from(".")) }
    }

    /// Throttles background thumbnail decoding while the viewer plays (one decoder, below-normal
    /// priority); paused, thumbnails decode at normal priority so they appear even on a busy machine.
    pub fn set_playing(&self, playing: bool) {
        let was = self.inner.throttle.swap(playing, std::sync::atomic::Ordering::Relaxed);
        if was != playing {
            self.inner.pool.set_thumbnails_background(playing);
        }
        if was && !playing {
            self.inner.cv.notify_all();
        }
    }

    /// The folder relative media paths are resolved against (the active project's folder).
    pub fn set_dir(&self, dir: PathBuf) {
        *self.dir.borrow_mut() = dir;
    }

    fn path(&self, asset: &MediaAsset) -> PathBuf {
        media_path(&self.dir.borrow(), asset)
    }

    /// Forgets a file no longer used by any open project (its textures, poster and waveform).
    pub fn forget(&self, path: &Path) {
        let mut st = lock(&self.inner.st);
        if let Some(m) = st.thumbs.remove(path) {
            for t in m.values().flat_map(|s| s.values()) {
                if let Thumb::Ready(_, _, b) = t {
                    st.bytes = st.bytes.saturating_sub(*b);
                    st.ready = st.ready.saturating_sub(1);
                }
            }
        }
        st.posters.remove(path);
        st.failed.remove(path);
        st.waves.remove(path);
    }

    /// Loads waveforms for these files in the background (used when a project opens).
    pub fn prefetch_waves(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let mut st = lock(&self.inner.st);
        for p in paths {
            if !st.waves.contains_key(&p) {
                st.waves.insert(p.clone(), Wave::Loading);
                st.wave_queue.push_back(p);
            }
        }
        drop(st);
        self.inner.cv.notify_all();
    }

    /// (ready textures, queued jobs)
    pub fn stats(&self) -> (usize, usize) {
        let st = lock(&self.inner.st);
        (st.ready, st.queue.len() + st.low.len())
    }

    fn pictureless(asset: &MediaAsset) -> bool {
        asset.kind == MediaKind::Audio || (asset.kind == MediaKind::Video && !asset.info.has_video && asset.info.width == 0)
    }
}

impl Drop for Previews {
    fn drop(&mut self) {
        lock(&self.inner.st).stop = true;
        self.inner.cv.notify_all();
    }
}

impl aiframes_ui::MediaPreview for Previews {
    fn thumbnail(&self, asset: &MediaAsset, t: f64) -> Option<egui::TextureId> {
        self.thumbnail_px(asset, t, THUMB_HEIGHT as f32, f64::INFINITY)
    }

    fn thumbnail_px(&self, asset: &MediaAsset, t: f64, px: f32, near: f64) -> Option<egui::TextureId> {
        if Self::pictureless(asset) {
            return None;
        }
        let path = self.path(asset);
        let size = size_for(px);
        let key = if asset.kind == MediaKind::Image { 0 } else { time_key(t) };
        let mut st = lock(&self.inner.st);
        if let Some(tex) = st.hit(&path, size, key) {
            return Some(tex);
        }
        let near_keys = if asset.kind == MediaKind::Image { i64::MAX } else if near.is_finite() { (near.max(0.0) * 10.0).round() as i64 } else { i64::MAX };
        let queued = st.request(&path, size, key, false);
        let out = st.stand_in(&path, size, key, near_keys);
        drop(st);
        if queued {
            self.inner.cv.notify_all();
        }
        out
    }

    fn poster(&self, asset: &MediaAsset, px: f32) -> Option<egui::TextureId> {
        if Self::pictureless(asset) {
            return None;
        }
        if asset.kind == MediaKind::Image || asset.info.duration.is_nan() || asset.info.duration <= 0.0 {
            return self.thumbnail_px(asset, 0.0, px, f64::INFINITY);
        }
        let path = self.path(asset);
        let at = {
            let mut st = lock(&self.inner.st);
            match st.posters.get(&path).copied() {
                Some(Poster::Ready(t)) => Some(t),
                Some(Poster::Searching) => None,
                None => {
                    st.posters.insert(path.clone(), Poster::Searching);
                    st.queue.push_back(Job::Poster { path: path.clone(), duration: asset.info.duration });
                    drop(st);
                    self.inner.cv.notify_all();
                    None
                }
            }
        };
        match at {
            Some(t) => self.thumbnail_px(asset, t, px, f64::INFINITY),
            // While the poster is chosen: any frame of the file already loaded (no request).
            None => {
                let st = lock(&self.inner.st);
                st.stand_in(&path, size_for(px), time_key(asset.info.duration * 0.1), i64::MAX)
            }
        }
    }

    fn prefetch(&self, asset: &MediaAsset, times: &[f64], px: f32) {
        if Self::pictureless(asset) || asset.kind == MediaKind::Image {
            return;
        }
        let path = self.path(asset);
        let size = size_for(px);
        let mut st = lock(&self.inner.st);
        let mut any = false;
        for t in times {
            any |= st.request(&path, size, time_key(*t), true);
        }
        drop(st);
        if any {
            self.inner.cv.notify_all();
        }
    }

    fn failed(&self, asset: &MediaAsset) -> bool {
        if Self::pictureless(asset) {
            return false;
        }
        let path = self.path(asset);
        lock(&self.inner.st).failed.get(&path).copied().unwrap_or(false)
    }

    fn waveform(&self, asset: &MediaAsset) -> Option<Arc<Waveform>> {
        if asset.kind == MediaKind::Image || (asset.kind == MediaKind::Video && !asset.info.has_audio) {
            return None;
        }
        let path = self.path(asset);
        let mut st = lock(&self.inner.st);
        match st.waves.get(&path) {
            Some(Wave::Ready(w)) => Some(w.clone()),
            Some(_) => None,
            None => {
                st.waves.insert(path.clone(), Wave::Loading);
                st.wave_queue.push_back(path);
                drop(st);
                self.inner.cv.notify_all();
                None
            }
        }
    }
}

fn upload(inner: &Inner, path: &Path, size: u32, key: i64, f: &aiframes_media::Frame) -> Option<(TextureHandle, usize)> {
    if f.width == 0 || f.height == 0 || f.rgba.len() != f.width as usize * f.height as usize * 4 {
        return None;
    }
    let img = egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
    // Mipmaps add a third.
    let bytes = f.rgba.len() * 4 / 3;
    Some((inner.ctx.load_texture(format!("thumb:{}:{size}:{key}", path.display()), img, THUMB_TEXTURE), bytes))
}

/// Picks the poster moment of a video: the first candidate that is not blank (else the first).
fn choose_poster(pool: &MediaPool, path: &Path, duration: f64) -> Option<f64> {
    let mut first = None;
    for frac in POSTER_AT {
        let t = time_key(duration * frac) as f64 / 10.0;
        match pool.thumbnail(path, t, POSTER_PROBE) {
            Ok(f) => {
                first.get_or_insert(t);
                if !is_blank(&f.rgba) {
                    return Some(t);
                }
            }
            Err(_) if first.is_none() && frac == POSTER_AT[0] => return None,
            Err(_) => {}
        }
    }
    first
}

fn thumb_worker(inner: Arc<Inner>, n: usize) {
    loop {
        let job = {
            let mut st = lock(&inner.st);
            loop {
                if st.stop {
                    return;
                }
                let paused = n > 0 && inner.throttle.load(std::sync::atomic::Ordering::Relaxed);
                // Most recent request first: that is what is on screen now; prefetches after.
                if !paused {
                    if let Some(j) = st.queue.pop_back() {
                        break j;
                    }
                    if n > 0 || !inner.throttle.load(std::sync::atomic::Ordering::Relaxed) {
                        if let Some(j) = st.low.pop_front() {
                            break j;
                        }
                    }
                }
                st = inner.cv.wait_timeout(st, std::time::Duration::from_millis(200)).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0);
            }
        };
        match job {
            Job::Poster { path, duration } => {
                let chosen = choose_poster(&inner.pool, &path, duration);
                let mut st = lock(&inner.st);
                match chosen {
                    Some(t) => {
                        st.posters.insert(path, Poster::Ready(t));
                    }
                    None => {
                        st.posters.insert(path.clone(), Poster::Ready(duration * 0.1));
                        st.failed.insert(path, true);
                    }
                }
            }
            Job::Thumb { path, size, key } => {
                let res = inner.pool.thumbnail(&path, key as f64 / 10.0, size);
                let tex = match &res {
                    Ok(f) => upload(&inner, &path, size, key, f),
                    Err(_) => None,
                };
                let mut st = lock(&inner.st);
                st.tick += 1;
                let tick = st.tick;
                // Still wanted (the file was not forgotten meanwhile)?
                let wanted = st.thumbs.get(&path).and_then(|s| s.get(&size)).is_some_and(|m| m.contains_key(&key));
                if wanted {
                    match tex {
                        Some((t, bytes)) => {
                            st.map(&path, size).insert(key, Thumb::Ready(t, tick, bytes));
                            st.bytes += bytes;
                            st.ready += 1;
                            st.failed.insert(path.clone(), false);
                            if st.bytes > TEXTURE_BUDGET + TEXTURE_BUDGET / 8 {
                                evict(&mut st);
                            }
                        }
                        None => {
                            st.map(&path, size).insert(key, Thumb::Failed);
                            // A file that never gave a picture is marked; one that did stays fine.
                            st.failed.entry(path.clone()).or_insert(true);
                        }
                    }
                }
            }
        }
        inner.ctx.request_repaint();
    }
}

/// Drops the least recently used thumbnails down to the byte budget.
fn evict(st: &mut State) {
    let mut all: Vec<(u64, PathBuf, u32, i64, usize)> = Vec::with_capacity(st.ready);
    for (p, sizes) in &st.thumbs {
        for (s, m) in sizes {
            for (k, t) in m {
                if let Thumb::Ready(_, used, b) = t {
                    all.push((*used, p.clone(), *s, *k, *b));
                }
            }
        }
    }
    all.sort_by_key(|x| x.0);
    let mut bytes: usize = all.iter().map(|x| x.4).sum();
    for (_, p, s, k, b) in all {
        if bytes <= TEXTURE_BUDGET {
            break;
        }
        if let Some(m) = st.thumbs.get_mut(&p).and_then(|x| x.get_mut(&s)) {
            m.remove(&k);
            bytes -= b;
        }
    }
    st.bytes = bytes;
    st.ready = st.thumbs.values().flat_map(|s| s.values()).map(|m| m.values().filter(|t| matches!(t, Thumb::Ready(..))).count()).sum();
}

fn wave_worker(inner: Arc<Inner>) {
    loop {
        let path = {
            let mut st = lock(&inner.st);
            loop {
                if st.stop {
                    return;
                }
                if let Some(p) = st.wave_queue.pop_front() {
                    break p;
                }
                st = inner.cv.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        };
        let w = match inner.pool.waveform(&path) {
            Ok(w) => Wave::Ready(w),
            Err(_) => Wave::Failed,
        };
        lock(&inner.st).waves.insert(path, w);
        inner.ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        let a = MediaAsset { id: "m1".into(), name: String::new(), path: "assets/../clips/a.mp4".into(), kind: MediaKind::Video, info: Default::default(), tags: vec![], notes: String::new() };
        let p = media_path(Path::new("C:/proj"), &a);
        assert_eq!(p, PathBuf::from("C:/proj/clips/a.mp4"));
        let abs = MediaAsset { path: "C:/media/b.wav".into(), ..a };
        assert_eq!(media_path(Path::new("C:/proj"), &abs), PathBuf::from("C:/media/b.wav"));
    }

    #[test]
    fn keys() {
        assert_eq!(time_key(1.26), 13);
        assert_eq!(time_key(-1.0), 0);
        assert_eq!(time_key(f64::NAN), 0);
    }

    #[test]
    fn sizes_cover_the_box() {
        // A 30 pt filmstrip row on a 1.5x screen is 45 px: decoded at 54, not upscaled from less.
        assert_eq!(size_for(45.0), 54);
        assert_eq!(size_for(54.0), 54);
        assert_eq!(size_for(56.0), 54, "within 5 %");
        assert_eq!(size_for(58.0), 108);
        // A 146 pt bin tile at 1.5x (219 px) gets 216 (within 5 %); bigger ones 432, capped there.
        assert_eq!(size_for(219.0), 216);
        assert_eq!(size_for(240.0), 432);
        assert_eq!(size_for(5000.0), 432);
        assert_eq!(size_for(f32::NAN), 108);
        assert_eq!(size_for(0.0), 54);
    }

    #[test]
    fn blank_frames_make_poor_posters() {
        let frame = |f: &dyn Fn(usize) -> [u8; 4]| (0..64 * 36).flat_map(f).collect::<Vec<u8>>();
        assert!(is_blank(&frame(&|_| [0, 0, 0, 255])), "black");
        assert!(is_blank(&frame(&|_| [12, 10, 14, 255])), "near-black fade");
        assert!(is_blank(&frame(&|_| [200, 40, 40, 255])), "one flat colour");
        assert!(is_blank(&[]));
        assert!(!is_blank(&frame(&|i| if (i / 8) % 2 == 0 { [230, 230, 230, 255] } else { [30, 60, 90, 255] })), "a picture");
        // A dark but detailed night shot is still a picture.
        assert!(!is_blank(&frame(&|i| if i % 5 == 0 { [120, 110, 100, 255] } else { [8, 8, 10, 255] })));
    }

    #[test]
    fn stand_ins_stay_close() {
        let ctx = egui::Context::default();
        let tex = |n: &str| ctx.load_texture(n, egui::ColorImage::filled([2, 2], egui::Color32::WHITE), TextureOptions::LINEAR);
        let mut st = State::default();
        let p = Path::new("a.mp4");
        let (a, b, c) = (tex("a"), tex("b"), tex("c"));
        let (ida, idb, idc) = (a.id(), b.id(), c.id());
        st.map(p, 54).insert(100, Thumb::Ready(a, 1, 4));
        st.map(p, 108).insert(105, Thumb::Ready(b, 1, 4));
        st.map(p, 216).insert(300, Thumb::Ready(c, 1, 4));
        // The same moment at another size wins over a nearer-in-time frame at this size.
        assert_eq!(st.stand_in(p, 216, 300, 0), Some(idc));
        assert_eq!(st.stand_in(p, 54, 300, 0), Some(idc));
        // Nearest in time within the tolerance (any size).
        assert_eq!(st.stand_in(p, 108, 103, 5), Some(idb));
        assert_eq!(st.stand_in(p, 54, 101, 5), Some(ida));
        // Nothing that close: no stand-in (a far frame would be the wrong picture).
        assert_eq!(st.stand_in(p, 108, 200, 20), None);
        assert_eq!(st.stand_in(p, 108, 200, 0), None);
        assert_eq!(st.stand_in(Path::new("other.mp4"), 108, 100, i64::MAX), None);
        // Requests are queued once.
        assert!(st.request(p, 108, 7, false));
        assert!(!st.request(p, 108, 7, false));
        assert!(!st.request(p, 108, 105, true), "already loaded");
        assert_eq!((st.queue.len(), st.low.len()), (1, 0));
        // Hand the texture bookkeeping to a frame so the context drops cleanly.
        drop(st);
        let mut out = ctx.run_ui(Default::default(), |_| {});
        out.textures_delta.clear();
    }
}
