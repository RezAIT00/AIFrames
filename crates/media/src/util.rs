//! Small shared helpers: stable hashing, cache folders, file identity, in-flight de-duplication,
//! positional reads, rational frame rates.

use crate::error::{MediaError, Result};
use crate::tools::lock;
use std::collections::HashMap;
use std::fs::File;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::UNIX_EPOCH;

/// FNV-1a 64: stable across Rust versions and runs (used for on-disk cache keys).
#[derive(Clone, Copy)]
pub(crate) struct Fnv(u64);

impl Fnv {
    pub fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    pub fn bytes(mut self, b: &[u8]) -> Self {
        for &x in b {
            self.0 ^= x as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self
    }
    pub fn u64(self, v: u64) -> Self {
        self.bytes(&v.to_le_bytes())
    }
    pub fn finish(self) -> u64 {
        self.0
    }
}

/// Identity of a file's current contents: path + size + modification time. Changes when the file
/// is replaced or edited, which invalidates every cache keyed by it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FileKey {
    pub id: u64,
    pub size: u64,
    pub mtime_ns: u128,
}

pub(crate) fn normalized_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    if cfg!(windows) {
        s.replace('/', "\\").to_lowercase()
    } else {
        s.into_owned()
    }
}

pub(crate) fn file_key(path: &Path) -> Result<FileKey> {
    let meta = std::fs::metadata(path).map_err(|e| MediaError::io(path, "read", &e))?;
    if meta.is_dir() {
        return Err(MediaError::unsupported(path, "is a folder, not a media file"));
    }
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let id = Fnv::new()
        .bytes(normalized_path(path).as_bytes())
        .u64(meta.len())
        .u64(mtime_ns as u64)
        .u64((mtime_ns >> 64) as u64)
        .finish();
    Ok(FileKey { id, size: meta.len(), mtime_ns })
}

/// A short readable, filesystem-safe name for cache files ("my_clip-<hash>").
pub(crate) fn cache_name(path: &Path, key: &FileKey, extra: &str) -> String {
    let stem: String = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(40)
        .collect();
    format!("{stem}-{:016x}{extra}", key.id)
}

/// The default cache root: `AIFRAMES_CACHE_DIR`, else `%LOCALAPPDATA%\AIFrames\cache`
/// (`$XDG_CACHE_HOME/aiframes` or `~/.cache/aiframes` elsewhere), else the temp folder.
pub fn default_cache_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("AIFRAMES_CACHE_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(d);
    }
    if cfg!(windows) {
        if let Some(l) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(l).join("AIFrames").join("cache");
        }
    } else {
        if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
            return PathBuf::from(x).join("aiframes");
        }
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h).join(".cache").join("aiframes");
        }
    }
    std::env::temp_dir().join("aiframes-cache")
}

pub(crate) fn ensure_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| MediaError::io(dir, "create the cache folder", &e))
}

/// A unique temporary sibling path for atomic writes (`<file>.<pid>-<n>.part`).
pub(crate) fn temp_sibling(target: &Path) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut name = target.file_name().map(|s| s.to_os_string()).unwrap_or_default();
    name.push(format!(".{}-{n}.part", std::process::id()));
    target.with_file_name(name)
}

/// Moves a finished temp file into place. If another process won the race and the target already
/// exists, keeps theirs.
pub(crate) fn commit_temp(tmp: &Path, target: &Path) -> Result<()> {
    match std::fs::rename(tmp, target) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(tmp);
            if target.is_file() {
                Ok(())
            } else {
                Err(MediaError::io(target, "write the cache file", &e))
            }
        }
    }
}

/// A unique scratch folder under the system temp dir, removed on drop.
pub(crate) struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> Result<Self> {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let p = std::env::temp_dir().join(format!("{prefix}-{}-{n}-{nanos:x}", std::process::id()));
        std::fs::create_dir_all(&p).map_err(|e| MediaError::io(&p, "create a temporary folder", &e))?;
        Ok(TempDir(p))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Fills `buf` from `file` at `offset` without moving a shared cursor (safe for concurrent use).
pub(crate) fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_read(file, buf, offset)?;
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::read_at(file, buf, offset)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
}

/// Ensures one computation per key at a time: concurrent callers for the same key wait for the
/// first one's result instead of repeating the work (e.g. decoding the same hour of audio twice).
pub(crate) struct Inflight<K, V> {
    map: Mutex<HashMap<K, Arc<Slot<V>>>>,
}

struct Slot<V> {
    result: Mutex<Option<Result<V>>>,
    cv: Condvar,
}

impl<K: Eq + Hash + Clone, V: Clone> Inflight<K, V> {
    pub fn new() -> Self {
        Inflight { map: Mutex::new(HashMap::new()) }
    }

