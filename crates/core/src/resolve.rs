//! Anchors → absolute timeline spans.
//!
//! Every clip start and marker time is either absolute or anchored to another clip, a marker or a
//! transcript word. `resolve` computes all of them in one linear pass (each item depends on at most one
//! other, so the dependency graph is a forest plus possible cycles). Nothing fails silently: unknown
//! targets, cycles, words cut out of their clip and items that depend on any of those are reported in
//! [`Resolved::errors`] and left out of [`Resolved::clips`] / [`Resolved::markers`].

use crate::eval::{local_time_of_source, source_span};
use crate::model::*;
use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Time tolerance used when comparing resolved times (1 µs).
pub const EPS: f64 = 1e-6;

/// A resolved time range in timeline seconds; `end` is exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub start: f64,
    pub end: f64,
}

impl Span {
    pub fn new(start: f64, end: f64) -> Self {
        Span { start, end }
    }
    pub fn duration(&self) -> f64 {
        self.end - self.start
    }
    /// `start <= t < end`.
    pub fn contains(&self, t: f64) -> bool {
        t >= self.start && t < self.end
    }
    /// True when the two spans share more than [`EPS`] of time (touching spans do not overlap).
    pub fn overlaps(&self, o: &Span) -> bool {
        self.start < o.end - EPS && o.start < self.end - EPS
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveErrorKind {
    /// The anchor names an id that does not exist (or is of the wrong kind, e.g. `marker` naming a clip).
    UnknownTarget,
    /// The item is part of an anchor loop.
    Cycle,
    /// A `word` anchor's word is outside the source range its clip plays.
    WordCutOut,
    /// A `word` anchor that cannot work: the clip is not a media clip, has no transcript, or the index is
    /// out of range.
    BadWord,
    /// The item's anchor chain leads to an item that is itself unresolved.
    Blocked,
    /// The time is not a finite number.
    InvalidTime,
    /// The clip's `until` end resolves at or before its start.
    EndBeforeStart,
}

/// Why a clip or marker could not be placed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolveError {
    /// The clip or marker that could not be placed.
    pub id: Id,
    /// True when `id` is a marker.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub marker: bool,
    pub kind: ResolveErrorKind,
    /// The anchor's target (clip/marker id, or the clip of a word anchor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Id>,
    /// For cycles: the ids around the loop, starting and ending with `id`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<Id>,
    /// For blocked items: the unresolved item at the root of the chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<Id>,
    pub message: String,
}

impl ResolveError {
    /// The validation code for this error: `unresolved_anchor`, `anchor_cycle` or `word_cut_out`.
    pub fn code(&self) -> &'static str {
        match self.kind {
            ResolveErrorKind::Cycle => "anchor_cycle",
            ResolveErrorKind::WordCutOut => "word_cut_out",
            ResolveErrorKind::EndBeforeStart => "until_before_start",
            _ => "unresolved_anchor",
        }
    }
}

/// Absolute times for a project.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Resolved {
    pub clips: HashMap<Id, Span>,
    pub markers: HashMap<Id, f64>,
    /// `settings.duration`, or the end of the last clip.
    pub duration: f64,
    /// Never silent: unknown target, cycle, word cut out, blocked by another error, invalid time.
    pub errors: Vec<ResolveError>,
}

impl Resolved {
    pub fn span(&self, id: &str) -> Option<Span> {
        self.clips.get(id).copied()
    }

    pub fn marker(&self, id: &str) -> Option<f64> {
        self.markers.get(id).copied()
    }

    /// The error for a clip or marker, if it could not be placed.
    pub fn error_for(&self, id: &str) -> Option<&ResolveError> {
        self.errors.iter().find(|e| e.id == id)
    }

    /// End of the last resolved clip (0 when there are none), ignoring `settings.duration`.
    pub fn content_end(&self) -> f64 {
        self.clips.values().map(|s| s.end).fold(0.0, f64::max)
    }

    /// Clips whose span contains `t` (`start <= t < end`), bottom track first (the stacking order),
    /// including disabled clips and clips on hidden/muted tracks (callers filter what they need).
    pub fn clips_at<'a>(&self, p: &'a Project, t: f64) -> Vec<&'a Clip> {
        let order: HashMap<&str, usize> = p.tracks.iter().enumerate().map(|(i, tr)| (tr.id.as_str(), i)).collect();
        let mut out: Vec<(&Clip, usize, f64)> = p
            .clips
            .iter()
            .filter_map(|c| {
                let s = self.clips.get(&c.id)?;
                s.contains(t).then(|| (c, order.get(c.track.as_str()).copied().unwrap_or(usize::MAX), s.start))
            })
            .collect();
        out.sort_by(|a, b| a.1.cmp(&b.1).then(a.2.total_cmp(&b.2)));
        out.into_iter().map(|(c, _, _)| c).collect()
    }

    /// The resolved clips of one track, sorted by start.
    pub fn on_track<'a>(&self, p: &'a Project, track: &str) -> Vec<(&'a Clip, Span)> {
        let mut v: Vec<(&Clip, Span)> =
            p.clips.iter().filter(|c| c.track == track).filter_map(|c| Some((c, self.span(&c.id)?))).collect();
        v.sort_by(|a, b| a.1.start.total_cmp(&b.1.start).then(a.1.end.total_cmp(&b.1.end)));
        v
    }
}

