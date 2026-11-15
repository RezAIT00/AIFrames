//! Keyframe editing on the timeline (pure parts): the value ↔ y mapping of a clip's keyframe lane,
//! moving a key in time and value, adding keys on the curve, dragging the volume line, the eases
//! offered by the key menu, and reordering tracks by dragging their headers.

use aiframes_core::{eval, Clip, Ease, Key, Op, Param, ParamSpec, Project, TrackKind};

use super::geom::Row;
use super::headers::{frac_to_gain, gain_to_frac, FADER_MAX_DB, FADER_MIN_DB};

/// How a lane maps values to heights.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LaneScale {
    /// `lo` at the bottom, `hi` at the top.
    Linear { lo: f64, hi: f64 },
    /// Linear gain on the dB fader scale of the track headers (−60 … +24.1 dB).
    Gain,
}

/// A keyframe lane: a band of a clip (screen y range) and its value scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lane {
    pub top: f32,
    pub bottom: f32,
    pub scale: LaneScale,
}

impl Lane {
    fn span(&self) -> f32 {
        (self.bottom - self.top - 6.0).max(1.0)
    }

    /// Screen y of a value.
    pub fn y(&self, v: f64) -> f32 {
        let f = match self.scale {
            LaneScale::Linear { lo, hi } => (((v - lo) / (hi - lo).max(1e-12)).clamp(0.0, 1.0)) as f32,
            LaneScale::Gain => gain_to_frac(v),
        };
        self.bottom - 3.0 - self.span() * f
    }

    /// The value at screen y.
    pub fn v(&self, y: f32) -> f64 {
        let f = ((self.bottom - 3.0 - y) / self.span()).clamp(0.0, 1.0);
        match self.scale {
            LaneScale::Linear { lo, hi } => lo + (hi - lo) * f as f64,
            LaneScale::Gain => frac_to_gain(f),
        }
    }

    /// `v` moved by a vertical drag of `dy` pixels (down = lower), as a delta in the lane's units
    /// (dB for gain lanes) — not re-derived from a screen position, so values outside the visible
    /// range are kept and no vertical motion means no change at all.
    pub fn shift(&self, v: f64, dy: f32) -> f64 {
        if dy == 0.0 || !dy.is_finite() {
            return v;
        }
        let px = self.span() as f64;
        match self.scale {
            LaneScale::Linear { lo, hi } => v - dy as f64 * (hi - lo) / px,
            LaneScale::Gain => {
                let db0 = if v > 1e-6 { 20.0 * v.log10() } else { FADER_MIN_DB };
                let db1 = db0 - dy as f64 * (FADER_MAX_DB - FADER_MIN_DB) / px;
                if db1 <= FADER_MIN_DB {
                    0.0
                } else {
                    10f64.powf(db1 / 20.0).min(16.0)
                }
            }
        }
    }
}

/// The value range a lane shows: the keys' range padded by 15 %, at least 20 % of the largest
/// magnitude or 1 % of the registry range (1 when both are 0), kept inside the registry range.
pub fn lane_range(p: &Param, spec: Option<&ParamSpec>) -> (f64, f64) {
    let vals: Vec<f64> = match p {
        Param::Value(v) => vec![*v],
        Param::Animated { keys } => keys.iter().map(|k| k.v).collect(),
    };
    let (mut lo, mut hi) = vals.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), v| (a.min(*v), b.max(*v)));
    if !lo.is_finite() {
        lo = spec.map(|s| s.default).unwrap_or(0.0);
        hi = lo;
    }
    let rel = 0.2 * lo.abs().max(hi.abs());
    let reg = spec.map(|s| (s.max - s.min) * 0.01).unwrap_or(0.0);
    let min_span = if rel.max(reg) > 1e-9 { rel.max(reg) } else { 1.0 };
    let pad = ((hi - lo) * 0.15).max((min_span - (hi - lo)).max(0.0) * 0.5);
    let (mut a, mut b) = (lo - pad, hi + pad);
    if let Some(s) = spec {
        if b - a > s.max - s.min {
            return (s.min, s.max);
        }
        if a < s.min {
            b += s.min - a;
            a = s.min;
        }
        if b > s.max {
            a -= b - s.max;
            b = s.max;
        }
    }
    (a, b)
}

