//! A min/max pyramid over a [`Waveform`] so drawing one pixel column costs O(1) at any zoom
//! (zoomed out, one column covers thousands of 5 ms bins), and the waveform painter shared by the
//! timeline and the media bin.
//!
//! Drawing, like Premiere and Resolve: one column per *physical* pixel, the peak envelope with the
//! RMS body inside it, on a dB scale by default ([`LOG_RANGE_DB`] of range, so quiet dialogue and
//! music beds stay readable instead of a flat line), scaled by the clip's volume and fades, with
//! clipping marked, and bottom-up ("rectified") on short rows so the whole height carries signal.

use aiframes_media::Waveform;
use egui::epaint::Mesh;
use egui::{pos2, Color32, Painter, Rect, Shape};

/// Level `k` has bins of `4^k` base bins.
#[derive(Clone, Debug, PartialEq)]
pub struct WavePyramid {
    pub bins_per_sec: f64,
    levels: Vec<(Vec<f32>, Vec<f32>)>,
    /// Per level, the sum over its base bins of RMS² (for the RMS of any range).
    sq: Vec<Vec<f32>>,
}

impl WavePyramid {
    pub fn new(w: &Waveform) -> Self {
        let mut levels = vec![(w.min.clone(), w.max.clone())];
        let base_sq: Vec<f32> = if w.rms.len() == w.min.len() { w.rms.iter().map(|r| r * r).collect() } else { w.min.iter().zip(&w.max).map(|(a, b)| { let p = a.abs().max(b.abs()) * std::f32::consts::FRAC_1_SQRT_2; p * p }).collect() };
        let mut sq = vec![base_sq];
        while levels.last().is_some_and(|(mn, _)| mn.len() > 4) {
            let (mn, mx) = levels.last().cloned().unwrap_or_default();
            let prev_sq = sq.last().cloned().unwrap_or_default();
            let n = mn.len().div_ceil(4);
            let mut a = Vec::with_capacity(n);
            let mut b = Vec::with_capacity(n);
            let mut s2 = Vec::with_capacity(n);
            for i in 0..n {
                let s = i * 4;
                let e = (s + 4).min(mn.len());
                a.push(mn[s..e].iter().copied().fold(0.0f32, f32::min));
                b.push(mx[s..e].iter().copied().fold(0.0f32, f32::max));
                s2.push(prev_sq[s..e.min(prev_sq.len())].iter().sum());
            }
            levels.push((a, b));
            sq.push(s2);
        }
        WavePyramid { bins_per_sec: if w.bins_per_sec > 0.0 { w.bins_per_sec } else { 200.0 }, levels, sq }
    }

    pub fn is_empty(&self) -> bool {
        self.levels.first().is_none_or(|(m, _)| m.is_empty())
    }

    /// Base bin range `[a, b)` of source seconds `[t0, t1)`.
    fn bins(&self, t0: f64, t1: f64) -> Option<(usize, usize)> {
        if self.is_empty() || !t0.is_finite() || !t1.is_finite() {
            return None;
        }
        let (t0, t1) = if t1 < t0 { (t1, t0) } else { (t0, t1) };
        let len = self.levels[0].0.len();
        let a = ((t0 * self.bins_per_sec).floor().max(0.0) as usize).min(len);
        let b = ((t1 * self.bins_per_sec).ceil() as usize).max(a + 1).min(len);
        (a < b).then_some((a, b))
    }

    /// Segment-tree walk over `[a, b)`: unaligned nodes at each level, then up one level.
    fn walk(&self, a: usize, b: usize, mut acc: impl FnMut(usize, usize)) {
        let (mut lo, mut hi, mut k) = (a, b, 0usize);
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
    }

    /// (min, max) over source seconds `[t0, t1)`; `(0, 0)` outside the audio.
    pub fn range(&self, t0: f64, t1: f64) -> (f32, f32) {
        let Some((a, b)) = self.bins(t0, t1) else { return (0.0, 0.0) };
        let (mut out_min, mut out_max) = (0.0f32, 0.0f32);
        self.walk(a, b, |k, i| {
            let (mn, mx) = &self.levels[k];
            if let (Some(a), Some(b)) = (mn.get(i), mx.get(i)) {
                out_min = out_min.min(*a);
                out_max = out_max.max(*b);
            }
        });
        (out_min, out_max)
    }

    /// RMS level over source seconds `[t0, t1)`; `0` outside the audio.
    pub fn rms(&self, t0: f64, t1: f64) -> f32 {
        let Some((a, b)) = self.bins(t0, t1) else { return 0.0 };
        let mut sum = 0.0f64;
        self.walk(a, b, |k, i| sum += self.sq[k].get(i).copied().unwrap_or(0.0) as f64);
        (sum / (b - a) as f64).max(0.0).sqrt() as f32
    }
}

/// Range of the dB waveform scale: a peak at 0 dBFS fills the height, one `LOG_RANGE_DB` below is
/// the baseline.
pub const LOG_RANGE_DB: f32 = 48.0;
/// Rows shorter than this (points) draw the waveform bottom-up.
pub const RECTIFY_BELOW: f32 = 30.0;

/// Display height (0..1) of a linear amplitude (`1.0` = full scale; above that it clips).
pub fn level(a: f32, log: bool) -> f32 {
    let a = a.abs();
    if !a.is_finite() {
        return if a.is_nan() { 0.0 } else { 1.0 };
    }
    if !log {
        return a.min(1.0);
    }
    if a <= 0.0 {
        return 0.0;
    }
    ((20.0 * a.log10() + LOG_RANGE_DB) / LOG_RANGE_DB).clamp(0.0, 1.0)
}

/// One column's signal, already scaled by the clip's gain.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Column {
    pub min: f32,
    pub max: f32,
    pub rms: f32,
}

