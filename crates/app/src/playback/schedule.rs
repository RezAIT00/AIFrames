//! Frame scheduling for the render workers and frame picking for the viewer (pure logic).

use std::collections::BTreeMap;

/// Frame-index step between rendered frames at a playback rate (2x shows every other frame).
pub fn step_for_rate(rate: f64) -> i64 {
    let s = rate.abs().round().max(1.0) as i64;
    if rate < 0.0 {
        -s
    } else {
        s
    }
}

/// Result of [`claim`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claim {
    /// The frame to render now, if any.
    pub index: Option<i64>,
    /// The next index to claim after this one.
    pub next: i64,
    /// Frames given up because the playhead already passed them (render was behind).
    pub skipped: u64,
}

/// Picks the next frame to render ahead of the playhead.
///
/// `next` is the next frame in play order, `clock` the frame the playhead is on, `step` the signed
/// frame step, `lookahead` how many steps ahead of the playhead may be rendered, `[lo, hi]` the
/// playable frame range. Frames the playhead already passed are skipped (and counted).
pub fn claim(next: i64, clock: i64, step: i64, lookahead: i64, lo: i64, hi: i64) -> Claim {
    let step = if step == 0 { 1 } else { step };
    let dir = step.signum();
    let mag = step.abs();
    let mut next = next;
    let mut skipped = 0;
    if (next - clock) * dir < 0 {
        // Behind: the playhead is past `next`; jump to the frame after the playhead.
        skipped = (((clock - next) * dir) / mag) as u64;
        next = clock + step;
    }
    if (next - clock) * dir > lookahead * mag || next < lo || next > hi {
        return Claim { index: None, next, skipped };
    }
    Claim { index: Some(next), next: next + step, skipped }
}

/// The ready frame to show for playhead frame `clock` moving in direction `dir` (+1/-1): the
/// latest frame not after the playhead. `None` when nothing suitable is ready.
pub fn pick<T>(ready: &BTreeMap<i64, T>, clock: i64, dir: i64) -> Option<i64> {
    if dir >= 0 {
        ready.range(..=clock).next_back().map(|(k, _)| *k)
    } else {
        ready.range(clock..).next().map(|(k, _)| *k)
    }
}

/// Removes frames the playhead has passed (keeps `keep_behind` frames behind it).
pub fn drop_passed<T>(ready: &mut BTreeMap<i64, T>, clock: i64, dir: i64, keep_behind: i64) {
    if dir >= 0 {
        let cut = clock - keep_behind;
        *ready = ready.split_off(&cut);
    } else {
        let cut = clock + keep_behind + 1;
        ready.split_off(&cut);
    }
}

/// Frames that should have been shown between two displayed frames but were not.
pub fn dropped_between(prev: i64, cur: i64, step: i64) -> u64 {
    let step = if step == 0 { 1 } else { step };
    let n = (cur - prev) * step.signum() / step.abs();
    (n - 1).max(0) as u64
}

/// Render scale for a viewer `display_px` physical pixels wide showing a `project_w` wide frame:
/// the smallest of a few fixed scales that still has (almost) as many pixels as the display,
/// so resizing the window only re-renders when a threshold is crossed.
pub fn auto_scale(display_px: f32, project_w: u32) -> f32 {
    const SCALES: [f32; 5] = [0.25, 0.375, 0.5, 0.75, 1.0];
    let w = project_w.max(1) as f32;
    let need = display_px.max(1.0) * 0.9;
    SCALES.into_iter().find(|s| s * w >= need).unwrap_or(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps() {
        assert_eq!(step_for_rate(1.0), 1);
        assert_eq!(step_for_rate(2.0), 2);
        assert_eq!(step_for_rate(-4.0), -4);
        assert_eq!(step_for_rate(0.5), 1);
    }

    #[test]
    fn claims_ahead_until_lookahead() {
        let c = claim(10, 10, 1, 4, 0, 100);
        assert_eq!(c, Claim { index: Some(10), next: 11, skipped: 0 });
        let c = claim(14, 10, 1, 4, 0, 100);
        assert_eq!(c.index, Some(14));
        let c = claim(15, 10, 1, 4, 0, 100);
        assert_eq!(c.index, None, "beyond the lookahead");
        assert_eq!(c.next, 15);
    }

    #[test]
    fn skips_when_behind() {
        let c = claim(10, 15, 1, 4, 0, 100);
        assert_eq!(c.skipped, 5);
        assert_eq!(c.index, Some(16));
        assert_eq!(c.next, 17);
        // 2x: frames 10, 12, 14 passed.
        let c = claim(10, 15, 2, 4, 0, 100);
        assert_eq!(c.skipped, 2);
        assert_eq!(c.index, Some(17));
    }

    #[test]
    fn reverse_and_range() {
        let c = claim(20, 20, -1, 3, 0, 100);
        assert_eq!(c, Claim { index: Some(20), next: 19, skipped: 0 });
        let c = claim(16, 20, -1, 3, 0, 100);
        assert_eq!(c.index, None);
        let c = claim(25, 20, -1, 3, 0, 100);
        assert_eq!(c.skipped, 5);
        assert_eq!(c.index, Some(19));
        assert_eq!(claim(101, 99, 1, 4, 0, 100).index, None, "past the end");
        assert_eq!(claim(-1, 0, -1, 4, 0, 100).index, None, "before the start");
    }

    #[test]
    fn picking_and_dropping() {
        let mut m: BTreeMap<i64, ()> = [3, 4, 5, 7, 9].into_iter().map(|k| (k, ())).collect();
        assert_eq!(pick(&m, 6, 1), Some(5));
        assert_eq!(pick(&m, 2, 1), None);
        assert_eq!(pick(&m, 6, -1), Some(7));
        assert_eq!(pick(&m, 10, -1), None);
        drop_passed(&mut m, 7, 1, 1);
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), vec![7, 9]);
        let mut m: BTreeMap<i64, ()> = [3, 4, 5, 7, 9].into_iter().map(|k| (k, ())).collect();
        drop_passed(&mut m, 5, -1, 0);
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), vec![3, 4, 5]);
    }

    #[test]
    fn dropped_counts() {
        assert_eq!(dropped_between(10, 11, 1), 0);
        assert_eq!(dropped_between(10, 13, 1), 2);
        assert_eq!(dropped_between(10, 14, 2), 1);
        assert_eq!(dropped_between(10, 7, -1), 2);
        assert_eq!(dropped_between(10, 10, 1), 0);
    }

    #[test]
    fn scales() {
        assert_eq!(auto_scale(1920.0, 1920), 1.0);
        assert_eq!(auto_scale(960.0, 1920), 0.5);
        assert_eq!(auto_scale(1000.0, 1920), 0.5, "10% upscaling is fine");
        assert_eq!(auto_scale(1200.0, 1920), 0.75);
        assert_eq!(auto_scale(300.0, 1920), 0.25);
        assert_eq!(auto_scale(5000.0, 1920), 1.0, "never above full resolution");
    }
}