/// The keys with key `i` moved to clip-local time `t` and value `v`: the time stays between its
/// neighbours (`gap` apart) and inside `0..=dur`, the value inside `range`.
pub fn move_key(keys: &[Key], i: usize, t: f64, v: f64, dur: f64, gap: f64, range: (f64, f64)) -> Vec<Key> {
    let mut out = keys.to_vec();
    if i >= out.len() {
        return out;
    }
    let lo = if i > 0 { out[i - 1].t + gap } else { 0.0 };
    let hi = if i + 1 < out.len() { out[i + 1].t - gap } else { dur };
    let t = if t.is_finite() && lo <= hi { t.clamp(lo, hi) } else { out[i].t };
    out[i].t = (t * 1e6).round() / 1e6;
    // A value that only differs by screen-rounding noise (e.g. a time-only drag that went through a
    // lane's y ↔ value mapping) keeps the key's exact value.
    let old = out[i].v;
    let scale = old.abs().max(1e-3 * (range.1 - range.0).abs().min(1e6));
    if v.is_finite() && (v - old).abs() > 2e-5 * scale {
        out[i].v = crate::util::clamp_safe(v, range.0, range.1);
    }
    out
}

/// One op that sets all the keys of a parameter.
pub fn set_keys_op(clip: &str, path: &str, keys: Vec<Key>) -> Op {
    Op::SetParam { clip: clip.into(), path: path.into(), value: None, keys: Some(keys) }
}

/// A new key on the curve at clip-local `t` (the curve keeps its shape).
pub fn add_key_op(clip: &str, path: &str, p: &Param, t: f64) -> Op {
    Op::AddKeyframe { clip: clip.into(), path: path.into(), t: (t * 1e6).round() / 1e6, v: eval::sample(p, t), ease: None }
}

/// Changes the ease of the segment after `key` (`add_keyframe` replaces the key at its time).
pub fn ease_op(clip: &str, path: &str, key: &Key, ease: Ease) -> Op {
    Op::AddKeyframe { clip: clip.into(), path: path.into(), t: key.t, v: key.v, ease: Some(ease) }
}

/// A parameter value as shown to people: % for fractions and scale, dB for volume, px, degrees.
pub fn format_value(spec: Option<&ParamSpec>, path: &str, v: f64) -> String {
    if path == "volume" {
        return super::headers::db_label(v);
    }
    match spec.map(|s| s.unit).unwrap_or("") {
        "frac" => format!("{:.0} %", v * 100.0),
        "x" if path == "transform.scale" => format!("{:.1} %", v * 100.0),
        "x" => format!("{v:.2}×"),
        "px" => format!("{v:.0} px"),
        "deg" => format!("{v:.1}°"),
        "s" => format!("{v:.2} s"),
        _ => format!("{v:.3}"),
    }
}

/// The eases offered by the keyframe menu.
pub const EASES: &[(&str, Ease)] = &[
    ("Linear", Ease::Linear),
    ("Hold", Ease::Hold),
    ("Ease in", Ease::In),
    ("Ease out", Ease::Out),
    ("Ease in-out", Ease::InOut),
    ("Back out (overshoot)", Ease::BackOut),
    ("Elastic", Ease::Elastic),
];

/// The volume line dragged so the gain at the grabbed point goes from `from` to `to`: a constant
/// volume becomes `to`; animated volume keys are all scaled by the same ratio (the shape is kept).
pub fn volume_drag_op(clip: &str, p: &Param, from: f64, to: f64) -> Option<Op> {
    // No audible change (below 0.005 dB, or both silent): no op.
    let same = |a: f64, b: f64| (a - b).abs() < 1e-9 || (a > 1e-6 && b > 1e-6 && (20.0 * (b / a).log10()).abs() < 0.005);
    if !to.is_finite() || same(from, to) {
        return None;
    }
    let to = (to * 1e4).round() / 1e4;
    match p {
        Param::Value(v) => (!same(*v, to)).then(|| Op::SetParam { clip: clip.into(), path: "volume".into(), value: Some(to.clamp(0.0, 16.0)), keys: None }),
        Param::Animated { keys } => {
            if from <= 1e-6 {
                return None;
            }
            let k = to / from;
            if (k - 1.0).abs() < 1e-6 {
                return None;
            }
            let keys = keys.iter().map(|x| Key { t: x.t, v: ((x.v * k).clamp(0.0, 16.0) * 1e4).round() / 1e4, ease: x.ease.clone() }).collect();
            Some(set_keys_op(clip, "volume", keys))
        }
    }
}

