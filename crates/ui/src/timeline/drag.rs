//! Pure conversions from timeline drags to ops: move (keeping anchors), duplicate, trim, roll,
//! slip, slide, fades, transition length, markers. The widget calls these every frame while dragging
//! (to preview) and once on release (to apply).

use aiframes_core::{eval, Anchor, Clip, ClipMove, Edge, Id, MediaKind, Op, Project, Resolved, Source, Time, TrackKind};
use std::collections::{HashMap, HashSet};

use super::geom::Row;
use crate::util::{self, ancestors, asset_of, span_or_fallback};

/// How a move drag becomes ops.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MovePlan {
    pub ids: Vec<Id>,
    /// Timeline seconds (already snapped / frame-quantised).
    pub dt: f64,
    /// Source track → destination track (tracks not listed stay).
    pub tracks: HashMap<Id, Id>,
    /// Alt: copy instead of move.
    pub duplicate: bool,
    /// Ctrl: drop anchors and place at absolute times.
    pub absolute: bool,
    /// Overwrite whatever is under the moved clips (otherwise the core refuses overlaps).
    pub overwrite: bool,
}

/// The ops for a move. Anchored clips keep their anchor and change its offset (unless `absolute`);
/// clips whose anchor chain leads to another moved clip are carried by it and only change track.
/// One clip moves with `move_clip`; a group with one atomic `move_clips` (so clips can move across
/// tracks and past each other without intermediate overlaps). Copies are `duplicate_clip`s.
pub fn move_ops(p: &Project, r: &Resolved, plan: &MovePlan) -> Vec<Op> {
    let set: HashSet<&str> = plan.ids.iter().map(String::as_str).collect();
    let mut list: Vec<(&Clip, f64, bool)> = plan
        .ids
        .iter()
        .filter_map(|id| p.clips.iter().find(|c| &c.id == id))
        .map(|c| {
            let (s, ok) = span_or_fallback(p, r, c);
            (c, s.start, ok)
        })
        .collect();
    if plan.dt > 0.0 {
        list.sort_by(|a, b| b.1.total_cmp(&a.1));
    } else {
        list.sort_by(|a, b| a.1.total_cmp(&b.1));
    }
    let mut ops = Vec::new();
    let mut moves: Vec<(ClipMove, bool)> = Vec::new();
    for (c, start, resolved) in list {
        let track = plan.tracks.get(&c.track).filter(|t| **t != c.track).cloned();
        let new_abs = (start + plan.dt).max(0.0);
        if plan.duplicate {
            if plan.dt.abs() < 1e-9 && track.is_none() {
                continue;
            }
            ops.push(Op::DuplicateClip { clip: c.id.clone(), start: Some(Time::At(new_abs)), track: Some(track.unwrap_or_else(|| c.track.clone())), new_id: None, overwrite: plan.overwrite });
            continue;
        }
        let carried = !plan.absolute && resolved && ancestors(p, &c.id).iter().any(|a| set.contains(a.as_str()));
        let new_start = if carried {
            c.start.clone()
        } else {
            match &c.start {
                Time::Anchored(a) if !plan.absolute && resolved => Time::Anchored(Anchor { to: a.to.clone(), offset: a.offset + plan.dt }),
                _ => Time::At(new_abs),
            }
        };
        if new_start == c.start && track.is_none() {
            continue;
        }
        moves.push((ClipMove { clip: c.id.clone(), start: new_start, track }, carried));
    }
    match moves.len() {
        0 => {}
        1 => {
            let (m, carried) = moves.remove(0);
            ops.push(Op::MoveClip { clip: m.clip, start: m.start, track: m.track, overwrite: plan.overwrite && !carried });
        }
        _ => ops.push(Op::MoveClips { moves: moves.into_iter().map(|(m, _)| m).collect(), overwrite: plan.overwrite }),
    }
    ops
}

/// Destination tracks for moving `ids` by `delta` rows among the tracks of `kind` (display order;
/// clips on the other kind's tracks stay). `None` when a clip would leave the list.
pub fn track_shift(p: &Project, rows: &[Row], ids: &[Id], kind: TrackKind, delta: i32) -> Option<HashMap<Id, Id>> {
    let order: Vec<&Id> = rows.iter().filter(|r| r.kind == kind).map(|r| &r.track).collect();
    let mut map = HashMap::new();
    for id in ids {
        let Some(c) = p.clips.iter().find(|c| &c.id == id) else { continue };
        let Some(i) = order.iter().position(|t| **t == c.track) else { continue };
        let j = i as i32 + delta;
        if j < 0 || j >= order.len() as i32 {
            return None;
        }
        map.insert(c.track.clone(), order[j as usize].clone());
    }
    Some(map)
}