/// The id an anchored time depends on (the clip of a word anchor), or `None` for absolute times.
pub fn anchor_target(t: &Time) -> Option<&str> {
    match t {
        Time::At(_) => None,
        Time::Anchored(a) => Some(target_of(&a.to)),
    }
}

pub(crate) fn target_of(t: &AnchorTarget) -> &str {
    match t {
        AnchorTarget::After(id) | AnchorTarget::With(id) | AnchorTarget::Before(id) | AnchorTarget::Marker(id) => id,
        AnchorTarget::Word(w) => &w.clip,
    }
}

/// Human description of an anchor: `after c6 (+0.3)`, `with c2`, `at marker m1`, `on word 17 of c3`.
pub fn describe_anchor(a: &Anchor) -> String {
    let base = match &a.to {
        AnchorTarget::After(id) => format!("after {id}"),
        AnchorTarget::With(id) => format!("with {id}"),
        AnchorTarget::Before(id) => format!("before {id}"),
        AnchorTarget::Marker(id) => format!("at marker {id}"),
        AnchorTarget::Word(w) => {
            format!("on word {}{} of {}", w.index, if w.end { " (end)" } else { "" }, w.clip)
        }
    };
    if a.offset != 0.0 {
        format!("{base} ({})", util::signed(a.offset))
    } else {
        base
    }
}

#[derive(Clone, Copy, PartialEq)]
enum St {
    New,
    Active,
    Ok(f64),
    Err,
}

/// Exact frame rate as a fraction: 30 → 30/1, 29.97 → 30000/1001, 23.976 → 24000/1001,
/// 59.94 → 60000/1001; other rates to a thousandth.
pub fn frame_rate(fps: f64) -> (u64, u64) {
    if !(fps.is_finite() && fps > 0.0) {
        return (30, 1);
    }
    let n = fps.round();
    if (fps - n).abs() < 1e-6 && n >= 1.0 {
        return (n as u64, 1);
    }
    let m = (fps * 1.001).round();
    if m >= 1.0 && (fps - m / 1.001).abs() < 0.0051 {
        return (m as u64 * 1000, 1001);
    }
    (((fps * 1000.0).round() as u64).max(1), 1000)
}

/// The nearest frame boundary to `t` on the project frame grid (see [`frame_rate`]). A time exactly
/// half-way between two frames (common with times in hundredths at 25 or 50 fps) rounds down, also
/// when floating-point noise puts it a hair above the half, so equal distances always round alike.
pub fn snap_to_frame(t: f64, fps: f64) -> f64 {
    if !t.is_finite() {
        return t;
    }
    let (num, den) = frame_rate(fps);
    let f = (t * num as f64 / den as f64 + 0.5 - 1e-7).floor();
    f * den as f64 / num as f64
}

/// Resolves every anchor of the project. Linear in the number of clips and markers (memoised,
/// iterative — long `after` chains cannot overflow the stack). Clips and markers with errors are left
/// out of `clips` / `markers` and explained in `errors`.
///
/// Spans of clips on video tracks are frame-accurate: start and end snap to the project frame grid
/// (exact rational rate, e.g. 30000/1001 for 29.97), so anchored chains never leave sub-frame gaps or
/// overlaps. Audio-track clips stay sample-accurate: their span is the raw span moved by one common
/// shift per group of connected sound (at most half a frame, inherited from the picture the group is
/// anchored to), never rounded per edge, so their length and the adjacency of their pieces are exact.
/// Anchors are computed from unsnapped times ([`resolve_raw`]).
pub fn resolve(p: &Project) -> Resolved {
    resolve_inner(p, true)
}

/// [`resolve`] without frame snapping: the exact times the anchors produce (used by the ops for
/// arithmetic on clip content).
pub fn resolve_raw(p: &Project) -> Resolved {
    resolve_inner(p, false)
}

