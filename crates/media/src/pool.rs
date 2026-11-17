//! `MediaPool`: the shared, thread-safe entry point for pictures and sound of media files.
//!
//! Video frames come from long-lived ffmpeg decode sessions (one per file and output size, up to
//! three per pair so a transition between two parts of the same file does not thrash), which
//! stream forward and are only re-seeked on backward or long forward jumps. Decoded frames are kept
//! in a byte-bounded LRU cache keyed by the interval of source time they are displayed for.

use crate::audio::{self, AudioData};
use crate::cache::{us, FrameCache, StreamKey, StreamKind};
use crate::decode::{HwAccel, Served, Session, SessionSpec, KEEP_PASSED};
use crate::error::{MediaError, Result};
use crate::probe::{probe_full, Probed};
use crate::still;
use crate::tools::lock;
use crate::util::{cache_name, commit_temp, default_cache_dir, ensure_dir, file_key, fit_size, temp_sibling, FileKey, Inflight};
use crate::waveform::Waveform;
use aiframes_core::model::{MediaInfo, MediaKind};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A decoded picture: straight (non-premultiplied) RGBA8, row-major, `width * height * 4` bytes.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
    /// Source seconds of the frame's presentation timestamp (0 for stills; for thumbnails the
    /// requested time rounded to the thumbnail grid).
    pub time: f64,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({}x{} @ {:.6}s)", self.width, self.height, self.time)
    }
}

impl Frame {
    /// The RGBA of pixel (x, y), or `None` outside the frame.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y as usize * self.width as usize + x as usize) * 4;
        self.rgba.get(i..i + 4).map(|p| [p[0], p[1], p[2], p[3]])
    }
}

/// How a decoded video frame is kept in the pool's shared frame cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FrameCaching {
    /// Cached like any frame (playback, snapshots).
    #[default]
    Normal,
    /// Cached at low priority: these frames never push normal frames below half the cache (they
    /// may use space normal frames leave free, and playback takes it back), so an export — whose
    /// prefetch thread decodes ahead for its render workers — cannot evict the playback cache.
    LowPriority,
    /// Not cached at all (frames already cached are still returned). Only for a single consumer
    /// reading strictly forward: other readers of the same times would have to decode again.
    Off,
    /// Cached like `Normal`, but decoded by a session of its own: never one that playback or
    /// export is reading from (a request ahead of their position would move it past them and
    /// make them seek back). Playback opens the next clip's decoder this way before a cut.
    /// Skipped (an error) when every decoder of the file is busy.
    Ahead,
}

/// Counters for diagnostics (`MediaPool::stats`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PoolStats {
    pub cache_bytes: usize,
    pub cache_budget: usize,
    pub cached_frames: usize,
    pub cache_hits: u64,
    pub cache_misses: u64,
    /// Live ffmpeg decode sessions.
    pub sessions: usize,
    /// Decode sessions started (initial opens + re-seeks).
    pub seeks: u64,
    /// Frames read from decode sessions.
    pub frames_decoded: u64,
    /// Memory held by decode sessions in this process: frames decoded ahead plus each session's
    /// current/next frame (the ffmpeg processes' own memory is not included).
    pub session_bytes: usize,
    /// The readahead budget shared by all sessions (`MediaPool::set_readahead_bytes`).
    pub readahead_budget: usize,
    /// Bytes of the frame cache held by low-priority frames (`FrameCaching::LowPriority`).
    pub cache_low_priority_bytes: usize,
}

/// Decoded audio by (file, rate), with a last-use tick.
type AudioMap = HashMap<(u64, u32), (Arc<AudioData>, u64)>;

/// Sessions kept per (file, output size) pair.
const MAX_PER_KEY: usize = 3;
/// Total live decode sessions before idle ones are closed.
const MAX_SESSIONS: usize = 12;
/// Sessions in sequential use (playback, export) that go unused this long are closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// Sessions that served a single request (snapshots, random access) are closed this soon...
const PROVISIONAL_IDLE: Duration = Duration::from_secs(2);
/// ...and only the most recent few are kept at all (a seek followed by Play reuses the newest).
const MAX_PROVISIONAL: usize = 2;
/// Requests a session must serve after the one it was opened for, without re-seeking, to count
/// as in sequential use (kept for `IDLE_TIMEOUT`, given a share of the readahead budget). One is
/// enough: a request at about the previous one + a frame means playback or export.
const SEQUENTIAL: u32 = 1;
/// A session used this recently is in active use and never reaped to free memory.
const ACTIVE_MS: u64 = 500;
/// How often idle sessions are looked for.
const REAP_INTERVAL_MS: u64 = 250;
/// Default memory budget for frames decoded ahead, shared by all sessions.
pub const DEFAULT_READAHEAD_BYTES: usize = 128 * 1024 * 1024;
/// One-off requests open lean sessions (fewer decoder threads, less memory) once this many
/// sessions are already running.
const LEAN_FROM_SESSIONS: usize = 2;
/// Requests at most this big (pixels) that are downscales of the source are "small": thumbnails,
/// contact sheets, filmstrips. They reuse larger cached frames or decode one-shot.
const SMALL_PIXELS: u64 = 480 * 270;
/// Thumbnails are cached on this grid (seconds).
const THUMB_STEP: f64 = 0.1;
/// Files with at most this much audio are decoded completely inside `audio()`.
const SYNC_AUDIO_SECS: f64 = 30.0;

/// Whether `HwAccel::Auto` decodes this stream on the GPU: streams whose CPU decode is the
/// bottleneck and that GPUs decode (HEVC, VP9, AV1, and 8-bit 4:2:0 H.264 above 1440p); see
/// `HwAccel` for the measurements. Everything else (1080p H.264, 10-bit H.264, ProRes, DNxHD, ...)
/// stays on the CPU.
fn auto_wants_gpu(p: &Probed) -> bool {
    let codec = p.info.codec.to_ascii_lowercase();
    let big = p.info.width as u64 * p.info.height as u64 > 2560 * 1440;
    match codec.as_str() {
        "hevc" | "h265" | "vp9" | "av1" => true,
        "h264" => big && matches!(p.pix_fmt.as_str(), "yuv420p" | "yuvj420p" | "nv12"),
        _ => false,
    }
}

/// Short files are decoded inside `audio()`; an unknown (0 / NaN) duration counts as long.
fn decode_synchronously(duration: f64) -> bool {
    duration.is_finite() && duration > 0.0 && duration <= SYNC_AUDIO_SECS
}
/// Concurrent thumbnail decodes (keeps the machine responsive during playback).
const THUMB_CONCURRENCY: usize = 4;
/// A request this far (or less) behind a session counts as stepping backward: the new session
/// starts `BACK_WINDOW` earlier and caches every frame on the way, so further steps back are hits.
const BACK_STEP: f64 = 2.0;
const BACK_WINDOW: f64 = 1.0;

