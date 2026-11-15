//! Pure helpers shared by the widgets: timecode formatting/parsing, frame quantisation, relative
//! times, and clip/media lookups (kind, label, linked clips, anchor chains).

use aiframes_core::{Anchor, AnchorTarget, Clip, Finding, Id, MediaAsset, MediaKind, Project, Resolved, Severity, Source, Span, Time, WordRef};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------------------------

/// The exact rate of the frame grid the renderer uses (`aiframes_media::fps_rational`): 29.97 →
/// 30000/1001, 23.976 → 24000/1001, 30 → 30. Every frame computation in the widgets goes through it,
/// so timecodes and snapped times agree with rendered frames.
pub fn exact_fps(fps: f64) -> f64 {
    let (n, d) = rational(fps);
    n as f64 / d as f64
}

fn rational(fps: f64) -> (u64, u64) {
    let (n, d) = aiframes_media::fps_rational(if fps.is_finite() && fps > 0.0 { fps } else { 30.0 });
    (n.max(1) as u64, d.max(1) as u64)
}

/// The frame containing `t` (the renderer's rule: frame `i` starts at `i·d/n`).
pub fn frame_index(t: f64, fps: f64) -> i64 {
    if !t.is_finite() {
        return 0;
    }
    (t * exact_fps(fps) + 1e-6).floor() as i64
}

/// The start time of frame `i` on the renderer's grid.
pub fn frame_time(i: i64, fps: f64) -> f64 {
    let (n, d) = rational(fps);
    i as f64 * d as f64 / n as f64
}

/// `v` clamped to `[a, b]` without ever panicking: the bounds are put in order and NaNs fall back
/// (a NaN bound is ignored, a NaN `v` becomes the lower bound). For bounds that come from data
/// (clip durations, source spans) where `f64::clamp` could panic on a hand-edited file.
pub fn clamp_safe(v: f64, a: f64, b: f64) -> f64 {
    let (lo, hi) = match (a.is_nan(), b.is_nan()) {
        (false, false) => (a.min(b), a.max(b)),
        (true, false) => (b, b),
        (false, true) => (a, a),
        (true, true) => return if v.is_nan() { 0.0 } else { v },
    };
    if v.is_nan() {
        lo
    } else {
        v.clamp(lo, hi)
    }
}

/// The project's exact frame rate (see [`exact_fps`]).
pub fn fps(p: &Project) -> f64 {
    if p.settings.fps.is_finite() && p.settings.fps >= 1.0 {
        exact_fps(p.settings.fps)
    } else {
        30.0
    }
}

/// One frame in seconds.
pub fn frame(p: &Project) -> f64 {
    1.0 / fps(p)
}

/// Rounds to the nearest frame boundary of the renderer's grid.
pub fn quantize(t: f64, fps: f64) -> f64 {
    if !t.is_finite() || fps.is_nan() || fps <= 0.0 {
        return t;
    }
    frame_time((t * exact_fps(fps)).round() as i64, fps)
}

/// Nominal (integer) frame rate used for timecode: 29.97 → 30, 23.976 → 24.
pub fn nominal_fps(fps: f64) -> u64 {
    (fps.round() as u64).max(1)
}

