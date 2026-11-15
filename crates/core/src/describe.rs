//! The AI's eyes on structure: the whole edit as compact text ([`describe`]) or JSON ([`timeline_json`]).
//!
//! Text format (tracks in the timeline's on-screen order — see [`tracks_in_display_order`] — clips by
//! time):
//! ```text
//! Project "Round 40" — 1920×1080, 30 fps, 48000 Hz, 95.40 s (ends with the last clip), 42 clips
//! Video tracks (top layer first):
//!   v2 video · overlay
//!     [12.40–15.90] c7 "title" text:"VICTORY!" — after c6 (+0.3) — fx: glow — the win moment
//!   v1 video · main
//!     [0.00–12.40] c1 media:gameplay.mp4@31.2s x2 speed — fx: color, blur — hook
//! Audio tracks (as shown, top to bottom):
//!   a1 audio · voice
//!     [0.00–60.00] c3 media:vo.wav@0s
//!   a2 audio · music (empty)
//! ```

use crate::effects::effect_spec;
use crate::eval::source_span;
use crate::model::*;
use crate::resolve::{describe_anchor, Resolved, Span};
use crate::util;
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub struct DescribeOptions {
    /// Only clips overlapping `[from, to]` timeline seconds (markers/transitions likewise).
    pub range: Option<(f64, f64)>,
    /// Cap per track (the rest is summarised as "… N more clips").
    pub max_clips_per_track: Option<usize>,
    /// Include the media list.
    pub media: bool,
    /// Include project and clip notes.
    pub notes: bool,
}

impl Default for DescribeOptions {
    fn default() -> Self {
        DescribeOptions { range: None, max_clips_per_track: None, media: true, notes: true }
    }
}

/// The tracks in the order the timeline shows them, top row first: video tracks from the topmost layer
/// (the last video track in the project, drawn over the others) down to the bottom layer, then the
/// audio tracks in project order (the first audio track is the top row of the audio section).
/// `describe`, `timeline_json` and the timeline all use this order.
pub fn tracks_in_display_order(p: &Project) -> Vec<&Track> {
    let video = p.tracks.iter().rev().filter(|t| t.kind == TrackKind::Video);
    let audio = p.tracks.iter().filter(|t| t.kind == TrackKind::Audio);
    video.chain(audio).collect()
}

/// Section headings of [`describe`].
pub const VIDEO_TRACKS_HEADING: &str = "Video tracks (top layer first):";
pub const AUDIO_TRACKS_HEADING: &str = "Audio tracks (as shown, top to bottom):";

/// Everything, as compact text.
pub fn describe(p: &Project, r: &Resolved) -> String {
    describe_with(p, r, &DescribeOptions::default())
}

fn in_range(o: &DescribeOptions, s: Option<Span>) -> bool {
    match (o.range, s) {
        (None, _) => true,
        (Some(_), None) => true, // unresolved items are always shown
        (Some((a, b)), Some(s)) => s.end >= a && s.start <= b,
    }
}

fn media_label(p: &Project, asset: &str) -> String {
    match p.media.iter().find(|m| m.id == asset) {
        Some(m) if !m.name.is_empty() => m.name.clone(),
        Some(m) => util::file_name(&m.path).to_string(),
        None => format!("{asset}(missing)"),
    }
}

fn param_text(p: &Param) -> Option<String> {
    match p {
        Param::Value(v) => Some(util::num(*v)),
        Param::Animated { keys } if keys.is_empty() => None,
        Param::Animated { keys } => Some(format!("{}→{}", util::num(keys[0].v), util::num(keys[keys.len() - 1].v))),
    }
}

/// The end an `until` anchor names: `end of c3`, `start of c4`, `marker mk1`, `word 17 of vo`,
/// `9.00 s`, plus the offset (`end of c3 (+0.5)`).
pub fn until_text(t: &Time) -> String {
    match t {
        Time::At(v) => format!("{} s", util::tsecs(*v)),
        Time::Anchored(a) => {
            let base = match &a.to {
                AnchorTarget::After(id) => format!("end of {id}"),
                AnchorTarget::With(id) | AnchorTarget::Before(id) => format!("start of {id}"),
                AnchorTarget::Marker(id) => format!("marker {id}"),
                AnchorTarget::Word(w) => format!("{} of word {}{} of {}", if w.end { "end" } else { "start" }, w.index, w.text.as_ref().map(|t| format!(" {t:?}")).unwrap_or_default(), w.clip),
            };
            if a.offset != 0.0 {
                format!("{base} ({})", util::signed(a.offset))
            } else {
                base
            }
        }
    }
}