/// `delta` reduced towards 0 until every clip stays on a track of its kind.
pub fn clamp_track_delta(p: &Project, rows: &[Row], ids: &[Id], kind: TrackKind, delta: i32) -> i32 {
    let mut d = delta;
    while d != 0 && track_shift(p, rows, ids, kind, d).is_none() {
        d -= d.signum();
    }
    d
}

/// Timeline seconds of unused media before the clip's first frame and after its last frame
/// (infinite for generators, stills and freezes).
pub fn media_handles(p: &Project, c: &Clip) -> (f64, f64) {
    let Some(a) = asset_of(p, c) else { return (f64::INFINITY, f64::INFINITY) };
    if a.kind == MediaKind::Image || a.info.duration <= 0.0 {
        return (f64::INFINITY, f64::INFINITY);
    }
    let Source::Media { in_point, .. } = &c.source else { return (f64::INFINITY, f64::INFINITY) };
    let cons = eval::consumed(c);
    let dur = a.info.duration;
    let (head, tail) = if c.reverse { (dur - (in_point + cons), *in_point) } else { (*in_point, dur - (in_point + cons)) };
    let to_tl = |src: f64, speed: f64| if speed.abs() <= 1e-9 { f64::INFINITY } else { src.max(0.0) / speed.abs() };
    (to_tl(head, eval::sample(&c.speed, 0.0)), to_tl(tail, eval::sample(&c.speed, c.duration)))
}

/// `(lo, hi)` widened to contain `at` (never inverted, never NaN): when a clip cannot honour a
/// limit (e.g. it is already shorter than a frame), the edge can stay where it is.
pub fn bounds_around(lo: f64, hi: f64, at: f64) -> (f64, f64) {
    let at = if at.is_finite() { at } else { 0.0 };
    let lo = if lo.is_nan() { at } else { lo.min(at) };
    let hi = if hi.is_nan() { at } else { hi.max(at) };
    (lo, hi)
}

/// Allowed new times for a clip edge: at least one frame long, not before 0, within the media.
/// Always contains the edge's current time (so a clamp never panics, even for sub-frame clips).
pub fn trim_bounds(p: &Project, r: &Resolved, clip: &str, edge: Edge) -> Option<(f64, f64)> {
    let c = p.clips.iter().find(|c| c.id == clip)?;
    let s = r.span(clip)?;
    let fr = util::frame(p);
    let (head, tail) = media_handles(p, c);
    Some(match edge {
        Edge::Start => bounds_around((s.start - head).max(0.0), s.end - fr, s.start),
        Edge::End => bounds_around(s.start + fr, s.end + tail, s.end),
    })
}

/// [`trim_bounds`] also stopped at the neighbouring clips the core would refuse to overlap: with
/// `ripple: Some(true)` later clips move out of the way (no limit on that side); magnetically
/// (`None`) clips anchored to the trimmed clip follow it; `Some(false)` moves nothing else. Linked
/// clips (detached audio) are trimmed too, so their tracks' neighbours count as well.
pub fn trim_bounds_with(p: &Project, r: &Resolved, clip: &str, edge: Edge, ripple: Option<bool>) -> Option<(f64, f64)> {
    let (mut lo, mut hi) = trim_bounds(p, r, clip, edge)?;
    let c = p.clips.iter().find(|c| c.id == clip)?;
    let s = r.span(clip)?;
    let mut group: Vec<Id> = vec![clip.to_string()];
    group.extend(aiframes_core::linked_partners(p, clip));
    let followers: HashSet<Id> = p.clips.iter().filter(|k| ancestors(p, &k.id).iter().any(|a| group.contains(a))).map(|k| k.id.clone()).collect();
    let tracks: HashSet<&str> = group.iter().filter_map(|id| p.clips.iter().find(|k| &k.id == id)).map(|k| k.track.as_str()).collect();
    let others = |include_followers: bool| -> Vec<aiframes_core::Span> {
        p.clips
            .iter()
            .filter(|k| tracks.contains(k.track.as_str()) && !group.contains(&k.id) && (include_followers || !followers.contains(&k.id)))
            .filter_map(|k| r.span(&k.id))
            .collect()
    };
    let chained = matches!(&c.start, Time::Anchored(Anchor { to: aiframes_core::AnchorTarget::After(_), .. }));
    match edge {
        Edge::End => {
            if ripple != Some(true) {
                let next = others(ripple == Some(false)).into_iter().filter(|o| o.start >= s.end - 1e-6).map(|o| o.start).fold(f64::INFINITY, f64::min);
                hi = hi.min(next);
            }
        }
        Edge::Start => {
            let keep_start = ripple.unwrap_or(chained);
            if !keep_start {
                let prev = others(true).into_iter().filter(|o| o.end <= s.start + 1e-6).map(|o| o.end).fold(f64::NEG_INFINITY, f64::max);
                lo = lo.max(prev);
            } else if ripple != Some(true) {
                // The start stays and the end moves by the same amount: stop at the next non-follower.
                let next = others(false).into_iter().filter(|o| o.start >= s.end - 1e-6).map(|o| o.start).fold(f64::INFINITY, f64::min);
                if next.is_finite() {
                    lo = lo.max(s.start - (next - s.end));
                }
            }
        }
    }
    let at = if edge == Edge::Start { s.start } else { s.end };
    Some(bounds_around(lo, hi, at))
}