/// `HH:MM:SS:FF` (non-drop-frame) for timeline seconds.
pub fn timecode(t: f64, fps: f64) -> String {
    if !t.is_finite() {
        return "--:--:--:--".into();
    }
    let neg = t < -1e-9;
    let n = nominal_fps(exact_fps(fps));
    let frames = frame_index(t.abs(), fps).max(0) as u64;
    let ff = frames % n;
    let secs = frames / n;
    let s = format!("{:02}:{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60, ff);
    if neg {
        format!("-{s}")
    } else {
        s
    }
}

/// Compact time for labels: `1:23.4`, `12.0 s`, `1:02:03`.
pub fn short_time(t: f64) -> String {
    if !t.is_finite() {
        return "–".into();
    }
    let neg = t < 0.0;
    let a = t.abs();
    let s = if a < 60.0 {
        format!("{a:.1} s")
    } else if a < 3600.0 {
        let m = (a / 60.0).floor();
        format!("{}:{:04.1}", m as u64, a - m * 60.0)
    } else {
        let h = (a / 3600.0).floor();
        let m = ((a - h * 3600.0) / 60.0).floor();
        format!("{}:{:02}:{:02}", h as u64, m as u64, (a - h * 3600.0 - m * 60.0).floor() as u64)
    };
    if neg {
        format!("-{s}")
    } else {
        s
    }
}

/// A duration as `m:ss` or `h:mm:ss` (media bin badges).
pub fn duration_label(d: f64) -> String {
    if !d.is_finite() || d <= 0.0 {
        return "still".into();
    }
    let s = d.round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else if d < 10.0 {
        format!("{d:.1}s")
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Parses typed times: `83.5`, `83.5s`, `1:23.5`, `1:02:03.5`, `00:01:23:12` (timecode with frames),
/// `+2` / `-0.5` stay plain numbers. `12f` = 12 frames.
pub fn parse_time(s: &str, fps: f64) -> Option<f64> {
    let s = s.trim().trim_end_matches('s').trim();
    if s.is_empty() {
        return None;
    }
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest.trim()),
        None => (false, s.strip_prefix('+').unwrap_or(s).trim()),
    };
    let v = if let Some(f) = body.strip_suffix('f') {
        frame_time(f.trim().parse::<f64>().ok()?.round() as i64, fps)
    } else {
        let parts: Vec<&str> = body.split([':', ';']).collect();
        match parts.len() {
            1 => parts[0].parse::<f64>().ok()?,
            2 => parts[0].parse::<f64>().ok()? * 60.0 + parts[1].parse::<f64>().ok()?,
            3 => parts[0].parse::<f64>().ok()? * 3600.0 + parts[1].parse::<f64>().ok()? * 60.0 + parts[2].parse::<f64>().ok()?,
            4 => {
                let n = nominal_fps(exact_fps(fps)) as f64;
                let frames: f64 = parts[3].parse().ok()?;
                let secs = parts[0].parse::<f64>().ok()? * 3600.0 + parts[1].parse::<f64>().ok()? * 60.0 + parts[2].parse::<f64>().ok()?;
                frame_time((secs * n + frames).round() as i64, fps)
            }
            _ => return None,
        }
    };
    v.is_finite().then_some(if neg { -v } else { v })
}

/// Unix seconds of an RFC 3339 time (`2026-11-08T21:41:00Z`, `...+01:00`, fractional seconds ok).
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (num(0, 4)?, num(5, 7)?, num(8, 10)?, num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_prefix('.') {
        let n = r.bytes().take_while(|b| b.is_ascii_digit()).count();
        rest = &r[n..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        r if r.len() == 6 && (r.starts_with('+') || r.starts_with('-')) => {
            let sign = if r.starts_with('-') { -1 } else { 1 };
            let oh: i64 = r.get(1..3)?.parse().ok()?;
            let om: i64 = r.get(4..6)?.parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + se - offset)
}

/// "now", "12 s ago", "3 min ago", "2 h ago", "3 d ago" for a history time.
pub fn relative_time(rfc3339: &str, now_unix: i64) -> String {
    let Some(t) = parse_rfc3339(rfc3339) else { return String::new() };
    let d = (now_unix - t).max(0);
    match d {
        0..=4 => "now".into(),
        5..=59 => format!("{d} s ago"),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86_399 => format!("{} h ago", d / 3600),
        _ => format!("{} d ago", d / 86_400),
    }
}

/// Current Unix time in seconds.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// Clips and media
// ---------------------------------------------------------------------------------------------

/// What a clip shows (drives colours and drawing).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClipKind {
    Video,
    Audio,
    Image,
    Text,
    Shape,
    Solid,
    Adjustment,
    /// A media clip whose asset id is unknown.
    Missing,
}

pub fn asset_of<'a>(p: &'a Project, c: &Clip) -> Option<&'a MediaAsset> {
    match &c.source {
        Source::Media { asset, .. } => p.media.iter().find(|m| &m.id == asset),
        _ => None,
    }
}

