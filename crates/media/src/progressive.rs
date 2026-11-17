//! Progressive audio decoding: `MediaPool::audio` returns at once and the samples arrive while you
//! read them.
//!
//! * The **main decoder** streams the whole file from the start into the disk cache's `.part` file.
//!   Everything below its *frontier* is final and is read straight from that file. When ffmpeg
//!   exits successfully the `.part` file is renamed to the cache name, so a crash or a failed decode
//!   never leaves a file that looks complete.
//! * A read (or `prefetch`) far ahead of the frontier starts a **region decoder**: ffmpeg seeking
//!   close to that position, into memory. It starts on a grid where input and output sample
//!   positions are both whole numbers and drops one second of pre-roll, so the decoder and resampler
//!   have settled and its samples are those of the main decode: bit-identical for PCM, FLAC, ALAC,
//!   Vorbis and Opus in Ogg/MP4; for AAC, bands coded with perceptual noise substitution (a running
//!   random generator) may differ by ~1e-5 (-90 dBFS), always sample-aligned. Regions are only used
//!   for containers whose seeking is sample-exact — not Matroska/WebM, whose 1 ms timestamps put a
//!   seek up to half a millisecond off (measured: Opus/WebM 48-65 samples, MP3/MKV 5-17, AAC/MKV 1);
//!   they stop once the main decoder catches up or nobody reads them.
//! * Reads of samples not decoded yet wait (bounded, 150 ms by default) while that position is
//!   prioritised, then fill silence and report it. `wait_complete` waits for the whole file.

use crate::audio::Blocks;
use crate::error::{MediaError, Result};
use crate::probe::Probed;
use crate::tools::{lock, Priority};
use crate::util::{commit_temp, read_exact_at, temp_sibling};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime};

/// Default bounded wait of a read for samples not decoded yet.
pub(crate) const READ_WAIT: Duration = Duration::from_millis(150);
/// Pre-roll a region decoder discards so its samples match the main decode exactly.
const REGION_PREROLL: f64 = 1.0;
/// A region decodes at most this far beyond the furthest position read from it.
const REGION_AHEAD: f64 = 20.0;
/// A region nobody read for this long is stopped.
const REGION_IDLE: Duration = Duration::from_secs(20);
const MAX_REGIONS: usize = 3;
/// Positions this close to the main frontier (seconds of audio) wait for the main decoder.
const NEAR: f64 = 2.0;
/// Main decoders running at once (more files queue; a read or `wait_complete` jumps the queue).
const MAX_MAIN: usize = 3;
/// `.part` files untouched for this long belong to a crashed run and are deleted.
const STALE_PART: Duration = Duration::from_secs(10 * 60);

/// Region decoders started in this process (diagnostics and tests).
pub(crate) static REGIONS_STARTED: AtomicU64 = AtomicU64::new(0);

/// Containers whose `-ss` seeking is sample-exact (regions are used only for these). Matroska and
/// WebM are excluded: their millisecond timestamps make a seek land up to 0.5 ms off.
pub(crate) fn seeks_exactly(format_name: &str) -> bool {
    if format_name.contains("matroska") || format_name.contains("webm") {
        return false;
    }
    ["mov", "mp4", "wav", "w64", "flac", "ogg", "aiff", "caf"].iter().any(|f| format_name.split(',').any(|n| n == *f))
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

struct Region {
    id: u64,
    /// Absolute frame of `data[0]`.
    start: u64,
    data: Vec<f32>,
    /// Furthest frame wanted from this region (reads and prefetches).
    want: u64,
    eof: bool,
    last_used: Instant,
}

impl Region {
    fn end(&self) -> u64 {
        self.start + (self.data.len() / 2) as u64
    }
}

struct PState {
    /// Frames `[0, frontier)` are in the cache file and final.
    frontier: u64,
    /// The main decode ended (successfully, or with `error`).
    done: bool,
    error: Option<MediaError>,
    /// When the main decoder started decoding (None while it waits for its turn).
    started: Option<Instant>,
    regions: Vec<Region>,
    next_id: u64,
}

pub(crate) struct Prog {
    source: PathBuf,
    probed: Probed,
    /// The sound stream decoded (0-based among the file's sound streams).
    stream: u32,
    rate: u32,
    /// Expected number of frames (from the probed duration) until the decode is done.
    estimate: u64,
    /// Region start grid in output frames (both sample positions are integers on it); 0 = no regions.
    grid: u64,
    /// Read handle on the cache file being written (stays valid after the rename).
    file: File,
    blocks: Mutex<Blocks>,
    st: Mutex<PState>,
    cv: Condvar,
    urgent: AtomicBool,
}

// --- main decoder admission -----------------------------------------------------------------

struct MainGate {
    running: Mutex<usize>,
    cv: Condvar,
}

fn gate() -> &'static MainGate {
    static G: OnceLock<MainGate> = OnceLock::new();
    G.get_or_init(|| MainGate { running: Mutex::new(0), cv: Condvar::new() })
}