pub fn trim_op(clip: &str, edge: Edge, to: f64, ripple: Option<bool>) -> Op {
    Op::TrimClip { clip: clip.into(), edge, to: Some(to), delta: None, ripple }
}

/// Allowed positions for the cut between two touching clips.
pub fn roll_bounds(p: &Project, r: &Resolved, left: &str, right: &str) -> Option<(f64, f64)> {
    let (l, rc) = (p.clips.iter().find(|c| c.id == left)?, p.clips.iter().find(|c| c.id == right)?);
    let (sl, sr) = (r.span(left)?, r.span(right)?);
    let fr = util::frame(p);
    let (_, l_tail) = media_handles(p, l);
    let (r_head, _) = media_handles(p, rc);
    let cut = sl.end;
    Some(bounds_around((sl.start + fr).max(cut - r_head), (sr.end - fr).min(cut + l_tail), cut))
}

pub fn roll_op(left: &str, right: &str, delta: f64) -> Op {
    Op::RollEdit { left: left.into(), right: right.into(), delta }
}

/// The in-point change range for slipping (source seconds).
pub fn slip_bounds(p: &Project, c: &Clip) -> Option<(f64, f64)> {
    let a = asset_of(p, c)?;
    if a.kind == MediaKind::Image || a.info.duration <= 0.0 {
        return None;
    }
    let Source::Media { in_point, .. } = &c.source else { return None };
    let cons = eval::consumed(c);
    Some((-in_point, (a.info.duration - in_point - cons).max(-in_point)))
}

/// Slip by dragging the body `dt` timeline seconds: the media follows the pointer (in_point −=
/// dt × speed; += for reversed clips, whose source runs backwards), clamped to the media.
pub fn slip_op(p: &Project, clip: &str, dt: f64) -> Option<Op> {
    let c = p.clips.iter().find(|c| c.id == clip)?;
    let (lo, hi) = slip_bounds(p, c)?;
    let speed = eval::sample(&c.speed, 0.0).max(1e-6);
    let dir = if c.reverse { 1.0 } else { -1.0 };
    let (lo, hi) = bounds_around(lo, hi, 0.0);
    let delta = (dir * dt * speed).clamp(lo, hi);
    (delta.abs() > 1e-9).then(|| Op::SlipClip { clip: clip.into(), delta, linked: true })
}

pub fn slide_op(clip: &str, dt: f64) -> Option<Op> {
    (dt.abs() > 1e-9).then(|| Op::SlideClip { clip: clip.into(), delta: dt })
}

/// A fade length set by dragging a fade handle (kept within the clip with the other fade).
pub fn fade_op(c: &Clip, out: bool, len: f64) -> Op {
    let other = if out { c.fade_in } else { c.fade_out };
    let v = len.clamp(0.0, (c.duration - other).max(0.0));
    let v = (v * 1000.0).round() / 1000.0;
    Op::SetParam { clip: c.id.clone(), path: if out { "fade_out" } else { "fade_in" }.into(), value: Some(v), keys: None }
}