/// How a clip is drawn: like [`clip_kind`], but media on an audio track (e.g. a video's detached
/// sound) is audio.
pub fn clip_kind_on_track(p: &Project, c: &Clip) -> ClipKind {
    let k = clip_kind(p, c);
    let on_audio = p.tracks.iter().any(|t| t.id == c.track && t.kind == aiframes_core::TrackKind::Audio);
    if on_audio && matches!(k, ClipKind::Video) {
        ClipKind::Audio
    } else {
        k
    }
}

pub fn clip_kind(p: &Project, c: &Clip) -> ClipKind {
    match &c.source {
        Source::Media { .. } => match asset_of(p, c).map(|a| a.kind) {
            Some(MediaKind::Video) => ClipKind::Video,
            Some(MediaKind::Audio) => ClipKind::Audio,
            Some(MediaKind::Image) => ClipKind::Image,
            None => ClipKind::Missing,
        },
        Source::Text(_) => ClipKind::Text,
        Source::Shape(_) => ClipKind::Shape,
        Source::Solid { .. } => ClipKind::Solid,
        Source::Adjustment => ClipKind::Adjustment,
    }
}

/// A media asset's display name: its name, else the file name.
pub fn media_name(a: &MediaAsset) -> String {
    if !a.name.is_empty() {
        return a.name.clone();
    }
    a.path.rsplit(['/', '\\']).next().unwrap_or(&a.path).to_string()
}

/// A clip's display label: name, else media name / text / shape kind.
pub fn clip_label(p: &Project, c: &Clip) -> String {
    if !c.name.is_empty() {
        return c.name.clone();
    }
    match &c.source {
        Source::Media { asset, .. } => asset_of(p, c).map(media_name).unwrap_or_else(|| format!("missing {asset}")),
        Source::Text(t) => {
            let first = t.text.lines().next().unwrap_or("").trim();
            if first.is_empty() {
                "Text".into()
            } else {
                first.to_string()
            }
        }
        Source::Shape(s) => format!("{:?}", s.kind),
        Source::Solid { color } => format!("Solid {}", color.to_hex()),
        Source::Adjustment => "Adjustment".into(),
    }
}

/// True when the clip makes sound (audio clips, video clips whose media has audio).
pub fn has_sound(p: &Project, c: &Clip) -> bool {
    match asset_of(p, c) {
        Some(a) => a.kind == MediaKind::Audio || (a.kind == MediaKind::Video && (a.info.has_audio || a.info.duration <= 0.0)),
        None => false,
    }
}

/// The id a clip's start is anchored to (word anchors: the word's clip).
pub fn anchor_parent(t: &Time) -> Option<&str> {
    match t {
        Time::At(_) => None,
        Time::Anchored(Anchor { to, .. }) => Some(match to {
            AnchorTarget::After(id) | AnchorTarget::With(id) | AnchorTarget::Before(id) | AnchorTarget::Marker(id) => id.as_str(),
            AnchorTarget::Word(w) => w.clip.as_str(),
        }),
    }
}

/// The ids up a clip's (or marker's) anchor chain, nearest first (cycle-safe).
pub fn ancestors(p: &Project, id: &str) -> Vec<Id> {
    let mut out: Vec<Id> = Vec::new();
    let mut cur = id.to_string();
    for _ in 0..p.clips.len() + p.markers.len() + 1 {
        let time = p.clips.iter().find(|c| c.id == cur).map(|c| &c.start).or_else(|| p.markers.iter().find(|m| m.id == cur).map(|m| &m.time));
        let Some(next) = time.and_then(anchor_parent) else { break };
        if next == id || out.iter().any(|x| x == next) {
            break;
        }
        out.push(next.to_string());
        cur = next.to_string();
    }
    out
}

/// (added, round 4) The trimmed text of word `index` of `clip`'s transcript (for `WordRef::text`).
pub fn word_text(p: &Project, clip: &str, index: usize) -> Option<String> {
    let c = p.clips.iter().find(|c| c.id == clip)?;
    let a = asset_of(p, c)?;
    Some(p.transcripts.get(&a.id)?.words.get(index)?.text.trim().to_string())
}