/// Live progressive decodes by cache file, so a second `AudioData` for the same file shares one.
fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<Prog>>> {
    static R: OnceLock<Mutex<HashMap<PathBuf, Weak<Prog>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Deletes `.part` files left behind by crashed runs (once per folder per process).
fn clean_stale_parts(dir: &Path) {
    static DONE: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    let done = DONE.get_or_init(|| Mutex::new(Vec::new()));
    {
        let mut d = lock(done);
        if d.iter().any(|p| p == dir) {
            return;
        }
        d.push(dir.to_path_buf());
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".part") {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > STALE_PART);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

impl Prog {
    /// Starts (or joins) the progressive decode of `source` into `cache` (the final cache path).
    pub(crate) fn start(source: &Path, probed: &Probed, rate: u32, cache: &Path, stream: u32) -> Result<Arc<Prog>> {
        let mut reg = lock(registry());
        if let Some(p) = reg.get(cache).and_then(Weak::upgrade) {
            return Ok(p);
        }
        reg.retain(|_, w| w.strong_count() > 0);
        if let Some(dir) = cache.parent() {
            clean_stale_parts(dir);
        }
        // A crashed run with the same process id may have left this name behind: take the next one.
        let (part, write) = {
            let mut attempt = 0;
            loop {
                let part = temp_sibling(cache);
                match std::fs::OpenOptions::new().write(true).create_new(true).open(&part) {
                    Ok(f) => break (part, f),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 16 => attempt += 1,
                    Err(e) => return Err(MediaError::io(&part, "create the audio cache file", &e)),
                }
            }
        };
        let file = File::open(&part).map_err(|e| MediaError::io(&part, "open the audio cache file", &e))?;
        let in_rate = probed.audio(stream).map(|a| a.rate).unwrap_or(probed.info.audio_rate) as u64;
        let grid = if seeks_exactly(&probed.format_name) && in_rate > 0 { rate as u64 / gcd(rate as u64, in_rate) } else { 0 };
        let estimate = (probed.info.duration.max(0.0) * rate as f64).round() as u64;
        let prog = Arc::new(Prog {
            source: source.to_path_buf(),
            probed: probed.clone(),
            stream,
            rate,
            estimate,
            grid,
            file,
            blocks: Mutex::new(Blocks::default()),
            st: Mutex::new(PState { frontier: 0, done: false, error: None, started: None, regions: Vec::new(), next_id: 0 }),
            cv: Condvar::new(),
            urgent: AtomicBool::new(false),
        });
        reg.insert(cache.to_path_buf(), Arc::downgrade(&prog));
        drop(reg);
        let p2 = prog.clone();
        let cache = cache.to_path_buf();
        std::thread::Builder::new()
            .name("audio-decode".into())
            .spawn(move || run_main(p2, write, part, cache))
            .map_err(|e| MediaError::InvalidInput(format!("could not start the audio decoder thread: {e}")))?;
        Ok(prog)
    }

    pub(crate) fn rate(&self) -> u32 {
        self.rate
    }

    /// Frames known so far: exact once done, else the larger of the estimate and what was decoded.
    pub(crate) fn frames(&self) -> u64 {
        let st = lock(&self.st);
        if st.done {
            st.frontier
        } else {
            let regions = st.regions.iter().map(Region::end).max().unwrap_or(0);
            self.estimate.max(st.frontier).max(regions)
        }
    }

    pub(crate) fn is_complete(&self) -> bool {
        let st = lock(&self.st);
        st.done && st.error.is_none()
    }

    pub(crate) fn error(&self) -> Option<MediaError> {
        lock(&self.st).error.clone()
    }

    pub(crate) fn progress(&self) -> f64 {
        let st = lock(&self.st);
        if st.done {
            1.0
        } else if self.estimate == 0 {
            0.0
        } else {
            (st.frontier as f64 / self.estimate as f64).clamp(0.0, 0.999)
        }
    }

    fn promote(&self) {
        if !self.urgent.swap(true, Ordering::Relaxed) {
            let g = gate();
            let _l = lock(&g.running);
            g.cv.notify_all();
        }
    }

    /// Blocks until the whole file is decoded (or the decode failed).
    pub(crate) fn wait_complete(&self, timeout: Option<Duration>) -> Result<bool> {
        self.promote();
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut st = lock(&self.st);
        while !st.done {
            match deadline {
                None => st = self.cv.wait(st).unwrap_or_else(|p| p.into_inner()),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return Ok(false);
                    }
                    st = self.cv.wait_timeout(st, d - now).unwrap_or_else(|p| p.into_inner()).0;
                }
            }
        }
        match &st.error {
            Some(e) => Err(e.clone()),
            None => Ok(true),
        }
    }

    /// Whether frames `[a, b)` can be read without waiting.
    pub(crate) fn ready(&self, a: u64, b: u64) -> bool {
        let st = lock(&self.st);
        let mut pos = a;
        while pos < b {
            if pos < st.frontier {
                pos = st.frontier;
                continue;
            }
            if st.done {
                return true; // past the end: silence, no wait
            }
            match st.regions.iter().find(|r| r.start <= pos && pos < r.end()) {
                Some(r) => pos = r.end(),
                None => return false,
            }
        }
        true
    }

    /// Makes sure `[a, b)` is being decoded soon (a region decoder if the main one is far away).
    pub(crate) fn prefetch(self: &Arc<Self>, a: u64, b: u64) {
        let mut st = lock(&self.st);
        self.prioritise(&mut st, a, b);
    }

    fn prioritise(self: &Arc<Self>, st: &mut PState, pos: u64, end: u64) {
        if st.started.is_none() {
            self.promote();
        }
        if st.done || pos < st.frontier {
            return;
        }
        let now = Instant::now();
        let rate = self.rate as f64;
        // Already covered, or a running region will get there soon.
        if let Some(r) = st.regions.iter_mut().find(|r| r.start <= pos && (pos < r.end() || (!r.eof && pos <= r.end() + (NEAR * rate) as u64))) {
            r.want = r.want.max(end);
            r.last_used = now;
            return;
        }
        if self.grid == 0 {
            return; // no exact seeking: the main decoder gets there
        }
        // The main decoder will arrive soon?
        let lag = pos - st.frontier;
        if lag <= (NEAR * rate) as u64 {
            return;
        }
        if let Some(t0) = st.started {
            let secs = t0.elapsed().as_secs_f64();
            if secs > 0.05 && st.frontier > 0 {
                let speed = st.frontier as f64 / secs;
                if lag as f64 / speed < 0.08 {
                    return;
                }
            }
        }
        if st.regions.len() >= MAX_REGIONS {
            if let Some(i) = st.regions.iter().enumerate().min_by_key(|(_, r)| r.last_used).map(|(i, _)| i) {
                st.regions.remove(i); // its thread notices and stops
            }
        }
        let preroll = (REGION_PREROLL * rate) as u64;
        let s0 = pos.saturating_sub(preroll) / self.grid * self.grid;
        let skip = if s0 == 0 { pos } else { pos - s0 };
        let id = st.next_id;
        st.next_id += 1;
        st.regions.push(Region { id, start: s0 + skip, data: Vec::new(), want: end, eof: false, last_used: now });
        REGIONS_STARTED.fetch_add(1, Ordering::Relaxed);
        let me = self.clone();
        let spawned = std::thread::Builder::new().name("audio-region".into()).spawn(move || run_region(me, id, s0, skip));
        if spawned.is_err() {
            st.regions.retain(|r| r.id != id);
        }
        self.cv.notify_all();
    }

    /// Reads frames from `start` into `out` (stereo interleaved), waiting up to `max_wait` for
    /// samples not decoded yet. Returns false if some samples were filled with silence because they
    /// did not arrive in time.
    pub(crate) fn read(self: &Arc<Self>, start: i64, out: &mut [f32], max_wait: Duration) -> bool {
        let n = (out.len() / 2) as i64;
        let len2 = n as usize * 2;
        out[len2..].fill(0.0);
        let end = (start + n).max(0) as u64;
        let mut pos = start.max(0) as u64;
        let lead = ((pos as i64 - start) * 2) as usize;
        out[..lead.min(len2)].fill(0.0);
        let deadline = Instant::now() + max_wait;
        let mut asked = false;
        let mut ok = true;
        let mut st = lock(&self.st);
        while pos < end {
            let off = ((pos as i64 - start) * 2) as usize;
            if pos < st.frontier {
                let upto = end.min(st.frontier);
                let (limit, done) = (st.frontier, st.done);
                drop(st);
                self.read_file(pos, &mut out[off..off + ((upto - pos) * 2) as usize], limit, done);
                pos = upto;
                st = lock(&self.st);
                continue;
            }
            if st.done {
                out[off..len2].fill(0.0); // past the end of the audio
                break;
            }
            if let Some(r) = st.regions.iter_mut().find(|r| r.start <= pos && pos < r.end()) {
                let upto = end.min(r.end());
                let from = ((pos - r.start) * 2) as usize;
                let cnt = ((upto - pos) * 2) as usize;
                out[off..off + cnt].copy_from_slice(&r.data[from..from + cnt]);
                r.want = r.want.max(end);
                r.last_used = Instant::now();
                pos = upto;
                continue;
            }
            if !asked {
                self.prioritise(&mut st, pos, end);
                asked = true;
            }
            let now = Instant::now();
            if now >= deadline {
                out[off..len2].fill(0.0);
                ok = false;
                break;
            }
            st = self.cv.wait_timeout(st, deadline - now).unwrap_or_else(|p| p.into_inner()).0;
        }
        ok
    }

    /// Copies final frames `[pos, pos + dst.len()/2)` (all below `limit`) from the cache file.
    fn read_file(&self, pos: u64, dst: &mut [f32], limit: u64, done: bool) {
        use crate::audio::BLOCK_FRAMES;
        let n = (dst.len() / 2) as u64;
        let mut p = pos;
        let mut written = 0usize;
        while p < pos + n {
            let bi = p / BLOCK_FRAMES;
            let bstart = bi * BLOCK_FRAMES;
            let in_block = (p - bstart) as usize;
            let take = ((pos + n - p) as usize).min(BLOCK_FRAMES as usize - in_block);
            let out = &mut dst[written..written + take * 2];
            if done || bstart + BLOCK_FRAMES <= limit {
                let total = if done { limit } else { u64::MAX };
                let mut blocks = lock(&self.blocks);
                match blocks.get(&self.file, bi, total) {
                    Some(s) => {
                        let avail = (s.len() / 2).saturating_sub(in_block).min(take);
                        out[..avail * 2].copy_from_slice(&s[in_block * 2..(in_block + avail) * 2]);
                        out[avail * 2..].fill(0.0);
                    }
                    None => out.fill(0.0),
                }
            } else {
                // The block at the frontier is still growing: read just this part, uncached.
                let mut bytes = vec![0u8; take * 8];
                match read_exact_at(&self.file, &mut bytes, p * 8) {
                    Ok(()) => {
                        for (o, c) in out.iter_mut().zip(bytes.as_chunks::<4>().0) {
                            *o = f32::from_le_bytes(*c);
                        }
                    }
                    Err(_) => out.fill(0.0),
                }
            }
            written += take * 2;
            p += take as u64;
        }
    }
}

