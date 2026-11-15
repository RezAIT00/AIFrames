//! Byte-bounded LRU cache of decoded frames. A video frame is stored with the interval of source
//! time it is displayed for (`[pts, next_pts)`), so any request time inside it hits.

use crate::pool::Frame;
use std::collections::{BTreeMap, HashMap};

/// What a cached frame belongs to: a file at an output size and purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct StreamKey {
    pub file: u64,
    pub w: u32,
    pub h: u32,
    pub kind: StreamKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum StreamKind {
    Video,
    Image,
    Thumb,
}

/// Microseconds, the cache's time unit.
pub(crate) fn us(t: f64) -> i64 {
    if t == f64::INFINITY {
        i64::MAX
    } else if t == f64::NEG_INFINITY {
        i64::MIN
    } else {
        (t * 1e6).round() as i64
    }
}

struct Entry {
    frame: Frame,
    end: i64,
    tick: u64,
    bytes: usize,
    /// Low priority (export frames): may never push normal frames below half the budget.
    low: bool,
}

pub(crate) struct FrameCache {
    budget: usize,
    used: usize,
    low_used: usize,
    tick: u64,
    streams: HashMap<StreamKey, BTreeMap<i64, Entry>>,
    lru: BTreeMap<u64, (StreamKey, i64)>,
    lru_low: BTreeMap<u64, (StreamKey, i64)>,
    pub hits: u64,
    pub misses: u64,
}

impl FrameCache {
    pub fn new(budget: usize) -> Self {
        FrameCache {
            budget,
            used: 0,
            low_used: 0,
            tick: 0,
            streams: HashMap::new(),
            lru: BTreeMap::new(),
            lru_low: BTreeMap::new(),
            hits: 0,
            misses: 0,
        }
    }

    /// Bytes held by low-priority frames.
    pub fn low_used(&self) -> usize {
        self.low_used
    }

    pub fn used(&self) -> usize {
        self.used
    }
    pub fn budget(&self) -> usize {
        self.budget
    }
    pub fn len(&self) -> usize {
        self.lru.len() + self.lru_low.len()
    }