/// (added, round 4) A word anchor target with the word's text recorded (the core warns when the
/// transcript later says something else at that index).
pub fn word_ref(p: &Project, clip: &str, index: usize, end: bool) -> WordRef {
    WordRef { clip: clip.into(), index, end, text: word_text(p, clip, index) }
}

/// The node of the anchor graph a time depends on: `(id, end)` where `end` is a clip's end (`after`).
fn dep_node(p: &Project, t: &Time) -> Option<(String, bool)> {
    let Time::Anchored(a) = t else { return None };
    Some(match &a.to {
        AnchorTarget::After(id) => (id.clone(), p.clips.iter().any(|c| &c.id == id)),
        AnchorTarget::With(id) | AnchorTarget::Before(id) | AnchorTarget::Marker(id) => (id.clone(), false),
        AnchorTarget::Word(w) => (w.clip.clone(), false),
    })
}

/// (added, round 4) Nodes of the anchor graph that depend on the end of `clip` — `(id, false)` for a
/// clip start or marker, `(id, true)` for a clip end. An `until` end anchor of `clip` must not point
/// at any of them (that would be a cycle). Follows start anchors, `until` end anchors and `after`.
pub fn end_dependants(p: &Project, clip: &str) -> HashSet<(String, bool)> {
    let mut d: HashSet<(String, bool)> = HashSet::from([(clip.to_string(), true)]);
    loop {
        let mut grew = false;
        for c in &p.clips {
            if !d.contains(&(c.id.clone(), false)) && dep_node(p, &c.start).is_some_and(|n| d.contains(&n)) {
                d.insert((c.id.clone(), false));
                grew = true;
            }
            if c.id != clip && !d.contains(&(c.id.clone(), true)) {
                let n = match &c.until {
                    Some(u) => dep_node(p, u),
                    None => Some((c.id.clone(), false)),
                };
                if n.is_some_and(|n| d.contains(&n)) {
                    d.insert((c.id.clone(), true));
                    grew = true;
                }
            }
        }
        for m in &p.markers {
            if !d.contains(&(m.id.clone(), false)) && dep_node(p, &m.time).is_some_and(|n| d.contains(&n)) {
                d.insert((m.id.clone(), false));
                grew = true;
            }
        }
        if !grew {
            return d;
        }
    }
}

/// (added, round 4) True when `until` as an end anchor of `clip` would not make an anchor cycle.
pub fn until_allowed(p: &Project, blocked: &HashSet<(String, bool)>, clip: &str, until: &Time) -> bool {
    match dep_node(p, until) {
        None => true,
        Some((id, end)) => id != clip && !blocked.contains(&(id, end)),
    }
}

/// (added, round 4) `validate` findings for the project, cached per thread until the project
/// changes (keyed by the revision and a cheap fingerprint of the clips and transcripts).
pub fn findings(p: &Project, r: &Resolved, revision: u64) -> Rc<Vec<Finding>> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    revision.hash(&mut h);
    p.clips.len().hash(&mut h);
    for c in &p.clips {
        c.id.hash(&mut h);
        c.track.hash(&mut h);
        c.duration.to_bits().hash(&mut h);
        c.until.is_some().hash(&mut h);
        if let Some(s) = r.span(&c.id) {
            (s.start.to_bits(), s.end.to_bits()).hash(&mut h);
        }
    }
    p.markers.len().hash(&mut h);
    for (k, t) in &p.transcripts {
        (k, t.words.len()).hash(&mut h);
    }
    r.errors.len().hash(&mut h);
    let key = h.finish();
    FINDINGS.with(|f| {
        let mut f = f.borrow_mut();
        if let Some((k, v)) = f.as_ref() {
            if *k == key {
                return v.clone();
            }
        }
        let v = Rc::new(aiframes_core::validate(p, r));
        *f = Some((key, v.clone()));
        v
    })
}

thread_local! {
    static FINDINGS: RefCell<Option<(u64, Rc<Vec<Finding>>)>> = const { RefCell::new(None) };
}