struct Slot {
    session: Mutex<Option<Session>>,
    /// f64 bits of the session position; NaN when no session.
    pos: AtomicU64,
    serves_earlier: AtomicBool,
    last_used: AtomicU64,
    /// Consecutive requests the current session served without re-seeking (0 = it has only
    /// served the request it was started for). Below `SEQUENTIAL` the session is provisional.
    seq: AtomicU32,
}

impl Slot {
    fn new() -> Self {
        Slot {
            session: Mutex::new(None),
            pos: AtomicU64::new(f64::NAN.to_bits()),
            serves_earlier: AtomicBool::new(false),
            last_used: AtomicU64::new(0),
            seq: AtomicU32::new(0),
        }
    }
    fn pos(&self) -> f64 {
        f64::from_bits(self.pos.load(Ordering::Relaxed))
    }
    fn publish(&self, s: Option<&Session>) {
        match s {
            Some(s) => {
                self.pos.store(s.position().to_bits(), Ordering::Relaxed);
                self.serves_earlier.store(s.serves_earlier(), Ordering::Relaxed);
            }
            None => {
                self.pos.store(f64::NAN.to_bits(), Ordering::Relaxed);
                self.serves_earlier.store(false, Ordering::Relaxed);
            }
        }
    }
}

struct Gate {
    n: Mutex<usize>,
    cv: Condvar,
}

impl Gate {
    fn enter(&self, max: usize) -> GateGuard<'_> {
        let mut n = lock(&self.n);
        while *n >= max {
            n = self.cv.wait(n).unwrap_or_else(|p| p.into_inner());
        }
        *n += 1;
        GateGuard(self)
    }
}

struct GateGuard<'a>(&'a Gate);

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        *lock(&self.0.n) -= 1;
        self.0.cv.notify_one();
    }
}

/// Shared media access for the renderer, the UI and the MCP server. `Send + Sync`; share it with
/// `Arc`. Every method may block (decoding, probing) — call it from worker threads, not the UI thread.
pub struct MediaPool {
    cache_dir: PathBuf,
    cache: Mutex<FrameCache>,
    groups: Mutex<HashMap<StreamKey, Vec<Arc<Slot>>>>,
    stamps: Mutex<HashMap<PathBuf, (FileKey, Instant)>>,
    probes: Mutex<HashMap<u64, Arc<Probed>>>,
    probe_inflight: Inflight<u64, Arc<Probed>>,
    audio: Mutex<AudioMap>,
    audio_inflight: Inflight<(u64, u32), Arc<AudioData>>,
    waves: Mutex<HashMap<u64, (Arc<Waveform>, u64)>>,
    wave_inflight: Inflight<u64, Arc<Waveform>>,
    image_inflight: Inflight<(u64, u32, u32), Frame>,
    thumb_inflight: Inflight<(u64, u32, i64), Frame>,
    thumb_gate: Gate,
    hwaccel: AtomicU8,
    hw_broken: AtomicBool,
    epoch: Instant,
    last_reap: AtomicU64,
    tick: AtomicU64,
    seeks: AtomicU64,
    frames_decoded: AtomicU64,
    /// Last frame time per file, learned when a session reached the end of the video stream.
    ends: Mutex<HashMap<u64, f64>>,
    /// Live decode sessions of this pool (capped at `MAX_SESSIONS` when a new one starts).
    live: Arc<AtomicUsize>,
    /// Bytes held by this pool's sessions (readahead queues + current/next frames).
    session_mem: Arc<AtomicUsize>,
    readahead_budget: AtomicUsize,
    /// Recent request times per stream (sequential-access detection): (source t, wall ms).
    recent: Mutex<HashMap<StreamKey, Vec<(f64, u64)>>>,
}

fn hw_to_u8(h: HwAccel) -> u8 {
    match h {
        HwAccel::Off => 0,
        HwAccel::Auto => 1,
        HwAccel::Cuda => 2,
        HwAccel::D3d11va => 3,
    }
}
fn hw_from_u8(v: u8) -> HwAccel {
    match v {
        0 => HwAccel::Off,
        2 => HwAccel::Cuda,
        3 => HwAccel::D3d11va,
        _ => HwAccel::Auto,
    }
}

impl MediaPool {
    /// A pool whose frame cache holds at most `cache_bytes` of decoded pictures, using the default
    /// on-disk cache folder (`%LOCALAPPDATA%\AIFrames\cache`, or `AIFRAMES_CACHE_DIR`).
    pub fn new(cache_bytes: usize) -> Arc<Self> {
        Self::with_cache_dir(cache_bytes, default_cache_dir())
    }

    /// Like `new` with an explicit on-disk cache folder (audio PCM, waveforms, thumbnails).
    pub fn with_cache_dir(cache_bytes: usize, cache_dir: impl Into<PathBuf>) -> Arc<Self> {
        let hw = std::env::var("AIFRAMES_HWACCEL").ok().and_then(|v| HwAccel::parse(&v)).unwrap_or_default();
        Arc::new(MediaPool {
            cache_dir: cache_dir.into(),
            cache: Mutex::new(FrameCache::new(cache_bytes)),
            groups: Mutex::new(HashMap::new()),
            stamps: Mutex::new(HashMap::new()),
            probes: Mutex::new(HashMap::new()),
            probe_inflight: Inflight::new(),
            audio: Mutex::new(HashMap::new()),
            audio_inflight: Inflight::new(),
            waves: Mutex::new(HashMap::new()),
            wave_inflight: Inflight::new(),
            image_inflight: Inflight::new(),
            thumb_inflight: Inflight::new(),
            thumb_gate: Gate { n: Mutex::new(0), cv: Condvar::new() },
            hwaccel: AtomicU8::new(hw_to_u8(hw)),
            hw_broken: AtomicBool::new(false),
            epoch: Instant::now(),
            last_reap: AtomicU64::new(0),
            tick: AtomicU64::new(0),
            seeks: AtomicU64::new(0),
            frames_decoded: AtomicU64::new(0),
            ends: Mutex::new(HashMap::new()),
            live: Arc::new(AtomicUsize::new(0)),
            session_mem: Arc::new(AtomicUsize::new(0)),
            readahead_budget: AtomicUsize::new(DEFAULT_READAHEAD_BYTES),
            recent: Mutex::new(HashMap::new()),
        })
    }

    /// The on-disk cache folder.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Chooses hardware decoding for new video sessions (default `Auto`, or `AIFRAMES_HWACCEL`).
    pub fn set_hwaccel(&self, h: HwAccel) {
        self.hwaccel.store(hw_to_u8(h), Ordering::Relaxed);
        self.hw_broken.store(false, Ordering::Relaxed);
    }

    pub fn hwaccel(&self) -> HwAccel {
        hw_from_u8(self.hwaccel.load(Ordering::Relaxed))
    }