    pub fn run(&self, key: K, f: impl FnOnce() -> Result<V>) -> Result<V> {
        let (slot, leader) = {
            let mut m = lock(&self.map);
            match m.get(&key) {
                Some(s) => (s.clone(), false),
                None => {
                    let s = Arc::new(Slot { result: Mutex::new(None), cv: Condvar::new() });
                    m.insert(key.clone(), s.clone());
                    (s, true)
                }
            }
        };
        if leader {
            // Remove the slot even if `f` panics, so later callers do not wait forever.
            struct Cleanup<'a, K: Eq + Hash, V> {
                map: &'a Mutex<HashMap<K, Arc<Slot<V>>>>,
                key: Option<K>,
                slot: Arc<Slot<V>>,
            }
            impl<K: Eq + Hash, V> Drop for Cleanup<'_, K, V> {
                fn drop(&mut self) {
                    if let Some(k) = self.key.take() {
                        lock(self.map).remove(&k);
                    }
                    let mut r = lock(&self.slot.result);
                    if r.is_none() {
                        *r = Some(Err(MediaError::InvalidInput("the computation panicked".into())));
                    }
                    self.slot.cv.notify_all();
                }
            }
            let cleanup = Cleanup { map: &self.map, key: Some(key), slot: slot.clone() };
            let r = f();
            *lock(&slot.result) = Some(r.clone());
            drop(cleanup);
            r
        } else {
            let mut g = lock(&slot.result);
            while g.is_none() {
                g = slot.cv.wait(g).unwrap_or_else(|p| p.into_inner());
            }
            g.clone().expect("result set")
        }
    }
}

/// `fps` as an exact rational for ffmpeg (29.97 -> 30000/1001, 23.976 -> 24000/1001 ...).
pub fn fps_rational(fps: f64) -> (u32, u32) {
    if !fps.is_finite() || fps <= 0.0 {
        return (30, 1);
    }
    for base in [24u32, 30, 48, 60, 120, 240] {
        let ntsc = base as f64 * 1000.0 / 1001.0;
        if (fps - ntsc).abs() < 0.006 {
            return (base * 1000, 1001);
        }
    }
    if (fps - fps.round()).abs() < 1e-6 {
        return (fps.round() as u32, 1);
    }
    ((fps * 1000.0).round() as u32, 1000)
}

/// Snaps a measured rate to the nearest common rate when within 0.5 %, else rounds to 3 decimals.
pub(crate) fn snap_fps(fps: f64) -> f64 {
    const COMMON: [f64; 20] = [
        23.976, 24.0, 25.0, 29.97, 30.0, 47.952, 48.0, 50.0, 59.94, 60.0, 72.0, 90.0, 100.0, 119.88, 120.0, 144.0, 165.0,
        200.0, 239.76, 240.0,
    ];
    let nearest = COMMON
        .iter()
        .copied()
        .map(|c| ((fps - c).abs() / c, c))
        .min_by(|a, b| a.0.total_cmp(&b.0));
    match nearest {
        Some((d, c)) if d < 0.005 => c,
        _ => (fps * 1000.0).round() / 1000.0,
    }
}

/// Output size that fits `max` (preserving aspect, never upscaling, at least 1x1).
pub(crate) fn fit_size(w: u32, h: u32, max: Option<(u32, u32)>) -> (u32, u32) {
    let (w, h) = (w.max(1), h.max(1));
    let Some((mw, mh)) = max else { return (w, h) };
    let (mw, mh) = (mw.max(1), mh.max(1));
    if w <= mw && h <= mh {
        return (w, h);
    }
    let s = (mw as f64 / w as f64).min(mh as f64 / h as f64);
    let ow = ((w as f64 * s).round() as u32).clamp(1, mw);
    let oh = ((h as f64 * s).round() as u32).clamp(1, mh);
    (ow, oh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_rates() {
        assert_eq!(fps_rational(29.97), (30000, 1001));
        assert_eq!(fps_rational(30000.0 / 1001.0), (30000, 1001));
        assert_eq!(fps_rational(23.976), (24000, 1001));
        assert_eq!(fps_rational(59.94), (60000, 1001));
        assert_eq!(fps_rational(25.0), (25, 1));
        assert_eq!(fps_rational(12.5), (12500, 1000));
    }

    #[test]
    fn snapping() {
        assert_eq!(snap_fps(30000.0 / 1001.0), 29.97);
        assert_eq!(snap_fps(59.87), 59.94);
        assert_eq!(snap_fps(60.0), 60.0);
        assert_eq!(snap_fps(37.123456), 37.123);
    }

    #[test]
    fn fitting() {
        assert_eq!(fit_size(1920, 1080, Some((960, 540))), (960, 540));
        assert_eq!(fit_size(1920, 1080, Some((960, 960))), (960, 540));
        assert_eq!(fit_size(1080, 1920, Some((960, 540))), (304, 540));
        assert_eq!(fit_size(640, 360, Some((1920, 1080))), (640, 360));
        assert_eq!(fit_size(320, 180, Some((100, 100))), (100, 56));
        assert_eq!(fit_size(5000, 10, Some((100, 100))), (100, 1));
        assert_eq!(fit_size(0, 0, None), (1, 1));
    }

    #[test]
    fn inflight_dedups() {
        let inf: Arc<Inflight<u32, u32>> = Arc::new(Inflight::new());
        let count = Arc::new(AtomicU64::new(0));
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let inf = inf.clone();
                let count = count.clone();
                std::thread::spawn(move || {
                    inf.run(1, || {
                        count.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        Ok(7)
                    })
                })
            })
            .collect();
        for h in hs {
            assert_eq!(h.join().unwrap().unwrap(), 7);
        }
        assert!(count.load(Ordering::SeqCst) <= 2);
    }
}