/// (added, round 4) Findings worth a badge on a clip: errors and warnings about it, except resolve
/// errors (drawn as unresolved clips already) and project-wide or framing notes.
pub fn shown_on_clip(f: &Finding) -> bool {
    f.severity != Severity::Info
        && !matches!(
            f.code,
            "unresolved_anchor" | "anchor_cycle" | "word_cut_out" | "until_before_start" | "gap_in_main" | "text_outside_title_safe" | "content_outside_action_safe" | "chapter_markers" | "past_fixed_duration" | "export_letterbox"
        )
}

/// (added, round 4) The badge-worthy findings about clip `id`.
pub fn clip_findings<'a>(all: &'a [Finding], id: &'a str) -> impl Iterator<Item = &'a Finding> + 'a {
    all.iter().filter(move |f| shown_on_clip(f) && f.ids.iter().any(|x| x == id))
}

/// (added, round 4) Text from the core (summaries, messages) made safe for egui's default fonts,
/// which have no arrows: "1.4→0.5 s" becomes "1.4->0.5 s".
pub fn plain(s: &str) -> String {
    s.replace('→', "->").replace('←', "<-").replace('⇒', "=>")
}

/// (added, round 4) Field documentation from the core's `schema_docs()` ("meaning (units)") for
/// tooltips, e.g. `schema_help("clip", "until")` or `schema_help("source.media", "in_point")`.
pub fn schema_help(ty: &str, field: &str) -> Option<String> {
    static DOCS: OnceLock<serde_json::Value> = OnceLock::new();
    let d = DOCS.get_or_init(aiframes_core::schema_docs);
    let t = match ty.split_once('.') {
        Some((a, v)) => &d["types"][a]["variants"][v],
        None => &d["types"][ty],
    };
    let f = &t["fields"][field];
    let m = f["meaning"].as_str()?;
    Some(match f["units"].as_str() {
        Some(u) if !u.is_empty() => format!("{m} ({u})"),
        _ => m.to_string(),
    })
}

/// Where to draw a clip that could not be resolved: its absolute time, else near its anchor target,
/// else 0.
pub fn fallback_span(_p: &Project, r: &Resolved, c: &Clip) -> Span {
    let start = match &c.start {
        Time::At(v) if v.is_finite() => *v,
        Time::At(_) => 0.0,
        Time::Anchored(a) => {
            let base = match &a.to {
                AnchorTarget::After(id) => r.span(id).map(|s| s.end),
                AnchorTarget::With(id) => r.span(id).map(|s| s.start),
                AnchorTarget::Before(id) => r.span(id).map(|s| s.start - c.duration).or_else(|| r.marker(id).map(|m| m - c.duration)),
                AnchorTarget::Marker(id) => r.marker(id),
                AnchorTarget::Word(w) => r.span(&w.clip).map(|s| s.start),
            };
            base.map(|b| b + if a.offset.is_finite() { a.offset } else { 0.0 }).unwrap_or(0.0)
        }
    };
    let start = if start.is_finite() { start.max(0.0) } else { 0.0 };
    let d = if c.duration.is_finite() && c.duration > 0.0 { c.duration } else { 1.0 };
    Span::new(start, start + d)
}

/// The span of a clip, or its fallback when unresolved.
pub fn span_or_fallback(p: &Project, r: &Resolved, c: &Clip) -> (Span, bool) {
    match r.span(&c.id) {
        Some(s) => (s, true),
        None => (fallback_span(p, r, c), false),
    }
}

/// Every edit point (clip starts/ends) plus markers, sorted and de-duplicated.
pub fn edit_points(p: &Project, r: &Resolved) -> Vec<f64> {
    let mut v: Vec<f64> = vec![0.0];
    for c in &p.clips {
        if let Some(s) = r.span(&c.id) {
            v.push(s.start);
            v.push(s.end);
        }
    }
    v.extend(r.markers.values().copied());
    v.retain(|t| t.is_finite());
    v.sort_by(f64::total_cmp);
    v.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    v
}

/// The edit point before `t` (strictly, by more than half a frame).
pub fn prev_point(points: &[f64], t: f64, tol: f64) -> Option<f64> {
    points.iter().rev().find(|&&x| x < t - tol).copied()
}