    /// Changes the frame cache budget (evicting immediately if smaller).
    pub fn set_cache_bytes(&self, bytes: usize) {
        lock(&self.cache).set_budget(bytes);
    }

    /// Sets the memory budget for frames decoded ahead, shared by all decode sessions (default
    /// 128 MB). Sessions in sequential use split it; one-off requests decode ahead one frame; idle
    /// sessions are closed early while the pool is over it.
    pub fn set_readahead_bytes(&self, bytes: usize) {
        self.readahead_budget.store(bytes, Ordering::Relaxed);
        self.enforce_budget(None);
    }

    pub fn readahead_bytes(&self) -> usize {
        self.readahead_budget.load(Ordering::Relaxed)
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// The file's current identity (path + size + mtime). Checked on every call (~15 us), so a
    /// file replaced on disk is never served from caches of its old contents.
    fn stamp(&self, path: &Path) -> Result<FileKey> {
        let now = Instant::now();
        let k = file_key(path)?;
        let mut st = lock(&self.stamps);
        if st.len() > 4096 {
            st.clear();
        }
        st.insert(path.to_path_buf(), (k.clone(), now));
        Ok(k)
    }

    fn lookup(&self, path: &Path) -> Result<(FileKey, Arc<Probed>)> {
        let fk = self.stamp(path)?;
        if let Some(p) = lock(&self.probes).get(&fk.id) {
            return Ok((fk, p.clone()));
        }
        let p = self.probe_inflight.run(fk.id, || probe_full(path).map(Arc::new))?;
        lock(&self.probes).insert(fk.id, p.clone());
        Ok((fk, p))
    }

    /// `probe`, cached per file version.
    pub fn probe(&self, path: &Path) -> Result<(MediaKind, MediaInfo)> {
        self.lookup(path).map(|(_, p)| (p.kind, p.info.clone()))
    }

    // -----------------------------------------------------------------------------------------
    // Video
    // -----------------------------------------------------------------------------------------

    /// The frame displayed at source time `t` (seconds from the start of the file): the last
    /// frame whose timestamp is `<= t`. Times before the first frame give the first frame, times
    /// past the end the last frame. `max` downscales to fit (aspect preserved, never upscaled).
    /// Still images return the image; audio-only files are an error.
    pub fn video_frame(&self, path: &Path, t: f64, max: Option<(u32, u32)>) -> Result<Frame> {
        self.video_frame_with(path, t, max, FrameCaching::Normal)
    }

    /// `video_frame` without inserting the frames it decodes into the shared cache
    /// (`FrameCaching::Off`).
    pub fn video_frame_uncached(&self, path: &Path, t: f64, max: Option<(u32, u32)>) -> Result<Frame> {
        self.video_frame_with(path, t, max, FrameCaching::Off)
    }

    /// `video_frame` with a choice of how decoded frames are cached (export: `LowPriority`).
    pub fn video_frame_with(&self, path: &Path, t: f64, max: Option<(u32, u32)>, caching: FrameCaching) -> Result<Frame> {
        let (fk, p) = self.lookup(path)?;
        match p.kind {
            MediaKind::Image => return self.image(path, max),
            MediaKind::Audio => return Err(MediaError::unsupported(path, "is an audio file; it has no picture")),
            MediaKind::Video => {}
        }
        let index = p.video_index.ok_or_else(|| MediaError::unsupported(path, "has no video stream"))?;
        let (w, h) = fit_size(p.info.width, p.info.height, max);
        let fps = if p.info.fps > 0.0 { p.info.fps } else { 30.0 };
        let eps = (0.25 / fps).min(0.0005);
        let mut t = if t.is_finite() { t.max(0.0) } else { 0.0 };
        let end = self.video_end(fk.id, &p);
        if end > 0.0 {
            t = t.min(end);
        }
        let key = StreamKey { file: fk.id, w, h, kind: StreamKind::Video };
        if let Some(f) = lock(&self.cache).get(&key, us(t), us(eps)) {
            return Ok(f);
        }
        let jump = (60.0 / fps).clamp(0.25, 2.0);
        let sequential = self.note_request(key, t, jump);
        let small = (w as u64) * (h as u64) <= SMALL_PIXELS && (w < p.info.width || h < p.info.height);
        if small && !sequential {
            // A thumbnail-sized one-off: shrink a larger frame of that moment if one is decoded.
            let big = lock(&self.cache).get_larger(fk.id, us(t), us(eps), w, h);
            if let Some(big) = big {
                let f = Frame { width: w, height: h, rgba: Arc::new(still::downscale(&big.rgba, big.width, big.height, w, h)), time: big.time };
                let end = big.time + 0.5 / fps;
                if caching != FrameCaching::Off {
                    lock(&self.cache).insert_with(key, us(big.time), us(end.max(t + eps)), f.clone(), caching == FrameCaching::LowPriority);
                }
                return Ok(f);
            }
        }
        // Otherwise a session: one-offs get a provisional one (at most `MAX_PROVISIONAL` are kept,
        // closed after `PROVISIONAL_IDLE`), so a seek followed by Play continues it. Small sizes
        // decode with fewer threads (less memory per ffmpeg process).
        let mut spec = self.spec_for(path, &p, index, w, h, fps);
        if small {
            spec.threads = Some(4);
        }
        // Lean sessions only pay off when several run at once (snapshot bursts): a lone playback
        // keeps every decoder thread.
        spec.lean = !sequential && self.live.load(Ordering::Relaxed) >= LEAN_FROM_SESSIONS;
        self.decode(key, spec, &p, t, eps, caching)
    }

    /// Records a request; true when it is close to recent requests of the same stream (up to
    /// `jump` after one, or up to `BACK_STEP` before one): playback, export, scrubbing and frame
    /// stepping, as opposed to one-off requests spread over the file (snapshots, contact sheets).
    fn note_request(&self, key: StreamKey, t: f64, jump: f64) -> bool {
        let now = self.now_ms();
        let mut r = lock(&self.recent);
        if r.len() > 512 {
            r.retain(|_, v| v.iter().any(|(_, at)| now.saturating_sub(*at) < 2000));
        }
        let v = r.entry(key).or_default();
        v.retain(|(_, at)| now.saturating_sub(*at) < 2000);
        let seq = v.iter().any(|(pt, _)| t - *pt <= jump && *pt - t <= BACK_STEP);
        v.push((t, now));
        if v.len() > 8 {
            v.remove(0);
        }
        seq
    }

    /// Source time where the file's video ends: the last frame's time once a session has reached
    /// it, else the probed stream end (start offset + duration).
    fn video_end(&self, file: u64, p: &Probed) -> f64 {
        lock(&self.ends).get(&file).copied().unwrap_or(p.video_end)
    }

    /// Starts a decode session counted against this pool's cap (reserved atomically, so a burst
    /// of requests on many threads cannot overshoot `MAX_SESSIONS`).
    fn start_session(&self, spec: &SessionSpec, seek: f64) -> Result<Session> {
        self.reserve_session();
        match Session::start_counted(Arc::new(spec.clone()), seek, self.session_mem.clone()) {
            Ok(mut s) => {
                s.adopt_count(self.live.clone());
                Ok(s)
            }
            Err(e) => {
                self.live.fetch_sub(1, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    /// Takes one of the `MAX_SESSIONS` places, closing the least recently used idle session when
    /// all are taken (waiting briefly if every session is busy, then going over rather than stall).
    fn reserve_session(&self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let n = self.live.load(Ordering::Relaxed);
            if n < MAX_SESSIONS {
                if self.live.compare_exchange(n, n + 1, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                    return;
                }
                continue;
            }
            // An idle session first, then one that served a single request; never one in use by
            // playback or export (rather go over the cap than make it re-seek every frame).
            let now = self.now_ms();
            let idle = |s: &Slot| now.saturating_sub(s.last_used.load(Ordering::Relaxed)) >= ACTIVE_MS;
            if self.close_lru(None, idle) || self.close_lru(None, |s| s.seq.load(Ordering::Relaxed) < SEQUENTIAL) {
                continue;
            }
            if Instant::now() >= deadline {
                self.live.fetch_add(1, Ordering::Relaxed);
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Closes the least recently used idle pooled session matching `pred` (not `keep`); false if
    /// none could be closed.
    fn close_lru(&self, keep: Option<&Arc<Slot>>, pred: impl Fn(&Slot) -> bool) -> bool {
        let mut slots: Vec<Arc<Slot>> = lock(&self.groups)
            .values()
            .flatten()
            .filter(|s| !s.pos().is_nan() && pred(s) && keep.is_none_or(|k| !Arc::ptr_eq(k, s)))
            .cloned()
            .collect();
        slots.sort_by_key(|s| s.last_used.load(Ordering::Relaxed));
        for s in slots {
            if let Ok(mut g) = s.session.try_lock() {
                if g.take().is_some() {
                    s.publish(None);
                    return true;
                }
            }
        }
        false
    }

    /// Keeps at most `MAX_PROVISIONAL` sessions that served a single, isolated request: ones not
    /// used in the last `ACTIVE_MS` (a session opened for one layer of a multi-layer frame is used
    /// again within milliseconds, so it is never counted here).
    fn trim_provisional(&self, keep: &Arc<Slot>) {
        let now = self.now_ms();
        let isolated = |s: &Slot| s.seq.load(Ordering::Relaxed) < SEQUENTIAL && now.saturating_sub(s.last_used.load(Ordering::Relaxed)) >= ACTIVE_MS;
        loop {
            let n = lock(&self.groups).values().flatten().filter(|s| !s.pos().is_nan() && !Arc::ptr_eq(s, keep) && isolated(s)).count();
            if n <= MAX_PROVISIONAL || !self.close_lru(Some(keep), isolated) {
                break;
            }
        }
    }

    /// Frames a session may decode ahead: one until it has proved sequential, then growing with
    /// each sequential request up to its share of the budget (split among sequential sessions).
    fn readahead_frames(&self, seq: u32, frame_bytes: usize) -> usize {
        if seq == 0 {
            return 1;
        }
        let now = self.now_ms();
        let active = lock(&self.groups)
            .values()
            .flatten()
            .filter(|s| !s.pos().is_nan() && s.seq.load(Ordering::Relaxed) >= SEQUENTIAL && now.saturating_sub(s.last_used.load(Ordering::Relaxed)) < 2000)
            .count()
            .max(1);
        // A quarter of the budget stays free for the current/next frames of the other sessions
        // (a decoder opened ahead of a cut, a transition's second clip): readahead filling the
        // whole budget made `enforce_budget` close exactly those.
        let share = self.readahead_budget.load(Ordering::Relaxed) / 4 * 3 / active;
        (share / frame_bytes.max(1)).clamp(1, 24).min(2usize << seq.min(4))
    }

    /// While sessions hold more than the budget, closes the least recently used idle ones
    /// (never one used in the last `ACTIVE_MS`, never `keep`). Sessions opened in the last
    /// `PROVISIONAL_IDLE` for a single request (playback opens the next clip's decoder ahead of a
    /// cut) are only closed when memory is well over the budget.
    fn enforce_budget(&self, keep: Option<&Arc<Slot>>) {
        let budget = self.readahead_budget.load(Ordering::Relaxed);
        let used = self.session_mem.load(Ordering::Relaxed);
        if used <= budget {
            return;
        }
        let critical = used > budget / 2 * 3;
        let now = self.now_ms();
        let fresh = |s: &Slot| s.seq.load(Ordering::Relaxed) < SEQUENTIAL && now.saturating_sub(s.last_used.load(Ordering::Relaxed)) < PROVISIONAL_IDLE.as_millis() as u64;
        let mut slots: Vec<Arc<Slot>> = lock(&self.groups)
            .values()
            .flatten()
            .filter(|s| !s.pos().is_nan() && now.saturating_sub(s.last_used.load(Ordering::Relaxed)) >= ACTIVE_MS)
            .filter(|s| critical || !fresh(s))
            .filter(|s| keep.is_none_or(|k| !Arc::ptr_eq(k, s)))
            .cloned()
            .collect();
        slots.sort_by_key(|s| s.last_used.load(Ordering::Relaxed));
        for s in slots {
            if self.session_mem.load(Ordering::Relaxed) <= budget {
                break;
            }
            if let Ok(mut g) = s.session.try_lock() {
                if g.take().is_some() {
                    s.publish(None);
                }
            }
        }
    }

    fn resolve_hw(&self, p: &Probed) -> HwAccel {
        if self.hw_broken.load(Ordering::Relaxed) || p.vpx_alpha.is_some() {
            return HwAccel::Off;
        }
        match self.hwaccel() {
            HwAccel::Auto if cfg!(windows) && auto_wants_gpu(p) => HwAccel::D3d11va,
            HwAccel::Auto => HwAccel::Off,
            other => other,
        }
    }

    fn spec_for(&self, path: &Path, p: &Probed, index: usize, w: u32, h: u32, fps: f64) -> SessionSpec {
        let matrix = p.untagged_matrix.then_some(if p.info.height >= 720 || p.info.width >= 1280 { "bt709" } else { "bt601" });
        SessionSpec {
            path: path.to_path_buf(),
            video_index: index,
            out_w: w,
            out_h: h,
            fps,
            matrix,
            decoder: p.vpx_alpha,
            hw: self.resolve_hw(p),
            threads: None,
            readahead: 1,
            background: false,
            lean: false,
        }
    }

    /// Picks the session best placed to serve `t` (creating one if needed). `ahead`: never a
    /// session in sequential or recent use, and none at all when every slot is taken.
    fn pick(&self, key: StreamKey, t: f64, eps: f64, jump: f64, ahead: bool) -> Option<(Arc<Slot>, bool)> {
        let mut groups = lock(&self.groups);
        self.reap(&mut groups);
        let now = self.now_ms();
        let g = groups.entry(key).or_default();
        let busy = |s: &Slot| s.seq.load(Ordering::Relaxed) >= SEQUENTIAL || now.saturating_sub(s.last_used.load(Ordering::Relaxed)) < ACTIVE_MS;
        let stepping_back = g.iter().any(|s| {
            let d = s.pos() - t;
            d > eps && d <= BACK_STEP
        });
        let mut best: Option<(f64, &Arc<Slot>)> = None;
        for s in g.iter() {
            let pos = s.pos();
            if pos.is_nan() || (ahead && busy(s)) {
                continue;
            }
            let ahead_ok = pos <= t + eps || s.serves_earlier.load(Ordering::Relaxed);
            let d = (t - pos).max(0.0);
            if ahead_ok && d <= jump && best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, s));
            }
        }
        if let Some((_, s)) = best {
            return Some((s.clone(), false));
        }
        // Not taking over the session we stepped back from: in a crossfade between two overlapping
        // parts of one file, both sessions keep streaming.
        // Prefer an idle (unstarted or unlocked) slot, else open another one, else the least recent.
        if let Some(s) = g.iter().find(|s| s.pos().is_nan() && s.session.try_lock().is_ok()) {
            return Some((s.clone(), stepping_back));
        }
        if g.len() < MAX_PER_KEY {
            let s = Arc::new(Slot::new());
            g.push(s.clone());
            return Some((s, stepping_back));
        }
        let s = g.iter().filter(|s| !ahead || !busy(s)).min_by_key(|s| s.last_used.load(Ordering::Relaxed)).cloned()?;
        Some((s, stepping_back))
    }

    /// Closes idle sessions and enforces the global session cap (never blocks on a busy session).
    fn reap(&self, groups: &mut HashMap<StreamKey, Vec<Arc<Slot>>>) {
        let now = self.now_ms();
        if now.saturating_sub(self.last_reap.load(Ordering::Relaxed)) < REAP_INTERVAL_MS {
            return;
        }
        self.last_reap.store(now, Ordering::Relaxed);
        let mut live: Vec<(u64, Arc<Slot>)> = Vec::new();
        for g in groups.values_mut() {
            g.retain(|s| {
                // A session that only served the request it was opened for is not kept long.
                let limit = if s.seq.load(Ordering::Relaxed) < SEQUENTIAL { PROVISIONAL_IDLE } else { IDLE_TIMEOUT };
                let idle = now.saturating_sub(s.last_used.load(Ordering::Relaxed)) > limit.as_millis() as u64;
                if idle {
                    if let Ok(mut sess) = s.session.try_lock() {
                        *sess = None;
                        s.publish(None);
                        return false;
                    }
                }
                if !s.pos().is_nan() {
                    live.push((s.last_used.load(Ordering::Relaxed), s.clone()));
                }
                true
            });
        }
        groups.retain(|_, g| !g.is_empty());
        if live.len() > MAX_SESSIONS {
            live.sort_by_key(|(t, _)| *t);
            let excess = live.len() - MAX_SESSIONS;
            for (_, s) in live.into_iter().take(excess) {
                if let Ok(mut sess) = s.session.try_lock() {
                    *sess = None;
                    s.publish(None);
                }
            }
        }
    }

    fn decode(&self, key: StreamKey, spec: SessionSpec, p: &Probed, t: f64, eps: f64, caching: FrameCaching) -> Result<Frame> {
        let jump = (60.0 / spec.fps).clamp(0.25, 2.0);
        let ahead = caching == FrameCaching::Ahead;
        let Some((slot, stepping_back)) = self.pick(key, t, eps, jump, ahead) else {
            return Err(MediaError::InvalidInput(format!("no free decoder to read {} ahead", spec.path.display())));
        };
        self.decode_in(slot, stepping_back, key, spec, p, t, eps, caching)
    }

    /// Serves `t` from the session in `slot` (a pooled slot, or a temporary one for a one-shot
    /// decode whose session ends with it).
    #[allow(clippy::too_many_arguments)]
    fn decode_in(
        &self,
        slot: Arc<Slot>,
        stepping_back: bool,
        key: StreamKey,
        mut spec: SessionSpec,
        p: &Probed,
        t: f64,
        eps: f64,
        caching: FrameCaching,
    ) -> Result<Frame> {
        let fps = spec.fps;
        let jump = (60.0 / fps).clamp(0.25, 2.0);
        // Preroll before `t` for a new session: 1.5 frames covers constant-rate sources; variable
        // rate ones may have long gaps between frames, so retries widen it.
        let base = if p.vfr { 0.5 } else { 1.5 / fps + 0.01 };
        let first = if stepping_back { base.max(BACK_WINDOW) } else { base };
        // Widening pre-rolls. A seek that finds no frames at all (past the end of the video) is
        // remembered, and the end is then narrowed down by bisection instead of decoding from 0.
        let prerolls = [first, 1.0_f64.max(first), 4.0, 15.0, 60.0, 240.0, 960.0, f64::INFINITY];
        let keep = if stepping_back { usize::MAX } else { KEEP_PASSED };
        slot.last_used.store(self.now_ms(), Ordering::Relaxed);
        let mut guard = lock(&slot.session);
        // Another thread may have produced this frame while we waited.
        if let Some(f) = lock(&self.cache).get(&key, us(t), us(eps)) {
            return Ok(f);
        }
        let mut attempt = 0usize;
        let mut restart = match guard.as_ref() {
            None => true,
            Some(s) => {
                let pos = s.position();
                let ahead_ok = pos <= t + eps || s.serves_earlier();
                !s.spec.compatible(&spec) || !ahead_ok || t - pos > jump
            }
        };
        let mut last_err: Option<MediaError> = None;
        let mut empty_at: Option<f64> = None; // lowest seek known to give no frames
        loop {
            if restart {
                if attempt >= prerolls.len() {
                    break;
                }
                let pre = prerolls[attempt];
                let seek = if pre.is_infinite() || t - pre < 0.3 { 0.0 } else { t - pre };
                *guard = None; // kill the old process before starting the next
                // Requests for the next frames (other render workers) wait for this session
                // instead of starting their own at nearly the same place.
                slot.pos.store(seek.to_bits(), Ordering::Relaxed);
                slot.serves_earlier.store(seek <= 0.0, Ordering::Relaxed);
                self.seeks.fetch_add(1, Ordering::Relaxed);
                let mut s = match self.start_session(&spec, seek) {
                    Ok(s) => s,
                    Err(e) => {
                        slot.publish(None);
                        return Err(e);
                    }
                };
                if let Some(hi) = empty_at {
                    // The video ends inside [seek, hi): bisect to within 2 s before decoding through.
                    if hi - seek > 2.0 && s.has_frames().unwrap_or(false) {
                        let (mut lo, mut hi) = (seek, hi);
                        while hi - lo > 2.0 {
                            let mid = 0.5 * (lo + hi);
                            self.seeks.fetch_add(1, Ordering::Relaxed);
                            let mut m = self.start_session(&spec, mid)?;
                            if m.has_frames().unwrap_or(false) {
                                lo = mid;
                                s = m;
                            } else {
                                hi = mid;
                            }
                        }
                    }
                }
                *guard = Some(s);
            }
            let sess = guard.as_mut().expect("session");
            let before = sess.frames;
            let served = sess.frame_at(t, eps, if restart { keep } else { KEEP_PASSED });
            self.frames_decoded.fetch_add(sess.frames - before, Ordering::Relaxed);
            match served {
                Ok(Served::Frame { frame, start, end, passed }) => {
                    if end.is_infinite() {
                        lock(&self.ends).insert(key.file, frame.pts); // the stream's last frame
                    }
                    // Sequential use earns readahead; a fresh session decodes one frame ahead.
                    let seq = if restart { 0 } else { slot.seq.load(Ordering::Relaxed).saturating_add(1) };
                    slot.seq.store(seq, Ordering::Relaxed);
                    sess.set_readahead(self.readahead_frames(seq, spec.frame_bytes()));
                    slot.publish(Some(sess));
                    let out = Frame { width: spec.out_w, height: spec.out_h, rgba: frame.rgba, time: frame.pts };
                    if caching != FrameCaching::Off {
                        let low = caching == FrameCaching::LowPriority;
                        let mut c = lock(&self.cache);
                        for (pf, pend) in passed {
                            let f = Frame { width: spec.out_w, height: spec.out_h, rgba: pf.rgba, time: pf.pts };
                            c.insert_with(key, us(pf.pts), us(pend), f, low);
                        }
                        c.insert_with(key, us(start), us(end), out.clone(), low);
                    }
                    drop(guard);
                    if seq < SEQUENTIAL {
                        self.trim_provisional(&slot);
                    }
                    self.enforce_budget(Some(&slot));
                    return Ok(out);
                }
                Ok(served @ (Served::TooEarly | Served::Empty)) => {
                    let at_zero = sess.seek <= 0.0;
                    if at_zero {
                        return Err(MediaError::unsupported(
                            &spec.path,
                            "no video frames could be decoded (the video stream is empty or unreadable)",
                        ));
                    }
                    if matches!(served, Served::Empty) {
                        let s = sess.seek;
                        empty_at = Some(empty_at.map_or(s, |e: f64| e.min(s)));
                    }
                    attempt += 1;
                    restart = true;
                }
                Ok(Served::Behind) => {
                    restart = true;
                }
                Err(e) => {
                    *guard = None;
                    slot.publish(None);
                    if spec.hw != HwAccel::Off {
                        // The GPU path failed (no device, unsupported codec): use the CPU from now on.
                        self.hw_broken.store(true, Ordering::Relaxed);
                        spec.hw = HwAccel::Off;
                        restart = true;
                        last_err = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        *guard = None;
        slot.publish(None);
        Err(last_err.unwrap_or_else(|| {
            MediaError::unsupported(&spec.path, format!("could not find the frame at {t:.3}s (the stream ended early)"))
        }))
    }

    /// A small picture of the file at `t` for timeline filmstrips, `height` pixels tall (width
    /// from the aspect ratio). `t` is snapped to a 0.1 s grid; results are cached in memory and on
    /// disk, and at most four thumbnails decode at once. Stills return the scaled image.
    pub fn thumbnail(&self, path: &Path, t: f64, height: u32) -> Result<Frame> {
        let (fk, p) = self.lookup(path)?;
        let height = height.clamp(8, 2160);
        match p.kind {
            MediaKind::Image => return self.image(path, Some((u32::MAX, height))),
            MediaKind::Audio => return Err(MediaError::unsupported(path, "is an audio file; it has no picture")),
            MediaKind::Video => {}
        }
        let index = p.video_index.ok_or_else(|| MediaError::unsupported(path, "has no video stream"))?;
        let (w, h) = fit_size(p.info.width, p.info.height, Some((u32::MAX, height)));
        let fps = if p.info.fps > 0.0 { p.info.fps } else { 30.0 };
        // The last frame's time: learned exactly once a session reached it, else about one frame
        // before the probed end of the stream.
        let learned = lock(&self.ends).get(&fk.id).copied();
        let last = learned.unwrap_or((p.video_end - 1.0 / fps).max(0.0));
        let t = if t.is_finite() { t.max(0.0) } else { 0.0 };
        let tq = ((t / THUMB_STEP).round() * THUMB_STEP).min(last);
        let tq_us = us(tq);
        let key = StreamKey { file: fk.id, w, h, kind: StreamKind::Thumb };
        let half = us(THUMB_STEP / 2.0);
        if let Some(f) = lock(&self.cache).get(&key, tq_us, 0) {
            return Ok(f);
        }
        let frame = self.thumb_inflight.run((fk.id, h, tq_us), || {
            let dir = self.cache_dir.join("thumbs");
            let file = dir.join(cache_name(path, &fk, &format!("-{h}-{}.rgba", tq_us / 1000)));
            if let Some(f) = load_raw_frame(&file, tq) {
                if f.width == w && f.height == h {
                    return Ok(f);
                }
            }
            // A larger frame of this moment already decoded (playback nearby): shrink it.
            let big = lock(&self.cache).get_larger(fk.id, tq_us, us(0.5 / fps), w, h);
            if let Some(big) = big {
                return Ok(Frame { width: w, height: h, rgba: Arc::new(still::downscale(&big.rgba, big.width, big.height, w, h)), time: tq });
            }
            let _g = self.thumb_gate.enter(THUMB_CONCURRENCY);
            let mut spec = self.spec_for(path, &p, index, w, h, fps);
            spec.readahead = 1;
            spec.threads = Some(4);
            spec.hw = HwAccel::Off;
            spec.background = true;
            spec.lean = true;
            let first = Session::start(Arc::new(spec.clone()), tq)?.first()?;
            let rgba = match first {
                Some(df) => df.rgba,
                None => {
                    // Past the last decodable frame (truncated file, video shorter than probed):
                    // the last frame at or before `tq`, like `video_frame`.
                    drop(_g);
                    let f = self.video_frame(path, tq, Some((u32::MAX, height)))?;
                    if (f.width, f.height) != (w, h) {
                        return Err(MediaError::unsupported(path, "no video frames could be decoded for a thumbnail"));
                    }
                    f.rgba
                }
            };
            let f = Frame { width: w, height: h, rgba, time: tq };
            if ensure_dir(&dir).is_ok() {
                let _ = save_raw_frame(&file, &f);
            }
            Ok(f)
        })?;
        lock(&self.cache).insert(key, tq_us - half, tq_us + half, frame.clone());
        Ok(frame)
    }

    // -----------------------------------------------------------------------------------------
    // Stills
    // -----------------------------------------------------------------------------------------

    /// A still image, upright (EXIF orientation applied), optionally downscaled to fit `max`.
    /// Video files return their first frame.
    pub fn image(&self, path: &Path, max: Option<(u32, u32)>) -> Result<Frame> {
        let (fk, p) = self.lookup(path)?;
        match p.kind {
            MediaKind::Video => return self.video_frame(path, 0.0, max),
            MediaKind::Audio => return Err(MediaError::unsupported(path, "is an audio file; it has no picture")),
            MediaKind::Image => {}
        }
        let (w, h) = fit_size(p.info.width, p.info.height, max);
        let key = StreamKey { file: fk.id, w, h, kind: StreamKind::Image };
        if let Some(f) = lock(&self.cache).get(&key, 0, 0) {
            return Ok(f);
        }
        let frame = self.image_inflight.run((fk.id, w, h), || {
            let full_key = StreamKey { file: fk.id, w: p.info.width.max(1), h: p.info.height.max(1), kind: StreamKind::Image };
            let cached = lock(&self.cache).get(&full_key, 0, 0);
            let full = match cached {
                Some(f) => f,
                None => {
                    let img = still::load(path)?;
                    let (iw, ih) = img.dimensions();
                    let f = Frame { width: iw, height: ih, rgba: Arc::new(img.into_raw()), time: 0.0 };
                    let k = StreamKey { file: fk.id, w: iw, h: ih, kind: StreamKind::Image };
                    lock(&self.cache).insert(k, i64::MIN, i64::MAX, f.clone());
                    f
                }
            };
            let (tw, th) = fit_size(full.width, full.height, max);
            if (tw, th) == (full.width, full.height) {
                return Ok(full);
            }
            let img = image::RgbaImage::from_raw(full.width, full.height, full.rgba.as_ref().clone())
                .ok_or_else(|| MediaError::unsupported(path, "decoded image has an unexpected size"))?;
            let r = still::resize(&img, tw, th);
            Ok(Frame { width: tw, height: th, rgba: Arc::new(r.into_raw()), time: 0.0 })
        })?;
        lock(&self.cache).insert(key, i64::MIN, i64::MAX, frame.clone());
        Ok(frame)
    }

    // -----------------------------------------------------------------------------------------
    // Audio
    // -----------------------------------------------------------------------------------------

    /// The file's audio as stereo f32 at `rate`, decoded once into the disk cache
    /// (`<cache>\audio\`, keyed by path + size + mtime + rate) and read on demand. Mono plays on
    /// both channels at unity gain; surround is downmixed. Files without audio give silence.
    ///
    /// Returns at once for long files not cached yet: the decode runs in the background, reads
    /// wait up to 150 ms for samples still being decoded (`AudioData::read_frames_wait` to choose),
    /// a read or `prefetch` far ahead starts decoding there first, and `AudioData::wait_complete`
    /// (or `audio_complete`) waits for everything — export must use that. Files up to 30 s are
    /// decoded completely before this returns (that takes milliseconds). If a background decode
    /// failed, the next call returns its error (and retries the decode after that).
    pub fn audio(&self, path: &Path, rate: u32) -> Result<Arc<AudioData>> {
        if !(1000..=768_000).contains(&rate) {
            return Err(MediaError::InvalidInput(format!("unsupported sample rate {rate} Hz (use 8000..768000)")));
        }
        let (fk, p) = self.lookup(path)?;
        let tick = self.tick.fetch_add(1, Ordering::Relaxed);
        {
            let mut m = lock(&self.audio);
            if let Some((a, t)) = m.get_mut(&(fk.id, rate)) {
                if let Some(e) = a.error() {
                    m.remove(&(fk.id, rate));
                    return Err(e);
                }
                *t = tick;
                return Ok(a.clone());
            }
        }
        let dir = self.cache_dir.join("audio");
        let a = self.audio_inflight.run((fk.id, rate), || {
            let a = Arc::new(audio::open_or_start(path, &p, &fk, rate, &dir)?);
            if !a.is_complete() && decode_synchronously(p.info.duration) {
                a.wait_complete()?;
            }
            Ok(a)
        })?;
        let mut m = lock(&self.audio);
        if m.len() >= 256 {
            if let Some(k) = m.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| *k) {
                m.remove(&k);
            }
        }
        m.insert((fk.id, rate), (a.clone(), tick));
        Ok(a)
    }

    /// `audio`, then waits until the whole file is decoded (what export needs: complete, exact audio).
    pub fn audio_complete(&self, path: &Path, rate: u32) -> Result<Arc<AudioData>> {
        let a = self.audio(path, rate)?;
        a.wait_complete()?;
        Ok(a)
    }

    /// Non-blocking start of the file's audio decode, prioritising `start_sec` (call on seek or
    /// before Play so the first `mix` finds its samples ready).
    pub fn prefetch_audio(&self, path: &Path, rate: u32, start_sec: f64) -> Result<Arc<AudioData>> {
        let a = self.audio(path, rate)?;
        a.prefetch(start_sec);
        Ok(a)
    }

    /// Min/max/RMS peaks at ~200 bins per second for drawing, cached on disk
    /// (`<cache>\waveform\`). Computed from the 48 kHz audio cache.
    pub fn waveform(&self, path: &Path) -> Result<Arc<Waveform>> {
        let (fk, p) = self.lookup(path)?;
        let tick = self.tick.fetch_add(1, Ordering::Relaxed);
        if let Some((w, t)) = lock(&self.waves).get_mut(&fk.id) {
            *t = tick;
            return Ok(w.clone());
        }
        let w = self.wave_inflight.run(fk.id, || {
            if !p.info.has_audio {
                return Ok(Arc::new(Waveform::empty(p.info.duration)));
            }
            let dir = self.cache_dir.join("waveform");
            let file = dir.join(cache_name(path, &fk, ".wf"));
            if let Some(w) = Waveform::load(&file) {
                return Ok(Arc::new(w));
            }
            let a = self.audio(path, 48_000)?;
            a.wait_complete()?;
            let w = Waveform::from_audio(&a);
            if ensure_dir(&dir).is_ok() {
                let _ = w.save(&file);
            }
            Ok(Arc::new(w))
        })?;
        let mut m = lock(&self.waves);
        if m.len() >= 256 {
            if let Some(k) = m.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| *k) {
                m.remove(&k);
            }
        }
        m.insert(fk.id, (w.clone(), tick));
        Ok(w)
    }

    // -----------------------------------------------------------------------------------------
    // Housekeeping
    // -----------------------------------------------------------------------------------------

    pub fn stats(&self) -> PoolStats {
        let c = lock(&self.cache);
        let sessions = self.live.load(Ordering::Relaxed);
        PoolStats {
            cache_bytes: c.used(),
            cache_budget: c.budget(),
            cached_frames: c.len(),
            cache_hits: c.hits,
            cache_misses: c.misses,
            sessions,
            seeks: self.seeks.load(Ordering::Relaxed),
            frames_decoded: self.frames_decoded.load(Ordering::Relaxed),
            session_bytes: self.session_mem.load(Ordering::Relaxed),
            readahead_budget: self.readahead_budget.load(Ordering::Relaxed),
            cache_low_priority_bytes: c.low_used(),
        }
    }

    /// Forgets everything about one file (decode sessions, cached frames, audio, probe) — call
    /// after relinking or when a file changed on disk. On-disk caches stay (keyed by mtime).
    pub fn release(&self, path: &Path) {
        let ids: Vec<u64> = {
            let mut st = lock(&self.stamps);
            let mut ids: Vec<u64> = st.get(path).map(|(k, _)| vec![k.id]).unwrap_or_default();
            if let Ok(k) = file_key(path) {
                ids.push(k.id);
            }
            st.remove(path);
            ids
        };
        for id in ids {
            lock(&self.cache).remove_file(id);
            lock(&self.probes).remove(&id);
            lock(&self.audio).retain(|(f, _), _| *f != id);
            lock(&self.waves).remove(&id);
            lock(&self.ends).remove(&id);
            let slots: Vec<Arc<Slot>> = {
                let mut g = lock(&self.groups);
                let keys: Vec<StreamKey> = g.keys().filter(|k| k.file == id).copied().collect();
                keys.into_iter().filter_map(|k| g.remove(&k)).flatten().collect()
            };
            for s in slots {
                if let Ok(mut sess) = s.session.try_lock() {
                    *sess = None;
                }
            }
        }
    }

    /// Closes all decode sessions and empties the in-memory caches.
    pub fn clear(&self) {
        let slots: Vec<Arc<Slot>> = lock(&self.groups).drain().flat_map(|(_, v)| v).collect();
        for s in slots {
            if let Ok(mut sess) = s.session.try_lock() {
                *sess = None;
            }
        }
        lock(&self.cache).clear();
        lock(&self.audio).clear();
        lock(&self.waves).clear();
        lock(&self.probes).clear();
        lock(&self.stamps).clear();
        lock(&self.ends).clear();
    }

    /// Deletes the oldest on-disk cache files until the cache folder is under `max_bytes`.
    /// Files in use by another process are skipped. Returns the bytes freed.
    pub fn prune_disk_cache(&self, max_bytes: u64) -> u64 {
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
        for sub in ["audio", "waveform", "thumbs"] {
            if let Ok(rd) = std::fs::read_dir(self.cache_dir.join(sub)) {
                for e in rd.flatten() {
                    if let Ok(m) = e.metadata() {
                        if m.is_file() {
                            files.push((m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), e.path()));
                        }
                    }
                }
            }
        }
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        files.sort_by_key(|f| f.0);
        let mut freed = 0;
        for (_, len, path) in files {
            if total <= max_bytes {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total -= len;
                freed += len;
            }
        }
        freed
    }
}

fn save_raw_frame(path: &Path, f: &Frame) -> Result<()> {
    let mut b = Vec::with_capacity(16 + f.rgba.len());
    b.extend_from_slice(b"AIFT");
    b.extend_from_slice(&f.width.to_le_bytes());
    b.extend_from_slice(&f.height.to_le_bytes());
    b.extend_from_slice(&[0u8; 4]);
    b.extend_from_slice(&f.rgba);
    let tmp = temp_sibling(path);
    std::fs::write(&tmp, &b).map_err(|e| MediaError::io(&tmp, "write the thumbnail cache", &e))?;
    commit_temp(&tmp, path)
}

fn load_raw_frame(path: &Path, time: f64) -> Option<Frame> {
    let b = std::fs::read(path).ok()?;
    if b.len() < 16 || &b[..4] != b"AIFT" {
        return None;
    }
    let w = u32::from_le_bytes(b[4..8].try_into().ok()?);
    let h = u32::from_le_bytes(b[8..12].try_into().ok()?);
    if b.len() != 16 + w as usize * h as usize * 4 {
        return None;
    }
    Some(Frame { width: w, height: h, rgba: Arc::new(b[16..].to_vec()), time })
}

#[cfg(test)]
mod tests {
    use super::{auto_wants_gpu, decode_synchronously};
    use crate::probe::Probed;
    use aiframes_core::model::{MediaInfo, MediaKind};

    fn probed(codec: &str, pix_fmt: &str, w: u32, h: u32) -> Probed {
        Probed {
            kind: MediaKind::Video,
            info: MediaInfo { codec: codec.into(), width: w, height: h, ..Default::default() },
            video_index: Some(0),
            audio_index: None,
            video_end: 10.0,
            video_start: 0.0,
            video_end_exact: true,
            vfr: false,
            untagged_matrix: false,
            vpx_alpha: None,
            pix_fmt: pix_fmt.into(),
            format_name: "mov,mp4,m4a,3gp,3g2,mj2".into(),
        }
    }

    #[test]
    fn auto_hardware_decoding_is_for_heavy_streams() {
        assert!(!auto_wants_gpu(&probed("h264", "yuv420p", 1920, 1080)), "8-bit H.264 1080p stays on the CPU");
        assert!(!auto_wants_gpu(&probed("h264", "yuv420p", 2560, 1440)));
        assert!(auto_wants_gpu(&probed("h264", "yuv420p", 3840, 2160)), "4K");
        assert!(auto_wants_gpu(&probed("hevc", "yuv420p", 1920, 1080)), "HEVC");
        assert!(auto_wants_gpu(&probed("hevc", "yuv420p10le", 3840, 2160)), "10-bit HEVC");
        assert!(auto_wants_gpu(&probed("av1", "yuv420p", 1280, 720)));
        assert!(!auto_wants_gpu(&probed("h264", "yuv420p10le", 3840, 2160)), "GPUs do not decode 10-bit H.264");
        assert!(!auto_wants_gpu(&probed("prores", "yuv422p10le", 3840, 2160)), "no GPU decoder");
    }

    #[test]
    fn only_known_short_durations_decode_inside_audio() {
        assert!(decode_synchronously(3.0));
        assert!(decode_synchronously(30.0));
        assert!(!decode_synchronously(30.5));
        // unknown durations (no metadata) are treated as long: never block on a possible hour
        assert!(!decode_synchronously(0.0));
        assert!(!decode_synchronously(f64::NAN));
        assert!(!decode_synchronously(f64::INFINITY));
        assert!(!decode_synchronously(-1.0));
    }
}
