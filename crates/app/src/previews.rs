//! Thumbnails and waveforms for the widgets ([`aiframes_ui::MediaPreview`]).
//!
//! Requests never block: a miss queues a job for a small worker pool (most recent request first, so
//! what is on screen loads first), returns the nearest thumbnail already loaded for that file (no
//! flicker while zooming), and the worker uploads the texture and requests a repaint. Textures are
//! LRU-limited.

use aiframes_core::{MediaAsset, MediaKind};
use aiframes_media::{MediaPool, Waveform};
use eframe::egui::{self, TextureHandle, TextureOptions};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

/// Thumbnail height in pixels (filmstrips and bin tiles).
pub const THUMB_HEIGHT: u32 = 90;
/// Most thumbnail textures kept.
const MAX_THUMBS: usize = 600;
/// Pending thumbnail jobs kept (older requests are forgotten and re-requested when visible again).
const MAX_QUEUE: usize = 256;
const THUMB_WORKERS: usize = 3;

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

enum Thumb {
    Loading,
    Ready(TextureHandle, u64),
    Failed,
}

enum Wave {
    Loading,
    Ready(Arc<Waveform>),
    Failed,
}

#[derive(Default)]
struct State {
    thumbs: HashMap<PathBuf, BTreeMap<i64, Thumb>>,
    ready: usize,
    waves: HashMap<PathBuf, Wave>,
    queue: VecDeque<(PathBuf, i64)>,
    wave_queue: VecDeque<PathBuf>,
    /// Waveforms of the other sound streams (files with several), by (file, stream >= 1).
    stream_waves: HashMap<(PathBuf, u32), Wave>,
    stream_wave_queue: VecDeque<(PathBuf, u32)>,
    tick: u64,
    stop: bool,
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
        Previews { inner, dir: std::cell::RefCell::new(PathBuf::from(".")) }
    }

    /// Throttles background thumbnail decoding while the viewer plays.
    pub fn set_playing(&self, playing: bool) {
        let was = self.inner.throttle.swap(playing, std::sync::atomic::Ordering::Relaxed);
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

    /// Forgets a file no longer used by any open project (its textures and waveform).
    pub fn forget(&self, path: &Path) {
        let mut st = lock(&self.inner.st);
        if let Some(m) = st.thumbs.remove(path) {
            let n = m.values().filter(|t| matches!(t, Thumb::Ready(..))).count();
            st.ready -= n.min(st.ready);
        }
        st.waves.remove(path);
        st.stream_waves.retain(|(p, _), _| p != path);
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

    pub fn stats(&self) -> (usize, usize) {
        let st = lock(&self.inner.st);
        (st.ready, st.queue.len())
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
        if asset.kind == MediaKind::Audio || (asset.kind == MediaKind::Video && !asset.info.has_video && asset.info.width == 0) {
            return None;
        }
        let path = self.path(asset);
        let key = if asset.kind == MediaKind::Image { 0 } else { time_key(t) };
        let mut st = lock(&self.inner.st);
        st.tick += 1;
        let tick = st.tick;
        let mut queue_it = false;
        let result = {
            let map = st.thumbs.entry(path.clone()).or_default();
            match map.get_mut(&key) {
                Some(Thumb::Ready(tex, used)) => {
                    *used = tick;
                    return Some(tex.id());
                }
                Some(Thumb::Failed) => None,
                Some(Thumb::Loading) => nearest(map, key),
                None => {
                    map.insert(key, Thumb::Loading);
                    queue_it = true;
                    nearest(map, key)
                }
            }
        };
        if queue_it {
            st.queue.push_back((path, key));
            while st.queue.len() > MAX_QUEUE {
                if let Some((p, k)) = st.queue.pop_front() {
                    if let Some(m) = st.thumbs.get_mut(&p) {
                        if matches!(m.get(&k), Some(Thumb::Loading)) {
                            m.remove(&k);
                        }
                    }
                }
            }
            drop(st);
            self.inner.cv.notify_all();
        }
        result
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

    fn waveform_stream(&self, asset: &MediaAsset, stream: u32) -> Option<Arc<Waveform>> {
        if stream == 0 {
            return self.waveform(asset);
        }
        if asset.kind == MediaKind::Image || (stream as usize) >= asset.info.audio_stream_count() {
            return None;
        }
        let key = (self.path(asset), stream);
        let mut st = lock(&self.inner.st);
        match st.stream_waves.get(&key) {
            Some(Wave::Ready(w)) => Some(w.clone()),
            Some(_) => None,
            None => {
                st.stream_waves.insert(key.clone(), Wave::Loading);
                st.stream_wave_queue.push_back(key);
                drop(st);
                self.inner.cv.notify_all();
                None
            }
        }
    }
}

/// The loaded thumbnail closest in time to `key` (a stand-in while the exact one loads).
fn nearest(map: &BTreeMap<i64, Thumb>, key: i64) -> Option<egui::TextureId> {
    let before = map.range(..key).rev().find_map(|(k, t)| if let Thumb::Ready(tex, _) = t { Some((*k, tex.id())) } else { None });
    let after = map.range(key..).find_map(|(k, t)| if let Thumb::Ready(tex, _) = t { Some((*k, tex.id())) } else { None });
    match (before, after) {
        (Some(b), Some(a)) => Some(if key - b.0 <= a.0 - key { b.1 } else { a.1 }),
        (Some(b), None) => Some(b.1),
        (None, Some(a)) => Some(a.1),
        (None, None) => None,
    }
}

fn thumb_worker(inner: Arc<Inner>, n: usize) {
    loop {
        let (path, key) = {
            let mut st = lock(&inner.st);
            loop {
                if st.stop {
                    return;
                }
                let paused = n > 0 && inner.throttle.load(std::sync::atomic::Ordering::Relaxed);
                // Most recent request first: that is what is on screen now.
                if !paused {
                    if let Some(j) = st.queue.pop_back() {
                        break j;
                    }
                }
                st = inner.cv.wait_timeout(st, std::time::Duration::from_millis(200)).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0);
            }
        };
        let res = inner.pool.thumbnail(&path, key as f64 / 10.0, THUMB_HEIGHT);
        let tex = match res {
            Ok(f) if f.width > 0 && f.height > 0 && f.rgba.len() == f.width as usize * f.height as usize * 4 => {
                let img = egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
                Some(inner.ctx.load_texture(format!("thumb:{}:{key}", path.display()), img, TextureOptions::LINEAR))
            }
            _ => None,
        };
        let mut st = lock(&inner.st);
        st.tick += 1;
        let tick = st.tick;
        let mut added = false;
        if let Some(m) = st.thumbs.get_mut(&path) {
            match tex {
                Some(t) => {
                    m.insert(key, Thumb::Ready(t, tick));
                    added = true;
                }
                None => {
                    m.insert(key, Thumb::Failed);
                }
            }
        }
        if added {
            st.ready += 1;
            if st.ready > MAX_THUMBS + 64 {
                evict(&mut st);
            }
        }
        drop(st);
        inner.ctx.request_repaint();
    }
}

/// Drops the least recently used thumbnails down to the limit.
fn evict(st: &mut State) {
    let mut all: Vec<(u64, PathBuf, i64)> = Vec::with_capacity(st.ready);
    for (p, m) in &st.thumbs {
        for (k, t) in m {
            if let Thumb::Ready(_, used) = t {
                all.push((*used, p.clone(), *k));
            }
        }
    }
    all.sort_by_key(|(u, _, _)| *u);
    let excess = all.len().saturating_sub(MAX_THUMBS);
    for (_, p, k) in all.into_iter().take(excess) {
        if let Some(m) = st.thumbs.get_mut(&p) {
            m.remove(&k);
        }
    }
    st.ready = st.thumbs.values().map(|m| m.values().filter(|t| matches!(t, Thumb::Ready(..))).count()).sum();
}

fn wave_worker(inner: Arc<Inner>) {
    loop {
        let (path, stream) = {
            let mut st = lock(&inner.st);
            loop {
                if st.stop {
                    return;
                }
                if let Some(p) = st.wave_queue.pop_front() {
                    break (p, 0);
                }
                if let Some(k) = st.stream_wave_queue.pop_front() {
                    break k;
                }
                st = inner.cv.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        };
        let w = match inner.pool.waveform_stream(&path, stream) {
            Ok(w) => Wave::Ready(w),
            Err(_) => Wave::Failed,
        };
        if stream == 0 {
            lock(&inner.st).waves.insert(path, w);
        } else {
            lock(&inner.st).stream_waves.insert((path, stream), w);
        }
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
}