/// The edit point after `t`.
pub fn next_point(points: &[f64], t: f64, tol: f64) -> Option<f64> {
    points.iter().find(|&&x| x > t + tol).copied()
}

/// The parameter at a `set_param` path (None for unknown paths and scalar paths).
pub fn param_at<'a>(c: &'a Clip, path: &str) -> Option<&'a aiframes_core::Param> {
    aiframes_core::clip_params(c).into_iter().find(|(p, _)| p == path).map(|(_, v)| v)
}

/// Clip-local times of every keyframe of every parameter (sorted, de-duplicated).
pub fn keyframe_times(c: &Clip) -> Vec<f64> {
    let mut v: Vec<f64> = aiframes_core::clip_params(c)
        .into_iter()
        .flat_map(|(_, p)| match p {
            aiframes_core::Param::Animated { keys } => keys.iter().map(|k| k.t).collect::<Vec<_>>(),
            _ => vec![],
        })
        .collect();
    v.sort_by(f64::total_cmp);
    v.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    v
}

/// The registry spec of a parameter path of `c`: clip-level paths from `CLIP_PARAMS`, effect
/// parameters (`effects.N.<name>`) from the effect registry.
pub fn param_spec(c: &Clip, path: &str) -> Option<&'static aiframes_core::ParamSpec> {
    if let Some(rest) = path.strip_prefix("effects.") {
        let (i, name) = rest.split_once('.')?;
        let e = c.effects.get(i.parse::<usize>().ok()?)?;
        return aiframes_core::effect_spec(&e.kind)?.param(name);
    }
    aiframes_core::clip_param_spec(path)
}

/// A human label for a parameter path ("Scale", "Blur · Radius").
pub fn param_label(c: &Clip, path: &str) -> String {
    if let Some(rest) = path.strip_prefix("effects.") {
        if let Some((i, _)) = rest.split_once('.') {
            let kind = i.parse::<usize>().ok().and_then(|i| c.effects.get(i)).and_then(|e| aiframes_core::effect_spec(&e.kind)).map(|s| s.label).unwrap_or("Effect");
            if let Some(p) = param_spec(c, path) {
                return format!("{kind} · {}", p.label);
            }
        }
    }
    param_spec(c, path).map(|p| p.label.to_string()).unwrap_or_else(|| path.to_string())
}

/// Font families offered by the text inspector, used until the installed list has loaded.
pub const FALLBACK_FONTS: &[&str] = &["Segoe UI", "Arial", "Inter", "Roboto", "Montserrat", "Helvetica", "Bebas Neue", "Impact", "Georgia", "Times New Roman", "Consolas", "Courier New", "Verdana", "Calibri"];

/// The installed font families (sorted, de-duplicated). Enumerated once on a background thread
/// (fontdb reads only the font tables' names); returns [`FALLBACK_FONTS`] until that finishes.
pub fn font_families() -> Vec<String> {
    use std::sync::Mutex;
    static LIST: OnceLock<Mutex<Option<Vec<String>>>> = OnceLock::new();
    let cell = LIST.get_or_init(|| {
        std::thread::Builder::new()
            .name("aiframes-font-list".into())
            .spawn(|| {
                let mut db = fontdb::Database::new();
                db.load_system_fonts();
                let mut names: Vec<String> = db
                    .faces()
                    .filter_map(|f| f.families.iter().find(|(_, l)| *l == fontdb::Language::English_UnitedStates).or(f.families.first()).map(|(n, _)| n.clone()))
                    .filter(|n| !n.trim().is_empty() && !n.starts_with('.') && !n.starts_with('@'))
                    .collect();
                names.sort_by_key(|n| n.to_lowercase());
                names.dedup();
                if let Some(cell) = LIST.get() {
                    if let Ok(mut g) = cell.lock() {
                        *g = Some(names);
                    }
                }
            })
            .ok();
        Mutex::new(None)
    });
    match cell.lock().ok().and_then(|g| g.clone()) {
        Some(v) if !v.is_empty() => v,
        _ => FALLBACK_FONTS.iter().map(|s| s.to_string()).collect(),
    }
}