fn resolve_inner(p: &Project, snap: bool) -> Resolved {
    let nc = p.clips.len();
    let nm = p.markers.len();
    let mut index: HashMap<&str, usize> = HashMap::with_capacity(nc + nm);
    for (i, c) in p.clips.iter().enumerate() {
        index.entry(c.id.as_str()).or_insert(i);
    }
    for (j, m) in p.markers.iter().enumerate() {
        index.entry(m.id.as_str()).or_insert(nc + j);
    }
    let mut end_of = vec![None; nc];
    let mut ends = Vec::new();
    for (i, c) in p.clips.iter().enumerate() {
        if c.until.is_some() {
            end_of[i] = Some(nc + nm + ends.len());
            ends.push(i);
        }
    }
    let n = nc + nm + ends.len();
    let r = Resolver { p, nc, nm, index, end_of, ends };
    let mut st = vec![St::New; n];
    // Root error node for every errored node (itself for root errors).
    let mut root = vec![usize::MAX; n];
    let mut root_kind: Vec<Option<ResolveErrorKind>> = vec![None; n];
    let mut errors: Vec<(usize, ResolveError)> = Vec::new();
    let mut path: Vec<usize> = Vec::new();

    for n0 in 0..n {
        if st[n0] != St::New {
            continue;
        }
        path.clear();
        let mut cur = n0;
        loop {
            match st[cur] {
                St::Ok(_) | St::Err => break,
                St::Active => {
                    // Loop: every node from `cur` to the end of the path is on the cycle.
                    let pos = path.iter().position(|&x| x == cur).unwrap_or(0);
                    let cyc: Vec<usize> = path[pos..].to_vec();
                    for (k, &x) in cyc.iter().enumerate() {
                        let mut chain: Vec<Id> = cyc[k..].iter().chain(cyc[..k].iter()).map(|&y| r.id(y).to_string()).collect();
                        chain.push(r.id(x).to_string());
                        let msg = format!(
                            "{} is in an anchor cycle: {} (each is placed relative to the next). Give one of them an absolute start or anchor it to something outside the loop.",
                            r.what(x),
                            chain.join(" → ")
                        );
                        st[x] = St::Err;
                        root[x] = x;
                        root_kind[x] = Some(ResolveErrorKind::Cycle);
                        let target = r.anchor(x).map(|a| target_of(&a.to).to_string());
                        errors.push((x, r.err(x, ResolveErrorKind::Cycle, target, chain, None, msg)));
                    }
                    break;
                }
                St::New => {
                    st[cur] = St::Active;
                    path.push(cur);
                    match r.dep(cur) {
                        Dep::Abs(v) => {
                            if v.is_finite() {
                                st[cur] = St::Ok(v);
                            } else {
                                st[cur] = St::Err;
                                root[cur] = cur;
                                root_kind[cur] = Some(ResolveErrorKind::InvalidTime);
                                let msg = format!("{} has a time that is not a number ({v}); give it a finite time in seconds.", r.what(cur));
                                errors.push((cur, r.err(cur, ResolveErrorKind::InvalidTime, None, vec![], None, msg)));
                            }
                            break;
                        }
                        Dep::Missing(e) => {
                            st[cur] = St::Err;
                            root[cur] = cur;
                            root_kind[cur] = Some(e.kind);
                            errors.push((cur, e));
                            break;
                        }
                        Dep::Node(t) => cur = t,
                    }
                }
            }
        }
        // Unwind: everything still active depends on an already decided node.
        for &x in path.iter().rev() {
            if st[x] != St::Active {
                continue;
            }
            let a = r.anchor(x).expect("active nodes are anchored");
            let t = match r.dep(x) {
                Dep::Node(t) => t,
                _ => unreachable!("active nodes have a node dependency"),
            };
            match st[t] {
                St::Ok(tv) => match r.compute(x, a, t, tv) {
                    Ok(v) => st[x] = St::Ok(v),
                    Err(e) => {
                        st[x] = St::Err;
                        root[x] = x;
                        root_kind[x] = Some(e.kind);
                        errors.push((x, e));
                    }
                },
                _ => {
                    st[x] = St::Err;
                    let rt = if root[t] == usize::MAX { t } else { root[t] };
                    root[x] = rt;
                    let msg = format!(
                        "{} cannot be placed: it is anchored {} and its chain leads to {}, which is unresolved ({}).",
                        r.what(x),
                        describe_anchor(a),
                        r.id(rt),
                        root_kind[rt].map(short_reason).unwrap_or("unresolved")
                    );
                    errors.push((
                        x,
                        r.err(x, ResolveErrorKind::Blocked, Some(target_of(&a.to).to_string()), vec![], Some(r.id(rt).to_string()), msg),
                    ));
                }
            }
        }
    }

    let mut out = Resolved { clips: HashMap::with_capacity(nc), markers: HashMap::with_capacity(nm), duration: 0.0, errors: Vec::new() };
    for (i, c) in p.clips.iter().enumerate() {
        let St::Ok(start) = st[i] else { continue };
        let end = match r.end_of[i] {
            None => start + c.duration,
            Some(e) => match st[e] {
                St::Ok(v) => {
                    if v - start <= 1e-9 {
                        let msg = format!(
                            "clip {} would end at {} s (its until: {}) but starts at {} s; anchor the end later or the start earlier.",
                            c.id,
                            util::secs(v),
                            match &c.until {
                                Some(Time::Anchored(a)) => describe_anchor(a),
                                _ => "absolute".into(),
                            },
                            util::secs(start)
                        );
                        errors.push((i, r.err(i, ResolveErrorKind::EndBeforeStart, None, vec![], None, msg)));
                        continue;
                    }
                    v
                }
                _ => continue,
            },
        };
        out.clips.entry(c.id.clone()).or_insert(Span { start, end });
    }
    for (j, m) in p.markers.iter().enumerate() {
        if let St::Ok(v) = st[nc + j] {
            out.markers.entry(m.id.clone()).or_insert(v);
        }
    }
    if snap {
        snap_spans(p, &mut out);
    }
    errors.sort_by_key(|(i, _)| *i);
    out.errors = errors.into_iter().map(|(_, e)| e).collect();
    out.duration = match p.settings.duration {
        Some(d) if d.is_finite() && d > 0.0 => d,
        _ => out.content_end(),
    };
    out
}

