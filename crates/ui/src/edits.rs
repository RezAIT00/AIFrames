//! Op builders shared by the widgets. Each returns the ops for one user action; nothing here touches
//! egui, so everything is unit-tested by applying the ops with `aiframes_core::apply_batch`.
//!
//! Builders that touch several clips (split all, trim to playhead, delete) simulate the batch on a
//! copy of the project so later ops never refer to something an earlier op already changed (e.g. a
//! linked audio clip that was split together with its video).

use aiframes_core::{
    apply, effects::transition_spec, CaptionStyle, Edge, Id, MediaKind, Op, Project, Resolved, Source, Time, TrackKind,
};
use serde_json::{json, Value};

use crate::util::{self, asset_of};

/// Fresh ids `prefix<N>` above every existing numeric id with that prefix and the project's
/// high-water mark (`id_counters`, so deleted ids are never reused) — the core's rule — `n` of them.
pub fn fresh_ids(p: &Project, prefix: &str, n: usize) -> Vec<Id> {
    let all: Vec<&str> = p
        .media
        .iter()
        .map(|m| m.id.as_str())
        .chain(p.tracks.iter().map(|t| t.id.as_str()))
        .chain(p.clips.iter().map(|c| c.id.as_str()))
        .chain(p.transitions.iter().map(|t| t.id.as_str()))
        .chain(p.markers.iter().map(|m| m.id.as_str()))
        .chain(p.ducking.iter().map(|d| d.id.as_str()))
        .collect();
    let mut next = all
        .iter()
        .filter_map(|id| id.strip_prefix(prefix))
        .filter(|rest| !rest.is_empty() && rest.len() < 18 && rest.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|rest| rest.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        .max(p.id_counters.get(prefix).copied().unwrap_or(0))
        + 1;
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let id = format!("{prefix}{next}");
        next += 1;
        if !all.contains(&id.as_str()) {
            out.push(id);
        }
    }
    out
}

/// True when the track has nothing in `[t0, t1)`.
pub fn track_free(p: &Project, r: &Resolved, track: &str, t0: f64, t1: f64, ignore: &[Id]) -> bool {
    r.on_track(p, track).iter().all(|(c, s)| ignore.contains(&c.id) || s.end <= t0 + 1e-6 || s.start >= t1 - 1e-6)
}

fn is_overlay_role(role: &str) -> bool {
    !matches!(role, "main")
}

/// `count` unlocked video tracks that are free over `[t, t + dur)` (overlay tracks, topmost first),
/// creating new overlay tracks on top when there are not enough. Returns the track-creating ops and
/// the track ids.
pub fn place_overlays(p: &Project, r: &Resolved, t: f64, dur: f64, count: usize) -> (Vec<Op>, Vec<Id>) {
    let mut tracks: Vec<Id> = p
        .tracks
        .iter()
        .rev()
        .filter(|tr| tr.kind == TrackKind::Video && !tr.locked && is_overlay_role(&tr.role))
        .filter(|tr| track_free(p, r, &tr.id, t, t + dur, &[]))
        .map(|tr| tr.id.clone())
        .take(count)
        .collect();
    let mut ops = Vec::new();
    if tracks.len() < count {
        let ids = fresh_ids(p, "v", count - tracks.len());
        let n_overlay = p.tracks.iter().filter(|t| t.kind == TrackKind::Video && t.role == "overlay").count();
        for (k, id) in ids.into_iter().enumerate() {
            ops.push(Op::AddTrack { id: Some(id.clone()), kind: TrackKind::Video, name: Some(format!("Overlay {}", n_overlay + k + 1)), role: Some("overlay".into()), index: None });
            tracks.push(id);
        }
    }
    (ops, tracks)
}

/// The track a role should use: the first track of that kind with the role, else creates one.
pub fn ensure_track(p: &Project, kind: TrackKind, role: &str, name: &str) -> (Vec<Op>, Id) {
    if let Some(t) = p.tracks.iter().find(|t| t.kind == kind && t.role == role && !t.locked) {
        return (vec![], t.id.clone());
    }
    let id = fresh_ids(p, if kind == TrackKind::Video { "v" } else { "a" }, 1).remove(0);
    (vec![Op::AddTrack { id: Some(id.clone()), kind, name: Some(name.into()), role: Some(role.into()), index: None }], id)
}

/// The main picture track (role "main", else the bottom video track); `None` when there is none.
pub fn main_track(p: &Project) -> Option<Id> {
    p.tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video && t.role == "main")
        .or_else(|| p.tracks.iter().find(|t| t.kind == TrackKind::Video))
        .map(|t| t.id.clone())
}

// ---------------------------------------------------------------------------------------------
// Presets (effects browser)
// ---------------------------------------------------------------------------------------------

/// Text overlays in a plain, editorial style (no gradients, no glow).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextPreset {
    Title,
    LowerThird,
    Caption,
    BigNumber,
    Label,
    Callout,
}

impl TextPreset {
    pub const ALL: [TextPreset; 6] = [TextPreset::Title, TextPreset::LowerThird, TextPreset::Caption, TextPreset::BigNumber, TextPreset::Label, TextPreset::Callout];
    pub fn label(self) -> &'static str {
        match self {
            TextPreset::Title => "Title",
            TextPreset::LowerThird => "Lower third",
            TextPreset::Caption => "Caption",
            TextPreset::BigNumber => "Big number",
            TextPreset::Label => "Label",
            TextPreset::Callout => "Arrow callout",
        }
    }
    pub fn help(self) -> &'static str {
        match self {
            TextPreset::Title => "Centred bold title, white, quick fade in.",
            TextPreset::LowerThird => "Name/role bar in the lower left on a dark box.",
            TextPreset::Caption => "One line of white text with a thin outline near the bottom.",
            TextPreset::BigNumber => "A huge number or stat in the middle of the frame.",
            TextPreset::Label => "Small tag on a dark pill (\"4× speed\", \"Round 40\").",
            TextPreset::Callout => "A short label with an arrow pointing at something (two clips).",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShapePreset {
    Box,
    Circle,
    Arrow,
    Line,
    Highlight,
}

impl ShapePreset {
    pub const ALL: [ShapePreset; 5] = [ShapePreset::Box, ShapePreset::Circle, ShapePreset::Arrow, ShapePreset::Line, ShapePreset::Highlight];
    pub fn label(self) -> &'static str {
        match self {
            ShapePreset::Box => "Box outline",
            ShapePreset::Circle => "Circle outline",
            ShapePreset::Arrow => "Arrow",
            ShapePreset::Line => "Line",
            ShapePreset::Highlight => "Highlight box",
        }
    }
    pub fn help(self) -> &'static str {
        match self {
            ShapePreset::Box => "Yellow rounded rectangle outline that draws itself on.",
            ShapePreset::Circle => "Yellow circle outline that draws itself on.",
            ShapePreset::Arrow => "A thick white arrow that draws itself on.",
            ShapePreset::Line => "A plain white line (underline, divider).",
            ShapePreset::Highlight => "A translucent yellow box to highlight an area.",
        }
    }
}

const YELLOW: &str = "#FFD60A";