/// The parameter a clip's keyframe lane shows: `preferred` when the clip has it, else the first
/// animated parameter (volume only for clips without sound — they have the volume line instead).
pub fn lane_param(c: &Clip, preferred: Option<&str>, has_sound: bool) -> Option<String> {
    if let Some(p) = preferred {
        if aiframes_core::param_paths(c).iter().any(|x| x == p) && !(has_sound && p == "volume") {
            return Some(p.to_string());
        }
    }
    aiframes_core::clip_params(c).into_iter().find(|(path, p)| p.is_animated() && !(has_sound && path == "volume")).map(|(path, _)| path)
}

/// Where a dragged track header would land: the display slot among the rows of its kind (0 = above
/// the first) for row-coordinate `y`, and the y of the insertion line.
pub fn reorder_slot(p: &Project, rows: &[Row], track: &str, y: f32) -> Option<(usize, f32)> {
    let kind = p.tracks.iter().find(|t| t.id == track)?.kind;
    let kr: Vec<&Row> = rows.iter().filter(|r| r.kind == kind).collect();
    if kr.is_empty() {
        return None;
    }
    let slot = kr.iter().filter(|r| r.y + r.h * 0.5 < y).count();
    let line = if slot < kr.len() { kr[slot].y } else { kr[kr.len() - 1].bottom() };
    Some((slot, line))
}