fn run_main(prog: Arc<Prog>, mut write: File, part: PathBuf, cache: PathBuf) {
    // Wait for a decoder slot unless someone needs this file now.
    let g = gate();
    {
        let mut running = lock(&g.running);
        while *running >= MAX_MAIN && !prog.urgent.load(Ordering::Relaxed) {
            running = g.cv.wait_timeout(running, Duration::from_millis(200)).unwrap_or_else(|p| p.into_inner()).0;
        }
        *running += 1;
    }
    struct Slot;
    impl Drop for Slot {
        fn drop(&mut self) {
            let g = gate();
            *lock(&g.running) -= 1;
            g.cv.notify_all();
        }
    }
    let _slot = Slot;
    lock(&prog.st).started = Some(Instant::now());

    let mut write_err: Option<std::io::Error> = None;
    // The whole-file decode is background work, unless a reader is already waiting for it.
    let priority = if prog.urgent.load(Ordering::Relaxed) { Priority::Normal } else { Priority::Background };
    let result = crate::audio::stream_raw(&prog.source, &prog.probed, prog.stream, prog.rate, 2, 0, priority, |bytes| {
        if let Err(e) = write.write_all(bytes) {
            write_err = Some(e);
            return false;
        }
        let mut st = lock(&prog.st);
        st.frontier += (bytes.len() / 8) as u64;
        // Regions the main decoder has overtaken are not needed any more.
        let f = st.frontier;
        st.regions.retain(|r| r.end() > f || (r.data.is_empty() && r.start > f));
        prog.cv.notify_all();
        true
    });
    let flushed = write.flush().and_then(|_| write.sync_data());
    drop(write);
    let outcome = match (result, write_err) {
        (_, Some(e)) => Err(MediaError::io(&part, "write the audio cache file", &e)),
        (Err(e), None) => Err(e),
        (Ok(_), None) => match flushed {
            Ok(()) => Ok(()),
            Err(e) => Err(MediaError::io(&part, "write the audio cache file", &e)),
        },
    };
    match &outcome {
        // Only a finished decode gets the cache name (the read handle keeps working after the rename).
        Ok(()) => {
            let _ = commit_temp(&part, &cache);
        }
        Err(_) => {
            let _ = std::fs::remove_file(&part);
        }
    }
    let mut st = lock(&prog.st);
    st.done = true;
    st.error = outcome.err();
    st.regions.clear();
    prog.cv.notify_all();
    drop(st);
    let mut reg = lock(registry());
    if reg.get(&cache).is_some_and(|w| w.as_ptr() == Arc::as_ptr(&prog)) {
        reg.remove(&cache);
    }
}