fn draw_on(secs: f64) -> Value {
    json!({"keys": [{"t": 0.0, "v": 0.0, "ease": "out"}, {"t": secs, "v": 1.0}]})
}

/// The clip JSON (without track/start) of a text preset for a frame of `w`×`h`.
pub fn text_preset_clip(preset: TextPreset, w: f64, h: f64) -> Value {
    let short = w.min(h);
    match preset {
        TextPreset::Title => json!({
            "name": "Title", "duration": 3.0,
            "source": {"type": "text", "text": "Title", "size": (short * 0.1).round(), "weight": 700, "reveal": {"kind": "fade", "duration": 0.3}},
        }),
        TextPreset::LowerThird => json!({
            "name": "Lower third", "duration": 5.0,
            "source": {"type": "text", "text": "Name Surname\nRole or place", "size": (short * 0.045).round(), "weight": 600, "align": "left", "line_height": 1.25,
                       "background": {"color": "#000000B3", "padding": (short * 0.018).round(), "radius": 4.0}, "reveal": {"kind": "slide_up", "duration": 0.35}},
            "transform": {"x": (-w * 0.27).round(), "y": (h * 0.31).round()},
        }),
        TextPreset::Caption => json!({
            "name": "Caption", "duration": 3.0,
            "source": {"type": "text", "text": "Caption text", "size": (short * 0.05).round(), "weight": 700, "max_width": (w * 0.8).round(),
                       "stroke": {"color": "#000000", "width": (short * 0.003).max(2.0).round()}},
            "transform": {"y": (h * 0.36).round()},
        }),
        TextPreset::BigNumber => json!({
            "name": "Big number", "duration": 2.5,
            "source": {"type": "text", "text": "42", "size": (short * 0.25).round(), "weight": 800, "reveal": {"kind": "pop_words", "duration": 0.25}},
        }),
        TextPreset::Label => json!({
            "name": "Label", "duration": 3.0,
            "source": {"type": "text", "text": "Label", "size": (short * 0.035).round(), "weight": 600,
                       "background": {"color": "#000000CC", "padding": (short * 0.012).round(), "radius": (short * 0.02).round()}},
            "transform": {"x": (w * 0.33).round(), "y": (-h * 0.38).round()},
        }),
        TextPreset::Callout => json!({
            "name": "Callout", "duration": 3.0,
            "source": {"type": "text", "text": "Look here", "size": (short * 0.045).round(), "weight": 700, "color": YELLOW},
            "transform": {"x": (w * 0.12).round(), "y": (-h * 0.22).round()},
        }),
    }
}

/// The clip JSON (without track/start) of a shape preset.
pub fn shape_preset_clip(preset: ShapePreset, w: f64, h: f64) -> Value {
    let short = w.min(h);
    let sw = (short * 0.007).max(3.0).round();
    match preset {
        ShapePreset::Box => json!({
            "name": "Box", "duration": 3.0,
            "source": {"type": "shape", "kind": "rect", "width": (w * 0.22).round(), "height": (h * 0.24).round(), "radius": (short * 0.01).round(),
                       "stroke": {"color": YELLOW, "width": sw}, "draw": draw_on(0.35)},
        }),
        ShapePreset::Circle => json!({
            "name": "Circle", "duration": 3.0,
            "source": {"type": "shape", "kind": "ellipse", "width": (short * 0.28).round(), "height": (short * 0.28).round(),
                       "stroke": {"color": YELLOW, "width": sw}, "draw": draw_on(0.35)},
        }),
        ShapePreset::Arrow => json!({
            "name": "Arrow", "duration": 3.0,
            "source": {"type": "shape", "kind": "arrow", "width": (short * 0.22).round(), "height": (short * 0.12).round(),
                       "stroke": {"color": "#FFFFFF", "width": (sw * 1.6).round()}, "draw": draw_on(0.3)},
        }),
        ShapePreset::Line => json!({
            "name": "Line", "duration": 3.0,
            "source": {"type": "shape", "kind": "line", "width": (w * 0.3).round(), "height": 0.0, "stroke": {"color": "#FFFFFF", "width": sw}},
            "transform": {"y": (h * 0.1).round()},
        }),
        ShapePreset::Highlight => json!({
            "name": "Highlight", "duration": 3.0,
            "source": {"type": "shape", "kind": "rect", "width": (w * 0.25).round(), "height": (h * 0.1).round(), "fill": "#FFD60A55", "radius": 4.0},
        }),
    }
}

fn with_place(mut clip: Value, track: &str, t: f64) -> Value {
    clip["track"] = json!(track);
    clip["start"] = json!(t);
    clip
}

fn clip_duration(clip: &Value) -> f64 {
    clip.get("duration").and_then(Value::as_f64).unwrap_or(3.0)
}

/// Adds the given clip JSONs (no track/start) at `t`: on `track` when it is a free unlocked video
/// track (only for a single clip), else on free overlay tracks (created when needed).
pub fn add_overlay_clips(p: &Project, r: &Resolved, clips: Vec<Value>, t: f64, track: Option<&str>) -> Vec<Op> {
    let t = t.max(0.0);
    let dur = clips.iter().map(clip_duration).fold(0.0, f64::max);
    let usable = |id: &str| p.tracks.iter().any(|tr| tr.id == id && tr.kind == TrackKind::Video && !tr.locked) && track_free(p, r, id, t, t + dur, &[]);
    let (mut ops, tracks) = match track {
        Some(id) if clips.len() == 1 && usable(id) => (vec![], vec![id.to_string()]),
        _ => place_overlays(p, r, t, dur, clips.len()),
    };
    for (c, tr) in clips.into_iter().zip(tracks) {
        ops.push(Op::AddClip { clip: with_place(c, &tr, t), overwrite: false });
    }
    ops
}

/// Ops for a text preset at `t` (on `track` if given and free).
pub fn text_preset_ops(p: &Project, r: &Resolved, preset: TextPreset, t: f64, track: Option<&str>) -> Vec<Op> {
    let (w, h) = (p.settings.width as f64, p.settings.height as f64);
    let mut clips = vec![text_preset_clip(preset, w, h)];
    if preset == TextPreset::Callout {
        let mut arrow = shape_preset_clip(ShapePreset::Arrow, w, h);
        arrow["name"] = json!("Callout arrow");
        arrow["transform"] = json!({"x": (w * 0.02).round(), "y": (-h * 0.12).round(), "rotation": 25.0});
        clips.push(arrow);
    }
    add_overlay_clips(p, r, clips, t, track)
}

/// Ops for a shape preset at `t`.
pub fn shape_preset_ops(p: &Project, r: &Resolved, preset: ShapePreset, t: f64, track: Option<&str>) -> Vec<Op> {
    let (w, h) = (p.settings.width as f64, p.settings.height as f64);
    add_overlay_clips(p, r, vec![shape_preset_clip(preset, w, h)], t, track)
}