/// Frame snapping (see [`resolve`]).
///
/// * Video-track clips: each edge to the nearest frame.
/// * Audio-track clips are never rounded per edge. Invariant: **audio span = raw span + one shift per
///   audio group**, so durations never change and pieces stay exactly adjacent. A group is the audio
///   clips connected by anchors between audio clips (start or `until`, incl. word anchors) or lying on
///   one track less than a frame apart (pieces of one cut, the sound of cut pictures, linked or not).
///   Its shift is the one its earliest clip (raw start, then file order) inherits from the picture it is
///   anchored to, directly or through other audio clips: that video clip's start rounding for `with`,
///   `before` and word anchors (detached/linked sound stays on its picture), its end rounding for
///   `after`; 0 for absolute times and markers.
/// * Every shift is at most half a frame, so audio clips of different groups (at least a frame apart)
///   can never be made to overlap or touch by snapping.
fn snap_spans(p: &Project, out: &mut Resolved) {
    let fps = p.settings.fps;
    let (num, den) = frame_rate(fps);
    let frame = den as f64 / num as f64;
    let kind: HashMap<&str, TrackKind> = p.tracks.iter().map(|t| (t.id.as_str(), t.kind)).collect();
    let is_video = |c: &Clip| kind.get(c.track.as_str()).is_some_and(|k| *k == TrackKind::Video);
    // 1. Pictures: each edge to the nearest frame; remember how far each edge moved.
    let mut start_shift: HashMap<&str, f64> = HashMap::new();
    let mut end_shift: HashMap<&str, f64> = HashMap::new();
    for c in p.clips.iter().filter(|c| is_video(c)) {
        if let Some(s) = out.clips.get_mut(&c.id) {
            let (a, b) = (snap_to_frame(s.start, fps), snap_to_frame(s.end, fps));
            start_shift.insert(c.id.as_str(), a - s.start);
            end_shift.insert(c.id.as_str(), b - s.end);
            *s = Span { start: a, end: b.max(a) };
        }
    }
    // 2. Sound: never rounded per edge. Each audio clip moves by its group's single shift.
    let audio: Vec<&Clip> = p.clips.iter().filter(|c| !is_video(c) && out.clips.contains_key(&c.id)).collect();
    if audio.is_empty() {
        return;
    }
    let clips: HashMap<&str, &Clip> = p.clips.iter().map(|c| (c.id.as_str(), c)).collect();
    let pos: HashMap<&str, usize> = audio.iter().enumerate().map(|(i, c)| (c.id.as_str(), i)).collect();
    let mut parent: Vec<usize> = (0..audio.len()).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let union = |parent: &mut Vec<usize>, i: usize, j: usize| {
        let (a, b) = (find(parent, i), find(parent, j));
        if a != b {
            parent[a.max(b)] = a.min(b);
        }
    };
    fn target(t: &Time) -> Option<&str> {
        match t {
            Time::Anchored(a) => match &a.to {
                AnchorTarget::After(id) | AnchorTarget::With(id) | AnchorTarget::Before(id) => Some(id.as_str()),
                AnchorTarget::Word(w) => Some(w.clip.as_str()),
                AnchorTarget::Marker(_) => None,
            },
            Time::At(_) => None,
        }
    }
    // (a) Audio clips anchored to audio clips move together.
    for (i, c) in audio.iter().enumerate() {
        for t in std::iter::once(&c.start).chain(c.until.iter()) {
            if let Some(&j) = target(t).and_then(|id| pos.get(id)) {
                union(&mut parent, i, j);
            }
        }
    }
    // (b) So do clips on one track less than a frame apart (pieces of one cut, a cut's linked sound):
    // a sweep in start order joins every clip that starts within a frame of the extent so far.
    let mut by_track: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, c) in audio.iter().enumerate() {
        by_track.entry(c.track.as_str()).or_default().push(i);
    }
    let raw = |i: usize| out.clips[&audio[i].id];
    for list in by_track.values_mut() {
        list.sort_by(|&x, &y| raw(x).start.total_cmp(&raw(y).start).then(x.cmp(&y)));
        let mut reach: Option<(f64, usize)> = None;
        for &i in list.iter() {
            let s = raw(i);
            if let Some((end, j)) = reach {
                if s.start < end + frame + EPS {
                    union(&mut parent, i, j);
                }
            }
            if reach.is_none_or(|(end, _)| s.end > end) {
                reach = Some((s.end, i));
            }
        }
    }
    // The shift a clip inherits from the picture it is anchored to, directly or through other sound:
    // the picture's start rounding (`with`, `before`, a word it speaks) or its end rounding (`after`).
    let inherited = |c: &Clip| -> Option<f64> {
        let mut cur = c;
        for _ in 0..=p.clips.len() {
            let Time::Anchored(a) = &cur.start else { return None };
            let (id, end) = match &a.to {
                AnchorTarget::With(id) | AnchorTarget::Before(id) => (id.as_str(), false),
                AnchorTarget::After(id) => (id.as_str(), true),
                AnchorTarget::Word(w) => (w.clip.as_str(), false),
                AnchorTarget::Marker(_) => return None,
            };
            let t = clips.get(id)?;
            if is_video(t) {
                return if end { end_shift.get(id).copied() } else { start_shift.get(id).copied() };
            }
            cur = t;
        }
        None
    };
    // Each group's shift comes from its earliest clip (by raw start, then file order).
    let mut head: HashMap<usize, usize> = HashMap::new();
    for i in 0..audio.len() {
        let g = find(&mut parent, i);
        let e = head.entry(g).or_insert(i);
        let (a, b) = (raw(i), raw(*e));
        if a.start < b.start - 1e-12 || ((a.start - b.start).abs() <= 1e-12 && i < *e) {
            *e = i;
        }
    }
    let shift: HashMap<usize, f64> = head.iter().map(|(&g, &h)| (g, inherited(audio[h]).unwrap_or(0.0))).collect();
    for i in 0..audio.len() {
        let d = shift[&find(&mut parent, i)];
        if d != 0.0 {
            if let Some(s) = out.clips.get_mut(&audio[i].id) {
                s.start += d;
                s.end += d;
            }
        }
    }
}