fn matte_mode_name(m: MatteMode) -> &'static str {
    match m {
        MatteMode::Alpha => "alpha",
        MatteMode::AlphaInverted => "alpha_inverted",
        MatteMode::Luma => "luma",
        MatteMode::LumaInverted => "luma_inverted",
    }
}

/// One clip as a line (without indentation).
pub fn clip_line(p: &Project, r: &Resolved, c: &Clip, notes: bool) -> String {
    let mut s = match r.span(&c.id) {
        Some(sp) => format!("[{}–{}] {}", util::tsecs(sp.start), util::tsecs(sp.end), c.id),
        None => format!("[unresolved] {}", c.id),
    };
    if !c.name.is_empty() {
        s.push_str(&format!(" {:?}", c.name));
    }
    s.push(' ');
    match &c.source {
        Source::Media { asset, in_point } => {
            let kind = p.media.iter().find(|m| &m.id == asset).map(|m| m.kind);
            if kind == Some(MediaKind::Image) {
                s.push_str(&format!("image:{}", media_label(p, asset)));
            } else {
                s.push_str(&format!("media:{}@{}s", media_label(p, asset), util::num(*in_point)));
                match &c.speed {
                    Param::Value(v) if *v == 1.0 => {}
                    Param::Value(v) if *v == 0.0 => s.push_str(" freeze"),
                    Param::Value(v) => s.push_str(&format!(" x{} speed", util::num(*v))),
                    other => {
                        if let Some(t) = param_text(other) {
                            s.push_str(&format!(" speed ramp {t}"));
                        }
                    }
                }
                if c.reverse {
                    s.push_str(" reversed");
                }
                if c.muted {
                    s.push_str(" muted");
                }
            }
        }
        Source::Text(t) => {
            s.push_str(&format!("text:{:?}", util::short(&t.text, 48)));
            if let Some(h) = &t.highlight {
                s.push_str(&format!(" karaoke({} words)", h.words.len()));
            }
            if t.reveal.kind != RevealKind::None {
                s.push_str(&format!(" reveal:{}", serde_json::to_value(t.reveal.kind).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()));
            }
        }
        Source::Solid { color } => s.push_str(&format!("solid:{}", color.to_hex())),
        Source::Shape(sh) => {
            let k = serde_json::to_value(sh.kind).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
            s.push_str(&format!("shape:{k} {}×{}", util::num(sh.width), util::num(sh.height)));
        }
        Source::Adjustment => s.push_str("adjustment"),
    }
    if let Time::Anchored(a) = &c.start {
        s.push_str(&format!(" — {}", describe_anchor(a)));
    }
    if let Some(u) = &c.until {
        s.push_str(&format!(" until {}", until_text(u)));
    }
    // Notable properties.
    let mut props: Vec<String> = Vec::new();
    if !c.enabled {
        props.push("disabled".into());
    }
    if c.opacity != Param::Value(1.0) {
        props.push(format!("opacity {}", param_text(&c.opacity).unwrap_or_default()));
    }
    if c.volume != Param::Value(1.0) {
        props.push(format!("vol {}", param_text(&c.volume).unwrap_or_default()));
    }
    if c.fade_in > 0.0 || c.fade_out > 0.0 {
        props.push(format!("fade {}s/{}s", util::num(c.fade_in), util::num(c.fade_out)));
    }
    let t = &c.transform;
    if t.x != Param::Value(0.0) || t.y != Param::Value(0.0) {
        props.push(format!("pos {},{}", param_text(&t.x).unwrap_or_default(), param_text(&t.y).unwrap_or_default()));
    }
    if t.scale != Param::Value(1.0) {
        props.push(format!("scale {}", param_text(&t.scale).unwrap_or_default()));
    }
    if t.rotation != Param::Value(0.0) {
        props.push(format!("rot {}°", param_text(&t.rotation).unwrap_or_default()));
    }
    if t.rotation_x != Param::Value(0.0) || t.rotation_y != Param::Value(0.0) {
        let mut s3 = format!("3d x{}° y{}°", param_text(&t.rotation_x).unwrap_or_default(), param_text(&t.rotation_y).unwrap_or_default());
        if t.perspective != 2000.0 {
            s3.push_str(&if t.perspective == 0.0 { " ortho".to_string() } else { format!(" persp {}", util::num(t.perspective)) });
        }
        props.push(s3);
    }
    if let Some(m) = &c.motion_blur {
        props.push(format!("mblur {}°×{}", util::num(m.shutter), m.samples));
    }
    if c.frame_blend {
        props.push("frame blend".into());
    }
    if let Some(m) = &c.matte {
        props.push(format!("matte {} {}", m.clip, matte_mode_name(m.mode)));
    }
    if p.clips.iter().any(|x| x.matte.as_ref().is_some_and(|m| m.clip == c.id)) {
        let users: Vec<&str> = p.clips.iter().filter(|x| x.matte.as_ref().is_some_and(|m| m.clip == c.id)).map(|x| x.id.as_str()).collect();
        props.push(format!("matte for {} (not drawn itself)", users.join(", ")));
    }
    if t.fit != Fit::Contain {
        props.push(format!("fit {}", serde_json::to_value(t.fit).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()));
    }
    if c.blend != BlendMode::Normal {
        props.push(format!("blend {}", serde_json::to_value(c.blend).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()));
    }
    let mut animated = Vec::new();
    crate::ops::prims::for_each_param(c, |path, prm| {
        if prm.is_animated() && path != "speed" {
            animated.push(path);
        }
    });
    if !animated.is_empty() {
        props.push(format!("anim: {}", animated.join(", ")));
    }
    if !props.is_empty() {
        s.push_str(&format!(" ({})", props.join(", ")));
    }
    if !c.effects.is_empty() {
        let fx: Vec<String> = c
            .effects
            .iter()
            .map(|e| {
                let mut n = e.kind.clone();
                if effect_spec(&e.kind).is_none() {
                    n.push_str("(unknown)");
                }
                if !e.enabled {
                    n.push_str("(off)");
                }
                n
            })
            .collect();
        s.push_str(&format!(" — fx: {}", fx.join(", ")));
    }
    if !c.tags.is_empty() {
        s.push_str(&format!(" #{}", c.tags.join(" #")));
    }
    if notes && !c.notes.is_empty() {
        s.push_str(&format!(" — {}", util::short(&c.notes, 100)));
    }
    if let Some(e) = r.error_for(&c.id) {
        s.push_str(&format!(" — ERROR: {}", e.message));
    }
    s
}