/// A plain generator clip (text / solid / adjustment) at `t`.
pub fn generator_ops(p: &Project, r: &Resolved, what: &str, t: f64, track: Option<&str>) -> Vec<Op> {
    let clip = match what {
        "solid" => json!({"name": "Solid", "duration": 3.0, "source": {"type": "solid", "color": "#202020"}}),
        "adjustment" => json!({"name": "Adjustment", "duration": 3.0, "source": {"type": "adjustment"}}),
        _ => text_preset_clip(TextPreset::Title, p.settings.width as f64, p.settings.height as f64),
    };
    add_overlay_clips(p, r, vec![clip], t, track)
}

// ---------------------------------------------------------------------------------------------
// Media
// ---------------------------------------------------------------------------------------------

fn media_track_kind(kind: MediaKind) -> TrackKind {
    if kind == MediaKind::Audio {
        TrackKind::Audio
    } else {
        TrackKind::Video
    }
}

/// Length a fresh clip of this asset gets.
pub fn media_clip_len(p: &Project, asset: &str) -> f64 {
    match p.media.iter().find(|m| m.id == asset) {
        Some(m) if m.kind != MediaKind::Image && m.info.duration > 0.0 => m.info.duration,
        _ => 3.0,
    }
}

/// Media dropped at `t`: on `track` (None = on a new track of the right kind). `insert` = insert
/// with ripple (pushes later clips), else overwrite what is under it.
pub fn drop_media_ops(p: &Project, asset: &str, track: Option<&str>, t: f64, insert: bool) -> Vec<Op> {
    let Some(m) = p.media.iter().find(|m| m.id == asset) else { return vec![] };
    let t = t.max(0.0);
    let mut ops = Vec::new();
    let track = match track {
        Some(t) => t.to_string(),
        None => {
            let kind = media_track_kind(m.kind);
            let id = fresh_ids(p, if kind == TrackKind::Video { "v" } else { "a" }, 1).remove(0);
            ops.push(Op::AddTrack { id: Some(id.clone()), kind, name: None, role: None, index: None });
            id
        }
    };
    if insert && !ops.is_empty() {
        // A new track is empty: inserting and adding are the same.
        ops.push(Op::AddClip { clip: json!({"track": track, "asset": asset, "start": t}), overwrite: false });
    } else if insert {
        ops.push(Op::InsertClip { track, at: t, clip: json!({"asset": asset}), ripple: true });
    } else {
        ops.push(Op::AddClip { clip: json!({"track": track, "asset": asset, "start": t}), overwrite: true });
    }
    ops
}

/// "Add at playhead" from the bin: the main picture track (or the first audio track for sound),
/// inserting with ripple when something is already there.
pub fn add_media_at_ops(p: &Project, r: &Resolved, asset: &str, t: f64) -> Vec<Op> {
    let Some(m) = p.media.iter().find(|m| m.id == asset) else { return vec![] };
    let kind = media_track_kind(m.kind);
    let track = if kind == TrackKind::Video {
        main_track(p).filter(|id| p.tracks.iter().any(|t| &t.id == id && !t.locked))
    } else {
        p.tracks.iter().find(|t| t.kind == TrackKind::Audio && !t.locked).map(|t| t.id.clone())
    };
    let Some(track) = track else { return drop_media_ops(p, asset, None, t, false) };
    let len = media_clip_len(p, asset);
    let t = t.max(0.0);
    if track_free(p, r, &track, t, t + len, &[]) {
        vec![Op::AddClip { clip: json!({"track": track, "asset": asset, "start": t}), overwrite: false }]
    } else {
        vec![Op::InsertClip { track, at: t, clip: json!({"asset": asset}), ripple: true }]
    }
}

/// Appends the asset magnetically at the end of the main (or first audio) track.
pub fn append_media_ops(p: &Project, asset: &str) -> Vec<Op> {
    let Some(m) = p.media.iter().find(|m| m.id == asset) else { return vec![] };
    let track = if m.kind == MediaKind::Audio { p.tracks.iter().find(|t| t.kind == TrackKind::Audio && !t.locked).map(|t| t.id.clone()) } else { main_track(p) };
    match track {
        Some(track) => vec![Op::AppendClip { track, after: None, clip: json!({"asset": asset}) }],
        None => drop_media_ops(p, asset, None, 0.0, false),
    }
}

