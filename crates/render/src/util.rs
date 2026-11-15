//! Small shared helpers: hashing, a byte-bounded LRU, frame-grid arithmetic.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, MutexGuard};

/// Locks a mutex, recovering from poisoning (a panicking render thread must not take the renderer
/// down with it).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A 64-bit hasher for cache keys.
pub(crate) struct KeyHasher(std::collections::hash_map::DefaultHasher);

impl KeyHasher {
    pub fn new(tag: &str) -> Self {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        tag.hash(&mut h);
        KeyHasher(h)
    }
    pub fn f64(&mut self, v: f64) -> &mut Self {
        v.to_bits().hash(&mut self.0);
        self
    }
    pub fn f32(&mut self, v: f32) -> &mut Self {
        v.to_bits().hash(&mut self.0);
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        v.hash(&mut self.0);
        self
    }
    pub fn str(&mut self, s: &str) -> &mut Self {
        s.hash(&mut self.0);
        self
    }
    /// Hashes any serialisable value through its JSON form.
    pub fn json<T: serde::Serialize>(&mut self, v: &T) -> &mut Self {
        match serde_json::to_vec(v) {
            Ok(b) => b.hash(&mut self.0),
            Err(_) => 0u8.hash(&mut self.0),
        }
        self
    }
    pub fn finish(&self) -> u64 {
        self.0.finish()
    }
}

/// A least-recently-used map bounded by total "cost" (bytes) and entry count.
pub(crate) struct Lru<V> {
    map: HashMap<u64, (V, u64, usize)>,
    tick: u64,
    used: usize,
    budget: usize,
    max_entries: usize,
}

impl<V: Clone> Lru<V> {
    pub fn new(budget: usize, max_entries: usize) -> Self {
        Lru { map: HashMap::new(), tick: 0, used: 0, budget, max_entries: max_entries.max(1) }
    }
    pub fn get(&mut self, k: u64) -> Option<V> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(&k).map(|e| {
            e.1 = tick;
            e.0.clone()
        })
    }
    pub fn insert(&mut self, k: u64, v: V, cost: usize) {
        if cost > self.budget {
            return;
        }
        self.tick += 1;
        if let Some(old) = self.map.insert(k, (v, self.tick, cost)) {
            self.used -= old.2;
        }
        self.used += cost;
        while (self.used > self.budget || self.map.len() > self.max_entries) && self.map.len() > 1 {
            let Some((&victim, _)) = self.map.iter().filter(|(kk, _)| **kk != k).min_by_key(|(_, e)| e.1) else { break };
            if let Some(e) = self.map.remove(&victim) {
                self.used -= e.2;
            }
        }
    }
    pub fn clear(&mut self) {
        self.map.clear();
        self.used = 0;
    }
    #[cfg(test)]
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.map.values().map(|e| &e.0)
    }
    #[cfg(test)]
    pub fn used(&self) -> usize {
        self.used
    }
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// Exact frame rate as a rational (29.97 → 30000/1001, 23.976 → 24000/1001).
pub(crate) fn fps_parts(fps: f64) -> (u64, u64) {
    let (n, d) = aiframes_media::fps_rational(if fps.is_finite() && fps > 0.0 { fps } else { 30.0 });
    (n.max(1) as u64, d.max(1) as u64)
}

/// The frame index containing timeline time `t` (frames start at `n / fps`).
pub fn frame_index(t: f64, fps: f64) -> i64 {
    let (n, d) = fps_parts(fps);
    if !t.is_finite() {
        return 0;
    }
    (t * n as f64 / d as f64 + 1e-6).floor() as i64
}

/// Start time of frame `i`.
pub fn frame_time(i: i64, fps: f64) -> f64 {
    let (n, d) = fps_parts(fps);
    i as f64 * d as f64 / n as f64
}

/// `t` snapped down to the start of its frame.
pub fn snap_time(t: f64, fps: f64) -> f64 {
    frame_time(frame_index(t, fps), fps)
}