fn track_header(t: &Track, n: usize) -> String {
    let mut s = format!("{} {}", t.id, if t.kind == TrackKind::Video { "video" } else { "audio" });
    if !t.role.is_empty() {
        s.push_str(&format!(" · {}", t.role));
    }
    if !t.name.is_empty() {
        s.push_str(&format!(" · {:?}", t.name));
    }
    let mut flags = Vec::new();
    if t.locked {
        flags.push("locked".to_string());
    }
    if t.hidden {
        flags.push("hidden".into());
    }
    if t.muted {
        flags.push("muted".into());
    }
    if t.solo {
        flags.push("solo".into());
    }
    if t.volume != 1.0 {
        flags.push(format!("vol {}", util::num(t.volume)));
    }
    if !flags.is_empty() {
        s.push_str(&format!(" [{}]", flags.join(", ")));
    }
    if n == 0 {
        s.push_str(" (empty)");
    }
    s
}

/// Text description with options (range, per-track cap, media/notes on or off).
pub fn describe_with(p: &Project, r: &Resolved, o: &DescribeOptions) -> String {
    let mut out = String::new();
    let line = |out: &mut String, s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    let st = &p.settings;
    line(
        &mut out,
        format!(
            "Project {:?} — {}×{}, {} fps, {} Hz, {} s ({}), {} clip(s){}",
            p.name,
            st.width,
            st.height,
            util::num(st.fps),
            st.sample_rate,
            util::secs(r.duration),
            if st.duration.is_some() { "fixed length" } else { "ends with the last clip" },
            p.clips.len(),
            if st.background != Color::BLACK { format!(", background {}", st.background.to_hex()) } else { String::new() }
        ),
    );
    if let Some((a, b)) = o.range {
        line(&mut out, format!("Showing {}–{} s only.", util::secs(a), util::secs(b)));
    }
    if o.notes && !p.notes.is_empty() {
        line(&mut out, "Notes:".into());
        for l in p.notes.lines() {
            line(&mut out, format!("  {l}"));
        }
    }
    if o.media && !p.media.is_empty() {
        line(&mut out, format!("Media ({}):", p.media.len()));
        for m in &p.media {
            let i = &m.info;
            let mut s = format!("  {} {}", m.id, util::file_name(&m.path));
            if !m.name.is_empty() {
                s.push_str(&format!(" {:?}", m.name));
            }
            let kind = match m.kind {
                MediaKind::Video => "video",
                MediaKind::Audio => "audio",
                MediaKind::Image => "image",
            };
            s.push_str(&format!(" — {kind}"));
            if m.kind != MediaKind::Image {
                s.push_str(&format!(" {} s", util::num(i.duration)));
            }
            if i.width > 0 {
                s.push_str(&format!(" {}×{}", i.width, i.height));
            }
            if i.fps > 0.0 {
                s.push_str(&format!(" {} fps", util::num(i.fps)));
            }
            if m.kind == MediaKind::Video {
                s.push_str(if i.has_audio { " +audio" } else { " (no audio)" });
            }
            if let Some(t) = p.transcripts.get(&m.id) {
                s.push_str(&format!(", transcript {} words", t.words.len()));
            }
            if !m.tags.is_empty() {
                s.push_str(&format!(" #{}", m.tags.join(" #")));
            }
            if o.notes && !m.notes.is_empty() {
                s.push_str(&format!(" — {}", util::short(&m.notes, 100)));
            }
            line(&mut out, s);
        }
    }
    // Clips grouped by track.
    let mut by_track: HashMap<&str, Vec<&Clip>> = HashMap::new();
    for c in &p.clips {
        by_track.entry(c.track.as_str()).or_default().push(c);
    }
    let key = |c: &&Clip| r.span(&c.id).map(|s| s.start).unwrap_or(f64::INFINITY);
    if p.tracks.is_empty() {
        line(&mut out, "Tracks: (none)".into());
    }
    let mut section = None;
    for t in tracks_in_display_order(p) {
        if section != Some(t.kind) {
            section = Some(t.kind);
            line(&mut out, if t.kind == TrackKind::Video { VIDEO_TRACKS_HEADING } else { AUDIO_TRACKS_HEADING }.into());
        }
        let mut clips = by_track.remove(t.id.as_str()).unwrap_or_default();
        clips.sort_by(|a, b| key(a).total_cmp(&key(b)).then(a.id.cmp(&b.id)));
        line(&mut out, format!("  {}", track_header(t, clips.len())));
        let shown: Vec<&&Clip> = clips.iter().filter(|c| in_range(o, r.span(&c.id))).collect();
        let cap = o.max_clips_per_track.unwrap_or(usize::MAX);
        for c in shown.iter().take(cap) {
            line(&mut out, format!("    {}", clip_line(p, r, c, o.notes)));
        }
        if shown.len() > cap {
            line(&mut out, format!("    … {} more clip(s)", shown.len() - cap));
        }
    }
    // Clips on tracks that do not exist.
    let mut orphans: Vec<&Clip> = by_track.into_values().flatten().collect();
    if !orphans.is_empty() {
        orphans.sort_by(|a, b| key(a).total_cmp(&key(b)));
        line(&mut out, "Clips on missing tracks:".into());
        for c in orphans {
            line(&mut out, format!("    {} (track {})", clip_line(p, r, c, o.notes), c.track));
        }
    }
    if !p.transitions.is_empty() {
        line(&mut out, format!("Transitions ({}):", p.transitions.len()));
        for t in &p.transitions {
            let at = r.span(&t.to).map(|s| s.start);
            if !in_range(o, at.map(|a| Span::new(a, a))) {
                continue;
            }
            line(
                &mut out,
                format!(
                    "  {} {} {} s {}→{}{}",
                    t.id,
                    t.kind,
                    util::num(t.duration),
                    t.from,
                    t.to,
                    at.map(|a| format!(" at {}", util::tsecs(a))).unwrap_or_default()
                ),
            );
        }
    }
    if !p.markers.is_empty() {
        line(&mut out, format!("Markers ({}):", p.markers.len()));
        let mut ms: Vec<&Marker> = p.markers.iter().collect();
        ms.sort_by(|a, b| r.marker(&a.id).unwrap_or(f64::INFINITY).total_cmp(&r.marker(&b.id).unwrap_or(f64::INFINITY)));
        for m in ms {
            let t = r.marker(&m.id);
            if !in_range(o, t.map(|a| Span::new(a, a))) {
                continue;
            }
            let mut s = format!("  [{}] {}", t.map(util::tsecs).unwrap_or_else(|| "unresolved".into()), m.id);
            if !m.name.is_empty() {
                s.push_str(&format!(" {:?}", m.name));
            }
            if m.kind != MarkerKind::Marker {
                s.push_str(&format!(" {}", serde_json::to_value(m.kind).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()));
            }
            if let Time::Anchored(a) = &m.time {
                s.push_str(&format!(" ({})", describe_anchor(a)));
            }
            if o.notes && !m.note.is_empty() {
                s.push_str(&format!(" — {}", util::short(&m.note, 100)));
            }
            line(&mut out, s);
        }
    }
    if !p.ducking.is_empty() {
        line(&mut out, "Ducking:".into());
        let role = |id: &str| p.tracks.iter().find(|t| t.id == id).filter(|t| !t.role.is_empty()).map(|t| format!(" ({})", t.role)).unwrap_or_default();
        for d in &p.ducking {
            line(
                &mut out,
                format!(
                    "  {}: {}{} ducks {} dB while {}{} is above {} dBFS (attack {} s, release {} s)",
                    d.id,
                    d.target,
                    role(&d.target),
                    util::num(d.amount_db),
                    d.trigger,
                    role(&d.trigger),
                    util::num(d.threshold_db),
                    util::num(d.attack),
                    util::num(d.release)
                ),
            );
        }
    }
    let e = &p.export;
    if *e != ExportSettings::default() {
        line(
            &mut out,
            format!(
                "Export: {} {}{}{}{}",
                e.codec,
                e.quality,
                if e.width.is_some() || e.height.is_some() {
                    let (w, h) = e.output_size(st);
                    format!(", {w}×{h} fit {}", serde_json::to_value(e.fit).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default())
                } else {
                    String::new()
                },
                e.loudness_lufs.map(|l| format!(", {} LUFS", util::num(l))).unwrap_or_default(),
                e.path.as_ref().map(|p| format!(" → {p}")).unwrap_or_default()
            ),
        );
    }
    if !r.errors.is_empty() {
        line(&mut out, format!("Errors ({}):", r.errors.len()));
        for e in &r.errors {
            line(&mut out, format!("  ! {}", e.message));
        }
    }
    out
}