/// Detaches a video clip's sound onto a free audio track (creating one when needed).
pub fn detach_audio_ops(p: &Project, r: &Resolved, clip: &str) -> Vec<Op> {
    let Some(c) = p.clips.iter().find(|c| c.id == clip) else { return vec![] };
    let span = util::span_or_fallback(p, r, c).0;
    let free = p.tracks.iter().find(|t| t.kind == TrackKind::Audio && !t.locked && track_free(p, r, &t.id, span.start, span.end, &[]));
    match free {
        Some(t) => vec![Op::DetachAudio { clip: clip.into(), track: t.id.clone(), new_id: None }],
        None => {
            let id = fresh_ids(p, "a", 1).remove(0);
            vec![
                Op::AddTrack { id: Some(id.clone()), kind: TrackKind::Audio, name: None, role: None, index: None },
                Op::DetachAudio { clip: clip.into(), track: id, new_id: None },
            ]
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Clip edits at a time
// ---------------------------------------------------------------------------------------------

fn strictly_inside(r: &Resolved, id: &str, t: f64) -> bool {
    r.span(id).is_some_and(|s| t > s.start + 1e-3 && t < s.end - 1e-3)
}

fn unlocked(p: &Project, clip: &str) -> bool {
    p.clips.iter().find(|c| c.id == clip).is_some_and(|c| p.tracks.iter().any(|t| t.id == c.track && !t.locked))
}

/// Splits clips at `t`: the given clips, or (None) every clip under `t` on unlocked tracks. Linked
/// clips split together, so they are not split twice.
pub fn split_ops(p: &Project, r: &Resolved, ids: Option<&[Id]>, t: f64) -> Vec<Op> {
    let mut cands: Vec<Id> = match ids {
        Some(ids) => ids.to_vec(),
        None => p.clips.iter().map(|c| c.id.clone()).collect(),
    };
    cands.retain(|id| strictly_inside(r, id, t) && unlocked(p, id));
    let mut work = p.clone();
    let mut ops = Vec::new();
    for id in cands {
        let rw = aiframes_core::resolve(&work);
        if !strictly_inside(&rw, &id, t) {
            continue;
        }
        let op = Op::SplitClip { clip: id, at: t, new_id: None, linked: true };
        if apply(&mut work, &op).is_ok() {
            ops.push(op);
        }
    }
    ops
}

/// Trims the `edge` of clips under `t` to `t` (Q/W/Alt+[/Alt+]). `ids` None = every clip under `t`
/// on unlocked tracks.
pub fn trim_to_ops(p: &Project, r: &Resolved, ids: Option<&[Id]>, t: f64, edge: Edge, ripple: Option<bool>) -> Vec<Op> {
    let mut cands: Vec<Id> = match ids {
        Some(ids) => ids.to_vec(),
        None => p.clips.iter().map(|c| c.id.clone()).collect(),
    };
    cands.retain(|id| strictly_inside(r, id, t) && unlocked(p, id));
    let mut work = p.clone();
    let mut ops = Vec::new();
    for id in cands {
        let rw = aiframes_core::resolve(&work);
        if !strictly_inside(&rw, &id, t) {
            continue;
        }
        let op = Op::TrimClip { clip: id, edge, to: Some(t), delta: None, ripple };
        if apply(&mut work, &op).is_ok() {
            ops.push(op);
        }
    }
    ops
}

/// Deletes clips, transitions and markers among `ids` (clips with `ripple`). Linked clips go with
/// their video clip; ids already removed by an earlier op are skipped.
pub fn delete_ops(p: &Project, ids: &[Id], ripple: bool) -> Vec<Op> {
    let mut work = p.clone();
    let mut ops = Vec::new();
    let mut ordered: Vec<&Id> = ids.iter().filter(|id| p.transitions.iter().any(|t| &t.id == *id)).collect();
    ordered.extend(ids.iter().filter(|id| p.markers.iter().any(|m| &m.id == *id)));
    // Leaders before linked followers, so the follower is deleted with its leader.
    let mut clips: Vec<&Id> = ids.iter().filter(|id| p.clips.iter().any(|c| &c.id == *id)).collect();
    clips.sort_by_key(|id| p.clips.iter().find(|c| &c.id == *id).map(|c| matches!(c.start, Time::Anchored(_))).unwrap_or(false));
    ordered.extend(clips);
    for id in ordered {
        let op = if work.transitions.iter().any(|t| &t.id == id) {
            Op::RemoveTransition { transition: id.clone() }
        } else if work.markers.iter().any(|m| &m.id == id) {
            Op::RemoveMarker { marker: id.clone() }
        } else if work.clips.iter().any(|c| &c.id == id) {
            if !unlocked(&work, id) {
                continue;
            }
            Op::DeleteClip { clip: id.clone(), ripple, linked: true }
        } else {
            continue;
        };
        if apply(&mut work, &op).is_ok() {
            ops.push(op);
        }
    }
    ops
}

/// The clip touching `clip` on its track on the right (`next`) or left.
pub fn neighbour(p: &Project, r: &Resolved, clip: &str, next: bool) -> Option<Id> {
    let c = p.clips.iter().find(|c| c.id == clip)?;
    let s = r.span(clip)?;
    r.on_track(p, &c.track)
        .into_iter()
        .find(|(k, ks)| k.id != clip && if next { (ks.start - s.end).abs() < 1e-3 } else { (ks.end - s.start).abs() < 1e-3 })
        .map(|(k, _)| k.id.clone())
}

/// A transition of `kind` on the cut after `clip` (or before it when it has no right neighbour).
pub fn add_transition_op(p: &Project, r: &Resolved, clip: &str, kind: &str) -> Option<Op> {
    let (from, to) = match neighbour(p, r, clip, true) {
        Some(n) => (clip.to_string(), n),
        None => (neighbour(p, r, clip, false)?, clip.to_string()),
    };
    transition_between_op(p, r, &from, &to, kind)
}

/// A transition of `kind` on the cut `from|to` (changes the kind when there already is one).
pub fn transition_between_op(p: &Project, r: &Resolved, from: &str, to: &str, kind: &str) -> Option<Op> {
    if let Some(t) = p.transitions.iter().find(|t| t.from == from && t.to == to) {
        return Some(Op::SetTransition { transition: t.id.clone(), kind: Some(kind.into()), duration: None, params: Default::default() });
    }
    let spec = transition_spec(kind)?;
    let max = r.span(from)?.duration().min(r.span(to)?.duration());
    let d = spec.default_duration.min(max);
    Some(Op::AddTransition { from: from.into(), to: to.into(), kind: kind.into(), duration: Some(d), id: None, params: Default::default() })
}

// ---------------------------------------------------------------------------------------------
// Round 6: motion blur, frame blending, 3-D cards, track mattes, transition colours
// ---------------------------------------------------------------------------------------------

/// (added, round 6) Motion blur on (`Some`, with its shutter angle and samples) or off (`None`).
pub fn motion_blur_op(clip: &str, mb: Option<&aiframes_core::MotionBlur>) -> Op {
    let v = match mb {
        Some(m) => json!({"shutter": m.shutter, "samples": m.samples}),
        None => Value::Null,
    };
    Op::SetClip { clip: clip.into(), patch: json!({ "motion_blur": v }) }
}

/// (added, round 6) Frame blending (smooth speed-ups) on or off.
pub fn frame_blend_op(clip: &str, on: bool) -> Op {
    Op::SetClip { clip: clip.into(), patch: json!({ "frame_blend": on }) }
}

/// (added, round 6) The track matte of `clip`: drawn through `matte` (`None` = no matte).
pub fn matte_op(clip: &str, matte: Option<(&str, aiframes_core::MatteMode)>) -> Op {
    let v = match matte {
        Some((src, mode)) => json!({"clip": src, "mode": mode}),
        None => Value::Null,
    };
    Op::SetClip { clip: clip.into(), patch: json!({ "matte": v }) }
}

/// (added, round 6) The clips that use `clip` as their matte (it is not drawn itself).
pub fn matte_users(p: &Project, clip: &str) -> Vec<Id> {
    p.clips.iter().filter(|c| c.matte.as_ref().is_some_and(|m| m.clip == clip)).map(|c| c.id.clone()).collect()
}

/// (added, round 6) Clips `clip` can use as its matte: visual clips on video tracks that overlap it in
/// time, not itself, and not ones whose own matte chain leads back to it (a loop).
pub fn matte_candidates(p: &Project, r: &Resolved, clip: &str) -> Vec<Id> {
    let Some(me) = r.span(clip) else { return vec![] };
    let video: std::collections::HashSet<&str> = p.tracks.iter().filter(|t| t.kind == TrackKind::Video).map(|t| t.id.as_str()).collect();
    let loops_back = |id: &str| {
        let mut cur = id.to_string();
        for _ in 0..=p.clips.len() {
            match p.clips.iter().find(|c| c.id == cur).and_then(|c| c.matte.as_ref()) {
                Some(m) if m.clip == clip => return true,
                Some(m) => cur = m.clip.clone(),
                None => return false,
            }
        }
        true
    };
    p.clips
        .iter()
        .filter(|c| c.id != clip && video.contains(c.track.as_str()))
        .filter(|c| !matches!(util::clip_kind(p, c), util::ClipKind::Audio | util::ClipKind::Adjustment | util::ClipKind::Missing))
        .filter(|c| r.span(&c.id).is_some_and(|s| s.overlaps(&me)))
        .filter(|c| !loops_back(&c.id))
        .map(|c| c.id.clone())
        .collect()
}

/// (added, round 6) True when the clip turns in 3-D (a tilt or turn that is not constantly 0).
pub fn is_3d(c: &aiframes_core::Clip) -> bool {
    let on = |p: &aiframes_core::Param| match p {
        aiframes_core::Param::Value(v) => *v != 0.0,
        aiframes_core::Param::Animated { .. } => true,
    };
    on(&c.transform.rotation_x) || on(&c.transform.rotation_y)
}

/// (added, round 6) Sets (`Some`) or resets to the registry default (`None`) a transition colour.
pub fn transition_color_op(transition: &str, name: &str, color: Option<aiframes_core::Color>) -> Op {
    let mut colors = std::collections::BTreeMap::new();
    colors.insert(name.to_string(), color);
    Op::SetTransitionColors { transition: transition.into(), colors }
}

/// (added, round 6) Named choices of a small whole-number parameter, spelled out in its registry help
/// ("0 = left, 1 = right, …", "1 = clockwise, -1 = counter-clockwise"), for a drop-down instead of a
/// number. `None` for continuous parameters (angles, amounts) and when fewer than two are named.
pub fn param_choices(ps: &aiframes_core::ParamSpec) -> Option<Vec<(f64, String)>> {
    let (help, min, max) = (ps.help, ps.min, ps.max);
    if !matches!(ps.unit, "count" | "") || max - min > 10.0 {
        return None;
    }
    let mut out: Vec<(f64, String)> = Vec::new();
    for part in help.split([',', ';', ':']) {
        let part = part.trim().trim_end_matches('.');
        let Some((num, label)) = part.split_once(" = ") else { continue };
        let num = num.trim().rsplit(' ').next().unwrap_or("");
        let (Ok(v), label) = (num.parse::<f64>(), label.trim()) else { continue };
        if v.fract() == 0.0 && v >= min && v <= max && !label.is_empty() && !out.iter().any(|(x, _)| *x == v) {
            out.push((v, label.to_string()));
        }
    }
    (out.len() >= 2).then_some(out)
}

/// Speed preset for a media clip (`keep: source`: faster = shorter).
pub fn speed_op(clip: &str, speed: f64) -> Op {
    Op::SetSpeed { clip: clip.into(), speed, keep: Default::default(), linked: true }
}

/// Caption styles offered by the transcript panel.
pub fn caption_styles() -> Vec<(&'static str, Option<CaptionStyle>)> {
    let white = aiframes_core::Color::WHITE;
    vec![
        ("Karaoke (yellow word)", None),
        ("Plain white", Some(CaptionStyle { highlight: Some(white), background: Some(aiframes_core::Color::TRANSPARENT), stroke: Some(aiframes_core::Stroke { color: aiframes_core::Color::BLACK, width: 3.0 }), ..Default::default() })),
        ("Bold uppercase", Some(CaptionStyle { uppercase: true, weight: Some(900), ..Default::default() })),
        ("Boxed, no highlight", Some(CaptionStyle { highlight: Some(white), ..Default::default() })),
    ]
}

/// `generate_captions` for the clips onto the captions track (created when missing). Round 4: keeps
/// captions a person edited and skips words whose captions a person deleted (`replace: false`, the
/// core's default); see [`caption_ops_with`] to replace everything.
pub fn caption_ops(p: &Project, clips: &[Id], style: Option<CaptionStyle>, max_words: Option<usize>) -> Vec<Op> {
    caption_ops_with(p, clips, style, max_words, false)
}

/// (added, round 4) [`caption_ops`] with an explicit `replace`: `true` regenerates every caption
/// (including edited ones) and recreates deleted ones.
pub fn caption_ops_with(p: &Project, clips: &[Id], style: Option<CaptionStyle>, max_words: Option<usize>, replace: bool) -> Vec<Op> {
    let (mut ops, track) = ensure_track(p, TrackKind::Video, "captions", "Captions");
    ops.push(Op::GenerateCaptions { clips: clips.to_vec(), track, style, max_words, max_chars: None, pause: None, replace });
    ops
}

/// (added, round 4) Generated captions on the captions track: `(all, edited by a person)`.
pub fn caption_counts(p: &Project) -> (usize, usize) {
    let Some(track) = p.tracks.iter().find(|t| t.kind == TrackKind::Video && t.role == "captions" && !t.locked) else { return (0, 0) };
    let caps: Vec<&aiframes_core::Clip> = p.clips.iter().filter(|c| c.track == track.id && c.tags.iter().any(|t| t == "caption")).collect();
    (caps.len(), caps.iter().filter(|c| c.tags.iter().any(|t| t == "edited")).count())
}

/// (added, round 4) A one-line receipt of what caption ops would do (dry run, as the person):
/// "Captions: 3 created · 9 replaced · 1 kept (edited) · 1 skipped (deleted)".
pub fn caption_receipt(p: &Project, ops: &[Op]) -> Result<String, String> {
    let mut q = p.clone();
    let outs = aiframes_core::apply_batch_as(&mut q, ops, "user").map_err(|e| e.to_string())?;
    let n = |k: &str| outs.iter().map(|o| o.details[k].as_array().map(|a| a.len()).unwrap_or(0)).sum::<usize>();
    let mut parts = vec![format!("{} created", n("created"))];
    for (k, what) in [("replaced", "replaced"), ("kept", "kept (edited by a person)"), ("skipped", "skipped (deleted by a person)")] {
        if n(k) > 0 {
            parts.push(format!("{} {what}", n(k)));
        }
    }
    Ok(format!("Captions: {}", parts.join(" · ")))
}

/// (added, round 4) `set_spacing` for a track, or for `clips` when given (at least two): every gap
/// set to `gap` seconds or multiplied by `scale`, then clamped to `min_gap`/`max_gap`.
pub fn spacing_op(track: &str, clips: Vec<Id>, gap: Option<f64>, scale: Option<f64>, min_gap: Option<f64>, max_gap: Option<f64>) -> Op {
    Op::SetSpacing { track: if clips.is_empty() { Some(track.into()) } else { None }, clips, gap, scale, min_gap, max_gap }
}

/// (added, round 4) The gaps (seconds, ≥ 0) between consecutive clips of `track` (or of `clips`), in
/// time order.
pub fn gaps(p: &Project, r: &Resolved, track: &str, clips: &[Id]) -> Vec<f64> {
    let mut spans: Vec<aiframes_core::Span> = p
        .clips
        .iter()
        .filter(|c| if clips.is_empty() { c.track == track } else { clips.contains(&c.id) })
        .filter_map(|c| r.span(&c.id))
        .collect();
    spans.sort_by(|a, b| a.start.total_cmp(&b.start));
    spans.windows(2).map(|w| (w[1].start - w[0].end).max(0.0)).collect()
}

/// Media clips with a transcript, in timeline order: the selected ones, else those on voice/main
/// tracks (else every transcribed clip).
pub fn transcript_clips(p: &Project, r: &Resolved, selection: &[Id]) -> Vec<Id> {
    let has_tr = |id: &str| p.clips.iter().find(|c| c.id == id).and_then(|c| asset_of(p, c)).is_some_and(|a| p.transcripts.contains_key(&a.id));
    let mut sel: Vec<Id> = selection.iter().filter(|id| has_tr(id)).cloned().collect();
    if sel.is_empty() {
        let role_of = |c: &aiframes_core::Clip| p.tracks.iter().find(|t| t.id == c.track).map(|t| t.role.as_str()).unwrap_or("");
        sel = p.clips.iter().filter(|c| has_tr(&c.id) && matches!(role_of(c), "voice" | "main")).map(|c| c.id.clone()).collect();
        if sel.is_empty() {
            sel = p.clips.iter().filter(|c| has_tr(&c.id)).map(|c| c.id.clone()).collect();
        }
    }
    sel.retain(|id| r.span(id).is_some());
    sel.sort_by(|a, b| r.span(a).map(|s| s.start).unwrap_or(0.0).total_cmp(&r.span(b).map(|s| s.start).unwrap_or(0.0)));
    sel
}

/// True when a clip is a media clip.
pub fn is_media(c: &aiframes_core::Clip) -> bool {
    matches!(c.source, Source::Media { .. })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use aiframes_core::*;

    pub fn track(id: &str, kind: TrackKind, role: &str) -> Track {
        Track { id: id.into(), name: String::new(), kind, role: role.into(), muted: false, hidden: false, locked: false, solo: false, volume: 1.0 }
    }

    pub fn asset(id: &str, kind: MediaKind, duration: f64) -> MediaAsset {
        MediaAsset {
            id: id.into(),
            name: String::new(),
            path: format!("media/{id}.mp4"),
            kind,
            info: MediaInfo { duration, width: 1920, height: 1080, fps: 30.0, has_video: kind != MediaKind::Audio, has_audio: kind != MediaKind::Image, ..Default::default() },
            tags: vec![],
            notes: String::new(),
        }
    }

    pub fn clip(id: &str, track: &str, start: Time, duration: f64, source: Source) -> Clip {
        serde_json::from_value(json!({"id": id, "track": track, "start": start, "duration": duration, "source": source})).unwrap()
    }

    pub fn media(id: &str, track: &str, asset: &str, start: f64, duration: f64, in_point: f64) -> Clip {
        clip(id, track, Time::At(start), duration, Source::Media { asset: asset.into(), in_point })
    }

    pub fn text(id: &str, track: &str, start: Time, duration: f64) -> Clip {
        clip(id, track, start, duration, Source::Text(TextSource { text: format!("text {id}"), ..Default::default() }))
    }

    pub fn after(id: &str, offset: f64) -> Time {
        Time::Anchored(Anchor { to: AnchorTarget::After(id.into()), offset })
    }

    /// v1 main, v2 overlay, a1 voice, a2 music; mv video 120 s with audio, ma audio 60 s, mi image.
    /// c1 mv 0–10 on v1, c2 mv 10–15 (after c1) on v1, c3 ma 0–20 on a1.
    pub fn demo() -> Project {
        let mut p = Project { name: "t".into(), ..Default::default() };
        p.tracks = vec![track("v1", TrackKind::Video, "main"), track("v2", TrackKind::Video, "overlay"), track("a1", TrackKind::Audio, "voice"), track("a2", TrackKind::Audio, "music")];
        p.media = vec![asset("mv", MediaKind::Video, 120.0), asset("ma", MediaKind::Audio, 60.0), asset("mi", MediaKind::Image, 0.0)];
        p.clips.push(media("c1", "v1", "mv", 0.0, 10.0, 5.0));
        let mut c2 = media("c2", "v1", "mv", 0.0, 5.0, 40.0);
        c2.start = after("c1", 0.0);
        p.clips.push(c2);
        p.clips.push(media("c3", "a1", "ma", 0.0, 20.0, 0.0));
        p
    }

    pub fn run(p: &Project, ops: &[Op]) -> (Project, Resolved) {
        let mut q = p.clone();
        apply_batch(&mut q, ops).unwrap_or_else(|e| panic!("ops failed: {e}\nops: {}", serde_json::to_string(ops).unwrap()));
        let r = resolve(&q);
        (q, r)
    }

    #[test]
    fn fresh_ids_follow_core_rule() {
        let p = demo();
        assert_eq!(fresh_ids(&p, "v", 2), vec!["v3", "v4"]);
        assert_eq!(fresh_ids(&p, "c", 1), vec!["c4"]);
        assert_eq!(new_id(&p, "v"), "v3");
        // Deleted ids are not reused: the core keeps a high-water mark.
        let (q, _) = run(&p, &[Op::AddTrack { id: None, kind: TrackKind::Video, name: None, role: None, index: None }, Op::RemoveTrack { track: "v3".into(), force: false }]);
        assert_eq!(fresh_ids(&q, "v", 1), vec![new_id(&q, "v")]);
        assert_ne!(fresh_ids(&q, "v", 1), vec!["v3".to_string()]);
    }

    #[test]
    fn overlays_use_free_tracks_then_create() {
        let p = demo();
        let r = resolve(&p);
        let ops = text_preset_ops(&p, &r, TextPreset::Title, 2.0, None);
        assert_eq!(ops.len(), 1, "v2 is free");
        let (q, rq) = run(&p, &ops);
        let t = q.clips.iter().find(|c| c.track == "v2").unwrap();
        assert!(matches!(t.source, Source::Text(_)));
        assert_eq!(rq.span(&t.id).unwrap().start, 2.0);
        // A second title at the same time needs a new track.
        let ops = text_preset_ops(&q, &rq, TextPreset::Title, 2.5, None);
        assert!(matches!(ops[0], Op::AddTrack { .. }));
        let (q2, _) = run(&q, &ops);
        assert_eq!(q2.tracks.iter().filter(|t| t.kind == TrackKind::Video).count(), 3);
        // Callouts make two clips on two tracks.
        let ops = text_preset_ops(&p, &r, TextPreset::Callout, 0.0, None);
        let (q3, _) = run(&p, &ops);
        assert_eq!(q3.clips.len(), p.clips.len() + 2);
        // Every shape preset is a valid clip.
        for s in ShapePreset::ALL {
            run(&p, &shape_preset_ops(&p, &r, s, 1.0, Some("v2")));
        }
        for t in TextPreset::ALL {
            run(&p, &text_preset_ops(&p, &r, t, 1.0, None));
        }
        for g in ["solid", "adjustment", "text"] {
            run(&p, &generator_ops(&p, &r, g, 30.0, Some("v1")));
        }
    }

    #[test]
    fn split_all_skips_linked_followers() {
        let mut p = demo();
        let r = resolve(&p);
        let (q, rq) = run(&p, &detach_audio_ops(&p, &r, "c1"));
        p = q;
        let audio = p.clips.iter().find(|c| c.track != "v1" && c.id != "c3").unwrap().id.clone();
        assert_eq!(rq.span(&audio).unwrap().start, 0.0);
        let r = resolve(&p);
        let ops = split_ops(&p, &r, None, 4.0);
        // c1 (with its linked audio) and c3: two ops, not three.
        assert_eq!(ops.len(), 2, "{ops:?}");
        let (q, rq) = run(&p, &ops);
        assert_eq!(q.clips.len(), p.clips.len() + 3);
        assert_eq!(rq.span("c1").unwrap().end, 4.0);
        // Selected only.
        let ops = split_ops(&p, &r, Some(&["c3".to_string()]), 4.0);
        assert_eq!(ops, vec![Op::SplitClip { clip: "c3".into(), at: 4.0, new_id: None, linked: true }]);
        assert!(split_ops(&p, &r, Some(&["c2".to_string()]), 4.0).is_empty(), "c2 is not under 4 s");
    }

    #[test]
    fn ripple_trim_to_playhead() {
        let p = demo();
        let r = resolve(&p);
        // W on c1 at 6: end trimmed, c2 follows (it is chained after c1).
        let ops = trim_to_ops(&p, &r, Some(&["c1".to_string()]), 6.0, Edge::End, Some(true));
        let (_, rq) = run(&p, &ops);
        assert_eq!(rq.span("c1").unwrap().end, 6.0);
        assert_eq!(rq.span("c2").unwrap().start, 6.0);
        // Q on c1 at 3: the head goes, the clip stays at 0 and is 3 s shorter.
        let ops = trim_to_ops(&p, &r, Some(&["c1".to_string()]), 3.0, Edge::Start, Some(true));
        let (q, rq) = run(&p, &ops);
        assert_eq!(rq.span("c1").unwrap(), Span::new(0.0, 7.0));
        assert!(matches!(q.clips[0].source, Source::Media { in_point, .. } if (in_point - 8.0).abs() < 1e-9));
        // Alt+[ (no ripple): the head moves to 3, leaving a gap.
        let ops = trim_to_ops(&p, &r, Some(&["c1".to_string()]), 3.0, Edge::Start, Some(false));
        let (_, rq) = run(&p, &ops);
        assert_eq!(rq.span("c1").unwrap(), Span::new(3.0, 10.0));
        assert_eq!(rq.span("c2").unwrap().start, 10.0);
    }

    #[test]
    fn delete_handles_links_transitions_and_markers() {
        let p = demo();
        let r = resolve(&p);
        let mut ops = vec![add_transition_op(&p, &r, "c1", "crossfade").unwrap()];
        ops.push(Op::AddMarker { time: Time::At(3.0), name: None, kind: None, id: Some("mk1".into()), color: None, note: None });
        let (q, rq) = run(&p, &ops);
        let tr = q.transitions[0].id.clone();
        assert_eq!(q.transitions[0].from, "c1");
        // Delete c1 (ripple) together with its transition and the marker.
        let ops = delete_ops(&q, &[tr.clone(), "c1".into(), "mk1".into()], true);
        let (q2, rq2) = run(&q, &ops);
        assert!(q2.transitions.is_empty() && q2.markers.is_empty());
        assert_eq!(rq2.span("c2").unwrap().start, 0.0, "ripple closes the gap");
        let _ = rq;
        // Deleting a transition whose clip is deleted first must not fail.
        let ops = delete_ops(&q, &["c1".into(), tr], false);
        run(&q, &ops);
    }

    #[test]
    fn media_drops_and_bin_adds() {
        let p = demo();
        let r = resolve(&p);
        // Overwrite drop on v1 at 12 cuts c2.
        let (q, rq) = run(&p, &drop_media_ops(&p, "mi", Some("v1"), 12.0, false));
        assert_eq!(rq.span("c2").unwrap().end, 12.0);
        assert_eq!(q.clips.len(), 4);
        // Insert drop pushes c2 right by the image's 3 s.
        let (_, rq) = run(&p, &drop_media_ops(&p, "mi", Some("v1"), 10.0, true));
        assert_eq!(rq.span("c2").unwrap().start, 13.0);
        // Below the last track: a new audio track.
        let (q, _) = run(&p, &drop_media_ops(&p, "ma", None, 0.0, false));
        assert_eq!(q.tracks.len(), 5);
        // Bin "Add to timeline at playhead" at 5 s inserts on the main track.
        let ops = add_media_at_ops(&p, &r, "mi", 5.0);
        assert!(matches!(ops[0], Op::InsertClip { .. }));
        let (_, rq) = run(&p, &ops);
        assert_eq!(rq.span("c2").unwrap().start, 13.0);
        // ...and adds when the spot is free.
        let ops = add_media_at_ops(&p, &r, "mi", 40.0);
        assert!(matches!(ops[0], Op::AddClip { .. }));
        run(&p, &ops);
        run(&p, &append_media_ops(&p, "mv"));
    }

    #[test]
    fn transitions_and_captions() {
        let mut p = demo();
        let r = resolve(&p);
        assert!(add_transition_op(&p, &r, "c3", "crossfade").is_none(), "c3 has no neighbour");
        let op = add_transition_op(&p, &r, "c2", "dip_black").unwrap();
        assert!(matches!(&op, Op::AddTransition { from, to, .. } if from == "c1" && to == "c2"));
        let (q, rq) = run(&p, &[op]);
        // Applying again on the same cut changes the kind.
        let op = add_transition_op(&q, &rq, "c1", "whip").unwrap();
        assert!(matches!(op, Op::SetTransition { .. }));
        // Captions: create the captions track.
        p.transcripts.insert("ma".into(), Transcript { language: "en".into(), words: (0..20).map(|i| Word { text: format!(" w{i}"), start: i as f64 * 0.5, end: i as f64 * 0.5 + 0.4, conf: 1.0 }).collect() });
        let r = resolve(&p);
        let clips = transcript_clips(&p, &r, &[]);
        assert_eq!(clips, vec!["c3"]);
        for (_, style) in caption_styles() {
            let (q, _) = run(&p, &caption_ops(&p, &clips, style, Some(4)));
            assert!(q.tracks.iter().any(|t| t.role == "captions"));
            assert!(q.clips.len() > p.clips.len());
        }
        let r = resolve(&p);
        run(&p, &detach_audio_ops(&p, &r, "c2"));
        run(&p, &[speed_op("c1", 4.0)]);
    }

    fn captioned() -> Project {
        let mut p = demo();
        p.transcripts.insert("ma".into(), Transcript { language: "en".into(), words: (0..24).map(|i| Word { text: format!(" w{i}"), start: i as f64 * 0.8, end: i as f64 * 0.8 + 0.4, conf: 1.0 }).collect() });
        let (mut q, _) = run(&p, &caption_ops(&p, &["c3".to_string()], None, Some(3)));
        let caps: Vec<Id> = q.clips.iter().filter(|c| c.tags.iter().any(|t| t == "caption")).map(|c| c.id.clone()).collect();
        assert!(caps.len() >= 4, "{caps:?}");
        // A person recolours one caption and deletes another.
        apply_as(&mut q, &Op::SetClip { clip: caps[1].clone(), patch: json!({"source": {"color": "#FF0000"}}) }, "user").unwrap();
        apply_as(&mut q, &Op::DeleteClip { clip: caps[3].clone(), ripple: false, linked: true }, "user").unwrap();
        q
    }

    #[test]
    fn caption_regeneration_keeps_human_edits_unless_replacing() {
        // Round 4: the panel's default no longer sends replace: true.
        let p = captioned();
        let ops = caption_ops(&p, &["c3".to_string()], None, Some(3));
        assert!(matches!(ops.last(), Some(Op::GenerateCaptions { replace: false, .. })), "{ops:?}");
        let (all, edited) = caption_counts(&p);
        assert_eq!(edited, 1, "the recoloured caption is marked edited");
        assert!(all >= 3);
        let receipt = caption_receipt(&p, &ops).unwrap();
        assert!(receipt.contains("1 kept") && receipt.contains("skipped") && receipt.contains("replaced"), "{receipt}");
        let (q, _) = run(&p, &ops);
        assert!(q.clips.iter().any(|c| matches!(&c.source, Source::Text(t) if t.color.to_hex() == "#FF0000")), "the edit survives");
        // "Replace all" brings everything back.
        let ops = caption_ops_with(&p, &["c3".to_string()], None, Some(3), true);
        let receipt = caption_receipt(&p, &ops).unwrap();
        assert!(!receipt.contains("kept") && !receipt.contains("skipped"), "{receipt}");
        let (q, _) = run(&p, &ops);
        assert!(!q.clips.iter().any(|c| matches!(&c.source, Source::Text(t) if t.color.to_hex() == "#FF0000")));
    }

    #[test]
    fn look_tools_ops_apply_under_the_strict_core() {
        // Round 6: motion blur, frame blend, track matte, 3-D, transition colours.
        let mut p = demo();
        p.clips.push(text("t1", "v2", Time::At(0.0), 8.0));
        p.clips.push(media("late", "v2", "mv", 30.0, 2.0, 0.0));
        let (q, _) = run(&p, &[motion_blur_op("c1", Some(&MotionBlur { shutter: 270.0, samples: 16 })), frame_blend_op("c1", true)]);
        let c1 = q.clips.iter().find(|c| c.id == "c1").unwrap();
        assert_eq!(c1.motion_blur, Some(MotionBlur { shutter: 270.0, samples: 16 }));
        assert!(c1.frame_blend);
        let (q, _) = run(&q, &[motion_blur_op("c1", None), frame_blend_op("c1", false)]);
        let c1 = q.clips.iter().find(|c| c.id == "c1").unwrap();
        assert!(c1.motion_blur.is_none() && !c1.frame_blend);
        // Matte candidates: overlapping visual clips on video tracks, not itself, not audio, not a
        // clip that plays at another time.
        let r = resolve(&p);
        let cands = matte_candidates(&p, &r, "c1");
        assert!(cands.contains(&"t1".to_string()), "{cands:?}");
        assert!(!cands.iter().any(|x| x == "c1" || x == "c3" || x == "late"), "{cands:?}");
        let (q, rq) = run(&p, &[matte_op("c1", Some(("t1", MatteMode::Luma)))]);
        assert_eq!(q.clips.iter().find(|c| c.id == "c1").unwrap().matte, Some(Matte { clip: "t1".into(), mode: MatteMode::Luma }));
        assert_eq!(matte_users(&q, "t1"), vec!["c1".to_string()]);
        // No loops: t1 cannot take c1 (which already uses t1) as its matte.
        assert!(!matte_candidates(&q, &rq, "t1").contains(&"c1".to_string()));
        let (q, _) = run(&q, &[matte_op("c1", Some(("t1", MatteMode::AlphaInverted)))]);
        assert_eq!(q.clips.iter().find(|c| c.id == "c1").unwrap().matte.as_ref().unwrap().mode, MatteMode::AlphaInverted);
        let (q, _) = run(&q, &[matte_op("c1", None)]);
        assert!(q.clips.iter().all(|c| c.matte.is_none()));
        // 3-D: tilt/turn are params (keyframable), perspective a scalar.
        assert!(!is_3d(&p.clips[0]));
        let (q, _) = run(&p, &[
            Op::AddKeyframe { clip: "c1".into(), path: "transform.rotation_y".into(), t: 0.0, v: -90.0, ease: Some(Ease::Out) },
            Op::AddKeyframe { clip: "c1".into(), path: "transform.rotation_y".into(), t: 1.0, v: 0.0, ease: None },
            Op::SetParam { clip: "c1".into(), path: "transform.perspective".into(), value: Some(900.0), keys: None },
        ]);
        let c1 = q.clips.iter().find(|c| c.id == "c1").unwrap();
        assert!(is_3d(c1) && c1.transform.perspective == 900.0);
        // Transition colours: set, then back to the registry default.
        let r = resolve(&p);
        let (q, _) = run(&p, &[add_transition_op(&p, &r, "c1", "film_burn").unwrap()]);
        let tid = q.transitions[0].id.clone();
        let (q, _) = run(&q, &[transition_color_op(&tid, "color", Some(Color::parse("#FF0040").unwrap()))]);
        assert_eq!(transition_color(&q.transitions[0], "color").to_hex(), "#FF0040");
        let (q, _) = run(&q, &[transition_color_op(&tid, "color", None)]);
        assert_eq!(transition_color(&q.transitions[0], "color").to_hex(), "#FF8A2B");
    }

    #[test]
    fn discrete_params_get_named_choices() {
        let named = |kind: &str, name: &str| transition_spec(kind).and_then(|s| s.params.iter().find(|p| p.name == name)).and_then(param_choices);
        let cube = named("cube", "direction").unwrap();
        assert_eq!(cube.iter().map(|(v, l)| (*v, l.as_str())).collect::<Vec<_>>(), vec![(0.0, "left"), (1.0, "right"), (2.0, "up"), (3.0, "down")]);
        let spin = named("spin", "direction").unwrap();
        assert!(spin.iter().any(|(v, l)| *v == -1.0 && l == "counter-clockwise"), "{spin:?}");
        assert_eq!(named("flip", "axis").map(|c| c.len()), Some(2));
        // Continuous or unexplained params stay numbers.
        assert!(named("spin", "turns").is_none() && named("glitch", "slices").is_none() && named("luma_wipe", "invert").is_none());
        let angle = effect_spec("directional_blur").unwrap().params.iter().find(|p| p.name == "angle").unwrap();
        assert!(param_choices(angle).is_none(), "an angle is continuous even if its help names 0 and 90");
    }

    #[test]
    fn spacing_ops_set_and_scale_the_gaps() {
        let mut p = demo();
        p.clips.retain(|c| c.track != "a1");
        p.clips.push(media("a", "a1", "ma", 0.0, 1.0, 0.0));
        p.clips.push(media("b", "a1", "ma", 2.4, 1.0, 1.0));
        p.clips.push(media("c", "a1", "ma", 4.0, 1.0, 2.0));
        let r = resolve(&p);
        let g = gaps(&p, &r, "a1", &[]);
        assert!(g.len() == 2 && (g[0] - 1.4).abs() < 1e-9 && (g[1] - 0.6).abs() < 1e-9, "{g:?}");
        let (q, rq) = run(&p, &[spacing_op("a1", vec![], Some(0.5), None, None, None)]);
        assert!(gaps(&q, &rq, "a1", &[]).iter().all(|g| (g - 0.5).abs() < 1e-9));
        let (q, rq) = run(&p, &[spacing_op("a1", vec!["a".into(), "b".into()], None, Some(0.5), None, None)]);
        assert!((gaps(&q, &rq, "a1", &[])[0] - 0.7).abs() < 1e-9, "only a→b scaled");
        let (q, rq) = run(&p, &[Op::CloseGaps { track: "a1".into(), max_gap: None }]);
        assert!(gaps(&q, &rq, "a1", &[]).iter().all(|g| g.abs() < 1e-9));
    }
}