/// "color" → "Color".
pub fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// Comma-separated tags → list.
pub fn parse_tags(s: &str) -> Vec<String> {
    s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect()
}

/// Truncates to `n` characters with an ellipsis.
pub fn ellipsize(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timecode_formats_and_parses() {
        assert_eq!(timecode(0.0, 30.0), "00:00:00:00");
        assert_eq!(timecode(83.5, 30.0), "00:01:23:15");
        assert_eq!(timecode(3600.0 + 1.0 / 30.0, 30.0), "01:00:00:01");
        assert_eq!(timecode(-1.0, 25.0), "-00:00:01:00");
        // 29.97 counts 30 nominal frames per timecode second
        assert_eq!(timecode(1.0, 29.97), "00:00:00:29");
        assert_eq!(parse_time("83.5", 30.0), Some(83.5));
        assert_eq!(parse_time("1:23.5", 30.0), Some(83.5));
        assert_eq!(parse_time("00:01:23:15", 30.0), Some(83.5));
        assert_eq!(parse_time("1:00:00", 30.0), Some(3600.0));
        assert_eq!(parse_time("12f", 24.0), Some(0.5));
        assert_eq!(parse_time("-0.5s", 30.0), Some(-0.5));
        assert_eq!(parse_time("abc", 30.0), None);
        assert_eq!(parse_time("", 30.0), None);
        assert_eq!(short_time(12.0), "12.0 s");
        assert_eq!(short_time(83.5), "1:23.5");
        assert_eq!(duration_label(125.0), "2:05");
        assert_eq!(duration_label(0.0), "still");
        assert!((quantize(1.016, 30.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn clamp_never_panics() {
        assert_eq!(clamp_safe(5.0, 0.0, 10.0), 5.0);
        assert_eq!(clamp_safe(5.0, 10.0, 0.0), 5.0, "bounds in either order");
        assert_eq!(clamp_safe(-1.0, 0.0, -2.0), -1.0);
        assert_eq!(clamp_safe(f64::NAN, 1.0, 2.0), 1.0);
        assert_eq!(clamp_safe(3.0, f64::NAN, 2.0), 2.0);
        assert_eq!(clamp_safe(3.0, f64::NAN, f64::NAN), 3.0);
    }

    #[test]
    fn ntsc_uses_the_renderer_frame_grid() {
        // Regression (review): 29.97 used n / 29.97 instead of the exact 30000/1001 grid, so frame-exact
        // times (ffprobe durations, render frame starts) read one frame early.
        let t = 300.0 * 1001.0 / 30000.0; // a 300-frame camera file = 10.01 s
        assert_eq!(frame_index(t, 29.97), 300);
        assert_eq!(timecode(t, 29.97), "00:00:10:00");
        assert_eq!(timecode(frame_time(2, 29.97), 29.97), "00:00:00:02");
        assert_eq!(timecode(frame_time(2, 23.976), 23.976), "00:00:00:02");
        assert_eq!(frame_time(1, 29.97), 1001.0 / 30000.0);
        assert_eq!(quantize(0.5, 29.97), frame_time(15, 29.97));
        assert_eq!(parse_time("00:00:10:00", 29.97), Some(t));
        assert_eq!(parse_time("2f", 29.97), Some(frame_time(2, 29.97)));
        // Integer rates are unchanged.
        assert_eq!(timecode(83.5, 30.0), "00:01:23:15");
        assert_eq!(quantize(1.016, 30.0), 1.0);
    }

    #[test]
    fn rfc3339_and_relative() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-11-08T21:41:00Z"), Some(1_794_174_060));
        assert_eq!(parse_rfc3339("2026-11-08T22:41:00+01:00"), Some(1_794_174_060));
        assert_eq!(parse_rfc3339("2026-11-08T21:41:00.250Z"), Some(1_794_174_060));
        assert_eq!(parse_rfc3339("garbage"), None);
        let t = "2026-11-08T21:41:00Z";
        let base = parse_rfc3339(t).unwrap();
        assert_eq!(relative_time(t, base + 2), "now");
        assert_eq!(relative_time(t, base + 30), "30 s ago");
        assert_eq!(relative_time(t, base + 150), "2 min ago");
        assert_eq!(relative_time(t, base + 7300), "2 h ago");
        assert_eq!(relative_time(t, base + 200_000), "2 d ago");
    }

    #[test]
    fn clip_helpers() {
        use crate::edits::tests::*;
        let mut p = demo();
        let r = aiframes_core::resolve(&p);
        assert_eq!(clip_kind(&p, &p.clips[0]), ClipKind::Video);
        assert_eq!(clip_kind(&p, &p.clips[2]), ClipKind::Audio);
        assert_eq!(clip_label(&p, &p.clips[0]), "mv.mp4");
        assert_eq!(ancestors(&p, "c2"), vec!["c1"]);
        assert!(ancestors(&p, "c1").is_empty());
        p.clips[0].opacity = aiframes_core::Param::Animated { keys: vec![aiframes_core::Key { t: 1.0, v: 0.0, ease: Default::default() }, aiframes_core::Key { t: 2.0, v: 1.0, ease: Default::default() }] };
        p.clips[0].transform.scale = aiframes_core::Param::Animated { keys: vec![aiframes_core::Key { t: 2.0, v: 1.0, ease: Default::default() }, aiframes_core::Key { t: 4.0, v: 1.2, ease: Default::default() }] };
        assert_eq!(keyframe_times(&p.clips[0]), vec![1.0, 2.0, 4.0]);
        assert!(param_at(&p.clips[0], "transform.scale").is_some_and(|x| x.is_animated()));
        assert!(param_at(&p.clips[0], "nope").is_none());
        assert_eq!(param_label(&p.clips[0], "transform.scale"), "Scale");
        assert_eq!(param_spec(&p.clips[0], "opacity").map(|s| s.max), Some(1.0));
        p.clips[0].effects.push(aiframes_core::Effect { kind: "blur".into(), enabled: true, params: Default::default(), colors: Default::default(), strings: Default::default() });
        assert_eq!(param_label(&p.clips[0], "effects.0.radius"), "Gaussian blur · Radius");
        assert!(!font_families().is_empty());
        // A broken anchor is drawn near its target.
        p.clips[1].start = Time::Anchored(Anchor { to: AnchorTarget::After("ghost".into()), offset: 0.0 });
        let r2 = aiframes_core::resolve(&p);
        assert!(r2.span("c2").is_none());
        let (s, ok) = span_or_fallback(&p, &r2, &p.clips[1]);
        assert!(!ok);
        assert_eq!(s.start, 0.0);
        assert_eq!(edit_points(&p, &r), vec![0.0, 10.0, 15.0, 20.0]);
    }

    #[test]
    fn linked_partners_both_ways() {
        use crate::edits::tests::*;
        let p = demo();
        let r = aiframes_core::resolve(&p);
        let (q, _) = run(&p, &crate::edits::detach_audio_ops(&p, &r, "c1"));
        let audio = q.clips.iter().find(|c| c.id != "c1" && c.id != "c2" && c.id != "c3").unwrap().id.clone();
        // The core's rule, both ways (the ui uses it for linked selection).
        assert_eq!(aiframes_core::linked_partners(&q, "c1"), vec![audio.clone()]);
        assert_eq!(aiframes_core::linked_partners(&q, &audio), vec!["c1".to_string()]);
        assert!(aiframes_core::linked_partners(&q, "c3").is_empty());
    }

    #[test]
    fn edit_point_navigation() {
        let pts = vec![0.0, 2.0, 5.0, 9.0];
        assert_eq!(next_point(&pts, 2.0, 0.01), Some(5.0));
        assert_eq!(prev_point(&pts, 2.0, 0.01), Some(0.0));
        assert_eq!(prev_point(&pts, 0.0, 0.01), None);
        assert_eq!(next_point(&pts, 9.0, 0.01), None);
        assert_eq!(parse_tags(" a, b ,,c"), vec!["a", "b", "c"]);
        assert_eq!(ellipsize("abcdef", 4), "abc…");
    }
}
