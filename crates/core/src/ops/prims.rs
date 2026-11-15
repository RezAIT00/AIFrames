//! Building blocks shared by the ops: lookups with helpful errors, linked clips, anchor re-targeting,
//! content trims and splits, repositioning that respects anchors, range lifting, clip JSON parsing and
//! clip validation.

use super::{fail, Cx, EditError, MAX_SPEED, MIN_DURATION};
use crate::effects::{effect_spec, EFFECTS};
use crate::eval::{consumed, integrate, local_time_of_source, shift_param, source_span, split_param};
use crate::model::*;
use crate::resolve::{resolve_raw as resolve, target_of, Resolved, Span, EPS};
use crate::util;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------------------------
// Lookups
// ---------------------------------------------------------------------------------------------

pub(crate) fn clip_ix(p: &Project, id: &str) -> Result<usize, EditError> {
    p.clips.iter().position(|c| c.id == id).ok_or_else(|| clip_not_found(p, id))
}

pub(crate) fn clip_not_found(p: &Project, id: &str) -> EditError {
    if p.clips.is_empty() {
        return EditError::new(format!("clip {id} not found: the project has no clips yet (add one with add_clip or append_clip)."));
    }
    let hint = util::did_you_mean(id, p.clips.iter().map(|c| c.id.as_str()));
    let mut parts = Vec::new();
    for t in &p.tracks {
        let ids: Vec<&str> = p.clips.iter().filter(|c| c.track == t.id).map(|c| c.id.as_str()).collect();
        if !ids.is_empty() {
            parts.push(format!("{}: {}", t.id, util::join_limited(&ids, 12)));
        }
    }
    let what = if p.markers.iter().any(|m| m.id == id) {
        format!(" ({id} is a marker.)")
    } else if p.tracks.iter().any(|t| t.id == id) {
        format!(" ({id} is a track.)")
    } else if p.media.iter().any(|m| m.id == id) {
        format!(" ({id} is a media asset.)")
    } else {
        String::new()
    };
    EditError::new(format!("clip {id} not found.{what}{hint} Clips by track — {}.", parts.join("; ")))
}

pub(crate) fn track_ix(p: &Project, id: &str) -> Result<usize, EditError> {
    p.tracks.iter().position(|t| t.id == id).ok_or_else(|| {
        let hint = util::did_you_mean(id, p.tracks.iter().map(|t| t.id.as_str()));
        let list: Vec<String> = p
            .tracks
            .iter()
            .map(|t| {
                let k = if t.kind == TrackKind::Video { "video" } else { "audio" };
                if t.role.is_empty() {
                    format!("{} ({k})", t.id)
                } else {
                    format!("{} ({k}, {})", t.id, t.role)
                }
            })
            .collect();
        if list.is_empty() {
            EditError::new(format!("track {id} not found: the project has no tracks; add one with {{\"op\":\"add_track\",\"kind\":\"video\"}}."))
        } else {
            EditError::new(format!("track {id} not found.{hint} Tracks: {}.", list.join(", ")))
        }
    })
}

pub(crate) fn media_ix(p: &Project, id: &str) -> Result<usize, EditError> {
    p.media.iter().position(|m| m.id == id).ok_or_else(|| {
        let hint = util::did_you_mean(id, p.media.iter().map(|m| m.id.as_str()));
        let list: Vec<String> = p.media.iter().map(|m| format!("{} ({})", m.id, util::file_name(&m.path))).collect();
        if list.is_empty() {
            EditError::new(format!("media {id} not found: no media has been imported yet (add_media / import)."))
        } else {
            EditError::new(format!("media {id} not found.{hint} Media: {}.", util::join_limited(&list, 15)))
        }
    })
}

pub(crate) fn named_ix<T>(list: &[T], id: &str, what: &str, get: impl Fn(&T) -> &str) -> Result<usize, EditError> {
    list.iter().position(|x| get(x) == id).ok_or_else(|| {
        let ids: Vec<&str> = list.iter().map(&get).collect();
        if ids.is_empty() {
            EditError::new(format!("{what} {id} not found: there are none."))
        } else {
            let hint = util::did_you_mean(id, ids.iter().copied());
            EditError::new(format!("{what} {id} not found.{hint} Existing: {}.", util::join_limited(&ids, 15)))
        }
    })
}

pub(crate) fn span_of(r: &Resolved, id: &str) -> Result<Span, EditError> {
    r.span(id).ok_or_else(|| match r.error_for(id) {
        Some(e) => EditError::new(format!("{id} has no position on the timeline because its anchor is broken ({}). Fix its start first (move_clip).", e.message)),
        None => EditError::new(format!("{id} has no resolved position.")),
    })
}

pub(crate) fn ensure_unlocked(p: &Project, track: &str) -> Result<(), EditError> {
    if let Some(t) = p.tracks.iter().find(|t| t.id == track) {
        if t.locked {
            return fail(format!("track {track} is locked; unlock it first with {{\"op\":\"set_track\",\"track\":\"{track}\",\"locked\":false}}."));
        }
    }
    Ok(())
}

/// Fails when the clip is on a locked track.
pub(crate) fn editable(p: &Project, clip: &str) -> Result<usize, EditError> {
    let i = clip_ix(p, clip)?;
    ensure_unlocked(p, &p.clips[i].track)?;
    Ok(i)
}

pub(crate) fn media_of<'a>(p: &'a Project, c: &Clip) -> Option<&'a MediaAsset> {
    match &c.source {
        Source::Media { asset, .. } => p.media.iter().find(|m| &m.id == asset),
        _ => None,
    }
}

pub(crate) fn kind_word(c: &Clip) -> &'static str {
    match &c.source {
        Source::Media { .. } => "media",
        Source::Text(_) => "text",
        Source::Solid { .. } => "solid",
        Source::Shape(_) => "shape",
        Source::Adjustment => "adjustment",
    }
}

// ---------------------------------------------------------------------------------------------
// Anchors and dependants
// ---------------------------------------------------------------------------------------------

/// The clip whose edits `id` follows: its leader when `id` is a linked follower (detached audio),
/// else `id` itself.
pub(crate) fn leader_of(p: &Project, id: &str) -> Id {
    if let Some(c) = p.clips.iter().find(|c| c.id == id) {
        if let Time::Anchored(Anchor { to: AnchorTarget::With(l), .. }) = &c.start {
            if linked_of(p, l).iter().any(|f| f == id) {
                return l.clone();
            }
        }
    }
    id.to_string()
}