fn short_reason(k: ResolveErrorKind) -> &'static str {
    match k {
        ResolveErrorKind::UnknownTarget => "its anchor target does not exist",
        ResolveErrorKind::Cycle => "it is in an anchor cycle",
        ResolveErrorKind::WordCutOut => "its word is cut out of the clip",
        ResolveErrorKind::BadWord => "its word anchor is invalid",
        ResolveErrorKind::Blocked => "blocked",
        ResolveErrorKind::InvalidTime => "its time is not a number",
        ResolveErrorKind::EndBeforeStart => "it would end before it starts",
    }
}

enum Dep {
    Abs(f64),
    Node(usize),
    Missing(ResolveError),
}

#[derive(Clone, Copy, PartialEq)]
enum NodeKind {
    /// A clip's start.
    Start(usize),
    Marker(usize),
    /// A clip's `until` end.
    End(usize),
}

struct Resolver<'a> {
    p: &'a Project,
    nc: usize,
    nm: usize,
    /// Clip id → its start node; marker id → its node.
    index: HashMap<&'a str, usize>,
    /// Clip index → its end node (clips with `until`).
    end_of: Vec<Option<usize>>,
    /// End node k (node `nc + nm + k`) → clip index.
    ends: Vec<usize>,
}

impl<'a> Resolver<'a> {
    fn kind(&self, i: usize) -> NodeKind {
        if i < self.nc {
            NodeKind::Start(i)
        } else if i < self.nc + self.nm {
            NodeKind::Marker(i - self.nc)
        } else {
            NodeKind::End(self.ends[i - self.nc - self.nm])
        }
    }