fn run_region(prog: Arc<Prog>, id: u64, s0: u64, skip: u64) {
    let mut to_skip = skip as usize;
    let rate = prog.rate as f64;
    let ahead = (REGION_AHEAD * rate) as u64;
    let mut samples: Vec<f32> = Vec::new();
    // Regions serve playback that is waiting for them: normal priority.
    let r = crate::audio::stream_raw(&prog.source, &prog.probed, prog.stream, prog.rate, 2, s0, Priority::Normal, |bytes| {
        let frames = bytes.len() / 8;
        let drop_n = to_skip.min(frames);
        to_skip -= drop_n;
        samples.clear();
        samples.extend(bytes[drop_n * 8..].as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)));
        let mut st = lock(&prog.st);
        let mut pushed = samples.is_empty();
        loop {
            if st.done {
                return false;
            }
            let frontier = st.frontier;
            let Some(r) = st.regions.iter_mut().find(|r| r.id == id) else { return false };
            if !pushed {
                r.data.extend_from_slice(&samples);
                pushed = true;
                prog.cv.notify_all();
            }
            let end = r.end();
            if frontier >= end && !r.data.is_empty() {
                st.regions.retain(|r| r.id != id); // the main decoder caught up
                return false;
            }
            if r.last_used.elapsed() > REGION_IDLE {
                st.regions.retain(|r| r.id != id);
                return false;
            }
            if end < r.want + ahead {
                return true; // keep decoding
            }
            // Far enough ahead of the readers: pause (ffmpeg blocks on the pipe meanwhile).
            st = prog.cv.wait_timeout(st, Duration::from_millis(250)).unwrap_or_else(|p| p.into_inner()).0;
        }
    });
    let mut st = lock(&prog.st);
    match r {
        Ok(_) => {
            if let Some(r) = st.regions.iter_mut().find(|r| r.id == id) {
                r.eof = true;
            }
        }
        Err(_) => st.regions.retain(|r| r.id != id), // readers fall back to the main decoder
    }
    prog.cv.notify_all();
}

#[cfg(test)]
mod tests {
    use super::seeks_exactly;

    #[test]
    fn exact_seek_containers() {
        for f in ["mov,mp4,m4a,3gp,3g2,mj2", "wav", "flac", "ogg", "aiff", "w64", "caf"] {
            assert!(seeks_exactly(f), "{f}");
        }
        for f in ["matroska,webm", "mp3", "aac", "mpegts", "avi", ""] {
            assert!(!seeks_exactly(f), "{f}");
        }
    }
}