fn source_json(p: &Project, c: &Clip) -> Value {
    match &c.source {
        Source::Media { asset, in_point } => {
            let m = p.media.iter().find(|m| &m.id == asset);
            let (lo, hi) = source_span(c).unwrap_or((*in_point, *in_point));
            let mut v = json!({
                "type": "media",
                "asset": asset,
                "file": m.map(|m| util::file_name(&m.path).to_string()),
                "kind": m.map(|m| m.kind),
                "in": in_point,
                "source_range": [lo, hi],
            });
            if c.speed != Param::Value(1.0) {
                v["speed"] = serde_json::to_value(&c.speed).unwrap_or(Value::Null);
            }
            if c.reverse {
                v["reverse"] = json!(true);
            }
            v
        }
        Source::Text(t) => {
            let mut v = json!({"type": "text", "text": t.text});
            if let Some(h) = &t.highlight {
                v["karaoke_words"] = json!(h.words.len());
            }
            v
        }
        Source::Solid { color } => json!({"type": "solid", "color": color}),
        Source::Shape(s) => json!({"type": "shape", "kind": s.kind, "width": s.width, "height": s.height}),
        Source::Adjustment => json!({"type": "adjustment"}),
    }
}

/// One clip as timeline JSON.
pub fn clip_json(p: &Project, r: &Resolved, c: &Clip) -> Value {
    let span = r.span(&c.id);
    let mut v = json!({
        "id": c.id,
        "track": c.track,
        "start": span.map(|s| s.start),
        "end": span.map(|s| s.end),
        "duration": c.duration,
        "source": source_json(p, c),
    });
    if !c.name.is_empty() {
        v["name"] = json!(c.name);
    }
    if let Time::Anchored(a) = &c.start {
        v["anchor"] = serde_json::to_value(&c.start).unwrap_or(Value::Null);
        v["anchor_text"] = json!(describe_anchor(a));
    }
    if let Some(u) = &c.until {
        v["until"] = serde_json::to_value(u).unwrap_or(Value::Null);
        v["until_text"] = json!(until_text(u));
    }
    if !c.effects.is_empty() {
        v["effects"] = json!(c.effects.iter().map(|e| e.kind.clone()).collect::<Vec<_>>());
    }
    if let Some(m) = &c.motion_blur {
        v["motion_blur"] = serde_json::to_value(m).unwrap_or(Value::Null);
    }
    if c.frame_blend {
        v["frame_blend"] = json!(true);
    }
    if let Some(m) = &c.matte {
        v["matte"] = serde_json::to_value(m).unwrap_or(Value::Null);
    }
    let users: Vec<&str> = p.clips.iter().filter(|x| x.matte.as_ref().is_some_and(|m| m.clip == c.id)).map(|x| x.id.as_str()).collect();
    if !users.is_empty() {
        v["matte_for"] = json!(users);
    }
    if !c.enabled {
        v["enabled"] = json!(false);
    }
    if c.muted {
        v["muted"] = json!(true);
    }
    if !c.tags.is_empty() {
        v["tags"] = json!(c.tags);
    }
    if !c.notes.is_empty() {
        v["notes"] = json!(c.notes);
    }
    if let Some(e) = r.error_for(&c.id) {
        v["error"] = json!(e.message);
    }
    v
}