/// Clips linked to `id`: same media, same in-point/duration/speed, anchored `with` it at offset 0
/// (what `detach_audio` creates). Split, trim, slip, speed and delete keep them in sync.
pub(crate) fn linked_of(p: &Project, id: &str) -> Vec<Id> {
    let Some(c) = p.clips.iter().find(|c| c.id == id) else { return vec![] };
    let Source::Media { asset, in_point } = &c.source else { return vec![] };
    p.clips
        .iter()
        .filter(|k| {
            k.id != id
                && matches!(&k.start, Time::Anchored(Anchor { to: AnchorTarget::With(t), offset }) if t == id && offset.abs() < EPS)
                && matches!(&k.source, Source::Media { asset: a2, in_point: i2 } if a2 == asset && (i2 - in_point).abs() < 1e-6)
                && (k.duration - c.duration).abs() < 1e-6
                && k.speed == c.speed
                && k.reverse == c.reverse
        })
        .map(|k| k.id.clone())
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Node {
    Clip(usize),
    Marker(usize),
    /// A clip's `until` end anchor.
    ClipEnd(usize),
}

/// Clips and markers whose start/time or `until` end anchor targets `id`.
pub(crate) fn dependants(p: &Project, id: &str) -> Vec<Node> {
    let mut v: Vec<Node> = p
        .clips
        .iter()
        .enumerate()
        .filter(|(_, c)| matches!(&c.start, Time::Anchored(a) if target_of(&a.to) == id))
        .map(|(i, _)| Node::Clip(i))
        .collect();
    v.extend(
        p.markers
            .iter()
            .enumerate()
            .filter(|(_, m)| matches!(&m.time, Time::Anchored(a) if target_of(&a.to) == id))
            .map(|(i, _)| Node::Marker(i)),
    );
    v.extend(
        p.clips
            .iter()
            .enumerate()
            .filter(|(_, c)| matches!(&c.until, Some(Time::Anchored(a)) if target_of(&a.to) == id))
            .map(|(i, _)| Node::ClipEnd(i)),
    );
    v
}

pub(crate) fn node_time(p: &mut Project, n: Node) -> &mut Time {
    match n {
        Node::Clip(i) => &mut p.clips[i].start,
        Node::Marker(i) => &mut p.markers[i].time,
        Node::ClipEnd(i) => p.clips[i].until.get_or_insert(Time::At(0.0)),
    }
}

pub(crate) fn node_id(p: &Project, n: Node) -> &str {
    match n {
        Node::Clip(i) | Node::ClipEnd(i) => &p.clips[i].id,
        Node::Marker(i) => &p.markers[i].id,
    }
}

/// The length an anchor of this node subtracts (`before` starts); 0 for markers and ends.
pub(crate) fn node_duration(p: &Project, n: Node) -> f64 {
    match n {
        Node::Clip(i) => p.clips[i].duration,
        Node::Marker(_) | Node::ClipEnd(_) => 0.0,
    }
}

/// The current resolved time of a node.
pub(crate) fn node_value(p: &Project, r: &Resolved, n: Node) -> Option<f64> {
    let id = node_id(p, n);
    match n {
        Node::Clip(_) => r.span(id).map(|s| s.start),
        Node::Marker(_) => r.marker(id),
        Node::ClipEnd(_) => r.span(id).map(|s| s.end),
    }
}

/// The project frame grid time nearest to `t` for clips on video tracks; `t` itself elsewhere.
pub(crate) fn snap_for(p: &Project, track: &str, t: f64) -> f64 {
    if p.tracks.iter().any(|x| x.id == track && x.kind == TrackKind::Video) {
        crate::resolve::snap_to_frame(t, p.settings.fps)
    } else {
        t
    }
}

/// What is wrong with a clip's matte reference, if anything: a missing clip, itself or a loop of
/// mattes, or a clip that is not a picture.
pub(crate) fn matte_problem(p: &Project, c: &Clip) -> Option<String> {
    let m = c.matte.as_ref()?;
    let id = &c.id;
    let Some(src) = p.clips.iter().find(|x| x.id == m.clip) else {
        return Some(format!("{id}'s matte clip {} does not exist; name a visual clip (or set matte to null).", m.clip));
    };
    if src.id == *id {
        return Some(format!("{id} cannot be its own matte."));
    }
    if p.tracks.iter().any(|t| t.id == src.track && t.kind == TrackKind::Audio) {
        return Some(format!("{id}'s matte {} is on audio track {}; a matte must be a picture (a clip on a video track).", src.id, src.track));
    }
    // Follow the chain of mattes: it must not come back to this clip.
    let mut cur = src;
    for _ in 0..p.clips.len() {
        let next = cur.matte.as_ref()?;
        if next.clip == *id {
            return Some(format!("{id} and {} use each other as mattes (a loop); remove one of the mattes.", cur.id));
        }
        cur = p.clips.iter().find(|x| x.id == next.clip)?;
    }
    None
}

/// One frame of the project on video tracks (`None` on audio tracks).
pub(crate) fn frame_len(p: &Project, track: &str) -> Option<f64> {
    p.tracks.iter().any(|x| x.id == track && x.kind == TrackKind::Video).then(|| {
        let (num, den) = crate::resolve::frame_rate(p.settings.fps);
        den as f64 / num as f64
    })
}

/// A change of `d` seconds on `track` as whole frames on video tracks (so every edge it moves keeps its
/// rounding and moves by exactly that many frames on screen); unchanged on audio tracks.
pub(crate) fn whole_frames(p: &Project, track: &str, d: f64) -> f64 {
    if p.tracks.iter().any(|x| x.id == track && x.kind == TrackKind::Video) {
        let (num, den) = crate::resolve::frame_rate(p.settings.fps);
        (d * num as f64 / den as f64).round() * den as f64 / num as f64
    } else {
        d
    }
}

/// Shifts a time by `d` keeping its anchor (absolute: value; anchored: offset).
pub(crate) fn shift_time(t: &mut Time, d: f64) {
    match t {
        Time::At(v) => *v += d,
        Time::Anchored(a) => a.offset += d,
    }
}

/// The time at which an item with duration `own_dur` starts `delta` seconds after where `base_clip`
/// starts, expressed through `base_clip`'s own start anchor (so magnetic chains survive deletion).
pub(crate) fn time_relative_to_start_of(base_clip: &Clip, delta: f64, own_dur: f64) -> Time {
    match &base_clip.start {
        Time::At(v) => Time::At(v + delta),
        Time::Anchored(a) => {
            let mut off = a.offset + delta;
            if let AnchorTarget::Before(_) = a.to {
                // `before` subtracts the anchored item's own duration.
                off += own_dur - base_clip.duration;
            }
            Time::Anchored(Anchor { to: a.to.clone(), offset: off })
        }
    }
}

/// For a dependant anchored to `deleted`: where it starts relative to `deleted`'s start once the
/// deleted clip's time is removed (ripple).
pub(crate) fn ripple_delta(dep: &Anchor, own_dur: f64) -> f64 {
    match &dep.to {
        AnchorTarget::Before(_) => dep.offset - own_dur,
        _ => dep.offset,
    }
}

// ---------------------------------------------------------------------------------------------
// Clip content
// ---------------------------------------------------------------------------------------------

/// Applies `f` to every animatable parameter of the clip.
pub(crate) fn map_params(c: &mut Clip, mut f: impl FnMut(&Param) -> Param) {
    let mut g = |p: &mut Param| *p = f(p);
    g(&mut c.speed);
    g(&mut c.opacity);
    g(&mut c.volume);
    let t = &mut c.transform;
    g(&mut t.x);
    g(&mut t.y);
    g(&mut t.scale);
    g(&mut t.rotation);
    g(&mut t.rotation_x);
    g(&mut t.rotation_y);
    g(&mut t.crop.left);
    g(&mut t.crop.top);
    g(&mut t.crop.right);
    g(&mut t.crop.bottom);
    if let Source::Shape(s) = &mut c.source {
        g(&mut s.draw);
    }
    for e in &mut c.effects {
        for v in e.params.values_mut() {
            g(v);
        }
    }
}

/// Visits every animatable parameter with its path.
/// One warning line for an item whose anchor changed, written against the final project; `None` when
/// the item no longer exists.
pub(crate) fn reanchor_note(p: &Project, r: &Resolved, id: &str, why: &str) -> Option<String> {
    let (time, at) = match p.clips.iter().find(|c| c.id == id) {
        Some(c) => (&c.start, r.span(id).map(|s| s.start)),
        None => {
            let m = p.markers.iter().find(|m| m.id == id)?;
            (&m.time, r.marker(id))
        }
    };
    Some(match (time, at) {
        (Time::Anchored(a), Some(t)) => format!("{id} {why}; it now starts {} ({} s).", crate::resolve::describe_anchor(a), util::tsecs(t)),
        (Time::Anchored(a), None) => format!("{id} {why}; it is now anchored {}.", crate::resolve::describe_anchor(a)),
        (Time::At(v), _) => format!("{id} {why}; it now starts at {} s.", util::tsecs(*v)),
    })
}

pub(crate) fn for_each_param(c: &Clip, mut f: impl FnMut(String, &Param)) {
    for (path, prm) in super::clip_params(c) {
        f(path, prm);
    }
}

fn shift_highlight(c: &mut Clip, d: f64) {
    if let Source::Text(t) = &mut c.source {
        if let Some(h) = &mut t.highlight {
            for w in &mut h.words {
                w[0] += d;
                w[1] += d;
            }
        }
    }
}

/// Keeps fades within the clip; returns a warning when they had to shrink.
pub(crate) fn clamp_fades(c: &mut Clip) -> Option<String> {
    let total = c.fade_in + c.fade_out;
    if total > c.duration + 1e-9 && total > 0.0 {
        let k = c.duration / total;
        c.fade_in *= k;
        c.fade_out *= k;
        return Some(format!("{}: fades shortened to fit its new {} s length.", c.id, util::num(c.duration)));
    }
    None
}

/// Removes `d` seconds from the start of the clip's content (negative = reveals earlier content). The
/// in-point advances by the source consumed; keys, highlight windows stay locked to the content.
pub(crate) fn trim_front_content(c: &mut Clip, d: f64) {
    if d == 0.0 {
        return;
    }
    if let Source::Media { in_point, .. } = &mut c.source {
        if !c.reverse {
            *in_point += integrate(&c.speed, 0.0, d);
        }
        // Reversed clips start at the far end of their range, so the in-point (the near end) stays.
    }
    c.duration -= d;
    if d > 0.0 {
        map_params(c, |p| split_param(p, d).1);
    } else {
        map_params(c, |p| shift_param(p, -d));
    }
    shift_highlight(c, -d);
}

/// Changes the clip's length from its end. Reversed media clips move their in-point (their end shows
/// the start of the source range). An end anchor (`until`) moves with the end.
pub(crate) fn trim_back_content(c: &mut Clip, new_dur: f64) {
    let old = c.duration;
    if let Some(u) = &mut c.until {
        shift_time(u, new_dur - old);
    }
    if let Source::Media { in_point, .. } = &mut c.source {
        if c.reverse {
            *in_point += integrate(&c.speed, new_dur, old);
        }
    }
    c.duration = new_dur;
    if new_dur < old {
        map_params(c, |p| split_param(p, new_dur).0);
    }
}

/// Multiplies opacity and volume by a gain ramp from `g0` at `t0` to `g1` at `t1` (constant `g0`
/// before, `g1` after).
fn apply_ramp(c: &mut Clip, t0: f64, t1: f64, g0: f64, g1: f64) {
    c.opacity = ramp_param(&c.opacity, t0, t1, g0, g1);
    c.volume = ramp_param(&c.volume, t0, t1, g0, g1);
}

/// `p(t) × g(t)` where g ramps linearly from `g0` (at and before `t0`) to `g1` (at and after `t1`).
/// Exact for constant parameters and outside the ramp; sampled densely inside it for animated ones.
pub(crate) fn ramp_param(p: &Param, t0: f64, t1: f64, g0: f64, g1: f64) -> Param {
    if t1 <= t0 + 1e-12 {
        return p.clone();
    }
    let scale = |k: &Key, g: f64, dt: f64| Key { t: k.t + dt, v: k.v * g, ease: k.ease.clone() };
    match p {
        Param::Value(v) => crate::eval::normalize_keys(vec![Key { t: t0, v: v * g0, ease: Ease::Linear }, Key { t: t1, v: v * g1, ease: Ease::Linear }]),
        Param::Animated { .. } => {
            let (pre, rest) = split_param(p, t0);
            let (_, post) = split_param(&rest, t1 - t0);
            let mut keys: Vec<Key> = Vec::new();
            if let Param::Animated { keys: k } = &pre {
                keys.extend(k.iter().filter(|k| k.t < t0 - 1e-9).map(|k| scale(k, g0, 0.0)));
            }
            let mut ts: Vec<f64> = (0..16).map(|i| t0 + (t1 - t0) * i as f64 / 16.0).collect();
            if let Param::Animated { keys: k } = p {
                ts.extend(k.iter().map(|k| k.t).filter(|t| *t > t0 && *t < t1));
            }
            ts.sort_by(f64::total_cmp);
            ts.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
            for t in ts {
                let g = g0 + (g1 - g0) * (t - t0) / (t1 - t0);
                keys.push(Key { t, v: crate::eval::sample(p, t) * g, ease: Ease::Linear });
            }
            match &post {
                Param::Animated { keys: k } if !k.is_empty() => {
                    if k[0].t > 1e-9 {
                        keys.push(Key { t: t1, v: crate::eval::sample(p, t1) * g1, ease: Ease::Linear });
                    }
                    keys.extend(k.iter().map(|k| scale(k, g1, t1)));
                }
                _ => keys.push(Key { t: t1, v: crate::eval::sample(p, t1) * g1, ease: Ease::Linear }),
            }
            crate::eval::normalize_keys(keys)
        }
    }
}

/// The two halves of a clip cut at clip-local time `a`. The right half gets `right_id` and starts
/// `after` the left half.
pub(crate) fn split_clip_data(orig: &Clip, a: f64, right_id: &str) -> (Clip, Clip, Option<String>) {
    let mut l = orig.clone();
    let mut r = orig.clone();
    r.id = right_id.to_string();
    l.duration = a;
    r.duration = orig.duration - a;
    if let Source::Media { in_point, .. } = &orig.source {
        let fa = integrate(&orig.speed, 0.0, a);
        if orig.reverse {
            let total = consumed(orig);
            if let Source::Media { in_point: lp, .. } = &mut l.source {
                *lp = in_point + total - fa;
            }
        } else if let Source::Media { in_point: rp, .. } = &mut r.source {
            *rp = in_point + fa;
        }
    }
    map_params(&mut l, |p| split_param(p, a).0);
    map_params(&mut r, |p| split_param(p, a).1);
    l.until = None;
    // Fades: a fade wholly inside one half stays a fade; one that the split crosses becomes gain keys on
    // opacity and volume, so the envelope is unchanged on both sides.
    let (d, fi, fo) = (orig.duration, orig.fade_in, orig.fade_out);
    l.fade_in = 0.0;
    l.fade_out = 0.0;
    r.fade_in = 0.0;
    r.fade_out = 0.0;
    if fi > 0.0 {
        if fi <= a + 1e-9 {
            l.fade_in = fi;
        } else {
            apply_ramp(&mut l, 0.0, a, 0.0, a / fi);
            apply_ramp(&mut r, 0.0, fi - a, a / fi, 1.0);
        }
    }
    if fo > 0.0 {
        if d - fo >= a - 1e-9 {
            r.fade_out = fo;
        } else {
            apply_ramp(&mut l, d - fo, a, 1.0, (d - a) / fo);
            apply_ramp(&mut r, 0.0, d - a, (d - a) / fo, 0.0);
        }
    }
    shift_highlight(&mut r, -a);
    let mut warning = None;
    if let Source::Text(t) = &mut r.source {
        if t.reveal.kind != RevealKind::None {
            if a < t.reveal.duration {
                warning = Some(format!("{}: the split falls inside the text reveal; the right half shows the text fully.", orig.id));
            }
            t.reveal = TextReveal::default();
        }
    }
    r.start = Time::Anchored(Anchor { to: AnchorTarget::After(orig.id.clone()), offset: 0.0 });
    (l, r, warning)
}

// ---------------------------------------------------------------------------------------------
// Structural primitives
// ---------------------------------------------------------------------------------------------

impl Cx<'_> {
    /// Splits `id` at timeline time `at`; returns the right half's id. Linked clips split too (their
    /// right halves start `with` the new right half).
    pub(crate) fn split(&mut self, id: &str, at: f64, new_id: Option<&str>, linked: bool) -> Result<Id, EditError> {
        let r = resolve(self.p);
        let span = span_of(&r, id)?;
        if !(at > span.start + MIN_DURATION && at < span.end - MIN_DURATION) {
            return fail(format!(
                "cannot split {id} at {} s: split points must be strictly inside the clip ({}–{} s).",
                util::secs(at),
                util::secs(span.start),
                util::secs(span.end)
            ));
        }
        self.split_local(id, at - span.start, new_id, linked)
    }

    /// Splits `id` at clip-local time `a` (no position lookup, so it is safe while other clips are being
    /// rearranged); returns the right half's id.
    pub(crate) fn split_local(&mut self, id: &str, a: f64, new_id: Option<&str>, linked: bool) -> Result<Id, EditError> {
        let old_start = resolve(self.p).span(id).map(|s| s.start);
        let links = if linked { linked_of(self.p, id) } else { vec![] };
        let right = match new_id {
            Some(n) => {
                self.ids.claim(n)?;
                n.to_string()
            }
            None => self.ids.fresh_piece(id),
        };
        self.made.push(right.clone());
        self.split_one(id, a, &right, None)?;
        for k in links {
            let kr = self.ids.fresh_piece(&k);
            let start = Time::Anchored(Anchor { to: AnchorTarget::With(right.clone()), offset: 0.0 });
            self.split_one(&k, a, &kr, Some(start))?;
        }
        // The left half keeps its place even when its start depends on its own length (`before`).
        if let Some(s) = old_start {
            reposition(self.p, &[(id.to_string(), s)])?;
        }
        Ok(right)
    }

    /// After `id` was split into `id` + `right`: clips using `id` as their matte use the piece that
    /// overlaps them most.
    fn repoint_mattes(&mut self, id: &str, right: &str) {
        if !self.p.clips.iter().any(|c| c.matte.as_ref().is_some_and(|m| m.clip == id)) {
            return;
        }
        let r = resolve(self.p);
        let overlap = |a: &str, b: &str| match (r.span(a), r.span(b)) {
            (Some(x), Some(y)) => (x.end.min(y.end) - x.start.max(y.start)).max(0.0),
            _ => 0.0,
        };
        let users: Vec<(usize, Id)> = self.p.clips.iter().enumerate().filter(|(_, c)| c.matte.as_ref().is_some_and(|m| m.clip == id)).map(|(i, c)| (i, c.id.clone())).collect();
        for (i, u) in users {
            if overlap(&u, right) > overlap(&u, id) + 1e-9 {
                if let Some(m) = &mut self.p.clips[i].matte {
                    m.clip = right.to_string();
                }
            }
        }
    }

    fn split_one(&mut self, id: &str, a: f64, right_id: &str, right_start: Option<Time>) -> Result<(), EditError> {
        let ci = clip_ix(self.p, id)?;
        let orig = self.p.clips[ci].clone();
        let (l, mut r, w) = split_clip_data(&orig, a, right_id);
        if let Some(w) = w {
            self.warn(w);
        }
        if let Some(s) = right_start {
            r.start = s;
        }
        self.p.clips[ci] = l;
        self.p.clips.insert(ci + 1, r);
        self.repoint_mattes(id, right_id);
        // Re-point dependants: `after` → right half; words → the half that plays them.
        let words = match &orig.source {
            Source::Media { asset, .. } => self.p.transcripts.get(asset).map(|t| t.words.clone()),
            _ => None,
        };
        for n in dependants(self.p, id) {
            if node_id(self.p, n) == right_id {
                continue;
            }
            let t = node_time(self.p, n);
            if let Time::Anchored(an) = t {
                match &mut an.to {
                    AnchorTarget::After(x) => *x = right_id.to_string(),
                    AnchorTarget::Word(w) => {
                        let s = words.as_ref().and_then(|ws| ws.get(w.index)).map(|wd| if w.end { wd.end } else { wd.start });
                        if let Some(lt) = s.and_then(|s| local_time_of_source(&orig, s)) {
                            let goes_right = if w.end { lt > a + 1e-9 } else { lt >= a - 1e-9 };
                            if goes_right {
                                w.clip = right_id.to_string();
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        for tr in &mut self.p.transitions {
            if tr.from == id {
                tr.from = right_id.to_string();
            }
        }
        Ok(())
    }

    /// Deletes a clip (and its linked clips). Ripple: dependants re-anchor to the deleted clip's own
    /// start anchor and, if `shift`, later clips on its track move left by its duration. Without ripple,
    /// dependants keep their current times as absolute times.
    pub(crate) fn delete(&mut self, id: &str, ripple: bool, linked: bool, shift: bool) -> Result<(), EditError> {
        let r = resolve(self.p);
        let mut targets: Vec<Id> = if linked { linked_of(self.p, id) } else { vec![] };
        targets.push(id.to_string());
        let target_set: HashSet<Id> = targets.iter().cloned().collect();
        let mut gaps: Vec<(Id, Span)> = Vec::new();
        for t in &targets {
            let ti = clip_ix(self.p, t)?;
            let tclip = self.p.clips[ti].clone();
            if let Some(s) = r.span(t) {
                gaps.push((tclip.track.clone(), s));
            }
            for n in dependants(self.p, t) {
                let nid = node_id(self.p, n).to_string();
                if target_set.contains(&nid) {
                    continue;
                }
                let own = node_duration(self.p, n);
                let current = node_value(self.p, &r, n);
                let time = node_time(self.p, n).clone();
                let Time::Anchored(a) = time else { continue };
                if ripple {
                    if let (AnchorTarget::Word(_), Node::Clip(_)) = (&a.to, n) {
                        self.notes.push((nid.clone(), "was anchored to a word that was cut".into()));
                    }
                    *node_time(self.p, n) = time_relative_to_start_of(&tclip, ripple_delta(&a, own), own);
                } else if let Some(v) = current {
                    *node_time(self.p, n) = Time::At(v);
                }
            }
            let gone: Vec<String> = self
                .p
                .transitions
                .iter()
                .filter(|tr| tr.from == *t || tr.to == *t)
                .map(|tr| {
                    let at = r.span(&tr.to).map(|s| s.start).or_else(|| r.span(&tr.from).map(|s| s.end));
                    format!("{}{}", tr.kind, at.map(|a| format!(" at {} s", util::secs(a))).unwrap_or_default())
                })
                .collect();
            self.p.transitions.retain(|tr| tr.from != *t && tr.to != *t);
            if !gone.is_empty() {
                self.warnings.push(format!("removed {} transition(s) on the cuts of a deleted clip: {}.", gone.len(), gone.join(", ")));
            }
            let ti = clip_ix(self.p, t)?;
            self.p.clips.remove(ti);
        }
        if ripple && shift {
            let mut desired: Vec<(Id, f64)> = Vec::new();
            for (track, gap) in &gaps {
                for c in &self.p.clips {
                    if &c.track != track {
                        continue;
                    }
                    if let Some(s) = r.span(&c.id) {
                        if s.start >= gap.end - EPS {
                            desired.push((c.id.clone(), s.start - gap.duration()));
                        }
                    }
                }
            }
            reposition_rigid(self.p, &desired, &r)?;
        }
        Ok(())
    }

    /// Removes `[from, to)` from the given tracks (except `exclude`). Clips fully inside are deleted,
    /// clips crossing an edge are trimmed, clips spanning the range are split. With `close`, the gap is
    /// closed on those tracks (ripple), else everything else keeps its time.
    pub(crate) fn lift(&mut self, from: f64, to: f64, tracks: &[Id], exclude: &HashSet<Id>, close: bool) -> Result<(), EditError> {
        let r0 = resolve(self.p);
        let tset: HashSet<&Id> = tracks.iter().collect();
        let len = to - from;
        // Explicit final starts (head-trimmed clips and right parts).
        let mut placed: Vec<(Id, f64)> = Vec::new();
        let ids: Vec<(Id, Span)> = self
            .p
            .clips
            .iter()
            .filter(|c| tset.contains(&c.track) && !exclude.contains(&c.id))
            .filter_map(|c| Some((c.id.clone(), r0.span(&c.id)?)))
            .collect();
        let edge_to = if close { from } else { to };
        // Linked clips (detached audio) on lifted tracks are edited together with their video clip.
        let mut links: HashMap<Id, Vec<Id>> = HashMap::new();
        let mut carried: HashSet<Id> = HashSet::new();
        for (id, _) in &ids {
            let l: Vec<Id> = linked_of(self.p, id)
                .into_iter()
                .filter(|k| !exclude.contains(k) && self.p.clips.iter().any(|c| &c.id == k && tset.contains(&c.track)))
                .collect();
            carried.extend(l.iter().cloned());
            links.insert(id.clone(), l);
        }
        let mut ids = ids;
        ids.retain(|(id, _)| !carried.contains(id));
        ids.sort_by(|a, b| b.1.start.total_cmp(&a.1.start));
        for (id, s) in ids {
            if !(s.start < to - EPS && s.end > from + EPS) {
                continue;
            }
            let mine = links.get(&id).cloned().unwrap_or_default();
            let with_links = |id: &Id| std::iter::once(id.clone()).chain(mine.iter().cloned()).collect::<Vec<Id>>();
            if s.start >= from - EPS && s.end <= to + EPS {
                self.delete(&id, close, !mine.is_empty(), false)?;
            } else if s.start < from && s.end > to {
                let right = self.split_local(&id, to - s.start, None, !mine.is_empty())?;
                for x in with_links(&id) {
                    let i = clip_ix(self.p, &x)?;
                    trim_back_content(&mut self.p.clips[i], from - s.start);
                }
                placed.push((right, edge_to));
            } else if s.start < from {
                for x in with_links(&id) {
                    let i = clip_ix(self.p, &x)?;
                    trim_back_content(&mut self.p.clips[i], from - s.start);
                }
            } else {
                for x in with_links(&id) {
                    let i = clip_ix(self.p, &x)?;
                    trim_front_content(&mut self.p.clips[i], to - s.start);
                }
                placed.push((id, edge_to));
            }
        }
        // Linked right halves made by the splits follow their video clip too.
        for l in links.values() {
            carried.extend(l.iter().cloned());
        }
        let followers: Vec<Id> = self
            .p
            .clips
            .iter()
            .filter(|c| matches!(&c.start, Time::Anchored(Anchor { to: AnchorTarget::With(t), offset }) if offset.abs() < EPS && placed.iter().any(|(pid, _)| pid == t)))
            .filter(|c| r0.span(&c.id).is_none())
            .map(|c| c.id.clone())
            .collect();
        carried.extend(followers);
        // Final positions: survivors keep their time (or move left by `len` when closing), except the
        // explicitly placed ones and the linked clips that follow them.
        let placed_ids: HashSet<Id> = placed.iter().map(|(i, _)| i.clone()).collect();
        let mut desired: Vec<(Id, f64)> = Vec::new();
        for c in &self.p.clips {
            if placed_ids.contains(&c.id) || exclude.contains(&c.id) || carried.contains(&c.id) {
                continue;
            }
            let Some(s) = r0.span(&c.id) else { continue };
            if close {
                if !tset.contains(&c.track) {
                    continue; // other tracks follow their anchors
                }
                let t = if s.start >= to - EPS { s.start - len } else { s.start };
                desired.push((c.id.clone(), t));
            } else {
                desired.push((c.id.clone(), s.start));
            }
        }
        desired.extend(placed);
        reposition_rigid(self.p, &desired, &r0)
    }

    /// Lifts the span of a just-placed clip on its own track (overwrite mode).
    pub(crate) fn overwrite_under(&mut self, id: &str) -> Result<(), EditError> {
        let r = resolve(self.p);
        let span = span_of(&r, id)?;
        let track = self.p.clips[clip_ix(self.p, id)?].track.clone();
        let mut ex = HashSet::new();
        ex.insert(id.to_string());
        self.lift(span.start, span.end, &[track], &ex, false)
    }
}

/// Moves the listed clips so they resolve to the given starts, keeping each clip's anchor kind (an
/// absolute start changes value, an anchored one changes its offset). Ancestors are processed first and
/// their shifts are accounted for, so a chain listed with consistent times moves as a unit. Unlisted
/// clips follow their anchors.
pub(crate) fn reposition(p: &mut Project, desired: &[(Id, f64)]) -> Result<(), EditError> {
    if desired.is_empty() {
        return Ok(());
    }
    let r = resolve(p);
    let nc = p.clips.len();
    let nm = p.markers.len();
    let mut index: HashMap<&str, usize> = HashMap::with_capacity(nc + nm);
    for (i, c) in p.clips.iter().enumerate() {
        index.entry(c.id.as_str()).or_insert(i);
    }
    for (j, m) in p.markers.iter().enumerate() {
        index.entry(m.id.as_str()).or_insert(nc + j);
    }
    // Clips with `until` have a separate end node: `after` such a clip follows its end, not its start.
    let mut end_of: Vec<Option<usize>> = vec![None; nc];
    let mut ends: Vec<usize> = Vec::new();
    for (i, c) in p.clips.iter().enumerate() {
        if c.until.is_some() {
            end_of[i] = Some(nc + nm + ends.len());
            ends.push(i);
        }
    }
    let n = nc + nm + ends.len();
    let zero = Time::At(0.0);
    let time_of = |i: usize| {
        if i < nc {
            &p.clips[i].start
        } else if i < nc + nm {
            &p.markers[i - nc].time
        } else {
            p.clips[ends[i - nc - nm]].until.as_ref().unwrap_or(&zero)
        }
    };
    let parent: Vec<Option<usize>> = (0..n)
        .map(|i| match time_of(i) {
            Time::Anchored(a) => index
                .get(target_of(&a.to))
                .copied()
                .map(|j| if matches!(a.to, AnchorTarget::After(_)) && j < nc { end_of[j].unwrap_or(j) } else { j })
                .filter(|&j| j != i),
            Time::At(_) => None,
        })
        .collect();
    let mut want: HashMap<usize, f64> = HashMap::new();
    for (id, t) in desired {
        if let Some(&i) = index.get(id.as_str()) {
            if i < nc {
                want.insert(i, *t);
            }
        }
    }
    drop(index);
    // Depth of each node (distance from its root), iteratively.
    let mut depth = vec![u32::MAX; n];
    let mut path = Vec::new();
    for i in 0..n {
        if depth[i] != u32::MAX {
            continue;
        }
        path.clear();
        let mut cur = Some(i);
        while let Some(c) = cur {
            if depth[c] != u32::MAX || path.len() > n {
                break;
            }
            path.push(c);
            cur = parent[c];
        }
        let mut d = match cur {
            Some(c) if depth[c] != u32::MAX => depth[c] + 1,
            _ => 0,
        };
        for &x in path.iter().rev() {
            if depth[x] == u32::MAX {
                depth[x] = d;
                d += 1;
            }
        }
    }
    let mut list: Vec<(usize, f64)> = want.into_iter().collect();
    list.sort_by_key(|(i, _)| (depth[*i], *i));
    // Total shift applied to each node so far (None = not yet known).
    let mut shift: Vec<Option<f64>> = vec![None; n];
    let mut chain = Vec::new();
    for (i, want_t) in list {
        let id = p.clips[i].id.clone();
        let Some(cur) = r.span(&id).map(|s| s.start) else {
            return fail(format!("cannot move {id}: its anchor is unresolved ({}).", r.error_for(&id).map(|e| e.message.as_str()).unwrap_or("unknown")));
        };
        // Inherited shift: the first known shift up the anchor chain.
        chain.clear();
        let mut cur_node = parent[i];
        let mut inh = 0.0;
        while let Some(c) = cur_node {
            if let Some(s) = shift[c] {
                inh = s;
                break;
            }
            chain.push(c);
            if chain.len() > n {
                break;
            }
            cur_node = parent[c];
        }
        for &c in &chain {
            shift[c] = Some(inh);
        }
        let adj = want_t - (cur + inh);
        if adj.abs() > 1e-12 {
            shift_time(&mut p.clips[i].start, adj);
        }
        shift[i] = Some(inh + adj);
    }
    Ok(())
}

/// [`reposition`] for clips that move as a whole (ripples, slides, rolls, spacing, freezes): every
/// clip that moves (its wanted start differs from where it was in `before`, the state the op started
/// from) and has an end anchor (`until`) keeps its length (its `duration`) — the anchor's offset
/// follows — unless the anchor already moved along with it. Without this an `until` would hold the
/// end in place and the clip would stretch or shrink (opening or closing gaps).
pub(crate) fn reposition_rigid(p: &mut Project, desired: &[(Id, f64)], before: &Resolved) -> Result<(), EditError> {
    let movers: Vec<Id> = desired
        .iter()
        .filter(|(id, want)| p.clips.iter().any(|c| &c.id == id && c.until.is_some()) && before.span(id).is_none_or(|s| (s.start - want).abs() > 1e-9))
        .map(|(id, _)| id.clone())
        .collect();
    reposition(p, desired)?;
    // Shifting one end can move clips chained after it, so place and fix until nothing changes.
    for _ in 0..8 {
        if movers.is_empty() {
            break;
        }
        let r = resolve(p);
        let mut changed = false;
        for id in &movers {
            let Some(i) = p.clips.iter().position(|c| &c.id == id) else { continue };
            let Some(s) = r.span(id) else { continue };
            let d = s.start + p.clips[i].duration - s.end;
            if d.abs() > 1e-9 {
                if let Some(u) = &mut p.clips[i].until {
                    shift_time(u, d);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
        reposition(p, desired)?;
    }
    Ok(())
}

/// Puts every clip not in `exempt` back where it was in `before`.
pub(crate) fn pin_all_except(p: &mut Project, before: &Resolved, exempt: &HashSet<Id>) -> Result<(), EditError> {
    let desired: Vec<(Id, f64)> = p
        .clips
        .iter()
        .filter(|c| !exempt.contains(&c.id))
        .filter_map(|c| Some((c.id.clone(), before.span(&c.id)?.start)))
        .collect();
    reposition(p, &desired)
}

/// The end of the last clip on a track (0 when empty) and that clip's id.
pub(crate) fn track_end(p: &Project, r: &Resolved, track: &str) -> (f64, Option<Id>) {
    let mut best: Option<(&Clip, Span)> = None;
    for c in p.clips.iter().filter(|c| c.track == track) {
        if let Some(s) = r.span(&c.id) {
            if best.is_none_or(|(_, b)| s.end > b.end + EPS || ((s.end - b.end).abs() <= EPS && s.start > b.start)) {
                best = Some((c, s));
            }
        }
    }
    match best {
        Some((c, s)) => (s.end, Some(c.id.clone())),
        None => (0.0, None),
    }
}

// ---------------------------------------------------------------------------------------------
// Clip JSON
// ---------------------------------------------------------------------------------------------

pub(crate) const CLIP_FIELDS: &[&str] = &[
    "id", "track", "name", "start", "duration", "until", "source", "speed", "reverse", "transform", "opacity", "volume", "fade_in", "fade_out", "muted",
    "blend", "effects", "enabled", "tags", "notes", "motion_blur", "frame_blend", "matte",
];

const SOURCE_HELP: &str = "a source: {\"type\":\"media\",\"asset\":\"m1\",\"in_point\":2.0}, {\"type\":\"text\",\"text\":\"Hello\"}, {\"type\":\"solid\",\"color\":\"#000000\"}, {\"type\":\"shape\",\"kind\":\"rect\",\"width\":400,\"height\":200,\"fill\":\"#FFFFFF\"} or {\"type\":\"adjustment\"} (shorthands: \"asset\":\"m1\" or \"text\":\"Hello\")";

pub(crate) fn check_fields(m: &Map<String, Value>) -> Result<(), EditError> {
    for k in m.keys() {
        if !CLIP_FIELDS.contains(&k.as_str()) {
            let hint = util::did_you_mean(k, CLIP_FIELDS.iter().copied());
            return fail(format!("unknown clip field `{k}`.{hint} Clip fields: {}.", CLIP_FIELDS.join(", ")));
        }
    }
    Ok(())
}

impl Cx<'_> {
    /// Builds a clip from partial JSON. `track` overrides the JSON's track; `start` is used when the JSON
    /// has none (`forbid_start` makes a JSON start an error, for ops that place the clip themselves).
    pub(crate) fn build_clip(&mut self, v: &Value, track: Option<&str>, start: Option<Time>, forbid_start: Option<&str>) -> Result<Clip, EditError> {
        let Value::Object(map) = v else {
            return fail(format!("`clip` must be a JSON object with {SOURCE_HELP}, e.g. {{\"track\":\"v1\",\"asset\":\"m1\",\"duration\":3}}."));
        };
        let mut m = map.clone();
        if !m.contains_key("source") {
            if let Some(asset) = m.remove("asset") {
                let mut s = json!({"type": "media", "asset": asset});
                if let Some(ip) = m.remove("in_point") {
                    s["in_point"] = ip;
                }
                m.insert("source".into(), s);
            } else if let Some(Value::String(text)) = m.get("text").cloned() {
                m.remove("text");
                m.insert("source".into(), json!({"type": "text", "text": text}));
            } else {
                return fail(format!("the clip needs {SOURCE_HELP}."));
            }
        }
        check_fields(&m)?;
        if let Some(why) = forbid_start {
            if m.contains_key("start") {
                return fail(format!("remove `start` from the clip: {why}"));
            }
        }
        if let Some(t) = track {
            if let Some(Value::String(jt)) = m.get("track") {
                if jt != t {
                    return fail(format!("the clip says track {jt} but the op says {t}; give the track once."));
                }
            }
            m.insert("track".into(), Value::String(t.to_string()));
        }
        let track_id = match m.get("track") {
            Some(Value::String(t)) => t.clone(),
            _ => return fail("the clip needs a `track` (e.g. \"track\":\"v1\")."),
        };
        track_ix(self.p, &track_id)?;
        match m.get("id") {
            Some(Value::String(id)) => {
                let id = id.clone();
                self.ids.claim(&id)?;
            }
            Some(_) => return fail("clip `id` must be a string like \"intro_title\"."),
            None => {
                let id = self.ids.fresh("c");
                m.insert("id".into(), Value::String(id));
            }
        }
        if let Some(Value::String(id)) = m.get("id") {
            self.made.push(id.clone());
        }
        let source: Source = serde_json::from_value(m["source"].clone()).map_err(|e| EditError::new(format!("invalid clip source: {e}. Use {SOURCE_HELP}.")))?;
        if !m.contains_key("duration") {
            let speed: Param = match m.get("speed") {
                Some(s) => serde_json::from_value(s.clone()).map_err(|e| EditError::new(format!("invalid speed: {e}")))?,
                None => Param::Value(1.0),
            };
            let d = default_duration(self.p, &source, &speed)?;
            m.insert("duration".into(), json!(d));
        }
        if !m.contains_key("start") {
            let s = match start {
                Some(s) => s,
                None => {
                    let r = resolve(self.p);
                    Time::At(track_end(self.p, &r, &track_id).0)
                }
            };
            m.insert("start".into(), serde_json::to_value(s).unwrap_or(json!(0.0)));
        }
        serde_json::from_value::<Clip>(Value::Object(m)).map_err(|e| EditError::new(format!("invalid clip: {e}.")))
    }
}

/// Rest of the media (at constant speed) for media clips; 3 s for stills, text and generators.
pub(crate) fn default_duration(p: &Project, source: &Source, speed: &Param) -> Result<f64, EditError> {
    match source {
        Source::Media { asset, in_point } => {
            let m = &p.media[media_ix(p, asset)?];
            if m.kind == MediaKind::Image {
                return Ok(3.0);
            }
            if m.info.duration <= 0.0 {
                return fail(format!("media {asset} has no known duration (probe it first), so give the clip a `duration`."));
            }
            let s = match speed {
                Param::Value(s) if *s > 0.0 => *s,
                _ => return Ok(3.0),
            };
            let rest = (m.info.duration - in_point) / s;
            if rest < MIN_DURATION {
                return fail(format!(
                    "in_point {} s is at or past the end of {asset} ({} s long); use an in_point below {}.",
                    util::num(*in_point),
                    util::num(m.info.duration),
                    util::num(m.info.duration)
                ));
            }
            Ok(rest)
        }
        _ => Ok(3.0),
    }
}

// ---------------------------------------------------------------------------------------------
// Validation of a single clip (used after every op on created/changed clips)
// ---------------------------------------------------------------------------------------------

fn finite_param(p: &Param) -> bool {
    match p {
        Param::Value(v) => v.is_finite(),
        Param::Animated { keys } => keys.iter().all(|k| k.t.is_finite() && k.v.is_finite()),
    }
}

fn param_finite(p: &Param) -> bool {
    match p {
        Param::Value(v) => v.is_finite(),
        Param::Animated { keys } => {
            keys.iter().all(|k| k.t.is_finite() && k.v.is_finite() && !matches!(&k.ease, Ease::Bezier(b) if !b.iter().all(|x| x.is_finite())))
        }
    }
}

/// The first field of a clip that is not a finite number, if any.
pub(crate) fn clip_non_finite(c: &Clip) -> Option<String> {
    let ok = |t: &Time| match t {
        Time::At(v) => v.is_finite(),
        Time::Anchored(a) => a.offset.is_finite(),
    };
    let start_ok = ok(&c.start) && c.until.as_ref().is_none_or(ok);
    if !start_ok {
        return Some("start".into());
    }
    let shutter = c.motion_blur.as_ref().map_or(0.0, |m| m.shutter);
    for (name, v) in [
        ("duration", c.duration),
        ("fade_in", c.fade_in),
        ("fade_out", c.fade_out),
        ("transform.anchor_x", c.transform.anchor_x),
        ("transform.anchor_y", c.transform.anchor_y),
        ("transform.perspective", c.transform.perspective),
        ("motion_blur.shutter", shutter),
    ] {
        if !v.is_finite() {
            return Some(name.into());
        }
    }
    for (path, p) in super::clip_params(c) {
        if !param_finite(p) {
            return Some(path);
        }
    }
    match &c.source {
        Source::Media { in_point, .. } if !in_point.is_finite() => return Some("in_point".into()),
        Source::Text(t) => {
            let mut vals = vec![("source.size", t.size), ("source.line_height", t.line_height), ("source.letter_spacing", t.letter_spacing), ("source.reveal.duration", t.reveal.duration)];
            if let Some(m) = t.max_width {
                vals.push(("source.max_width", m));
            }
            if let Some(b) = &t.background {
                vals.push(("source.background.padding", b.padding));
                vals.push(("source.background.radius", b.radius));
            }
            if let Some(s) = &t.stroke {
                vals.push(("source.stroke.width", s.width));
            }
            if let Some(s) = &t.shadow {
                vals.push(("source.shadow.blur", s.blur));
                vals.push(("source.shadow.x", s.x));
                vals.push(("source.shadow.y", s.y));
            }
            if let Some((n, _)) = vals.iter().find(|(_, v)| !v.is_finite()) {
                return Some((*n).into());
            }
            if t.highlight.as_ref().is_some_and(|h| h.words.iter().flatten().any(|v| !v.is_finite())) {
                return Some("source.highlight.words".into());
            }
        }
        Source::Shape(s) => {
            let mut vals = vec![("source.width", s.width), ("source.height", s.height), ("source.radius", s.radius)];
            if let Some(st) = &s.stroke {
                vals.push(("source.stroke.width", st.width));
            }
            if let Some((n, _)) = vals.iter().find(|(_, v)| !v.is_finite()) {
                return Some((*n).into());
            }
        }
        _ => {}
    }
    None
}

/// The first number in the project that is not finite (NaN or ±inf), described for a message.
pub(crate) fn first_non_finite(p: &Project) -> Option<String> {
    let s = &p.settings;
    if !(s.fps.is_finite() && s.title_safe.is_finite() && s.action_safe.is_finite() && s.duration.is_none_or(f64::is_finite)) {
        return Some("the project settings".into());
    }
    let e = &p.export;
    if !(e.true_peak_db.is_finite() && e.loudness_lufs.is_none_or(f64::is_finite) && e.range.is_none_or(|r| r.iter().all(|x| x.is_finite()))) {
        return Some("the export settings".into());
    }
    for m in &p.media {
        if !(m.info.duration.is_finite() && m.info.fps.is_finite()) {
            return Some(format!("the info of media {}", m.id));
        }
    }
    for c in &p.clips {
        if let Some(w) = clip_non_finite(c) {
            return Some(format!("{} of clip {}", w, c.id));
        }
    }
    for m in &p.markers {
        let ok = match &m.time {
            Time::At(v) => v.is_finite(),
            Time::Anchored(a) => a.offset.is_finite(),
        };
        if !ok {
            return Some(format!("the time of marker {}", m.id));
        }
    }
    for t in &p.transitions {
        if !(t.duration.is_finite() && t.params.values().all(|v| v.is_finite())) {
            return Some(format!("transition {}", t.id));
        }
    }
    for d in &p.ducking {
        if ![d.amount_db, d.threshold_db, d.attack, d.release].iter().all(|v| v.is_finite()) {
            return Some(format!("ducking {}", d.id));
        }
    }
    for (a, t) in &p.transcripts {
        if !t.words.iter().all(|w| w.start.is_finite() && w.end.is_finite() && w.conf.is_finite()) {
            return Some(format!("the transcript of {a}"));
        }
    }
    None
}

/// Problems that make a clip invalid for an edit (empty = fine). Each message says how to fix it.
/// Each problem is `(category, message)`; the post-check only refuses categories the clip did not
/// already have, so an edit that improves (but does not fully fix) a broken clip is allowed.
pub(crate) fn clip_problems(p: &Project, c: &Clip) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let id = &c.id;
    if let Some(what) = clip_non_finite(c) {
        out.push(("values".to_string(), format!("{id}: {what} is not a finite number.")));
    }
    if let Source::Text(t) = &c.source {
        let bad = !(t.size <= 10_000.0 && t.line_height > 0.0 && t.line_height <= 20.0)
            || t.max_width.is_some_and(|m| !(m > 0.0 && m <= 1e6))
            || t.background.as_ref().is_some_and(|b| !(b.padding >= 0.0 && b.padding <= 1e5 && b.radius >= 0.0 && b.radius <= 1e6))
            || t.stroke.as_ref().is_some_and(|s| !(s.width >= 0.0 && s.width <= 1e4))
            || t.shadow.as_ref().is_some_and(|s| !(s.blur >= 0.0 && s.blur <= 1e4 && s.x.abs() <= 1e5 && s.y.abs() <= 1e5));
        if bad {
            out.push((
                "text_style".to_string(),
                format!("{id}: text style values are out of range (size up to 10000 px, line_height 0–20, positive max_width, padding/radius/stroke/shadow 0 or more)."),
            ));
        }
    }
    if super::valid_id(id).is_err() {
        out.push(("id".to_string(), format!("invalid clip id {id:?}.")));
    }
    if let Some(m) = &c.motion_blur {
        if !(m.shutter >= 0.0 && m.shutter <= 720.0 && (2..=64).contains(&m.samples)) {
            out.push(("motion_blur".to_string(), format!("{id}: motion_blur needs shutter 0–720° and samples 2–64 (got {}° and {}).", util::num(m.shutter), m.samples)));
        }
    }
    if !(c.transform.perspective >= 0.0 && c.transform.perspective <= 100_000.0) {
        out.push(("perspective".to_string(), format!("{id}: transform.perspective must be 0 (orthographic) to 100000 px.")));
    }
    if let Some(problem) = matte_problem(p, c) {
        out.push(("matte".to_string(), problem));
    }
    let track = p.tracks.iter().find(|t| t.id == c.track);
    match track {
        None => out.push(("track".to_string(), track_ix(p, &c.track).err().map(|e| format!("{id}: {}", e.message)).unwrap_or_default())),
        Some(t) => {
            let asset_kind = media_of(p, c).map(|m| m.kind);
            if t.kind == TrackKind::Audio {
                let ok = matches!(c.source, Source::Media { .. }) && asset_kind != Some(MediaKind::Image);
                if !ok {
                    let vids: Vec<&str> = p.tracks.iter().filter(|t| t.kind == TrackKind::Video).map(|t| t.id.as_str()).collect();
                    out.push(("track_kind".to_string(), format!(
                        "{id} is a {} clip and cannot go on audio track {}; put it on a video track ({}).",
                        if asset_kind == Some(MediaKind::Image) { "still image" } else { kind_word(c) },
                        t.id,
                        if vids.is_empty() { "add one with add_track".to_string() } else { vids.join(", ") }
                    )));
                }
            } else if asset_kind == Some(MediaKind::Audio) {
                let auds: Vec<&str> = p.tracks.iter().filter(|t| t.kind == TrackKind::Audio).map(|t| t.id.as_str()).collect();
                out.push(("track_kind".to_string(), format!(
                    "{id} plays audio-only media and cannot go on video track {}; put it on an audio track ({}).",
                    t.id,
                    if auds.is_empty() { "add one with add_track".to_string() } else { auds.join(", ") }
                )));
            }
        }
    }
    if !c.duration.is_finite() || c.duration < MIN_DURATION {
        out.push(("duration".to_string(), format!("{id} would be {} s long; clips must be at least {MIN_DURATION} s.", util::num(c.duration))));
    }
    match &c.start {
        Time::At(v) if !v.is_finite() => out.push(("start".to_string(), format!("{id} has a start that is not a number."))),
        Time::Anchored(a) if !a.offset.is_finite() => out.push(("start".to_string(), format!("{id} has an anchor offset that is not a number."))),
        _ => {}
    }
    match &c.speed {
        Param::Value(s) if !s.is_finite() || *s < 0.0 || *s > MAX_SPEED => {
            out.push(("speed".to_string(), format!("{id} speed {} is out of range; use 0 (freeze) to {MAX_SPEED}.", util::num(*s))))
        }
        Param::Animated { keys } if keys.iter().any(|k| !k.v.is_finite() || k.v < 0.0 || k.v > MAX_SPEED) => {
            out.push(("speed".to_string(), format!("{id} has speed keys outside 0–{MAX_SPEED}.")))
        }
        _ => {}
    }
    if !(c.fade_in.is_finite() && c.fade_in >= 0.0 && c.fade_out.is_finite() && c.fade_out >= 0.0) {
        out.push(("fades".to_string(), format!("{id} fades must be 0 or more seconds.")));
    }
    let mut bad_param = None;
    for_each_param(c, |path, prm| {
        if bad_param.is_none() && !finite_param(prm) {
            bad_param = Some(path.clone());
        }
        if let Param::Animated { keys } = prm {
            if keys.windows(2).any(|w| w[1].t < w[0].t) && bad_param.is_none() {
                bad_param = Some(format!("{path} (keys out of time order)"));
            }
        }
    });
    if let Some(path) = bad_param {
        out.push(("params".to_string(), format!("{id}: {path} has an invalid value.")));
    }
    match &c.source {
        Source::Media { asset, in_point } => match p.media.iter().find(|m| &m.id == asset) {
            None => out.push(("media".to_string(), media_ix(p, asset).err().map(|e| format!("{id}: {}", e.message)).unwrap_or_default())),
            Some(m) => {
                if !in_point.is_finite() {
                    out.push(("media".to_string(), format!("{id} has an in_point that is not a number.")));
                } else if m.kind != MediaKind::Image && m.info.duration > 0.0 {
                    if let Some((lo, hi)) = source_span(c) {
                        let name = util::file_name(&m.path);
                        if lo < -1e-3 {
                            out.push(("media_start".to_string(), format!(
                                "{id} would start {} s before the beginning of {} \"{name}\" (source {} s); trim less or slip it later.",
                                util::num(-lo),
                                m.id,
                                util::num(lo)
                            )));
                        }
                        if hi > m.info.duration + 1e-3 {
                            out.push(("media_end".to_string(), format!(
                                "{id} would play source {}–{} s of {} \"{name}\", which is only {} s long ({} s past the end); trim the clip, slip it earlier or lower its speed.",
                                util::num(lo),
                                util::num(hi),
                                m.id,
                                util::num(m.info.duration),
                                util::num(hi - m.info.duration)
                            )));
                        }
                    }
                }
            }
        },
        Source::Text(t) => {
            if !(t.size.is_finite() && t.size > 0.0) {
                out.push(("text".to_string(), format!("{id} text size must be more than 0 px.")));
            }
        }
        Source::Shape(s) if !(s.width.is_finite() && s.height.is_finite()) => {
            out.push(("shape".to_string(), format!("{id} shape size must be numbers.")));
        }
        _ => {}
    }
    for (i, e) in c.effects.iter().enumerate() {
        match effect_spec(&e.kind) {
            None => out.push((format!("effect{i}"), format!(
                "{id}: effect {i} has unknown kind {:?}.{} Known effects: {}.",
                e.kind,
                util::did_you_mean(&e.kind, EFFECTS.iter().map(|s| s.kind)),
                EFFECTS.iter().map(|s| s.kind).collect::<Vec<_>>().join(", ")
            ))),
            Some(spec) => {
                for (n, v) in &e.params {
                    match spec.param(n) {
                        None => out.push((format!("effect{i}.{n}"), format!(
                            "{id}: effect {i} ({}) has no parameter {n:?}.{} Its parameters: {}.",
                            e.kind,
                            util::did_you_mean(n, spec.params.iter().map(|p| p.name)),
                            spec.params.iter().map(|p| p.name).collect::<Vec<_>>().join(", ")
                        ))),
                        Some(ps) => {
                            let vals: Vec<f64> = match v {
                                Param::Value(x) => vec![*x],
                                Param::Animated { keys } => keys.iter().map(|k| k.v).collect(),
                            };
                            if let Some(bad) = vals.iter().find(|x| !(x.is_finite() && **x >= ps.min - 1e-9 && **x <= ps.max + 1e-9)) {
                                out.push((format!("effect{i}.{n}.range"), format!(
                                    "{id}: {}.{n} = {} is outside its range {}–{}{}.",
                                    e.kind,
                                    util::num(*bad),
                                    util::num(ps.min),
                                    util::num(ps.max),
                                    if ps.unit.is_empty() { String::new() } else { format!(" {}", ps.unit) }
                                )));
                            }
                        }
                    }
                }
                for n in e.colors.keys() {
                    if spec.color_default(n).is_none() {
                        let names: Vec<&str> = spec.colors.iter().map(|(n, _)| *n).collect();
                        out.push((format!("effect{i}.color.{n}"), format!("{id}: effect {} has no colour {n:?} (colours: {}).", e.kind, if names.is_empty() { "none".into() } else { names.join(", ") })));
                    }
                }
                for n in e.strings.keys() {
                    if !spec.strings.contains(&n.as_str()) {
                        out.push((format!("effect{i}.string.{n}"), format!(
                            "{id}: effect {} has no string parameter {n:?} (strings: {}).",
                            e.kind,
                            if spec.strings.is_empty() { "none".into() } else { spec.strings.join(", ") }
                        )));
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Parameter paths
// ---------------------------------------------------------------------------------------------

pub(crate) const PARAM_PATHS: &str = "opacity, volume, speed, transform.x, transform.y, transform.scale, transform.rotation, transform.rotation_x, transform.rotation_y, transform.anchor_x, transform.anchor_y, transform.perspective, transform.crop.left|top|right|bottom, fade_in, fade_out, source.draw (shapes), effects.<index>.<param>";

pub(crate) enum Target<'a> {
    Param(&'a mut Param),
    Scalar(&'a mut f64),
}

/// The parameter at `path` (effect params are created with their registry default when missing).
pub(crate) fn param_at<'a>(c: &'a mut Clip, path: &str) -> Result<Target<'a>, EditError> {
    let id = c.id.clone();
    let unknown = || EditError::new(format!("unknown parameter path {path:?} for {id}.{} Paths: {PARAM_PATHS}.", util::did_you_mean(path, PATH_LIST.iter().copied())));
    if let Some(rest) = path.strip_prefix("effects.") {
        let (idx, name) = rest.split_once('.').ok_or_else(|| EditError::new(format!("effect paths look like effects.0.radius (index then parameter), not {path:?}.")))?;
        let i: usize = idx.parse().map_err(|_| EditError::new(format!("effect index {idx:?} in {path:?} must be a number.")))?;
        let n = c.effects.len();
        let e = c.effects.get_mut(i).ok_or_else(|| {
            EditError::new(format!("{id} has {n} effect(s) (indices 0–{}), so there is no effect {i}.", n.saturating_sub(1)))
        })?;
        let spec = effect_spec(&e.kind).ok_or_else(|| EditError::new(format!("effect {i} of {id} has unknown kind {:?}.", e.kind)))?;
        let ps = spec.param(name).ok_or_else(|| {
            EditError::new(format!(
                "effect {i} of {id} ({}) has no parameter {name:?}.{} Its parameters: {}.",
                e.kind,
                util::did_you_mean(name, spec.params.iter().map(|p| p.name)),
                spec.params.iter().map(|p| p.name).collect::<Vec<_>>().join(", ")
            ))
        })?;
        return Ok(Target::Param(e.params.entry(name.to_string()).or_insert(Param::Value(ps.default))));
    }
    Ok(match path {
        "opacity" => Target::Param(&mut c.opacity),
        "volume" => Target::Param(&mut c.volume),
        "speed" => Target::Param(&mut c.speed),
        "transform.x" => Target::Param(&mut c.transform.x),
        "transform.y" => Target::Param(&mut c.transform.y),
        "transform.scale" => Target::Param(&mut c.transform.scale),
        "transform.rotation" => Target::Param(&mut c.transform.rotation),
        "transform.rotation_x" => Target::Param(&mut c.transform.rotation_x),
        "transform.rotation_y" => Target::Param(&mut c.transform.rotation_y),
        "transform.perspective" => Target::Scalar(&mut c.transform.perspective),
        "transform.crop.left" => Target::Param(&mut c.transform.crop.left),
        "transform.crop.top" => Target::Param(&mut c.transform.crop.top),
        "transform.crop.right" => Target::Param(&mut c.transform.crop.right),
        "transform.crop.bottom" => Target::Param(&mut c.transform.crop.bottom),
        "transform.anchor_x" => Target::Scalar(&mut c.transform.anchor_x),
        "transform.anchor_y" => Target::Scalar(&mut c.transform.anchor_y),
        "fade_in" => Target::Scalar(&mut c.fade_in),
        "fade_out" => Target::Scalar(&mut c.fade_out),
        "source.draw" => {
            let kind = kind_word_src(&c.source);
            match &mut c.source {
                Source::Shape(s) => Target::Param(&mut s.draw),
                _ => return fail(format!("source.draw only exists on shape clips; {id} is a {kind} clip.")),
            }
        }
        _ => return Err(unknown()),
    })
}

const PATH_LIST: &[&str] = &[
    "opacity", "volume", "speed", "transform.x", "transform.y", "transform.scale", "transform.rotation", "transform.rotation_x", "transform.rotation_y", "transform.anchor_x", "transform.anchor_y", "transform.perspective",
    "transform.crop.left", "transform.crop.top", "transform.crop.right", "transform.crop.bottom", "fade_in", "fade_out", "source.draw",
];

fn kind_word_src(s: &Source) -> &'static str {
    match s {
        Source::Media { .. } => "media",
        Source::Text(_) => "text",
        Source::Solid { .. } => "solid",
        Source::Shape(_) => "shape",
        Source::Adjustment => "adjustment",
    }
}

/// Sorts keys by time; later duplicates (same time) win. Errors on non-finite numbers.
pub(crate) fn clean_keys(mut keys: Vec<Key>) -> Result<Vec<Key>, EditError> {
    if keys.is_empty() {
        return fail("`keys` is empty; give at least one {\"t\":0,\"v\":1} or use `value`.");
    }
    if let Some(k) = keys.iter().find(|k| !(k.t.is_finite() && k.v.is_finite())) {
        return fail(format!("key t={} v={} is not a pair of finite numbers.", k.t, k.v));
    }
    keys.sort_by(|a, b| a.t.total_cmp(&b.t));
    let mut out: Vec<Key> = Vec::with_capacity(keys.len());
    for k in keys {
        if let Some(last) = out.last_mut() {
            if (last.t - k.t).abs() < 1e-9 {
                *last = k;
                continue;
            }
        }
        out.push(k);
    }
    Ok(out)
}

/// Clip-local key times must be inside the clip; explains clip-local time when they are not.
pub(crate) fn check_local_time(c: &Clip, start: f64, t: f64) -> Result<(), EditError> {
    if !t.is_finite() || t < -1e-9 || t > c.duration + 1e-9 {
        let hint = if t > c.duration && t >= start && t <= start + c.duration {
            format!(" It looks like a timeline time: use t = {} − {} = {}.", util::num(t), util::num(start), util::num(t - start))
        } else {
            String::new()
        };
        return fail(format!(
            "key time {} is outside {} (clip-local time 0–{} s; 0 is the clip's first frame).{hint}",
            util::num(t),
            c.id,
            util::num(c.duration)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[test]
    fn reposition_moves_chains_as_units() {
        let mut p = project();
        p.clips.push(text_clip("c1", "v1", Time::At(0.0), 2.0));
        p.clips.push(text_clip("c2", "v1", after("c1", 0.0), 2.0));
        p.clips.push(text_clip("c3", "v1", after("c2", 1.0), 2.0));
        p.clips.push(text_clip("t1", "v2", with("c3", 0.5), 1.0));
        // Move c2 and c3 together by +3 (listed with consistent targets), c1 stays.
        reposition(&mut p, &[("c2".into(), 5.0), ("c3".into(), 8.0)]).unwrap();
        let r = resolve(&p);
        assert_eq!(r.span("c1").unwrap().start, 0.0);
        assert_eq!(r.span("c2").unwrap().start, 5.0);
        assert_eq!(r.span("c3").unwrap().start, 8.0);
        assert_eq!(r.span("t1").unwrap().start, 8.5, "unlisted dependants follow");
        // Anchors are preserved, offsets carry the change.
        assert_eq!(p.clips[1].start, after("c1", 3.0));
        assert_eq!(p.clips[2].start, after("c2", 1.0));
    }

    #[test]
    fn param_paths_resolve_and_explain() {
        let mut c = text_clip("c1", "v1", Time::At(0.0), 2.0);
        c.effects.push(effect("blur", &[]));
        assert!(matches!(param_at(&mut c, "transform.scale"), Ok(Target::Param(_))));
        assert!(matches!(param_at(&mut c, "fade_in"), Ok(Target::Scalar(_))));
        assert!(matches!(param_at(&mut c, "effects.0.radius"), Ok(Target::Param(Param::Value(v))) if *v == 10.0));
        let e = param_at(&mut c, "effects.0.radiu").err().unwrap();
        assert!(e.message.contains("radius"), "{}", e.message);
        let e = param_at(&mut c, "effects.3.radius").err().unwrap();
        assert!(e.message.contains("1 effect"), "{}", e.message);
        let e = param_at(&mut c, "opacty").err().unwrap();
        assert!(e.message.contains("Did you mean opacity"), "{}", e.message);
        assert!(param_at(&mut c, "source.draw").is_err());
    }

    #[test]
    fn local_time_hint_for_timeline_times() {
        let c = text_clip("c1", "v1", Time::At(10.0), 4.0);
        let e = check_local_time(&c, 10.0, 12.0).err().unwrap();
        assert!(e.message.contains("t = 12 − 10 = 2"), "{}", e.message);
        assert!(check_local_time(&c, 10.0, 4.0).is_ok());
    }
}