/// `mm:ss.ff` (minutes, seconds, frame within the second); hours are added as `h:mm:ss.ff`.
pub fn timecode(t: f64, fps: f64) -> String {
    let t = if t.is_finite() { t.max(0.0) } else { 0.0 };
    let i = frame_index(t, fps);
    let (n, d) = fps_parts(fps);
    let per_sec = (n as f64 / d as f64).round().max(1.0) as i64;
    let secs_total = frame_time(i, fps).floor() as i64;
    let frame_in_sec = i - frame_index(secs_total as f64, fps);
    let frame_in_sec = frame_in_sec.clamp(0, per_sec - 1);
    let h = secs_total / 3600;
    let m = (secs_total / 60) % 60;
    let s = secs_total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{frame_in_sec:02}")
    } else {
        format!("{m:02}:{s:02}.{frame_in_sec:02}")
    }
}

/// Smoothstep from `e0` to `e1`.
#[inline]
pub(crate) fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    if e1 <= e0 {
        return if x < e0 { 0.0 } else { 1.0 };
    }
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Integer hash (splitmix64 finaliser) for deterministic noise.
#[inline]
pub(crate) fn hash64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Deterministic value in `-1..1` from integer coordinates.
#[inline]
pub(crate) fn noise_i(seed: u64, x: i64, y: i64) -> f32 {
    let h = hash64(seed ^ hash64((x as u64).wrapping_mul(0x632B_E59B_D9B4_E019) ^ (y as u64).wrapping_mul(0x8CB9_2BA7_2F3D_8DD7)));
    ((h >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
}

/// Smooth 1-D value noise in `-1..1` (cubic interpolation between hashed lattice values).
pub(crate) fn noise_1d(seed: u64, x: f64) -> f64 {
    let i = x.floor();
    let f = x - i;
    let i = i as i64;
    let v = |k: i64| noise_i(seed, k, 0) as f64;
    let (p0, p1, p2, p3) = (v(i - 1), v(i), v(i + 1), v(i + 2));
    // Catmull-Rom.
    let a = -0.5 * p0 + 1.5 * p1 - 1.5 * p2 + 0.5 * p3;
    let b = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
    let c = -0.5 * p0 + 0.5 * p2;
    (((a * f + b) * f + c) * f + p1).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_grid() {
        assert_eq!(frame_index(0.0, 30.0), 0);
        assert_eq!(frame_index(1.0, 30.0), 30);
        assert_eq!(frame_index(1.0 - 1e-9, 30.0), 30);
        assert_eq!(frame_index(0.0499, 30.0), 1);
        assert!((snap_time(0.05, 30.0) - 1.0 / 30.0).abs() < 1e-12);
        assert_eq!(frame_index(1001.0 / 30000.0 * 7.0, 29.97), 7);
        assert_eq!(timecode(0.0, 30.0), "00:00.00");
        assert_eq!(timecode(61.5, 30.0), "01:01.15");
        assert_eq!(timecode(3725.0, 25.0), "1:02:05.00");
    }

    #[test]
    fn lru_evicts_oldest() {
        let mut l: Lru<u32> = Lru::new(100, 10);
        l.insert(1, 1, 40);
        l.insert(2, 2, 40);
        assert_eq!(l.get(1), Some(1));
        l.insert(3, 3, 40);
        assert_eq!(l.get(2), None);
        assert_eq!(l.get(1), Some(1));
        assert_eq!(l.get(3), Some(3));
    }

    #[test]
    fn noise_is_deterministic() {
        assert_eq!(noise_i(1, 2, 3), noise_i(1, 2, 3));
        assert_ne!(noise_i(1, 2, 3), noise_i(2, 2, 3));
        let v = noise_1d(5, 1.25);
        assert!((-1.0..=1.0).contains(&v));
        assert_eq!(v, noise_1d(5, 1.25));
    }
}