    fn id(&self, i: usize) -> &'a str {
        match self.kind(i) {
            NodeKind::Start(c) | NodeKind::End(c) => &self.p.clips[c].id,
            NodeKind::Marker(m) => &self.p.markers[m].id,
        }
    }

    fn what(&self, i: usize) -> String {
        match self.kind(i) {
            NodeKind::Start(_) => format!("clip {}", self.id(i)),
            NodeKind::Marker(_) => format!("marker {}", self.id(i)),
            NodeKind::End(_) => format!("the end (until) of clip {}", self.id(i)),
        }
    }

    fn time(&self, i: usize) -> &'a Time {
        static ZERO: Time = Time::At(0.0);
        match self.kind(i) {
            NodeKind::Start(c) => &self.p.clips[c].start,
            NodeKind::Marker(m) => &self.p.markers[m].time,
            NodeKind::End(c) => self.p.clips[c].until.as_ref().unwrap_or(&ZERO),
        }
    }

    fn anchor(&self, i: usize) -> Option<&'a Anchor> {
        match self.time(i) {
            Time::Anchored(a) => Some(a),
            Time::At(_) => None,
        }
    }

    fn err(&self, i: usize, kind: ResolveErrorKind, target: Option<Id>, chain: Vec<Id>, root: Option<Id>, message: String) -> ResolveError {
        ResolveError { id: self.id(i).to_string(), marker: matches!(self.kind(i), NodeKind::Marker(_)), kind, target, chain, root, message }
    }

    fn dep(&self, i: usize) -> Dep {
        let a = match self.time(i) {
            Time::At(v) => return Dep::Abs(*v),
            Time::Anchored(a) => a,
        };
        let target = target_of(&a.to);
        let Some(&j) = self.index.get(target) else {
            let hint = util::did_you_mean(target, self.p.clips.iter().map(|c| c.id.as_str()).chain(self.p.markers.iter().map(|m| m.id.as_str())));
            let msg = format!("{} is anchored {}, but no clip or marker has the id {target}.{hint}", self.what(i), describe_anchor(a));
            return Dep::Missing(self.err(i, ResolveErrorKind::UnknownTarget, Some(target.to_string()), vec![], None, msg));
        };
        if let (NodeKind::Start(c), AnchorTarget::Before(_)) = (self.kind(i), &a.to) {
            if self.p.clips[c].until.is_some() {
                let msg = format!(
                    "{} starts `before` {target} and also has an `until` end, so its length would depend on itself; anchor its start `with`/`after` something, or remove until.",
                    self.what(i)
                );
                return Dep::Missing(self.err(i, ResolveErrorKind::InvalidTime, Some(target.to_string()), vec![], None, msg));
            }
        }
        match &a.to {
            AnchorTarget::Marker(_) if j < self.nc => {
                let msg = format!(
                    "{} is anchored at marker {target}, but {target} is a clip; use {{\"with\":\"{target}\"}} to start with a clip.",
                    self.what(i)
                );
                Dep::Missing(self.err(i, ResolveErrorKind::UnknownTarget, Some(target.to_string()), vec![], None, msg))
            }
            AnchorTarget::Word(_) if j >= self.nc => {
                let msg = format!("{} is anchored to a word of {target}, but {target} is a marker, not a media clip.", self.what(i));
                Dep::Missing(self.err(i, ResolveErrorKind::BadWord, Some(target.to_string()), vec![], None, msg))
            }
            // `after` a clip that has an `until` end depends on that end.
            AnchorTarget::After(_) if j < self.nc => Dep::Node(self.end_of[j].unwrap_or(j)),
            _ => Dep::Node(j),
        }
    }

    /// Time of node `i` given its anchor `a` and the resolved time `tv` of its target `j`.
    #[allow(clippy::result_large_err)]
    fn compute(&self, i: usize, a: &Anchor, j: usize, tv: f64) -> Result<f64, ResolveError> {
        let base = match &a.to {
            AnchorTarget::After(_) => match self.kind(j) {
                NodeKind::Start(c) => tv + self.p.clips[c].duration,
                _ => tv,
            },
            AnchorTarget::With(_) | AnchorTarget::Marker(_) => tv,
            AnchorTarget::Before(_) => match self.kind(i) {
                NodeKind::Start(c) => tv - self.p.clips[c].duration,
                _ => tv,
            },
            AnchorTarget::Word(w) => match self.kind(j) {
                NodeKind::Start(c) => tv + self.word_local(i, &self.p.clips[c], w)?,
                _ => tv,
            },
        };
        let v = base + a.offset;
        if v.is_finite() {
            Ok(v)
        } else {
            let msg = format!("{} resolves to a time that is not a number (check its offset {}).", self.what(i), a.offset);
            Err(self.err(i, ResolveErrorKind::InvalidTime, Some(target_of(&a.to).to_string()), vec![], None, msg))
        }
    }

    #[allow(clippy::result_large_err)]
    fn word_local(&self, i: usize, clip: &Clip, w: &WordRef) -> Result<f64, ResolveError> {
        let bad = |kind: ResolveErrorKind, msg: String| Err(self.err(i, kind, Some(clip.id.clone()), vec![], None, msg));
        let Source::Media { asset, .. } = &clip.source else {
            return bad(ResolveErrorKind::BadWord, format!("{} is anchored to word {} of {}, but {} is not a media clip, so it has no transcript.", self.what(i), w.index, clip.id, clip.id));
        };
        let Some(tr) = self.p.transcripts.get(asset) else {
            return bad(
                ResolveErrorKind::BadWord,
                format!("{} is anchored to word {} of {}, but its media {asset} has no transcript; transcribe it (set_transcript) first.", self.what(i), w.index, clip.id),
            );
        };
        let Some(word) = tr.words.get(w.index) else {
            return bad(
                ResolveErrorKind::BadWord,
                format!(
                    "{} is anchored to word {} of {}, but the transcript of {asset} has {} words (indices 0–{}).",
                    self.what(i),
                    w.index,
                    clip.id,
                    tr.words.len(),
                    tr.words.len().saturating_sub(1)
                ),
            );
        };
        let s = if w.end { word.end } else { word.start };
        match local_time_of_source(clip, s) {
            Some(l) => Ok(l),
            None => {
                let (a, b) = source_span(clip).unwrap_or((0.0, 0.0));
                bad(
                    ResolveErrorKind::WordCutOut,
                    format!(
                        "{} is anchored to word {} {:?} (source {} s), which is cut out of clip {}: it plays source {}–{} s. Re-anchor it to a word that is still in the clip, or extend/slip {}.",
                        self.what(i),
                        w.index,
                        word.text.trim(),
                        util::secs(s),
                        clip.id,
                        util::secs(a),
                        util::secs(b),
                        clip.id
                    ),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[test]
    fn absolute_and_chains() {
        let mut p = project();
        p.clips.push(text_clip("c1", "v1", Time::At(1.0), 2.0));
        p.clips.push(text_clip("c2", "v1", after("c1", 0.5), 3.0));
        p.clips.push(text_clip("c3", "v1", after("c2", 0.0), 1.0));
        p.clips.push(text_clip("c4", "v2", with("c2", 1.0), 1.0));
        p.clips.push(text_clip("c5", "v2", before("c3", 0.0), 2.0));
        let r = resolve(&p);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.span("c1"), Some(Span::new(1.0, 3.0)));
        assert_eq!(r.span("c2"), Some(Span::new(3.5, 6.5)));
        assert_eq!(r.span("c3"), Some(Span::new(6.5, 7.5)));
        assert_eq!(r.span("c4"), Some(Span::new(4.5, 5.5)));
        assert_eq!(r.span("c5"), Some(Span::new(4.5, 6.5)));
        assert_eq!(r.duration, 7.5);
        p.settings.duration = Some(60.0);
        assert_eq!(resolve(&p).duration, 60.0);
    }

    #[test]
    fn markers_can_be_anchored_and_targeted() {
        let mut p = project();
        p.clips.push(text_clip("c1", "v1", Time::At(2.0), 2.0));
        p.markers.push(marker("mk1", after("c1", 1.0)));
        p.clips.push(text_clip("c2", "v1", Time::Anchored(Anchor { to: AnchorTarget::Marker("mk1".into()), offset: 0.5 }), 1.0));
        p.clips.push(text_clip("c3", "v2", before("mk1", 0.0), 2.0));
        let r = resolve(&p);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.marker("mk1"), Some(5.0));
        assert_eq!(r.span("c2").unwrap().start, 5.5);
        assert_eq!(r.span("c3"), Some(Span::new(3.0, 5.0)), "before a marker = ends at it");
    }

    #[test]
    fn unknown_targets_cycles_and_blocked_are_reported() {
        let mut p = project();
        p.clips.push(text_clip("c1", "v1", after("c9", 0.0), 1.0));
        p.clips.push(text_clip("c2", "v1", after("c1", 0.0), 1.0));
        p.clips.push(text_clip("c3", "v1", after("c4", 0.0), 1.0));
        p.clips.push(text_clip("c4", "v1", after("c3", 0.0), 1.0));
        p.clips.push(text_clip("c5", "v1", after("c4", 0.0), 1.0));
        p.clips.push(text_clip("c6", "v1", after("c6", 0.0), 1.0));
        p.clips.push(text_clip("c7", "v1", Time::Anchored(Anchor { to: AnchorTarget::Marker("c1".into()), offset: 0.0 }), 1.0));
        let r = resolve(&p);
        assert!(r.clips.is_empty(), "nothing resolves: {:?}", r.clips);
        let kind = |id: &str| r.error_for(id).map(|e| e.kind);
        assert_eq!(kind("c1"), Some(ResolveErrorKind::UnknownTarget));
        assert!(r.error_for("c1").unwrap().message.contains("c9"));
        assert_eq!(kind("c2"), Some(ResolveErrorKind::Blocked));
        assert_eq!(r.error_for("c2").unwrap().root.as_deref(), Some("c1"));
        assert_eq!(kind("c3"), Some(ResolveErrorKind::Cycle));
        assert_eq!(kind("c4"), Some(ResolveErrorKind::Cycle));
        assert_eq!(r.error_for("c3").unwrap().chain, vec!["c3", "c4", "c3"]);
        assert_eq!(kind("c5"), Some(ResolveErrorKind::Blocked));
        assert_eq!(kind("c6"), Some(ResolveErrorKind::Cycle));
        assert_eq!(kind("c7"), Some(ResolveErrorKind::UnknownTarget));
        assert_eq!(r.errors.len(), 7);
        assert_eq!(r.error_for("c3").unwrap().code(), "anchor_cycle");
        assert_eq!(r.error_for("c1").unwrap().code(), "unresolved_anchor");
    }

    #[test]
    fn word_anchors_follow_in_point_and_speed() {
        let mut p = project_with_voice(); // m1 has words at 1.0, 2.0, 3.0 ... (word i: i+1 .. i+1.5)
        p.clips.push(media_clip("c1", "a1", "m1", Time::At(10.0), 4.0, 2.0)); // plays source 2..6
        p.clips.push(text_clip("t1", "v1", word("c1", 2, false, 0.0), 1.0)); // word 2 starts at source 3.0
        p.clips.push(text_clip("t2", "v1", word("c1", 2, true, -0.1), 1.0)); // ends at 3.5
        let r = resolve(&p);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!((r.span("t1").unwrap().start - 11.0).abs() < 1e-9);
        assert!((r.span("t2").unwrap().start - 11.4).abs() < 1e-9);

        // Double speed: source 3.0 is 0.5 s into the clip.
        p.clips[0].speed = Param::Value(2.0);
        let r = resolve(&p);
        assert!((r.span("t1").unwrap().start - 10.5).abs() < 1e-9);

        // Reverse: source runs 2+8=10 → 2 ... word 2 (3.0) is at local (10-3)/2 = 3.5.
        p.clips[0].reverse = true;
        let r = resolve(&p);
        assert!((r.span("t1").unwrap().start - 13.5).abs() < 1e-9);

        // Word cut out: in_point past it.
        p.clips[0].reverse = false;
        p.clips[0].speed = Param::Value(1.0);
        p.clips[0].source = Source::Media { asset: "m1".into(), in_point: 3.2 };
        let r = resolve(&p);
        assert_eq!(r.error_for("t1").unwrap().kind, ResolveErrorKind::WordCutOut);
        assert_eq!(r.error_for("t1").unwrap().code(), "word_cut_out");
        assert!(r.span("t2").is_some(), "word end (3.5) is still inside");

        // Bad indices and non-media clips.
        p.clips.push(text_clip("t3", "v1", word("c1", 999, false, 0.0), 1.0));
        p.clips.push(text_clip("t4", "v1", word("t2", 0, false, 0.0), 1.0));
        let r = resolve(&p);
        assert_eq!(r.error_for("t3").unwrap().kind, ResolveErrorKind::BadWord);
        assert_eq!(r.error_for("t4").unwrap().kind, ResolveErrorKind::BadWord);
    }

    #[test]
    fn long_chain_is_fast_and_does_not_overflow() {
        let mut p = project();
        let n = 10_000;
        p.clips.push(text_clip("c0", "v1", Time::At(0.0), 1.0));
        // Insert in reverse so the resolver walks the whole chain from the first node it visits.
        for i in (1..n).rev() {
            p.clips.push(text_clip(&format!("c{i}"), "v1", after(&format!("c{}", i - 1), 0.0), 1.0));
        }
        let t = std::time::Instant::now();
        let r = resolve(&p);
        assert!(t.elapsed().as_millis() < 2000, "resolve took {:?}", t.elapsed());
        assert!(r.errors.is_empty());
        assert_eq!(r.span(&format!("c{}", n - 1)).unwrap().start, (n - 1) as f64);
        assert_eq!(r.duration, n as f64);
    }

    #[test]
    fn clips_at_is_bottom_first() {
        let mut p = project();
        p.clips.push(text_clip("top", "v2", Time::At(0.0), 5.0));
        p.clips.push(text_clip("bottom", "v1", Time::At(1.0), 5.0));
        let r = resolve(&p);
        let ids: Vec<&str> = r.clips_at(&p, 2.0).iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["bottom", "top"]);
        assert!(r.clips_at(&p, 6.0).is_empty());
        assert_eq!(r.clips_at(&p, 0.5).len(), 1);
        assert_eq!(r.on_track(&p, "v1").len(), 1);
    }
}