/// The `move_track` that puts `track` at display slot `slot` among its kind (None = no change).
/// Video rows show the highest index on top, so slots are converted to project indices.
pub fn reorder_op(p: &Project, rows: &[Row], track: &str, slot: usize) -> Option<Op> {
    let kind = p.tracks.iter().find(|t| t.id == track)?.kind;
    let display: Vec<&str> = rows.iter().filter(|r| r.kind == kind).map(|r| r.track.as_str()).collect();
    let a = display.iter().position(|t| *t == track)?;
    if slot == a || slot == a + 1 {
        return None;
    }
    let mut d: Vec<&str> = display.clone();
    d.remove(a);
    d.insert(if slot > a { slot - 1 } else { slot }, track);
    let desired: Vec<&str> = if kind == TrackKind::Video { d.into_iter().rev().collect() } else { d };
    let ids: Vec<&str> = p.tracks.iter().map(|t| t.id.as_str()).collect();
    let from = ids.iter().position(|t| *t == track)?;
    (0..ids.len()).find_map(|i| {
        let mut v = ids.clone();
        let x = v.remove(from);
        v.insert(i, x);
        let order: Vec<&str> = v.iter().copied().filter(|id| p.tracks.iter().any(|t| t.id == *id && t.kind == kind)).collect();
        (order == desired && i != from).then(|| Op::MoveTrack { track: track.into(), index: i })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::tests::*;
    use crate::timeline::geom::layout_rows;
    use aiframes_core::*;

    fn keys(k: &[(f64, f64)]) -> Vec<Key> {
        k.iter().map(|(t, v)| Key { t: *t, v: *v, ease: Ease::Linear }).collect()
    }

    #[test]
    fn lanes_map_values_both_ways() {
        let l = Lane { top: 100.0, bottom: 160.0, scale: LaneScale::Linear { lo: 0.0, hi: 2.0 } };
        assert_eq!(l.y(0.0), 157.0);
        assert_eq!(l.y(2.0), 103.0);
        assert!((l.v(l.y(1.3)) - 1.3).abs() < 1e-6);
        assert_eq!(l.v(0.0), 2.0, "clamped at the top");
        let g = Lane { scale: LaneScale::Gain, ..l };
        assert!((g.v(g.y(1.0)) - 1.0).abs() < 1e-5);
        assert!((g.v(g.y(0.5)) - 0.5).abs() < 1e-5);
        // Ranges hug the keys but keep a minimum height and stay inside the registry range.
        let scale = clip_param_spec("transform.scale");
        let (a, b) = lane_range(&Param::Animated { keys: keys(&[(0.0, 1.0), (2.0, 1.2)]) }, scale);
        assert!(a < 1.0 && b > 1.2 && b - a >= 0.2 * 1.15 - 1e-9, "{a}..{b}");
        let (a, b) = lane_range(&Param::Value(1.0), clip_param_spec("opacity"));
        assert!(a >= 0.0 && b <= 1.0 && b > a, "{a}..{b}");
        let (a, b) = lane_range(&Param::Value(0.0), None);
        assert!((b - a - 1.0).abs() < 1e-9);
    }

    #[test]
    fn moving_keys_keeps_order_and_ranges() {
        let k = keys(&[(0.0, 1.0), (2.0, 1.2), (4.0, 1.0)]);
        let m = move_key(&k, 1, 3.0, 5.0, 10.0, 0.1, (0.0, 2.0));
        assert_eq!((m[1].t, m[1].v), (3.0, 2.0), "value clamped to the range");
        let m = move_key(&k, 1, 9.0, 1.0, 10.0, 0.1, (0.0, 2.0));
        assert!((m[1].t - 3.9).abs() < 1e-9, "stays before the next key");
        let m = move_key(&k, 0, -5.0, 1.0, 10.0, 0.1, (0.0, 2.0));
        assert_eq!(m[0].t, 0.0, "not before the clip");
        let m = move_key(&k, 2, 50.0, 1.0, 10.0, 0.1, (0.0, 2.0));
        assert_eq!(m[2].t, 10.0, "not past the clip");
        // The ops apply.
        let p = demo();
        let (q, _) = run(&p, &[set_keys_op("c1", "transform.scale", m.clone())]);
        assert_eq!(eval::sample(&q.clips[0].transform.scale, 10.0), 1.0);
        let (q, _) = run(&q, &[add_key_op("c1", "transform.scale", &q.clips[0].transform.scale.clone(), 1.0)]);
        if let Param::Animated { keys } = &q.clips[0].transform.scale {
            assert_eq!(keys.len(), 4);
            assert!((eval::sample(&q.clips[0].transform.scale, 1.0) - 1.1).abs() < 1e-9, "added on the curve");
            let (q2, _) = run(&q, &[ease_op("c1", "transform.scale", &keys[1], Ease::Hold)]);
            assert!(matches!(&q2.clips[0].transform.scale, Param::Animated { keys } if keys[1].ease == Ease::Hold));
        } else {
            panic!("not animated");
        }
    }

    #[test]
    fn volume_drags_without_vertical_motion_keep_the_gain() {
        // Regression (review): the gain lane spanned −48…+12 dB and drags re-derived values from the
        // screen, so a sideways drag rewrote +18 dB to +12 dB (and quiet keys to silence).
        let lane = Lane { top: 100.0, bottom: 160.0, scale: LaneScale::Gain };
        let g0 = 8.0; // +18 dB
        assert!(volume_drag_op("c1", &Param::Value(g0), g0, lane.v(lane.y(g0))).is_none(), "+18 dB is on the lane now");
        assert!(volume_drag_op("c1", &Param::Value(g0), g0, lane.shift(g0, 0.0)).is_none());
        assert_eq!(lane.shift(g0, 0.0), g0);
        // Vertical drags are dB deltas: down 10 px on a 54 px lane = −15.6 dB from wherever it was.
        let g1 = lane.shift(g0, 10.0);
        assert!((20.0 * (g1 / g0).log10() + 10.0 * 84.1 / 54.0).abs() < 1e-6);
        assert_eq!(lane.shift(0.003, 1000.0), 0.0, "down past the bottom = silence");
        assert!(lane.shift(0.0, -10.0) > 0.0, "up from silence starts at the bottom of the scale");
        // Moving keys in time only keeps their values exactly (even through the lane mapping).
        let keys0 = vec![Key { t: 0.0, v: 8.0, ease: Ease::Linear }, Key { t: 2.0, v: 0.003, ease: Ease::Linear }, Key { t: 4.0, v: 1.0, ease: Ease::Linear }];
        for i in 0..2 {
            for v in [lane.v(lane.y(keys0[i].v)), lane.shift(keys0[i].v, 0.0)] {
                let moved = move_key(&keys0, i, keys0[i].t + 0.5, v, 10.0, 1.0 / 60.0, (0.0, 16.0));
                assert_eq!(moved[i].v, keys0[i].v, "key {i}");
                assert_eq!(moved[i].t, keys0[i].t + 0.5);
            }
        }
        // Linear lanes shift in their units.
        let l = Lane { top: 0.0, bottom: 106.0, scale: LaneScale::Linear { lo: 0.0, hi: 100.0 } };
        assert_eq!(l.shift(50.0, -10.0), 60.0);
    }

    #[test]
    fn values_read_like_the_inspector() {
        assert_eq!(format_value(clip_param_spec("opacity"), "opacity", 0.5), "50 %");
        assert_eq!(format_value(clip_param_spec("transform.scale"), "transform.scale", 1.18), "118.0 %");
        assert_eq!(format_value(clip_param_spec("volume"), "volume", 1.0), "0.0 dB");
        assert_eq!(format_value(clip_param_spec("transform.rotation"), "transform.rotation", 12.0), "12.0°");
        assert_eq!(format_value(None, "effects.0.x", 0.25), "0.250");
    }

    #[test]
    fn volume_line_drags() {
        let p = demo();
        let op = volume_drag_op("c3", &Param::Value(1.0), 1.0, 0.5).unwrap();
        let (q, _) = run(&p, &[op]);
        assert_eq!(q.clips[2].volume, Param::Value(0.5));
        let anim = Param::Animated { keys: keys(&[(0.0, 1.0), (5.0, 0.5)]) };
        let op = volume_drag_op("c3", &anim, 1.0, 0.5).unwrap();
        let (q, _) = run(&p, &[op]);
        assert_eq!(eval::sample(&q.clips[2].volume, 5.0), 0.25, "the whole curve scales");
        assert!(volume_drag_op("c3", &Param::Value(1.0), 1.0, 1.0).is_none());
        // Lane choice: preferred, else the first animated one; volume stays on its own line.
        let mut c = p.clips[0].clone();
        assert_eq!(lane_param(&c, None, true), None);
        c.volume = anim.clone();
        assert_eq!(lane_param(&c, None, true), None);
        assert_eq!(lane_param(&c, None, false).as_deref(), Some("volume"));
        c.opacity = anim;
        assert_eq!(lane_param(&c, None, true).as_deref(), Some("opacity"));
        assert_eq!(lane_param(&c, Some("transform.rotation"), true).as_deref(), Some("transform.rotation"));
        assert_eq!(lane_param(&c, Some("nope"), true).as_deref(), Some("opacity"));
    }

    #[test]
    fn header_drags_reorder_tracks() {
        let mut p = demo();
        p.tracks.insert(1, crate::edits::tests::track("a9", TrackKind::Audio, "sfx")); // interleaved kinds
        let rows = layout_rows(&p, &Default::default());
        // Display: v2, v1 | a9, a1, a2. Drag v1 above v2 (slot 0).
        let (slot, _) = reorder_slot(&p, &rows, "v1", rows[0].y + 2.0).unwrap();
        assert_eq!(slot, 0);
        let op = reorder_op(&p, &rows, "v1", 0).unwrap();
        let (q, _) = run(&p, &[op]);
        let top = layout_rows(&q, &Default::default());
        assert_eq!(top[0].track, "v1", "v1 is now the top layer");
        // Dropping in place is no change.
        assert!(reorder_op(&p, &rows, "v1", 1).is_none());
        assert!(reorder_op(&p, &rows, "v1", 2).is_none());
        // Audio: move a9 to the bottom.
        let op = reorder_op(&p, &rows, "a9", 3).unwrap();
        let (q, _) = run(&p, &[op]);
        let order: Vec<String> = layout_rows(&q, &Default::default()).into_iter().filter(|r| r.kind == TrackKind::Audio).map(|r| r.track).collect();
        assert_eq!(order, vec!["a1", "a2", "a9"]);
        let (slot, line) = reorder_slot(&p, &rows, "a9", 10_000.0).unwrap();
        assert_eq!(slot, 3);
        assert_eq!(line, rows.last().unwrap().bottom());
    }
}