/// The longest a transition may be: the shorter of its two clips.
pub fn transition_max(r: &Resolved, from: &str, to: &str) -> f64 {
    match (r.span(from), r.span(to)) {
        (Some(a), Some(b)) => a.duration().min(b.duration()),
        _ => 0.0,
    }
}

pub fn transition_len_op(p: &Project, r: &Resolved, id: &str, len: f64) -> Option<Op> {
    let t = p.transitions.iter().find(|t| t.id == id)?;
    let max = transition_max(r, &t.from, &t.to);
    let (lo, hi) = bounds_around(util::frame(p).min(max), max.max(util::frame(p)), util::frame(p).min(max));
    let d = if len.is_finite() { len.clamp(lo, hi) } else { t.duration };
    let d = (d * 1000.0).round() / 1000.0;
    ((d - t.duration).abs() > 1e-9).then(|| Op::SetTransition { transition: id.into(), kind: None, duration: Some(d), params: Default::default() })
}

/// Moves a marker to `new_t` (an anchored marker keeps its anchor and changes its offset).
pub fn marker_move_op(p: &Project, r: &Resolved, id: &str, new_t: f64) -> Option<Op> {
    let m = p.markers.iter().find(|m| m.id == id)?;
    let new_t = new_t.max(0.0);
    let time = match (&m.time, r.marker(id)) {
        (Time::Anchored(a), Some(cur)) => Time::Anchored(Anchor { to: a.to.clone(), offset: a.offset + (new_t - cur) }),
        _ => Time::At(new_t),
    };
    (time != m.time).then(|| Op::SetMarker { marker: id.into(), time: Some(time), name: None, kind: None, color: None, note: None })
}