/// How to draw a waveform.
#[derive(Clone, Copy, Debug)]
pub struct WaveLook {
    pub color: Color32,
    /// dB scale (else linear amplitude).
    pub log: bool,
    /// Bottom-up from the lower edge (short rows) instead of mirrored around the middle.
    pub rectified: bool,
    /// Physical pixels per point (one column per physical pixel).
    pub ppp: f32,
}

/// Paints a waveform over `area`, columns from `x0` to `x1` (points, clipped by the painter).
/// `column(a, b)` gives the signal between points `a` and `b`, or `None` where there is no audio.
pub fn paint(p: &Painter, area: Rect, x0: f32, x1: f32, look: WaveLook, mut column: impl FnMut(f32, f32) -> Option<Column>) {
    if area.height() <= 1.0 || x1 <= x0 {
        return;
    }
    let step = 1.0 / look.ppp.clamp(0.5, 4.0);
    let body = look.color;
    let peak = body.gamma_multiply(0.5);
    let clip = Color32::from_rgb(0xFF, 0x5A, 0x4A);
    let mid = area.center().y;
    let half = (area.height() * 0.5 - 0.5).max(0.5);
    let full = (area.height() - 1.0).max(1.0);
    let floor = area.bottom() - 0.5;
    let mut peaks = Mesh::default();
    let mut bodies = Mesh::default();
    let mut x = (x0 / step).floor() * step;
    while x < x1 {
        let x_end = x + step;
        if let Some(c) = column(x, x_end) {
            let clipped = c.max >= 0.999 || c.min <= -0.999;
            if look.rectified {
                let pk = level(c.max.abs().max(c.min.abs()), look.log);
                let rm = level(c.rms, look.log).min(pk);
                let top = floor - (pk * full).max(0.5);
                peaks.add_colored_rect(Rect::from_min_max(pos2(x, top), pos2(x_end, floor)), peak);
                if rm > 0.0 {
                    bodies.add_colored_rect(Rect::from_min_max(pos2(x, floor - rm * full), pos2(x_end, floor)), body);
                }
                if clipped {
                    bodies.add_colored_rect(Rect::from_min_max(pos2(x, area.top()), pos2(x_end, area.top() + 1.5)), clip);
                }
            } else {
                let up = level(c.max.max(0.0), look.log);
                let down = level(c.min.min(0.0), look.log);
                let rm = level(c.rms, look.log).min(up.max(down));
                peaks.add_colored_rect(Rect::from_min_max(pos2(x, mid - (up * half).max(0.5)), pos2(x_end, mid + (down * half).max(0.5))), peak);
                if rm > 0.0 {
                    bodies.add_colored_rect(Rect::from_min_max(pos2(x, mid - rm * half), pos2(x_end, mid + rm * half)), body);
                }
                if clipped {
                    bodies.add_colored_rect(Rect::from_min_max(pos2(x, area.top()), pos2(x_end, area.top() + 1.5)), clip);
                    bodies.add_colored_rect(Rect::from_min_max(pos2(x, area.bottom() - 1.5), pos2(x_end, area.bottom())), clip);
                }
            }
        }
        x = x_end;
    }
    p.add(Shape::mesh(peaks));
    p.add(Shape::mesh(bodies));
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
            let (r, dr) = (p.rms(t0, t1), w.rms_range(t0, t1));
            assert!((r - dr).abs() < 1e-3, "rms {t0}..{t1}: {r} vs {dr}");
        }
        assert_eq!(p.range(100.0, 101.0), (0.0, 0.0));
        assert_eq!(p.rms(100.0, 101.0), 0.0);
        assert!(WavePyramid::new(&Waveform::default()).is_empty());
        assert_eq!(WavePyramid::new(&Waveform::default()).range(0.0, 1.0), (0.0, 0.0));
        assert_eq!(WavePyramid::new(&Waveform::default()).rms(0.0, 1.0), 0.0);
    }

    #[test]
    fn db_scale_keeps_quiet_audio_visible() {
        // Full scale fills the row; a -26 dB music bed (0.05) is still a third of it, where the
        // linear scale drew it as a 5 % sliver.
        assert_eq!(level(1.0, true), 1.0);
        assert!((level(0.05, true) - 0.458).abs() < 0.01, "{}", level(0.05, true));
        assert!((level(0.05, false) - 0.05).abs() < 1e-6);
        assert_eq!(level(0.001, true), 0.0, "below the range is the baseline");
        assert_eq!(level(0.0, true), 0.0);
        assert_eq!(level(-0.5, false), 0.5, "negative peaks measure the same");
        assert_eq!(level(2.0, true), 1.0, "over full scale (gain) stays inside the row");
        assert_eq!(level(f32::NAN, true), 0.0);
        // Turning a clip up 6 dB moves its waveform up by 6/48 of the row on the dB scale.
        let d = level(0.2, true) - level(0.1, true);
        assert!((d - 6.02 / LOG_RANGE_DB).abs() < 0.01, "{d}");
    }

    #[test]
    fn paints_one_column_per_physical_pixel() {
        let ctx = egui::Context::default();
        let mut calls = 0;
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), egui::vec2(400.0, 300.0))), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| {
            let p = ui.painter().clone();
            let area = Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 40.0));
            for rectified in [false, true] {
                paint(&p, area, 0.0, 100.0, WaveLook { color: Color32::WHITE, log: true, rectified, ppp: 2.0 }, |_, _| {
                    calls += 1;
                    Some(Column { min: -1.0, max: 0.5, rms: 0.3 })
                });
            }
        });
        out.textures_delta.clear();
        assert_eq!(calls, 400, "two columns per point at 2x, both styles");
    }
}