/// The same picture as [`describe`], as JSON: tracks in the timeline's on-screen order (video top
/// layer first, then audio as shown; see [`tracks_in_display_order`]) with their clips (absolute
/// start/end, source summary, anchor), transitions, markers, ducking, media and resolve errors.
pub fn timeline_json(p: &Project, r: &Resolved) -> Value {
    let key = |c: &&Clip| r.span(&c.id).map(|s| s.start).unwrap_or(f64::INFINITY);
    let tracks: Vec<Value> = tracks_in_display_order(p)
        .into_iter()
        .map(|t| {
            let mut clips: Vec<&Clip> = p.clips.iter().filter(|c| c.track == t.id).collect();
            clips.sort_by(|a, b| key(a).total_cmp(&key(b)).then(a.id.cmp(&b.id)));
            json!({
                "id": t.id,
                "kind": t.kind,
                "role": t.role,
                "name": t.name,
                "muted": t.muted,
                "hidden": t.hidden,
                "locked": t.locked,
                "solo": t.solo,
                "volume": t.volume,
                "clips": clips.iter().map(|c| clip_json(p, r, c)).collect::<Vec<_>>(),
            })
        })
        .collect();
    let markers: Vec<Value> = p
        .markers
        .iter()
        .map(|m| {
            let mut v = json!({"id": m.id, "time": r.marker(&m.id), "name": m.name, "kind": m.kind});
            if let Time::Anchored(a) = &m.time {
                v["anchor_text"] = json!(describe_anchor(a));
            }
            if !m.note.is_empty() {
                v["note"] = json!(m.note);
            }
            v
        })
        .collect();
    let transitions: Vec<Value> = p
        .transitions
        .iter()
        .map(|t| json!({"id": t.id, "kind": t.kind, "from": t.from, "to": t.to, "duration": t.duration, "at": r.span(&t.to).map(|s| s.start), "params": t.params}))
        .collect();
    let media: Vec<Value> = p
        .media
        .iter()
        .map(|m| json!({"id": m.id, "file": util::file_name(&m.path), "path": m.path, "kind": m.kind, "duration": m.info.duration, "has_audio": m.info.has_audio, "transcript_words": p.transcripts.get(&m.id).map(|t| t.words.len())}))
        .collect();
    json!({
        "name": p.name,
        "settings": p.settings,
        "duration": r.duration,
        "notes": p.notes,
        "track_order": "as the timeline shows them, top row first: video tracks from the top layer (drawn over the others) down to the bottom layer, then audio tracks top to bottom (project order)",
        "tracks": tracks,
        "transitions": transitions,
        "markers": markers,
        "ducking": p.ducking,
        "media": media,
        "export": p.export,
        "export_size": p.export.output_size(&p.settings),
        "errors": r.errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::resolve;
    use crate::testutil::*;

    fn sample() -> Project {
        let mut p = project();
        p.name = "Round 40".into();
        p.notes = "Fast cuts.\nCaptions on.".into();
        let mut c1 = media_clip("c1", "v1", "mv", Time::At(0.0), 12.4, 31.2);
        c1.speed = Param::Value(2.0);
        c1.notes = "hook".into();
        c1.effects.push(effect("color", &[]));
        c1.effects.push(effect("blur", &[]));
        p.clips.push(c1);
        let mut c6 = media_clip("c6", "v1", "mv", after("c1", 0.0), 3.0, 50.0);
        c6.transform.scale = keys(&[(0.0, 1.0), (3.0, 1.2)]);
        p.clips.push(c6);
        let mut c7 = text_clip("c7", "v2", after("c6", 0.3), 3.5);
        c7.name = "title".into();
        p.clips.push(c7);
        p.clips.push(text_clip("bad", "v2", after("zz", 0.0), 1.0));
        p.markers.push(Marker { id: "mk1".into(), time: Time::At(3.2), name: "Hook".into(), kind: MarkerKind::Chapter, color: None, note: "start here".into() });
        p.transitions.push(Transition { id: "tr1".into(), from: "c1".into(), to: "c6".into(), kind: "crossfade".into(), duration: 0.5, params: Default::default(), colors: Default::default() });
        p.ducking.push(Ducking { id: "d1".into(), target: "a2".into(), trigger: "a1".into(), amount_db: -14.0, threshold_db: -45.0, attack: 0.12, release: 0.45 });
        p
    }

    #[test]
    fn text_shows_tracks_top_down_with_compact_clip_lines() {
        let p = sample();
        let r = resolve(&p);
        let d = describe(&p, &r);
        assert!(d.starts_with("Project \"Round 40\" — 1920×1080, 30 fps, 48000 Hz"), "{d}");
        assert!(d.contains("[0.00–12.40] c1 media:gameplay.mp4@31.2s x2 speed — fx: color, blur — hook"), "{d}");
        assert!(d.contains("[15.70–19.20] c7 \"title\" text:\"text c7\" — after c6 (+0.3)"), "{d}");
        assert!(d.contains("anim: transform.scale"), "{d}");
        assert!(d.contains("[unresolved] bad"), "{d}");
        let at = |s: &str| d.find(s).unwrap_or_else(|| panic!("{s} missing: {d}"));
        // The timeline's on-screen order: video top layer first, then audio in project order.
        assert!(at(VIDEO_TRACKS_HEADING) < at("v2 video · overlay"));
        assert!(at("v2 video · overlay") < at("v1 video · main"));
        assert!(at("v1 video · main") < at(AUDIO_TRACKS_HEADING));
        assert!(at(AUDIO_TRACKS_HEADING) < at("a1 audio · voice"));
        assert!(at("a1 audio · voice") < at("a2 audio · music"), "{d}");
        assert_eq!(d.matches(AUDIO_TRACKS_HEADING).count(), 1);
        assert!(d.contains("tr1 crossfade 0.5 s c1→c6 at 12.40"));
        assert!(d.contains("[3.20] mk1 \"Hook\" chapter — start here"));
        assert!(d.contains("d1: a2 (music) ducks -14 dB while a1 (voice)"));
        assert!(d.contains("Errors (1):"));
        assert!(d.contains("a1 audio · voice (empty)"));
        let short = describe_with(&p, &r, &DescribeOptions { range: Some((0.0, 5.0)), media: false, notes: false, max_clips_per_track: None });
        assert!(!short.contains("c7 \"title\""));
        assert!(!short.contains("Media ("));
        assert!(!short.contains("hook"));
    }

    #[test]
    fn json_has_absolute_times_and_anchors() {
        let p = sample();
        let r = resolve(&p);
        let j = timeline_json(&p, &r);
        let order: Vec<&str> = j["tracks"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap()).collect();
        assert_eq!(order, tracks_in_display_order(&p).iter().map(|t| t.id.as_str()).collect::<Vec<_>>());
        assert_eq!(order[0], "v2", "top layer first");
        assert!(order.iter().position(|t| *t == "a1") < order.iter().position(|t| *t == "a2"), "audio as shown: {order:?}");
        let v1 = j["tracks"].as_array().unwrap().iter().find(|t| t["id"] == "v1").unwrap();
        let c6 = &v1["clips"][1];
        assert_eq!(c6["id"], "c6");
        assert_eq!(c6["start"], 12.4);
        assert_eq!(c6["anchor"], json!({"after": "c1"}));
        assert_eq!(c6["anchor_text"], "after c1");
        assert_eq!(v1["clips"][0]["source"]["source_range"], json!([31.2, 56.0]));
        assert_eq!(j["errors"].as_array().unwrap().len(), 1);
        assert_eq!(j["markers"][0]["time"], 3.2);
    }
}