/// A transition dropped on a clip at time `t`: on its nearer cut.
pub fn transition_drop_op(p: &Project, r: &Resolved, clip: &str, t: f64, kind: &str) -> Option<Op> {
    let s = r.span(clip)?;
    let (from, to) = if t - s.start < s.end - t {
        (crate::edits::neighbour(p, r, clip, false)?, clip.to_string())
    } else {
        (clip.to_string(), crate::edits::neighbour(p, r, clip, true)?)
    };
    crate::edits::transition_between_op(p, r, &from, &to, kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::tests::*;
    use crate::timeline::geom::layout_rows;
    use aiframes_core::*;

    fn plan(ids: &[&str], dt: f64) -> MovePlan {
        MovePlan { ids: ids.iter().map(|s| s.to_string()).collect(), dt, ..Default::default() }
    }

    #[test]
    fn move_absolute_and_anchored() {
        let p = demo();
        let r = resolve(&p);
        let ops = move_ops(&p, &r, &plan(&["c3"], 2.0));
        assert_eq!(ops, vec![Op::MoveClip { clip: "c3".into(), start: Time::At(2.0), track: None, overwrite: false }]);
        let (_, rq) = run(&p, &ops);
        assert_eq!(rq.span("c3").unwrap().start, 2.0);
        // c2 is chained after c1: it keeps the anchor and gets an offset (a 1 s gap).
        let ops = move_ops(&p, &r, &plan(&["c2"], 1.0));
        let (q, rq) = run(&p, &ops);
        assert_eq!(rq.span("c2").unwrap().start, 11.0);
        assert_eq!(q.clips[1].start, after("c1", 1.0));
        // Ctrl: absolute.
        let mut pl = plan(&["c2"], 1.0);
        pl.absolute = true;
        let (q, _) = run(&p, &move_ops(&p, &r, &pl));
        assert_eq!(q.clips[1].start, Time::At(11.0));
        // Zero moves produce nothing.
        assert!(move_ops(&p, &r, &plan(&["c1"], 0.0)).is_empty());
    }

    #[test]
    fn chains_move_as_a_unit() {
        let p = demo();
        let r = resolve(&p);
        let ops = move_ops(&p, &r, &plan(&["c1", "c2"], 3.0));
        assert_eq!(ops.len(), 1, "c2 is carried by c1: {ops:?}");
        let (_, rq) = run(&p, &ops);
        assert_eq!(rq.span("c1").unwrap(), Span::new(3.0, 13.0));
        assert_eq!(rq.span("c2").unwrap(), Span::new(13.0, 18.0));
        // Absolute: both get times, in one atomic move_clips.
        let mut pl = plan(&["c1", "c2"], 3.0);
        pl.absolute = true;
        let ops = move_ops(&p, &r, &pl);
        assert!(matches!(&ops[..], [Op::MoveClips { moves, .. }] if moves.len() == 2), "{ops:?}");
        let (q, rq) = run(&p, &ops);
        assert_eq!(rq.span("c2").unwrap().start, 13.0);
        assert_eq!(q.clips[1].start, Time::At(13.0));
        // A group across tracks: c1 (v1) and c3 (a1) both move by +2; with c2 carried by c1.
        let ops = move_ops(&p, &r, &plan(&["c1", "c2", "c3"], 2.0));
        assert!(matches!(&ops[..], [Op::MoveClips { moves, .. }] if moves.len() == 2), "c2 is carried: {ops:?}");
        let (_, rq) = run(&p, &ops);
        assert_eq!(rq.span("c3").unwrap().start, 2.0);
        assert_eq!(rq.span("c2").unwrap().start, 12.0);
    }

    #[test]
    fn tracks_duplicates_and_overwrite() {
        let mut p = demo();
        p.clips.push(media("c9", "a2", "ma", 30.0, 5.0, 0.0));
        let r = resolve(&p);
        let rows = layout_rows(&p, &Default::default());
        let ids = vec!["c1".to_string()];
        let map = track_shift(&p, &rows, &ids, TrackKind::Video, -1).unwrap();
        assert_eq!(map.get("v1").map(String::as_str), Some("v2"), "up one row = the track above");
        assert!(track_shift(&p, &rows, &ids, TrackKind::Video, -2).is_none());
        assert_eq!(clamp_track_delta(&p, &rows, &ids, TrackKind::Video, -5), -1);
        assert_eq!(clamp_track_delta(&p, &rows, &ids, TrackKind::Video, 3), 0);
        // Alt-drag c1 up to v2: a copy.
        let pl = MovePlan { ids: ids.clone(), dt: 0.0, tracks: map, duplicate: true, ..Default::default() };
        let ops = move_ops(&p, &r, &pl);
        assert!(matches!(&ops[0], Op::DuplicateClip { track: Some(t), .. } if t == "v2"));
        let (q, _) = run(&p, &ops);
        assert_eq!(q.clips.iter().filter(|c| c.track == "v2").count(), 1);
        // Move c9 from a2 onto a1 over c3 with overwrite: c3 is cut.
        let map = track_shift(&p, &rows, &["c9".to_string()], TrackKind::Audio, -1).unwrap();
        let pl = MovePlan { ids: vec!["c9".into()], dt: -20.0, tracks: map.clone(), overwrite: true, ..Default::default() };
        let (_, rq) = run(&p, &move_ops(&p, &r, &pl));
        assert_eq!(rq.span("c9").unwrap(), Span::new(10.0, 15.0));
        assert_eq!(rq.span("c3").unwrap().end, 10.0);
        // Without overwrite the core refuses.
        let pl = MovePlan { overwrite: false, ..pl };
        let mut q = p.clone();
        assert!(apply_batch(&mut q, &move_ops(&p, &r, &pl)).is_err());
    }

    #[test]
    fn trims_respect_media() {
        let p = demo();
        let r = resolve(&p);
        let fr = 1.0 / 30.0;
        let (lo, hi) = trim_bounds(&p, &r, "c1", Edge::Start).unwrap();
        assert_eq!(lo, 0.0);
        assert!((hi - (10.0 - fr)).abs() < 1e-9);
        let (lo, hi) = trim_bounds(&p, &r, "c1", Edge::End).unwrap();
        assert!((lo - fr).abs() < 1e-9);
        assert!((hi - 115.0).abs() < 1e-9, "120 s media, in 5, 10 used: 105 s of tail");
        let (_, rq) = run(&p, &[trim_op("c1", Edge::End, 12.0, None)]);
        assert_eq!(rq.span("c1").unwrap().end, 12.0);
        assert_eq!(rq.span("c2").unwrap().start, 12.0, "magnetic: c2 follows");
        let (_, rq) = run(&p, &[trim_op("c3", Edge::Start, 4.0, Some(false))]);
        assert_eq!(rq.span("c3").unwrap(), Span::new(4.0, 20.0));
        // Speed 2: handles are half as long on the timeline.
        let (q, rq) = run(&p, &[crate::edits::speed_op("c1", 2.0)]);
        let c = q.clips.iter().find(|c| c.id == "c1").unwrap();
        assert_eq!(media_handles(&q, c), (2.5, 52.5));
        assert!(trim_bounds(&q, &rq, "c1", Edge::End).is_some());
    }

    #[test]
    fn subframe_clips_have_usable_bounds() {
        // Regression (review): bounds of clips shorter than a frame were inverted (lo > hi) and the
        // trim/roll drags panicked in f64::clamp.
        let mut p = demo();
        p.clips.push(media("t1", "v2", "mv", 1.0, 0.033, 0.0));
        let r = resolve(&p);
        for edge in [Edge::Start, Edge::End] {
            let (lo, hi) = trim_bounds(&p, &r, "t1", edge).unwrap();
            assert!(lo <= hi, "{edge:?}: ({lo}, {hi})");
            let (lo, hi) = trim_bounds_with(&p, &r, "t1", edge, None).unwrap();
            assert!(lo <= hi, "{edge:?} with neighbours: ({lo}, {hi})");
        }
        // Exactly one NTSC frame on the renderer's grid.
        let mut q = demo();
        q.settings.fps = 29.97;
        q.clips.push(media("t2", "v2", "mv", 2.0, 1001.0 / 30000.0, 0.0));
        let rq = resolve(&q);
        let (lo, hi) = trim_bounds(&q, &rq, "t2", Edge::Start).unwrap();
        assert!(lo <= hi, "({lo}, {hi})");
        // Roll between a 0.02 s clip at the end of its media and its neighbour.
        let mut q = demo();
        q.clips[0].duration = 0.02;
        q.clips[0].source = Source::Media { asset: "mv".into(), in_point: 119.98 };
        let rq = resolve(&q);
        let (lo, hi) = roll_bounds(&q, &rq, "c1", "c2").unwrap();
        assert!(lo <= hi, "roll ({lo}, {hi})");
        assert_eq!(bounds_around(5.0, 3.0, 4.0), (4.0, 4.0));
        assert_eq!(bounds_around(f64::NAN, 3.0, 4.0), (4.0, 4.0));
    }

    #[test]
    fn trims_stop_at_neighbours_the_core_would_refuse() {
        let mut p = demo();
        p.clips.push(media("c9", "a1", "ma", 25.0, 5.0, 10.0)); // a1: c3 0–20, c9 25–30 (10 s of head media)
        let r = resolve(&p);
        // c3's end can grow up to c9 (absolute, not a follower); with ripple c9 moves away.
        let (_, hi) = trim_bounds_with(&p, &r, "c3", Edge::End, None).unwrap();
        assert_eq!(hi, 25.0);
        let (_, hi) = trim_bounds_with(&p, &r, "c3", Edge::End, Some(true)).unwrap();
        assert!(hi > 25.0);
        // c1's end: c2 follows it magnetically (no limit), but not with ripple: false.
        let (_, hi) = trim_bounds_with(&p, &r, "c1", Edge::End, None).unwrap();
        assert!(hi > 10.0);
        let (_, hi) = trim_bounds_with(&p, &r, "c1", Edge::End, Some(false)).unwrap();
        assert_eq!(hi, 10.0);
        // c9's start can come back to c3's end, not further.
        let (lo, _) = trim_bounds_with(&p, &r, "c9", Edge::Start, Some(false)).unwrap();
        assert_eq!(lo, 20.0);
        // What the bounds allow, the core accepts.
        for (clip, edge, ripple) in [("c3", Edge::End, None), ("c1", Edge::End, Some(false)), ("c9", Edge::Start, Some(false))] {
            let (lo, hi) = trim_bounds_with(&p, &r, clip, edge, ripple).unwrap();
            for to in [lo, hi] {
                let mut q = p.clone();
                let cur = if edge == Edge::Start { r.span(clip).unwrap().start } else { r.span(clip).unwrap().end };
                if (to - cur).abs() > 1e-9 {
                    apply_batch(&mut q, &[trim_op(clip, edge, to, ripple)]).unwrap_or_else(|e| panic!("{clip} {edge:?} to {to}: {e}"));
                }
            }
        }
    }

    #[test]
    fn slip_follows_the_pointer_on_reversed_clips() {
        // Regression (review): reversed clips slipped against the pointer.
        for reverse in [false, true] {
            let mut p = demo();
            p.clips[0].reverse = reverse;
            p.clips[0].source = Source::Media { asset: "mv".into(), in_point: 20.0 };
            let op = slip_op(&p, "c1", 1.0).unwrap();
            let (q, _) = run(&p, &[op]);
            let before = eval::source_time(&p.clips[0], 2.0).unwrap();
            let after = eval::source_time(&q.clips[0], 3.0).unwrap();
            assert!((before - after).abs() < 1e-9, "reverse={reverse}: {before} vs {after}");
        }
    }

    #[test]
    fn roll_and_slip() {
        let p = demo();
        let r = resolve(&p);
        let (lo, hi) = roll_bounds(&p, &r, "c1", "c2").unwrap();
        assert!((lo - 1.0 / 30.0).abs() < 1e-9);
        assert!((hi - (15.0 - 1.0 / 30.0)).abs() < 1e-9);
        let (q, rq) = run(&p, &[roll_op("c1", "c2", 2.0)]);
        assert_eq!(rq.span("c1").unwrap().end, 12.0);
        assert_eq!(rq.span("c2").unwrap(), Span::new(12.0, 15.0));
        assert!(matches!(q.clips[1].source, Source::Media { in_point, .. } if (in_point - 42.0).abs() < 1e-9));
        // Slip: drag right 2 s shows 2 s earlier media.
        let op = slip_op(&p, "c1", 2.0).unwrap();
        assert_eq!(op, Op::SlipClip { clip: "c1".into(), delta: -2.0, linked: true });
        // Clamped at the media start.
        let op = slip_op(&p, "c1", 50.0).unwrap();
        let (q, rq) = run(&p, &[op]);
        assert!(matches!(q.clips[0].source, Source::Media { in_point, .. } if in_point.abs() < 1e-9));
        assert_eq!(rq.span("c1").unwrap(), Span::new(0.0, 10.0), "slip never moves the clip");
        assert!(slip_op(&p, "c1", 0.0).is_none());
        run(&p, &[slide_op("c2", -1.0).unwrap()]);
    }

    #[test]
    fn fades_transitions_markers() {
        let mut p = demo();
        p.markers.push(Marker { id: "mk1".into(), time: Time::Anchored(Anchor { to: AnchorTarget::With("c2".into()), offset: 1.0 }), name: String::new(), kind: MarkerKind::Chapter, color: None, note: String::new() });
        p.markers.push(Marker { id: "mk2".into(), time: Time::At(3.0), name: String::new(), kind: MarkerKind::Marker, color: None, note: String::new() });
        let r = resolve(&p);
        let c1 = p.clips[0].clone();
        let (q, _) = run(&p, &[fade_op(&c1, false, 2.0)]);
        assert_eq!(q.clips[0].fade_in, 2.0);
        let c1q = q.clips[0].clone();
        let op = fade_op(&c1q, true, 50.0);
        assert_eq!(op, Op::SetParam { clip: "c1".into(), path: "fade_out".into(), value: Some(8.0), keys: None });
        run(&q, &[op]);
        // Transition length is limited by the shorter clip (c2: 5 s).
        let (q, rq) = run(&p, &[crate::edits::add_transition_op(&p, &r, "c1", "crossfade").unwrap()]);
        let tr = q.transitions[0].id.clone();
        let op = transition_len_op(&q, &rq, &tr, 30.0).unwrap();
        assert!(matches!(op, Op::SetTransition { duration: Some(d), .. } if (d - 5.0).abs() < 1e-9));
        run(&q, &[op]);
        // Markers: anchored keeps its anchor.
        let op = marker_move_op(&p, &r, "mk1", 13.0).unwrap();
        let (q, rq) = run(&p, &[op]);
        assert_eq!(rq.marker("mk1"), Some(13.0));
        assert!(matches!(&q.markers[0].time, Time::Anchored(a) if (a.offset - 3.0).abs() < 1e-9));
        let (_, rq) = run(&p, &[marker_move_op(&p, &r, "mk2", 4.5).unwrap()]);
        assert_eq!(rq.marker("mk2"), Some(4.5));
        // A transition dropped near c2's start goes on the c1|c2 cut.
        let op = transition_drop_op(&p, &r, "c2", 10.5, "whip").unwrap();
        assert!(matches!(&op, Op::AddTransition { from, to, .. } if from == "c1" && to == "c2"));
        assert!(transition_drop_op(&p, &r, "c2", 14.5, "whip").is_none(), "nothing after c2");
    }
}