    /// The frame displayed at `t` (µs), allowing `eps` µs of timestamp jitter.
    pub fn get(&mut self, key: &StreamKey, t: i64, eps: i64) -> Option<Frame> {
        let found = self.streams.get(key).and_then(|m| {
            let (&start, e) = m.range(..=t.saturating_add(eps)).next_back()?;
            (t < e.end.saturating_sub(eps) || e.end == i64::MAX).then_some(start)
        });
        match found {
            Some(start) => {
                self.hits += 1;
                self.tick += 1;
                let tick = self.tick;
                let e = self.streams.get_mut(key).and_then(|m| m.get_mut(&start)).expect("entry");
                let list = if e.low { &mut self.lru_low } else { &mut self.lru };
                list.remove(&e.tick);
                e.tick = tick;
                list.insert(tick, (*key, start));
                Some(e.frame.clone())
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    /// The smallest cached video frame of `file` at least `w`x`h` (same aspect) displayed at `t`.
    pub fn get_larger(&mut self, file: u64, t: i64, eps: i64, w: u32, h: u32) -> Option<Frame> {
        let aspect = w as f64 / h.max(1) as f64;
        let mut keys: Vec<StreamKey> = self
            .streams
            .keys()
            .filter(|k| {
                k.file == file
                    && k.kind == StreamKind::Video
                    && k.w >= w
                    && k.h >= h
                    && (k.w > w || k.h > h)
                    && ((k.w as f64 / k.h.max(1) as f64) - aspect).abs() / aspect < 0.02
            })
            .copied()
            .collect();
        keys.sort_by_key(|k| k.w as u64 * k.h as u64);
        for k in keys {
            let (hits, misses) = (self.hits, self.misses);
            let f = self.get(&k, t, eps);
            (self.hits, self.misses) = (hits, misses);
            if f.is_some() {
                return f;
            }
        }
        None
    }

    /// Stores a frame displayed over `[start, end)` µs (`end == i64::MAX` = until the end).
    pub fn insert(&mut self, key: StreamKey, start: i64, end: i64, frame: Frame) {
        self.insert_with(key, start, end, frame, false);
    }

    /// `insert` into the normal or the low-priority class. Each class may hold half the budget
    /// and borrow what the other leaves free: when the cache is full, a new frame evicts from the
    /// other class only while that class holds more than half, else its own class's oldest. So low
    /// priority (export) frames never push normal (playback) frames below half the cache.
    pub fn insert_with(&mut self, key: StreamKey, start: i64, end: i64, frame: Frame, low: bool) {
        let bytes = frame.rgba.len() + 64;
        if bytes > self.budget {
            return;
        }
        self.tick += 1;
        let tick = self.tick;
        let m = self.streams.entry(key).or_default();
        if let Some(old) = m.insert(start, Entry { frame, end, tick, bytes, low }) {
            self.used -= old.bytes;
            if old.low {
                self.low_used -= old.bytes;
                self.lru_low.remove(&old.tick);
            } else {
                self.lru.remove(&old.tick);
            }
        }
        self.used += bytes;
        if low {
            self.low_used += bytes;
            self.lru_low.insert(tick, (key, start));
        } else {
            self.lru.insert(tick, (key, start));
        }
        self.evict_for(low, tick);
    }

    /// Evicts until within budget, making room for a frame of class `low` (see `insert_with`).
    fn evict_for(&mut self, low: bool, keep_tick: u64) {
        while self.used > self.budget {
            let half = self.budget / 2;
            let other_over_half = if low { self.used - self.low_used > half } else { self.low_used > half };
            // Evict from the other class while it is over its half, else from this class.
            let first = if other_over_half { !low } else { low };
            let evicted = self.evict_one(first, keep_tick) || self.evict_one(!first, keep_tick);
            if !evicted {
                break;
            }
        }
    }

    /// Evicts the least recently used entry of one class; false if there is none (or it is `keep`).
    fn evict_one(&mut self, low: bool, keep_tick: u64) -> bool {
        let list = if low { &mut self.lru_low } else { &mut self.lru };
        let Some((&tick, &(key, start))) = list.iter().next() else { return false };
        if tick == keep_tick {
            return false;
        }
        list.remove(&tick);
        if let Some(m) = self.streams.get_mut(&key) {
            if let Some(e) = m.remove(&start) {
                self.used -= e.bytes;
                if e.low {
                    self.low_used -= e.bytes;
                }
            }
            if m.is_empty() {
                self.streams.remove(&key);
            }
        }
        true
    }

    /// Drops every frame of a file (all sizes).
    pub fn remove_file(&mut self, file: u64) {
        let keys: Vec<StreamKey> = self.streams.keys().filter(|k| k.file == file).copied().collect();
        for k in keys {
            if let Some(m) = self.streams.remove(&k) {
                for e in m.values() {
                    self.used -= e.bytes;
                    if e.low {
                        self.low_used -= e.bytes;
                        self.lru_low.remove(&e.tick);
                    } else {
                        self.lru.remove(&e.tick);
                    }
                }
            }
        }
    }

    pub fn clear(&mut self) {
        self.streams.clear();
        self.lru.clear();
        self.lru_low.clear();
        self.used = 0;
        self.low_used = 0;
    }

    pub fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
        self.evict_for(false, u64::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn frame(n: u8, bytes: usize) -> Frame {
        Frame { width: 1, height: 1, rgba: Arc::new(vec![n; bytes]), time: n as f64 }
    }
    const K: StreamKey = StreamKey { file: 1, w: 1, h: 1, kind: StreamKind::Video };

    #[test]
    fn interval_lookup() {
        let mut c = FrameCache::new(1 << 20);
        c.insert(K, us(1.0), us(1.5), frame(1, 10));
        c.insert(K, us(1.5), us(2.0), frame(2, 10));
        assert_eq!(c.get(&K, us(1.2), 10).unwrap().rgba[0], 1);
        assert_eq!(c.get(&K, us(1.5), 10).unwrap().rgba[0], 2);
        assert_eq!(c.get(&K, us(1.4999999), 10).unwrap().rgba[0], 2); // within eps of the next frame
        assert!(c.get(&K, us(2.0), 10).is_none());
        assert!(c.get(&K, us(0.5), 10).is_none());
        c.insert(K, us(2.0), i64::MAX, frame(3, 10));
        assert_eq!(c.get(&K, us(99.0), 10).unwrap().rgba[0], 3);
    }

    #[test]
    fn lru_eviction_by_bytes() {
        let mut c = FrameCache::new(3 * (100 + 64));
        for i in 0..3u8 {
            c.insert(K, us(i as f64), us(i as f64 + 1.0), frame(i, 100));
        }
        // touch frame 0 so frame 1 becomes the least recently used
        assert!(c.get(&K, us(0.5), 0).is_some());
        c.insert(K, us(3.0), us(4.0), frame(3, 100));
        assert!(c.used() <= c.budget());
        assert!(c.get(&K, us(1.5), 0).is_none());
        assert!(c.get(&K, us(0.5), 0).is_some());
        assert!(c.get(&K, us(3.5), 0).is_some());
        assert_eq!(c.len(), 3);
        c.remove_file(1);
        assert_eq!(c.used(), 0);
    }

    #[test]
    fn low_priority_frames_never_push_normal_frames_below_half() {
        let unit = 100 + 64;
        let mut c = FrameCache::new(10 * unit);
        for i in 0..10u8 {
            c.insert(K, us(i as f64), us(i as f64 + 1.0), frame(i, 100)); // playback fills the cache
        }
        let e = StreamKey { file: 2, w: 1, h: 1, kind: StreamKind::Video };
        for i in 0..30u8 {
            c.insert_with(e, us(i as f64), us(i as f64 + 1.0), frame(i, 100), true); // an export streams through
            assert!(c.used() <= c.budget() && c.low_used() <= c.budget() / 2);
        }
        let normal = (0..10).filter(|i| c.get(&K, us(*i as f64 + 0.5), 0).is_some()).count();
        assert_eq!(normal, 5, "half the playback frames survive");
        assert!((25..30).all(|i| c.get(&e, us(i as f64 + 0.5), 0).is_some()), "the newest export frames are cached");
        // at half each, a playback frame replaces an older playback frame
        c.insert(K, us(50.0), us(51.0), frame(50, 100));
        assert_eq!((0..10).filter(|i| c.get(&K, us(*i as f64 + 0.5), 0).is_some()).count(), 4);
        assert!(c.get(&K, us(50.5), 0).is_some());
        assert_eq!(c.low_used(), 5 * unit);
        // with no playback frames, an export may use the whole cache (nothing is evicted for it)...
        let mut c = FrameCache::new(10 * unit);
        for i in 0..10u8 {
            c.insert_with(e, us(i as f64), us(i as f64 + 1.0), frame(i, 100), true);
        }
        assert_eq!(c.low_used(), 10 * unit);
        // ...and playback takes back half of it
        for i in 0..10u8 {
            c.insert(K, us(i as f64), us(i as f64 + 1.0), frame(i, 100));
        }
        assert_eq!(c.low_used(), 5 * unit);
        assert_eq!(c.used(), 10 * unit);
    }
}
