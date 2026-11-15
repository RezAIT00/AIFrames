//! A min/max pyramid over a [`Waveform`] so drawing one pixel column costs O(1) at any zoom
//! (zoomed out, one column covers thousands of 5 ms bins).

use aiframes_media::Waveform;

/// Level `k` has bins of `4^k` base bins.
#[derive(Clone, Debug, PartialEq)]
pub struct WavePyramid {
    pub bins_per_sec: f64,
    levels: Vec<(Vec<f32>, Vec<f32>)>,
}

impl WavePyramid {
    pub fn new(w: &Waveform) -> Self {
        let mut levels = vec![(w.min.clone(), w.max.clone())];
        while levels.last().is_some_and(|(mn, _)| mn.len() > 4) {
            let (mn, mx) = levels.last().cloned().unwrap_or_default();
            let n = mn.len().div_ceil(4);
            let mut a = Vec::with_capacity(n);
            let mut b = Vec::with_capacity(n);
            for i in 0..n {
                let s = i * 4;
                let e = (s + 4).min(mn.len());
                a.push(mn[s..e].iter().copied().fold(0.0f32, f32::min));
                b.push(mx[s..e].iter().copied().fold(0.0f32, f32::max));
            }
            levels.push((a, b));
        }
        WavePyramid { bins_per_sec: if w.bins_per_sec > 0.0 { w.bins_per_sec } else { 200.0 }, levels }
    }

    pub fn is_empty(&self) -> bool {
        self.levels.first().is_none_or(|(m, _)| m.is_empty())
    }

    /// (min, max) over source seconds `[t0, t1)`; `(0, 0)` outside the audio.
    pub fn range(&self, t0: f64, t1: f64) -> (f32, f32) {
        if self.is_empty() || !t0.is_finite() || !t1.is_finite() {
            return (0.0, 0.0);
        }
        let (t0, t1) = if t1 < t0 { (t1, t0) } else { (t0, t1) };
        let len = self.levels[0].0.len();
        let a = ((t0 * self.bins_per_sec).floor().max(0.0) as usize).min(len);
        let b = ((t1 * self.bins_per_sec).ceil() as usize).max(a + 1).min(len);
        // Segment-tree walk: unaligned bins at each level, then up one level.
        let (mut lo, mut hi, mut k) = (a, b, 0usize);
        let (mut out_min, mut out_max) = (0.0f32, 0.0f32);
        let mut acc = |k: usize, i: usize| {
            let (mn, mx) = &self.levels[k];
            if let (Some(a), Some(b)) = (mn.get(i), mx.get(i)) {
                out_min = out_min.min(*a);
                out_max = out_max.max(*b);
            }
        };
        while lo < hi {
            if k + 1 >= self.levels.len() {
                for i in lo..hi {
                    acc(k, i);
                }
                break;
            }
            while lo < hi && lo % 4 != 0 {
                acc(k, lo);
                lo += 1;
            }
            while lo < hi && hi % 4 != 0 {
                hi -= 1;
                acc(k, hi);
            }
            lo /= 4;
            hi /= 4;
            k += 1;
        }
        (out_min, out_max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wf(n: usize) -> Waveform {
        // A ramp: bin i peaks at i / n.
        let max: Vec<f32> = (0..n).map(|i| i as f32 / n as f32).collect();
        let min: Vec<f32> = max.iter().map(|v| -v * 0.5).collect();
        Waveform { bins_per_sec: 200.0, duration: n as f64 / 200.0, rms: max.clone(), min, max }
    }

    #[test]
    fn pyramid_matches_direct_ranges() {
        let w = wf(10_000);
        let p = WavePyramid::new(&w);
        for (t0, t1) in [(0.0, 0.01), (1.0, 1.5), (3.3, 49.9), (0.0, 50.0), (10.0, 10.004)] {
            let (a, b) = p.range(t0, t1);
            let (da, db) = w.range(t0, t1);
            // Coarse levels may include up to a few extra bins at the edges.
            assert!((a - da).abs() < 0.01 && (b - db).abs() < 0.01, "{t0}..{t1}: {a},{b} vs {da},{db}");
        }
        assert_eq!(p.range(100.0, 101.0), (0.0, 0.0));
        assert!(WavePyramid::new(&Waveform::default()).is_empty());
        assert_eq!(WavePyramid::new(&Waveform::default()).range(0.0, 1.0), (0.0, 0.0));
    }
}
