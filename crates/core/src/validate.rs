//! Project checks. Every finding says what is wrong (`message`), how to fix it (`fix`) and, when the
//! fix is mechanical, the exact ops to apply (`fix_ops`, ready for `Document::apply`).
//!
//! Linear or `n log n` in the number of clips. See [`CODES`] for every code.

use crate::effects::{effect_spec, transition_spec};
use crate::eval::{sample, source_span, split_param};
use crate::model::*;
use crate::ops::{all_ids, overlaps, Edge, Op, SpeedKeep, MAX_SPEED};
use crate::resolve::{ResolveErrorKind, Resolved, Span, EPS};
use crate::util;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    /// Timeline seconds the finding is about, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<Id>,
    /// One sentence on how to fix it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// Ops that fix it mechanically (apply them as one batch).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fix_ops: Vec<Op>,
}

/// Every code `validate` (and `Document::validate`) can report, with its usual severity and meaning.
pub static CODES: &[(&str, Severity, &str)] = &[
    ("unresolved_anchor", Severity::Error, "A start/marker anchor names something that does not exist, an invalid word, or depends on an unresolved item."),
    ("anchor_cycle", Severity::Error, "Items are anchored to each other in a loop."),
    ("word_cut_out", Severity::Error, "A word anchor's word is no longer in its clip's source range."),
    ("duplicate_id", Severity::Error, "Two items share an id."),
    ("unknown_track", Severity::Error, "A clip's track does not exist."),
    ("track_kind_mismatch", Severity::Error, "A picture clip on an audio track, or audio-only media on a video track."),
    ("missing_media", Severity::Error, "A clip names a media asset id that does not exist."),
    ("zero_length_clip", Severity::Error, "A clip is 0 s long (or shorter than one frame: warning)."),
    ("speed_out_of_range", Severity::Error, "Speed below 0, above the maximum, or not a number."),
    ("clip_past_media_end", Severity::Error, "A media clip plays past the end of its media."),
    ("clip_before_media_start", Severity::Error, "A media clip starts before the start of its media (negative source time)."),
    ("negative_start", Severity::Error, "A clip starts before 0 s."),
    ("overlap_same_track", Severity::Error, "Two clips overlap on one track."),
    ("gap_in_main", Severity::Warning, "A gap on the main track shows black frames."),
    ("keyframe_outside_clip", Severity::Warning, "Animation keys outside the clip's clip-local time 0..duration (often absolute times by mistake)."),
    ("keys_unsorted", Severity::Error, "Animation keys are not in time order."),
    ("ease_on_last_key", Severity::Warning, "The last key of an animation has an ease, which does nothing: a key's ease shapes the segment that starts at it (eases written for the move INTO each key belong one key earlier)."),
    ("fade_too_long", Severity::Warning, "fade_in + fade_out is longer than the clip."),
    ("text_outside_title_safe", Severity::Warning, "Text (estimated box) reaches outside the title-safe area."),
    ("content_outside_action_safe", Severity::Info, "A shape or picture-in-picture reaches outside the action-safe area."),
    ("empty_text", Severity::Warning, "A text clip has no text."),
    ("highlight_mismatch", Severity::Warning, "Karaoke highlight windows do not match the number of words in the text."),
    ("unknown_effect", Severity::Error, "An effect kind is not in the registry."),
    ("unknown_effect_param", Severity::Warning, "An effect has a parameter, colour or string its kind does not define."),
    ("effect_param_out_of_range", Severity::Warning, "An effect parameter is outside its range."),
    ("transition_not_adjacent", Severity::Error, "A transition's clips are missing, on different tracks or do not touch."),
    ("transition_too_long", Severity::Warning, "A transition is longer than a clip, or needs media handles that do not exist (edge frames are held)."),
    ("unknown_transition", Severity::Error, "A transition kind is not in the registry."),
    ("duplicate_transition", Severity::Error, "Two transitions on the same cut."),
    ("ducking_track_missing", Severity::Error, "A ducking rule names a track that does not exist (or ducks a track by itself)."),
    ("empty_track", Severity::Info, "A track has no clips."),
    ("past_fixed_duration", Severity::Warning, "A clip ends after the project's fixed duration."),
    ("no_audio_stream", Severity::Info, "An audio clip plays media without an audio stream (silence)."),
    ("missing_audio_stream", Severity::Error, "A clip plays a sound stream its file does not have (source.stream past media_info.audio_streams)."),
    ("chapter_markers", Severity::Warning, "Chapter markers break YouTube's rules (first at 0:00, at least 3, 10 s apart)."),
    ("solo_active", Severity::Info, "A track is soloed, so other tracks are silent."),
    ("unknown_transcript_asset", Severity::Info, "A transcript belongs to a media id that does not exist."),
    ("until_before_start", Severity::Error, "A clip's `until` end anchor resolves at or before its start."),
    ("word_anchor_text_mismatch", Severity::Warning, "A word anchor's stored text no longer matches the transcript word at its index (the transcript changed)."),
    ("matte_unknown", Severity::Error, "A clip's track matte names a clip that does not exist."),
    ("matte_not_visual", Severity::Error, "A clip's track matte is on an audio track (a matte must be a picture)."),
    ("matte_cycle", Severity::Error, "A clip is its own matte, or clips use each other as mattes in a loop."),
    ("matte_not_overlapping", Severity::Warning, "A clip and its matte never play at the same time, so the clip is invisible (the matte is evaluated at the same timeline time)."),
    ("motion_blur_out_of_range", Severity::Warning, "A clip's motion blur is outside shutter 0–720° / samples 2–64 (the renderer clamps it)."),
    ("unknown_transition_color", Severity::Warning, "A transition has a colour its kind does not use."),
    ("overlay_outlives_base", Severity::Warning, "A clip anchored `with` another clip runs past that clip's end (set an `until` end anchor so it follows)."),
    ("export_letterbox", Severity::Warning, "The export frame size has another aspect than the project and fit is contain, so the export gets bars."),
    ("media_offline", Severity::Error, "(Document::validate) A media file does not exist on disk."),
];

struct V<'a> {
    p: &'a Project,
    r: &'a Resolved,
    out: Vec<Finding>,
    /// Further candidate fixes per finding (tried in order when `fix_ops` does not verify).
    alts: Vec<Vec<Vec<Op>>>,
}

fn op(v: serde_json::Value) -> Op {
    serde_json::from_value(v).expect("validate builds valid ops")
}

impl V<'_> {
    #[allow(clippy::too_many_arguments)]
    fn push(&mut self, severity: Severity, code: &'static str, message: String, at: Option<f64>, ids: Vec<Id>, fix: Option<String>, fix_ops: Vec<Op>) {
        self.out.push(Finding { severity, code, message, at, ids, fix, fix_ops });
        self.alts.push(Vec::new());
    }

    /// Adds alternative fixes to the finding pushed last.
    fn alternatives(&mut self, alts: Vec<Vec<Op>>) {
        if let Some(a) = self.alts.last_mut() {
            a.extend(alts.into_iter().filter(|x| !x.is_empty()));
        }
    }
}

/// Fix candidates are checked for at most this many findings per `validate` call (each check applies
/// the ops to a copy and validates it; on large projects the budget shrinks to keep `validate` fast);
/// later findings keep their `fix` text but no `fix_ops` (apply the first fixes and validate again).
pub const VERIFIED_FIXES: usize = 64;

fn error_keys(f: &[Finding]) -> HashSet<String> {
    f.iter().filter(|f| f.severity == Severity::Error).map(|f| format!("{}:{}", f.code, f.ids.join(","))).collect()
}

/// A fix is kept only if it applies, removes its own finding and adds no new error.
fn fix_works(p: &Project, errors: &HashSet<String>, f: &Finding, ops: &[Op]) -> bool {
    if ops.is_empty() {
        return false;
    }
    let mut q = p.clone();
    if crate::ops::apply_batch(&mut q, ops).is_err() {
        return false;
    }
    let rq = crate::resolve::resolve(&q);
    let (after, _) = findings(&q, &rq);
    if after.iter().any(|g| g.code == f.code && g.ids == f.ids) {
        return false;
    }
    error_keys(&after).is_subset(errors)
}

/// Checks the project. Findings are sorted: errors first, then warnings, then info; by time within.
///
/// Every finding's `fix_ops` has been checked: applied to a copy of the project it succeeds, removes
/// that finding and introduces no new error (see [`VERIFIED_FIXES`]).
pub fn validate(p: &Project, r: &Resolved) -> Vec<Finding> {
    let (mut out, alts) = findings(p, r);
    let errors = error_keys(&out);
    let budget = (200_000 / (p.clips.len() + 1)).clamp(4, VERIFIED_FIXES);
    let mut checked = 0;
    for (f, more) in out.iter_mut().zip(alts) {
        if f.fix_ops.is_empty() && more.is_empty() {
            continue;
        }
        if checked >= budget {
            f.fix_ops.clear();
            continue;
        }
        checked += 1;
        let candidates: Vec<Vec<Op>> = std::iter::once(std::mem::take(&mut f.fix_ops)).chain(more).collect();
        if let Some(ok) = candidates.into_iter().find(|ops| fix_works(p, &errors, f, ops)) {
            f.fix_ops = ok;
        }
    }
    out.sort_by(|a, b| a.severity.cmp(&b.severity).then(a.at.unwrap_or(f64::MAX).total_cmp(&b.at.unwrap_or(f64::MAX))));
    out
}

/// All findings (unsorted) with their alternative fixes, before verification.
fn findings(p: &Project, r: &Resolved) -> (Vec<Finding>, Vec<Vec<Vec<Op>>>) {
    let mut v = V { p, r, out: Vec::new(), alts: Vec::new() };
    duplicate_ids(&mut v);
    resolve_errors(&mut v);
    let tracks: HashMap<&str, &Track> = p.tracks.iter().map(|t| (t.id.as_str(), t)).collect();
    let media: HashMap<&str, &MediaAsset> = p.media.iter().map(|m| (m.id.as_str(), m)).collect();
    for c in &p.clips {
        clip_checks(&mut v, c, &tracks, &media);
    }
    overlap_checks(&mut v);
    gap_checks(&mut v);
    transition_checks(&mut v);
    ducking_checks(&mut v, &tracks);
    track_checks(&mut v);
    duration_checks(&mut v);
    marker_checks(&mut v);
    export_checks(&mut v);
    word_text_checks(&mut v);
    overlay_checks(&mut v);
    matte_checks(&mut v);
    look_checks(&mut v);
    for a in p.transcripts.keys() {
        if !media.contains_key(a.as_str()) {
            v.push(Severity::Info, "unknown_transcript_asset", format!("there is a transcript for {a}, but no media asset has that id."), None, vec![a.clone()], Some("import the media with that id again, or ignore it.".into()), vec![]);
        }
    }
    (v.out, v.alts)
}

fn duplicate_ids(v: &mut V) {
    let mut seen: BTreeMap<&str, Vec<&'static str>> = BTreeMap::new();
    for (id, kind) in all_ids(v.p) {
        seen.entry(id).or_default().push(kind);
    }
    let dups: Vec<(String, Vec<&'static str>)> = seen.into_iter().filter(|(_, k)| k.len() > 1).map(|(i, k)| (i.to_string(), k)).collect();
    for (id, kinds) in dups {
        v.push(
            Severity::Error,
            "duplicate_id",
            format!("id {id:?} is used by {} items ({}); ids must be unique across the project.", kinds.len(), kinds.join(", ")),
            None,
            vec![id],
            Some("rename one of them in the project file (ops never create duplicates and cannot rename ids).".into()),
            vec![],
        );
    }
}

fn resolve_errors(v: &mut V) {
    let errors = &v.r.errors;
    let mut blocked: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in errors.iter().filter(|e| e.kind == ResolveErrorKind::Blocked) {
        if let Some(root) = &e.root {
            blocked.entry(root.as_str()).or_default().push(e.id.as_str());
        }
    }
    for e in errors {
        if e.kind == ResolveErrorKind::Blocked {
            // Reported with its root, unless the root has no error of its own (should not happen).
            if e.root.as_ref().is_some_and(|r| errors.iter().any(|x| &x.id == r && x.kind != ResolveErrorKind::Blocked)) {
                continue;
            }
        }
        let deps = blocked.get(e.id.as_str()).cloned().unwrap_or_default();
        let mut message = e.message.clone();
        if !deps.is_empty() {
            message.push_str(&format!(" {} item(s) anchored to it cannot be placed either: {}.", deps.len(), util::join_limited(&deps, 10)));
        }
        let fix = match e.kind {
            ResolveErrorKind::UnknownTarget => format!("anchor {} to an existing clip or marker, or give it an absolute start (move_clip / set_marker).", e.id),
            ResolveErrorKind::Cycle => format!("give one of {} an absolute start or anchor it outside the loop.", e.chain.join(", ")),
            ResolveErrorKind::WordCutOut => format!("re-anchor {} to a word still in the clip, or extend/slip the clip so the word plays again.", e.id),
            ResolveErrorKind::BadWord => format!("anchor {} to a valid word index of a media clip with a transcript.", e.id),
            ResolveErrorKind::InvalidTime => format!("give {} a finite time.", e.id),
            ResolveErrorKind::Blocked => "fix the item at the root of its anchor chain.".into(),
            ResolveErrorKind::EndBeforeStart => format!("anchor the end of {} later (set_until) or remove its until.", e.id),
        };
        let ids = std::iter::once(e.id.clone()).chain(deps.iter().map(|d| d.to_string())).collect();
        let code = e.code();
        v.push(Severity::Error, code, message, None, ids, Some(fix), vec![]);
    }
}

#[allow(clippy::too_many_lines)]
fn clip_checks(v: &mut V, c: &Clip, tracks: &HashMap<&str, &Track>, media: &HashMap<&str, &MediaAsset>) {
    let p = v.p;
    let id = c.id.clone();
    let span = v.r.span(&id);
    let at = span.map(|s| s.start);
    let fps = p.settings.fps.max(1.0);
    let asset = match &c.source {
        Source::Media { asset, .. } => media.get(asset.as_str()).copied(),
        _ => None,
    };
    // Track.
    match tracks.get(c.track.as_str()) {
        None => {
            let alt = free_track(v, c, wanted_kind(c, asset));
            let fix_ops = alt.iter().map(|t| op(json!({"op":"move_clip","clip":id,"start":c.start,"track":t}))).collect();
            v.push(
                Severity::Error,
                "unknown_track",
                format!("{id} is on track {}, which does not exist.", c.track),
                at,
                vec![id.clone()],
                Some(match &alt {
                    Some(t) => format!("move it to track {t}."),
                    None => "add a track and move the clip onto it.".into(),
                }),
                fix_ops,
            );
        }
        Some(t) => {
            let want = wanted_kind(c, asset);
            // Audio tracks play the sound of any media with sound (detached audio of a video is normal);
            // only text, shapes, solids, adjustments and stills cannot go there.
            let mismatch = match t.kind {
                TrackKind::Audio => !matches!(c.source, Source::Media { .. }) || asset.is_some_and(|a| a.kind == MediaKind::Image),
                TrackKind::Video => asset.is_some_and(|a| a.kind == MediaKind::Audio),
            };
            if mismatch {
                let alt = free_track(v, c, want);
                let fix_ops = alt.iter().map(|t| op(json!({"op":"move_clip","clip":id,"start":c.start,"track":t}))).collect();
                v.push(
                    Severity::Error,
                    "track_kind_mismatch",
                    format!(
                        "{id} ({}) is on {} track {}.",
                        if want == TrackKind::Video { "a picture clip" } else { "audio-only media" },
                        if t.kind == TrackKind::Audio { "audio" } else { "video" },
                        t.id
                    ),
                    at,
                    vec![id.clone()],
                    Some(format!("move it to a {} track.", if want == TrackKind::Video { "video" } else { "audio" })),
                    fix_ops,
                );
            }
            if t.kind == TrackKind::Audio {
                if let Some(a) = asset {
                    if a.info.duration > 0.0 && !a.info.has_audio {
                        v.push(
                            Severity::Info,
                            "no_audio_stream",
                            format!("{id} is on audio track {} but its media {} has no audio stream, so it plays silence.", t.id, a.id),
                            at,
                            vec![id.clone()],
                            Some("use media with sound, or delete the clip.".into()),
                            vec![],
                        );
                    }
                }
            }
        }
    }
    // Media.
    if let (Source::Media { stream, .. }, Some(a)) = (&c.source, asset) {
        if let Some(why) = crate::ops::prims::stream_problem(a, *stream) {
            v.push(
                Severity::Error,
                "missing_audio_stream",
                format!("{id}: {why}"),
                at,
                vec![id.clone()],
                Some("pick an existing stream (set_audio_stream) or re-import the file to refresh its info.".into()),
                vec![Op::SetAudioStream { clip: id.clone(), stream: 0 }],
            );
        }
    }
    if let Source::Media { asset: aid, .. } = &c.source {
        if asset.is_none() {
            v.push(
                Severity::Error,
                "missing_media",
                format!("{id} plays media {aid}, which does not exist (known: {}).", util::join_limited(&p.media.iter().map(|m| m.id.clone()).collect::<Vec<_>>(), 10)),
                at,
                vec![id.clone()],
                Some(format!("import the file as {aid} (add_media with id), or point the clip at another asset with replace_media.")),
                vec![],
            );
        }
    }
    // Length.
    if !c.duration.is_finite() || c.duration <= 0.0 {
        v.push(
            Severity::Error,
            "zero_length_clip",
            format!("{id} is {} s long.", util::num(c.duration)),
            at,
            vec![id.clone()],
            Some("delete it or give it a positive duration.".into()),
            vec![op(json!({"op":"delete_clip","clip":id}))],
        );
    } else if c.duration < 1.0 / fps - 1e-9 {
        v.push(
            Severity::Warning,
            "zero_length_clip",
            format!("{id} is {} s long, shorter than one frame at {} fps, so it may never be seen.", util::num(c.duration), util::num(fps)),
            at,
            vec![id.clone()],
            Some("lengthen it to at least one frame or delete it.".into()),
            vec![op(json!({"op":"delete_clip","clip":id}))],
        );
    }
    // Speed.
    let bad_speed = match &c.speed {
        Param::Value(s) => !s.is_finite() || *s < 0.0 || *s > MAX_SPEED,
        Param::Animated { keys } => keys.iter().any(|k| !k.v.is_finite() || k.v < 0.0 || k.v > MAX_SPEED),
    };
    if bad_speed {
        v.push(
            Severity::Error,
            "speed_out_of_range",
            format!("{id} has a speed outside 0–{MAX_SPEED} (0 = freeze, 1 = normal)."),
            at,
            vec![id.clone()],
            Some("set a speed between 0 and 100.".into()),
            if matches!(c.source, Source::Media { .. }) {
                vec![Op::SetSpeed { clip: id.clone(), speed: 1.0, keep: SpeedKeep::Duration, linked: true }]
            } else {
                vec![Op::SetParam { clip: id.clone(), path: "speed".into(), value: Some(1.0), keys: None }]
            },
        );
    }
    // Source range.
    if let (Some(a), Some((lo, hi))) = (asset, source_span(c)) {
        let tol = if a.info.fps > 0.0 { 0.5 / a.info.fps } else { 1e-3 }.max(1e-3);
        if a.kind != MediaKind::Image && a.info.duration > 0.0 && !bad_speed {
            if hi > a.info.duration + tol {
                let excess = hi - a.info.duration;
                let edge_speed = if c.reverse { sample(&c.speed, 0.0) } else { sample(&c.speed, c.duration) };
                let fix_ops = if edge_speed > 1e-6 {
                    let dt = excess / edge_speed;
                    if dt < c.duration - 1e-3 {
                        if c.reverse {
                            vec![Op::TrimClip { clip: id.clone(), edge: Edge::Start, to: None, delta: Some(dt), ripple: Some(false) }]
                        } else {
                            vec![Op::TrimClip { clip: id.clone(), edge: Edge::End, to: None, delta: Some(-dt), ripple: Some(false) }]
                        }
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                };
                v.push(
                    Severity::Error,
                    "clip_past_media_end",
                    format!(
                        "{id} plays source {}–{} s of {} \"{}\", which is {} s long ({} s past the end: those frames would be black/held).",
                        util::num(lo),
                        util::num(hi),
                        a.id,
                        util::file_name(&a.path),
                        util::num(a.info.duration),
                        util::num(excess)
                    ),
                    at,
                    vec![id.clone()],
                    Some("shorten the clip, slip it earlier, or lower its speed.".into()),
                    fix_ops,
                );
            }
            if lo < -tol {
                v.push(
                    Severity::Error,
                    "clip_before_media_start",
                    format!("{id} starts {} s before the beginning of {} (source {} s).", util::num(-lo), a.id, util::num(lo)),
                    at,
                    vec![id.clone()],
                    Some("slip it later so the in-point is 0 or more.".into()),
                    if hi - lo <= a.info.duration + tol { vec![op(json!({"op":"slip_clip","clip":id,"delta":-lo}))] } else { vec![] },
                );
            }
        }
    }
    // Start before 0.
    if let Some(s) = span {
        if s.start < -EPS {
            let fix_ops = vec![op(json!({"op":"nudge","clips":[id],"delta": -s.start}))];
            v.push(
                Severity::Error,
                "negative_start",
                format!("{id} starts at {} s, before the timeline begins.", util::secs(s.start)),
                Some(s.start),
                vec![id.clone()],
                Some(format!("move it {} s later.", util::num(-s.start))),
                fix_ops,
            );
            v.alternatives(vec![vec![op(json!({"op":"move_clip","clip":id,"start":0.0}))], vec![op(json!({"op":"move_clip","clip":id,"start":0.0,"overwrite":true}))]]);
        }
    }
    // Keys (one finding per clip and code, so one fix covers every parameter).
    let mut params: Vec<(String, Param)> = Vec::new();
    crate::ops::prims::for_each_param(c, |path, prm| {
        if prm.is_animated() {
            params.push((path, prm.clone()));
        }
    });
    let mut unsorted: Vec<(String, Vec<Key>)> = Vec::new();
    let mut outside_all: Vec<(String, Vec<f64>, Param)> = Vec::new();
    let mut last_eased: Vec<(String, Vec<Key>)> = Vec::new();
    for (path, prm) in params {
        let Param::Animated { keys } = &prm else { continue };
        if keys.windows(2).any(|w| w[1].t < w[0].t) {
            let mut sorted = keys.clone();
            sorted.sort_by(|a, b| a.t.total_cmp(&b.t));
            unsorted.push((path, sorted));
            continue;
        }
        if let [first, .., last] = keys.as_slice() {
            // An ease on the last key is ignored. When the first key has none, the eases were almost
            // certainly written "into each key" (GSAP style), so the fix moves every ease one key
            // earlier; otherwise only the dead ease is cleared.
            if keys.len() >= 2 && last.ease != Ease::Linear {
                let mut fixed = keys.clone();
                if first.ease == Ease::Linear {
                    for (i, k) in fixed.iter_mut().enumerate() {
                        k.ease = keys.get(i + 1).map(|n| n.ease.clone()).unwrap_or_default();
                    }
                } else if let Some(k) = fixed.last_mut() {
                    k.ease = Ease::Linear;
                }
                last_eased.push((path.clone(), fixed));
            }
        }
        let outside: Vec<f64> = keys.iter().map(|k| k.t).filter(|t| *t < -1e-6 || *t > c.duration + 1e-6).collect();
        if !outside.is_empty() {
            let (left, _) = split_param(&prm, c.duration);
            let (_, inside) = split_param(&left, 0.0);
            outside_all.push((path, outside, inside));
        }
    }
    if !unsorted.is_empty() {
        let paths: Vec<String> = unsorted.iter().map(|(p, _)| p.clone()).collect();
        v.push(
            Severity::Error,
            "keys_unsorted",
            format!("{id}: {} {} keys out of time order, so it animates unpredictably.", paths.join(", "), if paths.len() == 1 { "has" } else { "have" }),
            at,
            vec![id.clone()],
            Some("sort the keys by time.".into()),
            unsorted.into_iter().map(|(path, keys)| Op::SetParam { clip: id.clone(), path, value: None, keys: Some(keys) }).collect(),
        );
    }
    if !last_eased.is_empty() {
        let paths: Vec<String> = last_eased.iter().map(|(p, _)| p.clone()).collect();
        v.push(
            Severity::Warning,
            "ease_on_last_key",
            format!(
                "{id}: the ease on the last key of {} does nothing, so the move before it is linear. A key's ease shapes the segment that starts at that key.",
                paths.join(", ")
            ),
            at,
            vec![id.clone()],
            Some("put each ease on the key where its move starts (fix_ops move every ease one key earlier when the first key has none, else drop the dead one).".into()),
            last_eased.into_iter().map(|(path, keys)| Op::SetParam { clip: id.clone(), path, value: None, keys: Some(keys) }).collect(),
        );
    }
    if !outside_all.is_empty() {
        let looks_absolute = span.is_some_and(|s| s.start > 0.0 && outside_all.iter().all(|(_, o, _)| o.iter().all(|t| *t >= s.start - 1e-6 && *t <= s.end + 1e-6)));
        let hint = if looks_absolute {
            format!(" They look like timeline times: key times are clip-local, so subtract the clip start ({} s).", util::num(span.map(|s| s.start).unwrap_or(0.0)))
        } else {
            String::new()
        };
        let detail: Vec<String> = outside_all.iter().map(|(p, o, _)| format!("{p} at {} s", o.iter().map(|t| util::num(*t)).collect::<Vec<_>>().join(", "))).collect();
        let fix_ops: Vec<Op> = outside_all
            .into_iter()
            .map(|(path, _, inside)| match inside {
                Param::Value(x) => Op::SetParam { clip: id.clone(), path, value: Some(x), keys: None },
                Param::Animated { keys } => Op::SetParam { clip: id.clone(), path, value: None, keys: Some(keys) },
            })
            .collect();
        v.push(
            Severity::Warning,
            "keyframe_outside_clip",
            format!("{id} has keys outside the clip (clip-local 0–{} s): {}.{hint}", util::num(c.duration), detail.join("; ")),
            at,
            vec![id.clone()],
            Some(if looks_absolute { "re-time the keys in clip-local seconds.".into() } else { "move the keys inside the clip or drop them (fix_ops keep the animation's values at the clip edges).".into() }),
            if looks_absolute { vec![] } else { fix_ops },
        );
    }
    // Fades.
    if c.duration > 0.0 && c.fade_in + c.fade_out > c.duration + 1e-9 {
        let k = c.duration / (c.fade_in + c.fade_out);
        v.push(
            Severity::Warning,
            "fade_too_long",
            format!("{id}'s fades ({} s in + {} s out) are longer than the clip ({} s).", util::num(c.fade_in), util::num(c.fade_out), util::num(c.duration)),
            at,
            vec![id.clone()],
            Some("shorten the fades.".into()),
            vec![op(json!({"op":"set_clip","clip":id,"patch":{"fade_in": c.fade_in * k, "fade_out": c.fade_out * k}}))],
        );
    }
    // Text.
    if let Source::Text(t) = &c.source {
        if t.text.trim().is_empty() {
            v.push(Severity::Warning, "empty_text", format!("{id} is a text clip with no text."), at, vec![id.clone()], Some("give it text or delete it.".into()), vec![op(json!({"op":"delete_clip","clip":id}))]);
        }
        if let Some(h) = &t.highlight {
            let n = crate::ops::captions::caption_words(&t.text).len();
            if h.words.len() != n {
                v.push(
                    Severity::Warning,
                    "highlight_mismatch",
                    format!("{id} has {} highlight window(s) but its text has {n} word(s); highlighting will drift.", h.words.len()),
                    at,
                    vec![id.clone()],
                    Some("regenerate the captions (generate_captions) or edit text/highlight to match.".into()),
                    vec![],
                );
            }
        }
        if c.enabled {
            text_safe(v, c, t, at);
        }
    }
    if let Source::Shape(s) = &c.source {
        if c.enabled {
            let (w, h) = (s.width.abs(), s.height.abs());
            box_safe(v, c, w, h, at);
        }
    }
    if let (Source::Media { .. }, Some(a)) = (&c.source, asset) {
        if a.kind != MediaKind::Audio && c.enabled && a.info.width > 0 && a.info.height > 0 {
            if let Param::Value(scale) = c.transform.scale {
                if scale < 0.999 {
                    let (fw, fh) = (p.settings.width as f64, p.settings.height as f64);
                    let (mw, mh) = (a.info.width as f64, a.info.height as f64);
                    let fit = match c.transform.fit {
                        Fit::Contain => (fw / mw).min(fh / mh),
                        Fit::Cover => (fw / mw).max(fh / mh),
                        Fit::Stretch | Fit::None => 1.0,
                    };
                    let (w, h) = if c.transform.fit == Fit::Stretch { (fw, fh) } else { (mw * fit, mh * fit) };
                    box_safe(v, c, w, h, at);
                }
            }
        }
    }
    // Effects.
    for (i, e) in c.effects.iter().enumerate() {
        match effect_spec(&e.kind) {
            None => v.push(
                Severity::Error,
                "unknown_effect",
                format!("{id}: effect {i} has unknown kind {:?}.{}", e.kind, util::did_you_mean(&e.kind, crate::effects::effect_kinds())),
                at,
                vec![id.clone()],
                Some("remove it or change it to a known effect kind.".into()),
                vec![op(json!({"op":"remove_effect","clip":id,"index":i}))],
            ),
            Some(spec) => {
                let mut unknown_params: BTreeMap<String, Option<Param>> = BTreeMap::new();
                let mut names = Vec::new();
                for (n, prm) in &e.params {
                    match spec.param(n) {
                        None => {
                            unknown_params.insert(n.clone(), None);
                            names.push(n.clone());
                        }
                        Some(ps) => {
                            if let Param::Value(x) = prm {
                                if x.is_finite() && (*x < ps.min || *x > ps.max) {
                                    let clamped = x.clamp(ps.min, ps.max);
                                    let mut set = BTreeMap::new();
                                    set.insert(n.clone(), Some(Param::Value(clamped)));
                                    v.push(
                                        Severity::Warning,
                                        "effect_param_out_of_range",
                                        format!("{id}: {}.{n} = {} is outside {}–{}.", e.kind, util::num(*x), util::num(ps.min), util::num(ps.max)),
                                        at,
                                        vec![id.clone()],
                                        Some(format!("set it within range (e.g. {}).", util::num(clamped))),
                                        vec![Op::SetEffect { clip: id.clone(), index: i, enabled: None, params: set, colors: BTreeMap::new(), strings: BTreeMap::new() }],
                                    );
                                }
                            }
                        }
                    }
                }
                let unknown_colors: BTreeMap<String, Option<Color>> = e.colors.keys().filter(|n| spec.color_default(n).is_none()).map(|n| (n.clone(), None)).collect();
                let unknown_strings: BTreeMap<String, Option<String>> = e.strings.keys().filter(|n| !spec.strings.contains(&n.as_str())).map(|n| (n.clone(), None)).collect();
                names.extend(unknown_colors.keys().cloned());
                names.extend(unknown_strings.keys().cloned());
                if !names.is_empty() {
                    v.push(
                        Severity::Warning,
                        "unknown_effect_param",
                        format!(
                            "{id}: effect {i} ({}) has unknown setting(s) {} (it has: {}).",
                            e.kind,
                            names.join(", "),
                            spec.params.iter().map(|p| p.name).chain(spec.colors.iter().map(|(n, _)| *n)).chain(spec.strings.iter().copied()).collect::<Vec<_>>().join(", ")
                        ),
                        at,
                        vec![id.clone()],
                        Some("remove them (they are ignored) or rename them to real parameters.".into()),
                        vec![Op::SetEffect { clip: id.clone(), index: i, enabled: None, params: unknown_params, colors: unknown_colors, strings: unknown_strings }],
                    );
                }
            }
        }
    }
}

/// The first unlocked track of `kind` where the clip's current span is free.
fn free_track(v: &V, c: &Clip, kind: TrackKind) -> Option<Id> {
    let span = v.r.span(&c.id)?;
    v.p.tracks
        .iter()
        .filter(|t| t.kind == kind && !t.locked && t.id != c.track)
        .find(|t| !v.p.clips.iter().any(|o| o.id != c.id && o.track == t.id && v.r.span(&o.id).is_some_and(|s| s.overlaps(&span))))
        .map(|t| t.id.clone())
}

fn wanted_kind(c: &Clip, asset: Option<&MediaAsset>) -> TrackKind {
    match (&c.source, asset) {
        (Source::Media { .. }, Some(a)) if a.kind == MediaKind::Audio => TrackKind::Audio,
        _ => TrackKind::Video,
    }
}

/// Estimated text box (width, height) in px before transform scale.
fn text_box(t: &TextSource) -> (f64, f64) {
    let char_w = t.size * if t.weight >= 700 { 0.58 } else { 0.53 } + t.letter_spacing;
    let pad = t.background.as_ref().map(|b| b.padding).unwrap_or(0.0) + t.stroke.as_ref().map(|s| s.width).unwrap_or(0.0);
    let mut lines = 0usize;
    let mut widest = 0f64;
    for para in t.text.split('\n') {
        let w = para.chars().count() as f64 * char_w;
        match t.max_width {
            Some(mw) if mw > 0.0 && w > mw => {
                let n = (w / mw).ceil() as usize;
                lines += n;
                widest = widest.max(mw);
            }
            _ => {
                lines += 1;
                widest = widest.max(w);
            }
        }
    }
    (widest + 2.0 * pad, lines.max(1) as f64 * t.size * t.line_height + 2.0 * pad)
}

fn text_safe(v: &mut V, c: &Clip, t: &TextSource, at: Option<f64>) {
    let (w, h) = text_box(t);
    box_check(v, c, w, h, at, v.p.settings.title_safe, "text_outside_title_safe", Severity::Warning, "title-safe");
}

fn box_safe(v: &mut V, c: &Clip, w: f64, h: f64, at: Option<f64>) {
    box_check(v, c, w, h, at, v.p.settings.action_safe, "content_outside_action_safe", Severity::Info, "action-safe");
}

#[allow(clippy::too_many_arguments)]
fn box_check(v: &mut V, c: &Clip, w: f64, h: f64, at: Option<f64>, safe: f64, code: &'static str, sev: Severity, area: &str) {
    let (fw, fh) = (v.p.settings.width as f64, v.p.settings.height as f64);
    let (sx0, sx1) = (fw * (1.0 - safe) / 2.0, fw * (1.0 + safe) / 2.0);
    let (sy0, sy1) = (fh * (1.0 - safe) / 2.0, fh * (1.0 + safe) / 2.0);
    // Check at the start and at every transform key.
    let mut times = vec![0.0];
    for prm in [&c.transform.x, &c.transform.y, &c.transform.scale] {
        if let Param::Animated { keys } = prm {
            times.extend(keys.iter().map(|k| k.t).filter(|t| *t >= 0.0 && *t <= c.duration));
        }
    }
    let mut worst: Option<(f64, f64, f64, f64)> = None; // (t, overflow, cx, cy)
    for t in times {
        let s = sample(&c.transform.scale, t).abs();
        let (bw, bh) = (w * s, h * s);
        let cx = fw / 2.0 + sample(&c.transform.x, t);
        let cy = fh / 2.0 + sample(&c.transform.y, t);
        let over = (sx0 - (cx - bw / 2.0)).max((cx + bw / 2.0) - sx1).max(sy0 - (cy - bh / 2.0)).max((cy + bh / 2.0) - sy1);
        if over > 2.0 && worst.is_none_or(|w| over > w.1) {
            worst = Some((t, over, cx, cy));
        }
    }
    let Some((t, over, _, _)) = worst else { return };
    let id = c.id.clone();
    let s0 = sample(&c.transform.scale, 0.0).abs();
    let (bw, bh) = (w * s0, h * s0);
    let fits = bw <= sx1 - sx0 && bh <= sy1 - sy0;
    let mut fix_ops = Vec::new();
    if fits {
        if let (Param::Value(x), Param::Value(y), Param::Value(_)) = (&c.transform.x, &c.transform.y, &c.transform.scale) {
            let cx = (fw / 2.0 + x).clamp(sx0 + bw / 2.0, sx1 - bw / 2.0) - fw / 2.0;
            let cy = (fh / 2.0 + y).clamp(sy0 + bh / 2.0, sy1 - bh / 2.0) - fh / 2.0;
            if (cx - x).abs() > 0.5 {
                fix_ops.push(Op::SetParam { clip: id.clone(), path: "transform.x".into(), value: Some(cx.round()), keys: None });
            }
            if (cy - y).abs() > 0.5 {
                fix_ops.push(Op::SetParam { clip: id.clone(), path: "transform.y".into(), value: Some(cy.round()), keys: None });
            }
        }
    }
    v.push(
        sev,
        code,
        format!(
            "{id} (≈{}×{} px) reaches about {} px outside the {area} area{}.",
            bw.round(),
            bh.round(),
            over.round(),
            if t > 0.0 { format!(" at {} s into the clip", util::num(t)) } else { String::new() }
        ),
        at.map(|a| a + t),
        vec![id],
        Some(if fits { format!("move it inside the {area} area.") } else { format!("make it smaller (scale, size or wrap width) so it fits the {area} area.") }),
        fix_ops,
    );
}

type OverlapPair = ((Id, Id), (Id, Span, Span));

/// 90% of `shortest`, rounded down to the millisecond (never 0).
fn shorter_duration(shortest: f64) -> f64 {
    let d = (shortest * 0.9 * 1000.0).floor() / 1000.0;
    if d >= 0.001 {
        d
    } else {
        shortest * 0.9
    }
}

fn overlap_checks(v: &mut V) {
    let mut pairs: Vec<OverlapPair> = overlaps(v.p, v.r).into_iter().collect();
    pairs.sort_by(|a, b| a.1 .1.start.min(a.1 .2.start).total_cmp(&b.1 .1.start.min(b.1 .2.start)).then(a.0.cmp(&b.0)));
    for ((a, b), (track, sa, sb)) in pairs {
        let ((first, sf), (second, ss)) = if sa.start <= sb.start { ((a, sa), (b, sb)) } else { ((b, sb), (a, sa)) };
        let fix_ops = if ss.start > sf.start + 1e-3 {
            vec![Op::TrimClip { clip: first.clone(), edge: Edge::End, to: Some(ss.start), delta: None, ripple: Some(false) }]
        } else {
            vec![op(json!({"op":"move_clip","clip":second,"start":{"after":first}}))]
        };
        v.push(
            Severity::Error,
            "overlap_same_track",
            format!(
                "{first} [{}–{}] and {second} [{}–{}] overlap on track {track} by {} s.",
                util::secs(sf.start),
                util::secs(sf.end),
                util::secs(ss.start),
                util::secs(ss.end),
                util::num(sf.end.min(ss.end) - ss.start)
            ),
            Some(ss.start),
            vec![first.clone(), second.clone()],
            Some(if ss.start > sf.start + 1e-3 { format!("trim {first} to end where {second} starts, or move one of them.") } else { format!("move {second} after {first} or onto another track.") }),
            fix_ops,
        );
        v.alternatives(vec![
            vec![op(json!({"op":"move_clip","clip":second,"start":{"after":first}}))],
            vec![Op::TrimClip { clip: second.clone(), edge: Edge::Start, to: Some(sf.end.min(ss.end - 0.001)), delta: None, ripple: Some(false) }],
            vec![op(json!({"op":"move_clip","clip":second,"start":ss.start,"overwrite":true}))],
        ]);
    }
}

fn main_tracks(p: &Project) -> Vec<&Track> {
    let mains: Vec<&Track> = p.tracks.iter().filter(|t| t.role == "main" && t.kind == TrackKind::Video).collect();
    if !mains.is_empty() {
        return mains;
    }
    p.tracks.iter().filter(|t| t.kind == TrackKind::Video).find(|t| p.clips.iter().any(|c| c.track == t.id)).into_iter().collect()
}

fn gap_checks(v: &mut V) {
    let p = v.p;
    let half_frame = 0.5 / p.settings.fps.max(1.0);
    // Opaque full-frame coverage on other video tracks (media/solid) hides a gap.
    let covers: Vec<(String, Span)> = p
        .clips
        .iter()
        .filter(|c| c.enabled && matches!(c.source, Source::Media { .. } | Source::Solid { .. }))
        .filter(|c| p.tracks.iter().any(|t| t.id == c.track && t.kind == TrackKind::Video && !t.hidden))
        .filter_map(|c| Some((c.track.clone(), v.r.span(&c.id)?)))
        .collect();
    for track in main_tracks(p) {
        let on = v.r.on_track(p, &track.id);
        if on.is_empty() {
            continue;
        }
        let mut gaps: Vec<(f64, f64, Option<&Clip>, Option<&Clip>)> = Vec::new();
        let mut end = 0.0f64;
        let mut prev: Option<&Clip> = None;
        for (c, s) in &on {
            if s.start > end + half_frame {
                gaps.push((end, s.start, prev, Some(*c)));
            }
            if s.end >= end {
                end = s.end;
                prev = Some(*c);
            }
        }
        if let Some(d) = p.settings.duration {
            if d > end + half_frame {
                gaps.push((end, d, prev, None));
            }
        }
        for (g0, g1, before, after) in gaps {
            let covered = covers.iter().any(|(t, s)| t != &track.id && s.start <= g0 + EPS && s.end >= g1 - EPS);
            if covered {
                continue;
            }
            let mut ids = Vec::new();
            let mut fix_ops = Vec::new();
            let fix = match (before, after) {
                (Some(b), Some(a)) => {
                    ids.push(b.id.clone());
                    ids.push(a.id.clone());
                    let chainable = matches!(a.start, Time::At(_)) || matches!(&a.start, Time::Anchored(an) if matches!(an.to, AnchorTarget::After(_)));
                    if chainable {
                        fix_ops.push(op(json!({"op":"move_clip","clip":a.id,"start":{"after":b.id}})));
                    } else {
                        fix_ops.push(op(json!({"op":"nudge","clips":[a.id],"delta": -(g1 - g0)})));
                    }
                    format!("close it: start {} right after {} (magnetic), or fill it with a clip.", a.id, b.id)
                }
                (None, Some(a)) => {
                    ids.push(a.id.clone());
                    fix_ops.push(op(json!({"op":"nudge","clips":[a.id],"delta": -(g1 - g0)})));
                    format!("start {} at 0 s, or put something before it.", a.id)
                }
                (Some(b), None) => {
                    ids.push(b.id.clone());
                    fix_ops.push(op(json!({"op":"set_settings","duration": end})));
                    format!("extend the edit to the fixed duration ({} s) or shorten the project to end with the last clip.", util::num(g1))
                }
                (None, None) => continue,
            };
            let alt: Vec<Vec<Op>> = match after {
                Some(a) => vec![vec![op(json!({"op":"nudge","clips":[a.id],"delta": -(g1 - g0)}))], vec![op(json!({"op":"extract_range","from":g0,"to":g1,"tracks":[track.id]}))]],
                None => vec![],
            };
            v.push(
                Severity::Warning,
                "gap_in_main",
                format!("{} s gap on the main track {} at {}–{} s shows black frames.", util::num(g1 - g0), track.id, util::secs(g0), util::secs(g1)),
                Some(g0),
                ids,
                Some(fix),
                fix_ops,
            );
            v.alternatives(alt);
        }
    }
}

fn transition_checks(v: &mut V) {
    let p = v.p;
    let clips: HashMap<&str, &Clip> = p.clips.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut seen: HashMap<(&str, &str), &str> = HashMap::new();
    for t in &p.transitions {
        let id = t.id.clone();
        let remove = vec![op(json!({"op":"remove_transition","transition":id}))];
        if let Some(first) = seen.get(&(t.from.as_str(), t.to.as_str())) {
            v.push(
                Severity::Error,
                "duplicate_transition",
                format!("{id} and {first} are both on the cut {}→{}.", t.from, t.to),
                v.r.span(&t.to).map(|s| s.start),
                vec![id.clone(), first.to_string()],
                Some(format!("remove {id}.")),
                remove.clone(),
            );
            continue;
        }
        seen.insert((t.from.as_str(), t.to.as_str()), t.id.as_str());
        let spec = transition_spec(&t.kind);
        if spec.is_none() {
            v.push(
                Severity::Error,
                "unknown_transition",
                format!("{id} has unknown kind {:?}.{}", t.kind, util::did_you_mean(&t.kind, crate::effects::transition_kinds())),
                v.r.span(&t.to).map(|s| s.start),
                vec![id.clone()],
                Some("change it to a known kind such as crossfade.".into()),
                vec![op(json!({"op":"set_transition","transition":id,"kind":"crossfade"}))],
            );
        }
        let (Some(a), Some(b)) = (clips.get(t.from.as_str()), clips.get(t.to.as_str())) else {
            v.push(
                Severity::Error,
                "transition_not_adjacent",
                format!("{id} joins {}→{}, but {} does not exist.", t.from, t.to, if clips.contains_key(t.from.as_str()) { &t.to } else { &t.from }),
                None,
                vec![id.clone()],
                Some("remove the transition.".into()),
                remove,
            );
            continue;
        };
        let (sa, sb) = (v.r.span(&a.id), v.r.span(&b.id));
        let touching = a.track == b.track && matches!((sa, sb), (Some(x), Some(y)) if (x.end - y.start).abs() <= 1e-3);
        if !touching {
            let why = if a.track != b.track {
                format!("{} is on {} and {} on {}", a.id, a.track, b.id, b.track)
            } else {
                match (sa, sb) {
                    (Some(x), Some(y)) => format!("{} ends at {} s but {} starts at {} s", a.id, util::secs(x.end), b.id, util::secs(y.start)),
                    _ => "one of them is unresolved".into(),
                }
            };
            v.push(
                Severity::Error,
                "transition_not_adjacent",
                format!("{id} ({}) needs touching clips on one track, but {why}.", t.kind),
                sb.map(|s| s.start),
                vec![id.clone(), a.id.clone(), b.id.clone()],
                Some(format!("move {} to start right after {} (magnetic) or remove the transition.", b.id, a.id)),
                remove,
            );
            continue;
        }
        let (sa, sb) = (sa.expect("touching"), sb.expect("touching"));
        let cut = sb.start;
        let shortest = sa.duration().min(sb.duration());
        if t.duration > shortest + 1e-9 {
            v.push(
                Severity::Warning,
                "transition_too_long",
                format!("{id} lasts {} s but the shorter of {} and {} is {} s.", util::num(t.duration), a.id, b.id, util::num(shortest)),
                Some(cut),
                vec![id.clone()],
                Some("shorten the transition.".into()),
                vec![Op::SetTransition { transition: id.clone(), kind: None, duration: Some(shorter_duration(shortest)), params: BTreeMap::new() }],
            );
            continue;
        }
        // Media handles: half the transition reaches past each clip's edge.
        let half = t.duration / 2.0;
        let mut held = Vec::new();
        if let (Some((_, hi)), Some(m)) = (source_span(a), crate::ops::prims::media_of(p, a)) {
            let need = hi + half * sample(&a.speed, a.duration).abs();
            if m.kind == MediaKind::Video && m.info.duration > 0.0 && !a.reverse && need > m.info.duration + 1e-3 {
                held.push(format!("{} has {} s of media after its out-point (needs {})", a.id, util::num((m.info.duration - hi).max(0.0)), util::num(half)));
            }
        }
        if let (Some((lo, _)), Some(m)) = (source_span(b), crate::ops::prims::media_of(p, b)) {
            let need = lo - half * sample(&b.speed, 0.0).abs();
            if m.kind == MediaKind::Video && !b.reverse && need < -1e-3 {
                held.push(format!("{} has {} s of media before its in-point (needs {})", b.id, util::num(lo.max(0.0)), util::num(half)));
            }
        }
        if !held.is_empty() {
            v.push(
                Severity::Warning,
                "transition_too_long",
                format!("{id}: {}; edge frames will be held (frozen) during part of it.", held.join("; ")),
                Some(cut),
                vec![id.clone()],
                Some("shorten the transition, or slip/trim the clips to leave media handles.".into()),
                vec![],
            );
        }
    }
}

fn ducking_checks(v: &mut V, tracks: &HashMap<&str, &Track>) {
    for d in &v.p.ducking {
        let missing: Vec<&str> = [d.target.as_str(), d.trigger.as_str()].into_iter().filter(|t| !tracks.contains_key(t)).collect();
        let selfish = d.target == d.trigger;
        if missing.is_empty() && !selfish {
            continue;
        }
        let msg = if selfish {
            format!("ducking {} lowers track {} by itself.", d.id, d.target)
        } else {
            format!("ducking {} names track(s) {} that do not exist.", d.id, missing.join(", "))
        };
        v.push(
            Severity::Error,
            "ducking_track_missing",
            msg,
            None,
            vec![d.id.clone()],
            Some("point it at existing tracks (set_ducking) or remove it.".into()),
            vec![op(json!({"op":"remove_ducking","ducking":d.id}))],
        );
    }
}

fn track_checks(v: &mut V) {
    let p = v.p;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for c in &p.clips {
        *counts.entry(c.track.as_str()).or_default() += 1;
    }
    for t in &p.tracks {
        if counts.get(t.id.as_str()).copied().unwrap_or(0) == 0 {
            v.push(
                Severity::Info,
                "empty_track",
                format!("track {} has no clips.", t.id),
                None,
                vec![t.id.clone()],
                Some("remove it if it is not needed.".into()),
                if t.locked { vec![] } else { vec![op(json!({"op":"remove_track","track":t.id}))] },
            );
        }
        if t.solo {
            v.push(
                Severity::Info,
                "solo_active",
                format!("track {} is soloed, so other tracks are silent.", t.id),
                None,
                vec![t.id.clone()],
                Some("un-solo it before export.".into()),
                vec![op(json!({"op":"set_track","track":t.id,"solo":false}))],
            );
        }
    }
}

fn duration_checks(v: &mut V) {
    let Some(d) = v.p.settings.duration else { return };
    for c in &v.p.clips {
        let Some(s) = v.r.span(&c.id) else { continue };
        if s.end > d + 1e-6 {
            let fix_ops = if s.start < d - 1e-3 {
                vec![Op::TrimClip { clip: c.id.clone(), edge: Edge::End, to: Some(d), delta: None, ripple: Some(false) }]
            } else {
                vec![op(json!({"op":"delete_clip","clip":c.id}))]
            };
            v.push(
                Severity::Warning,
                "past_fixed_duration",
                format!("{} ends at {} s, after the project's fixed duration of {} s, so its end is cut off.", c.id, util::secs(s.end), util::num(d)),
                Some(d),
                vec![c.id.clone()],
                Some("trim it to the duration, or lengthen the project (set_settings duration).".into()),
                fix_ops,
            );
        }
    }
}

/// Word anchors whose stored word text no longer matches the transcript.
fn word_text_checks(v: &mut V) {
    let p = v.p;
    let words_of = |clip: &str| -> Option<&Vec<Word>> {
        let c = p.clips.iter().find(|c| c.id == clip)?;
        let Source::Media { asset, .. } = &c.source else { return None };
        Some(&p.transcripts.get(asset)?.words)
    };
    let norm = crate::ops::captions::norm_word;
    // (owner id, what, the anchor, how to re-anchor)
    let mut items: Vec<(Id, &'static str, Anchor)> = Vec::new();
    for c in &p.clips {
        if let Time::Anchored(a) = &c.start {
            items.push((c.id.clone(), "start", a.clone()));
        }
        if let Some(Time::Anchored(a)) = &c.until {
            items.push((c.id.clone(), "until", a.clone()));
        }
    }
    for m in &p.markers {
        if let Time::Anchored(a) = &m.time {
            items.push((m.id.clone(), "marker", a.clone()));
        }
    }
    for (id, what, a) in items {
        let AnchorTarget::Word(w) = &a.to else { continue };
        let Some(want) = &w.text else { continue };
        let Some(words) = words_of(&w.clip) else { continue };
        let Some(now) = words.get(w.index) else { continue };
        if norm(&now.text) == norm(want) {
            continue;
        }
        let near = (w.index.saturating_sub(8)..(w.index + 9).min(words.len())).filter(|j| norm(&words[*j].text) == norm(want)).min_by_key(|j| j.abs_diff(w.index));
        let fix_ops = match near {
            Some(j) => {
                let mut w2 = w.clone();
                w2.index = j;
                w2.text = Some(words[j].text.trim().to_string());
                let t = Time::Anchored(Anchor { to: AnchorTarget::Word(w2), offset: a.offset });
                match what {
                    "start" => vec![op(json!({"op":"move_clip","clip":id,"start":t}))],
                    "until" => vec![op(json!({"op":"set_until","clip":id,"until":t}))],
                    _ => vec![op(json!({"op":"set_marker","marker":id,"time":t}))],
                }
            }
            None => vec![],
        };
        let at = v.r.span(&id).map(|s| s.start).or_else(|| v.r.marker(&id));
        v.push(
            Severity::Warning,
            "word_anchor_text_mismatch",
            format!(
                "{id}{} is anchored to word {} of {}, made on {want:?}, but that word is now {:?}.{}",
                if what == "until" { " (its end)" } else { "" },
                w.index,
                w.clip,
                now.text.trim(),
                match near {
                    Some(j) => format!(" {want:?} is now word {j}."),
                    None => String::new(),
                }
            ),
            at,
            vec![id.clone()],
            Some(if near.is_some() { "re-anchor it to the word it was made on (fix_ops)".into() } else { "re-anchor it to the word you mean (move_clip), or keep it and accept the new word".into() }),
            fix_ops,
        );
    }
}

/// Track mattes: the matte clip must exist, be a picture, not loop, and play while the clip plays.
fn matte_checks(v: &mut V) {
    let p = v.p;
    for c in &p.clips {
        let Some(m) = &c.matte else { continue };
        let at = v.r.span(&c.id).map(|s| s.start);
        let clear = vec![op(json!({"op":"set_clip","clip":c.id,"patch":{"matte":null}}))];
        let Some(src) = p.clips.iter().find(|x| x.id == m.clip) else {
            v.push(
                Severity::Error,
                "matte_unknown",
                format!("{} uses {} as its matte, but there is no clip {}.{}", c.id, m.clip, m.clip, util::did_you_mean(&m.clip, p.clips.iter().map(|x| x.id.as_str()))),
                at,
                vec![c.id.clone()],
                Some("name an existing visual clip as the matte, or remove the matte.".into()),
                clear,
            );
            continue;
        };
        if p.tracks.iter().any(|t| t.id == src.track && t.kind == TrackKind::Audio) {
            v.push(
                Severity::Error,
                "matte_not_visual",
                format!("{}'s matte {} is on audio track {}; a matte must be a picture.", c.id, src.id, src.track),
                at,
                vec![c.id.clone(), src.id.clone()],
                Some("use a clip on a video track (text, shape, footage) as the matte, or remove the matte.".into()),
                clear,
            );
            continue;
        }
        if let Some(problem) = crate::ops::prims::matte_problem(p, c) {
            v.push(Severity::Error, "matte_cycle", problem, at, vec![c.id.clone(), src.id.clone()], Some("remove one of the mattes.".into()), clear);
            continue;
        }
        if let (Some(a), Some(b)) = (v.r.span(&c.id), v.r.span(&src.id)) {
            if a.end.min(b.end) - a.start.max(b.start) <= 1e-9 {
                v.push(
                    Severity::Warning,
                    "matte_not_overlapping",
                    format!(
                        "{} ({}–{} s) is drawn through {} ({}–{} s), which never plays at the same time, so {} is invisible.",
                        c.id,
                        util::tsecs(a.start),
                        util::tsecs(a.end),
                        src.id,
                        util::tsecs(b.start),
                        util::tsecs(b.end),
                        c.id
                    ),
                    Some(a.start),
                    vec![c.id.clone(), src.id.clone()],
                    Some(format!("move {} to play with {} (e.g. start {{\"with\":\"{}\"}}), or remove the matte.", src.id, c.id, c.id)),
                    vec![op(json!({"op":"move_clip","clip":src.id,"start":{"with":c.id}}))],
                );
                v.alternatives(vec![clear]);
            }
        }
    }
}

/// Motion blur ranges and transition colours.
fn look_checks(v: &mut V) {
    let p = v.p;
    for c in &p.clips {
        let Some(m) = &c.motion_blur else { continue };
        if m.shutter >= 0.0 && m.shutter <= 720.0 && (2..=64).contains(&m.samples) {
            continue;
        }
        let shutter = if m.shutter.is_finite() { m.shutter.clamp(0.0, 720.0) } else { 180.0 };
        v.push(
            Severity::Warning,
            "motion_blur_out_of_range",
            format!("{}: motion blur shutter {}° / {} samples is outside 0–720° / 2–64.", c.id, util::num(m.shutter), m.samples),
            v.r.span(&c.id).map(|s| s.start),
            vec![c.id.clone()],
            Some("set shutter 0–720 and samples 2–64.".into()),
            vec![op(json!({"op":"set_clip","clip":c.id,"patch":{"motion_blur":{"shutter":shutter,"samples":m.samples.clamp(2, 64)}}}))],
        );
    }
    for t in &p.transitions {
        let Some(spec) = transition_spec(&t.kind) else { continue };
        let bad: BTreeMap<String, Option<Color>> = t.colors.keys().filter(|n| spec.color_default(n).is_none()).map(|n| (n.clone(), None)).collect();
        if bad.is_empty() {
            continue;
        }
        v.push(
            Severity::Warning,
            "unknown_transition_color",
            format!(
                "{}: {} has no colour {} (it has: {}).",
                t.id,
                t.kind,
                bad.keys().cloned().collect::<Vec<_>>().join(", "),
                if spec.colors.is_empty() { "none".to_string() } else { spec.colors.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ") }
            ),
            v.r.span(&t.to).map(|s| s.start),
            vec![t.id.clone()],
            Some("remove those colours.".into()),
            vec![Op::SetTransitionColors { transition: t.id.clone(), colors: bad }],
        );
    }
}

/// Overlays anchored `with` a clip that outlast it.
fn overlay_checks(v: &mut V) {
    let p = v.p;
    for c in &p.clips {
        if c.until.is_some() {
            continue;
        }
        let Time::Anchored(Anchor { to: AnchorTarget::With(base), offset }) = &c.start else { continue };
        let Some(b) = p.clips.iter().find(|x| &x.id == base) else { continue };
        if crate::ops::linked_clips(p, base).contains(&c.id) {
            continue;
        }
        // Only pictures laid over pictures (badges, titles, callouts on a shot). Sound anchored to a
        // shot (voice-over, music bed) and pictures anchored to sound routinely run longer on purpose.
        let on_video = |track: &str| p.tracks.iter().any(|t| t.id == track && t.kind == TrackKind::Video);
        if !on_video(&c.track) || !on_video(&b.track) {
            continue;
        }
        let (Some(sc), Some(sb)) = (v.r.span(&c.id), v.r.span(base)) else { continue };
        let fps = p.settings.fps.max(1.0);
        if sc.end <= sb.end + 0.5 / fps {
            continue;
        }
        let _ = (b, offset);
        v.push(
            Severity::Warning,
            "overlay_outlives_base",
            format!(
                "{} starts with {base} but runs {} s past its end ({} vs {} s), e.g. after {base} got shorter.",
                c.id,
                util::num(sc.end - sb.end),
                util::tsecs(sc.end),
                util::tsecs(sb.end)
            ),
            Some(sb.end),
            vec![c.id.clone()],
            Some(format!("end it with {base}: set_until {{\"after\":\"{base}\"}} (it then follows {base}'s length), or shorten it.")),
            vec![op(json!({"op":"set_until","clip":c.id,"until":{"after":base}}))],
        );
    }
}

fn export_checks(v: &mut V) {
    let e = &v.p.export;
    let s = &v.p.settings;
    if (e.width.is_none() && e.height.is_none()) || e.fit != Fit::Contain || s.width == 0 || s.height == 0 {
        return;
    }
    let (ow, oh) = e.output_size(s);
    let (pw, ph) = (s.width as f64, s.height as f64);
    let (pa, oa) = (pw / ph, ow as f64 / oh.max(1) as f64);
    if (pa - oa).abs() / pa <= 0.005 {
        return;
    }
    let k = (ow as f64 / pw).min(oh as f64 / ph);
    let empty = (1.0 - (pw * ph * k * k) / (ow as f64 * oh as f64)).max(0.0);
    v.push(
        Severity::Warning,
        "export_letterbox",
        format!(
            "exports are {ow}×{oh} but the project is {}×{}; with fit contain the picture gets {} bars ({}% of the output is empty).",
            s.width,
            s.height,
            if oa > pa { "pillarbox (left/right)" } else { "letterbox (top/bottom)" },
            (empty * 100.0).round()
        ),
        None,
        vec![],
        Some(format!("use fit cover to fill the frame by cropping, or set the project to {ow}×{oh} (set_settings) and re-frame the shots.")),
        vec![Op::SetExportSize { width: None, height: None, fit: Some(Fit::Cover) }],
    );
}

fn marker_checks(v: &mut V) {
    let mut chapters: Vec<(f64, &Marker)> = v.p.markers.iter().filter(|m| m.kind == MarkerKind::Chapter).filter_map(|m| Some((v.r.marker(&m.id)?, m))).collect();
    if chapters.is_empty() {
        return;
    }
    chapters.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut problems = Vec::new();
    let mut fix_ops = Vec::new();
    if chapters[0].0 > 0.5 {
        problems.push(format!("the first chapter starts at {} s, not 0:00", util::secs(chapters[0].0)));
        if chapters[0].0 < 10.0 {
            fix_ops.push(op(json!({"op":"set_marker","marker":chapters[0].1.id,"time":0.0})));
        } else {
            fix_ops.push(op(json!({"op":"add_marker","time":0.0,"name":"Intro","kind":"chapter"})));
        }
    }
    let count = chapters.len() + usize::from(chapters[0].0 > 0.5);
    if count < 3 {
        problems.push(format!("there are {count} chapter(s); YouTube needs at least 3"));
    }
    for w in chapters.windows(2) {
        if w[1].0 - w[0].0 < 10.0 {
            problems.push(format!("{} and {} are only {} s apart (minimum 10 s)", w[0].1.id, w[1].1.id, util::num(w[1].0 - w[0].0)));
        }
    }
    if problems.is_empty() {
        return;
    }
    let ids = chapters.iter().map(|(_, m)| m.id.clone()).collect();
    v.push(
        Severity::Warning,
        "chapter_markers",
        format!("YouTube will ignore these chapters: {}.", problems.join("; ")),
        Some(chapters[0].0),
        ids,
        Some("start a chapter at 0:00, keep chapters 10 s or more apart, and have at least 3.".into()),
        fix_ops,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::apply_batch;
    use crate::resolve::resolve;
    use crate::testutil::*;

    fn codes(p: &Project) -> Vec<&'static str> {
        let r = resolve(p);
        validate(p, &r).into_iter().map(|f| f.code).collect()
    }

    fn find(p: &Project, code: &str) -> Finding {
        let r = resolve(p);
        validate(p, &r).into_iter().find(|f| f.code == code).unwrap_or_else(|| panic!("no {code} in {:?}", codes(p)))
    }

    /// Applying a finding's fix_ops must remove that finding.
    fn fixes(p: &Project, code: &str) -> Project {
        let f = find(p, code);
        assert!(!f.fix_ops.is_empty(), "{code} has no fix_ops");
        assert!(f.fix.is_some());
        let mut q = p.clone();
        apply_batch(&mut q, &f.fix_ops).unwrap_or_else(|e| panic!("fix for {code} failed: {e}"));
        let r = resolve(&q);
        let left: Vec<Finding> = validate(&q, &r).into_iter().filter(|g| g.code == code && g.ids == f.ids).collect();
        assert!(left.is_empty(), "{code} still reported after its fix: {left:?}");
        q
    }

    fn clean() -> Project {
        let mut p = project();
        p.clips.push(media_clip("c1", "v1", "mv", Time::At(0.0), 4.0, 0.0));
        p.clips.push(media_clip("c2", "v1", "mv", after("c1", 0.0), 4.0, 10.0));
        p.clips.push(media_clip("c3", "a1", "m1", Time::At(0.0), 8.0, 0.0));
        p.clips.push(text_clip("t1", "v2", with("c2", 0.0), 2.0));
        p.clips.push(media_clip("c4", "a2", "m1", Time::At(0.0), 8.0, 10.0));
        p
    }

    #[test]
    fn clean_project_has_no_errors_or_warnings() {
        let p = clean();
        let r = resolve(&p);
        let f = validate(&p, &r);
        assert!(f.iter().all(|f| f.severity == Severity::Info), "{f:#?}");
    }

    #[test]
    fn resolve_errors_are_grouped_with_their_dependants() {
        let mut p = clean();
        p.clips[1].start = after("nope", 0.0);
        let f = find(&p, "unresolved_anchor");
        assert_eq!(f.ids, vec!["c2", "t1"], "t1 is blocked by c2");
        assert!(f.message.contains("cannot be placed either"));
        p.clips[0].start = after("c2", 0.0);
        p.clips[1].start = after("c1", 0.0);
        let f = find(&p, "anchor_cycle");
        assert!(f.message.contains("c1 → c2 → c1") || f.message.contains("c2 → c1 → c2"));
        let mut p = project_with_voice();
        p.clips.push(media_clip("v", "a1", "m1", Time::At(0.0), 3.0, 10.0));
        p.clips.push(text_clip("w", "v2", word("v", 2, false, 0.0), 1.0));
        assert!(codes(&p).contains(&"word_cut_out"));
    }

    #[test]
    fn structural_codes_and_fixes() {
        let mut p = clean();
        p.clips.push(media_clip("x1", "v1", "mv", Time::At(2.0), 1.0, 0.0));
        fixes(&p, "overlap_same_track");

        let mut p = clean();
        p.clips[1].start = after("c1", 1.5);
        let q = fixes(&p, "gap_in_main");
        assert_eq!(q.clips[1].start, after("c1", 0.0));

        let mut p = clean();
        p.clips[1].duration = 115.0; // source 10..125 of a 120 s file
        let q = fixes(&p, "clip_past_media_end");
        assert!((q.clips[1].duration - 110.0).abs() < 1e-6);

        let mut p = clean();
        p.clips[0].source = Source::Media { asset: "mv".into(), in_point: -2.0, stream: 0 };
        fixes(&p, "clip_before_media_start");

        let mut p = clean();
        p.clips[0].source = Source::Media { asset: "zz".into(), in_point: 0.0, stream: 0 };
        assert!(codes(&p).contains(&"missing_media"));

        let mut p = clean();
        p.clips[0].track = "v9".into();
        fixes(&p, "unknown_track");

        let mut p = clean();
        p.clips[3].track = "a2".into(); // text on an audio track
        p.clips[4].track = "a1".into();
        p.clips[4].start = Time::At(20.0);
        fixes(&p, "track_kind_mismatch");

        let mut p = clean();
        p.clips[3].duration = 0.0;
        fixes(&p, "zero_length_clip");

        let mut p = clean();
        p.clips[0].speed = Param::Value(-1.0);
        fixes(&p, "speed_out_of_range");

        let mut p = clean();
        p.clips[3].start = with("c2", -9.0);
        fixes(&p, "negative_start");
    }

    #[test]
    fn keyframe_codes() {
        let mut p = clean();
        p.clips[3].opacity = keys(&[(-1.0, 0.0), (1.0, 1.0), (5.0, 0.5)]);
        let q = fixes(&p, "keyframe_outside_clip");
        assert!(crate::eval::sample(&q.clips[3].opacity, 0.0) - 0.5 < 1e-9);
        // Absolute-looking keys get a hint, not an automatic fix.
        let mut p = clean();
        p.clips[3].opacity = keys(&[(4.5, 0.0), (5.5, 1.0)]); // t1 is at 4..6 on the timeline, 2 s long
        let f = find(&p, "keyframe_outside_clip");
        assert!(f.message.contains("timeline times"), "{}", f.message);
        let mut p = clean();
        p.clips[3].opacity = Param::Animated { keys: vec![Key { t: 1.0, v: 1.0, ease: Ease::Linear }, Key { t: 0.0, v: 0.0, ease: Ease::Linear }] };
        fixes(&p, "keys_unsorted");
        let mut p = clean();
        p.clips[3].fade_in = 1.5;
        p.clips[3].fade_out = 1.5;
        fixes(&p, "fade_too_long");
        // An ease on the last key does nothing; the fix moves every ease one key earlier.
        let mut p = clean();
        p.clips[3].opacity = Param::Animated {
            keys: vec![
                Key { t: 0.0, v: 0.0, ease: Ease::Linear },
                Key { t: 0.5, v: 1.0, ease: Ease::Hold },
                Key { t: 1.0, v: 0.2, ease: Ease::Out },
            ],
        };
        let q = fixes(&p, "ease_on_last_key");
        let Param::Animated { keys } = &q.clips[3].opacity else { panic!("still animated") };
        assert_eq!(keys.iter().map(|k| k.ease.clone()).collect::<Vec<_>>(), vec![Ease::Hold, Ease::Out, Ease::Linear]);
        // Eases already on the keys where their moves start are left alone.
        let mut p = clean();
        p.clips[3].opacity = Param::Animated { keys: vec![Key { t: 0.0, v: 0.0, ease: Ease::Out }, Key { t: 1.0, v: 1.0, ease: Ease::Linear }] };
        assert!(!codes(&p).contains(&"ease_on_last_key"));
    }

    #[test]
    fn text_and_safe_areas() {
        let mut p = clean();
        p.clips[3].transform.y = Param::Value(500.0);
        let q = fixes(&p, "text_outside_title_safe");
        assert!(matches!(q.clips[3].transform.y, Param::Value(y) if y < 400.0));
        let mut p = clean();
        if let Source::Text(t) = &mut p.clips[3].source {
            t.text = "  ".into();
        }
        assert!(codes(&p).contains(&"empty_text"));
        let mut p = clean();
        if let Source::Text(t) = &mut p.clips[3].source {
            t.highlight = Some(WordHighlight { color: Color::WHITE, words: vec![[0.0, 0.5]], background: None });
        }
        assert!(codes(&p).contains(&"highlight_mismatch"));
        let mut p = clean();
        p.clips.push(base_clip(
            "s1",
            "v2",
            Time::At(10.0),
            2.0,
            Source::Shape(ShapeSource { kind: ShapeKind::Rect, width: 400.0, height: 200.0, fill: Some(Color::WHITE), stroke: None, radius: 0.0, draw: Param::Value(1.0) }),
        ));
        p.clips.last_mut().unwrap().transform.x = Param::Value(900.0);
        fixes(&p, "content_outside_action_safe");
    }

    #[test]
    fn effects_transitions_ducking_tracks() {
        let mut p = clean();
        p.clips[0].effects.push(effect("blurr", &[]));
        fixes(&p, "unknown_effect");
        let mut p = clean();
        p.clips[0].effects.push(effect("blur", &[("size", 3.0)]));
        fixes(&p, "unknown_effect_param");
        let mut p = clean();
        p.clips[0].effects.push(effect("blur", &[("radius", 999.0)]));
        fixes(&p, "effect_param_out_of_range");

        let tr = |id: &str, from: &str, to: &str, kind: &str, d: f64| Transition { id: id.into(), from: from.into(), to: to.into(), kind: kind.into(), duration: d, params: Default::default(), colors: Default::default() };
        let mut p = clean();
        p.transitions.push(tr("tr1", "c1", "c2", "wobble", 0.5));
        fixes(&p, "unknown_transition");
        let mut p = clean();
        p.transitions.push(tr("tr1", "c1", "c3", "crossfade", 0.5));
        fixes(&p, "transition_not_adjacent");
        let mut p = clean();
        p.transitions.push(tr("tr1", "c1", "c2", "crossfade", 6.0));
        fixes(&p, "transition_too_long");
        let mut p = clean();
        p.clips[1].source = Source::Media { asset: "mv".into(), in_point: 0.0, stream: 0 }; // no handle before c2
        p.transitions.push(tr("tr1", "c1", "c2", "crossfade", 1.0));
        let f = find(&p, "transition_too_long");
        assert!(f.message.contains("held"), "{}", f.message);
        let mut p = clean();
        p.transitions.push(tr("tr1", "c1", "c2", "crossfade", 0.5));
        p.transitions.push(tr("tr2", "c1", "c2", "crossfade", 0.5));
        fixes(&p, "duplicate_transition");

        let mut p = clean();
        p.ducking.push(Ducking { id: "d1".into(), target: "a9".into(), trigger: "a1".into(), amount_db: -12.0, threshold_db: -45.0, attack: 0.1, release: 0.4 });
        fixes(&p, "ducking_track_missing");

        let mut p = clean();
        p.tracks.push(track("v3", TrackKind::Video, ""));
        fixes(&p, "empty_track");
        let mut p = clean();
        p.tracks[2].solo = true;
        fixes(&p, "solo_active");
    }

    #[test]
    fn project_level_codes() {
        let mut p = clean();
        p.settings.duration = Some(6.0);
        fixes(&p, "past_fixed_duration");
        let mut p = clean();
        p.markers.push(marker("mk1", Time::At(30.0)));
        p.markers[0].kind = MarkerKind::Chapter;
        let f = find(&p, "chapter_markers");
        assert!(f.message.contains("0:00") && f.message.contains("at least 3"));
        let mut p = clean();
        p.markers.push(marker("c1", Time::At(1.0)));
        let f = find(&p, "duplicate_id");
        assert!(f.message.contains("clip") && f.message.contains("marker"));
        let mut p = clean();
        p.transcripts.insert("ghost".into(), Transcript::default());
        assert!(codes(&p).contains(&"unknown_transcript_asset"));
        let mut p = clean();
        p.media[0].info.has_audio = false;
        assert!(codes(&p).contains(&"no_audio_stream"));
    }

    #[test]
    fn export_letterbox() {
        let mut p = clean();
        p.export.width = Some(3840);
        p.export.height = Some(2160);
        assert!(!codes(&p).contains(&"export_letterbox"), "same aspect");
        p.export.width = Some(1080);
        p.export.height = Some(1920);
        let f = find(&p, "export_letterbox");
        assert!(f.message.contains("1080×1920") && f.message.contains("letterbox") && f.message.contains("68% of the output is empty"), "{}", f.message);
        let q = fixes(&p, "export_letterbox");
        assert_eq!(q.export.fit, Fit::Cover);
        p.export.height = None; // width alone keeps the aspect
        assert!(!codes(&p).contains(&"export_letterbox"));
    }

    #[test]
    fn every_code_is_documented_and_fast() {
        let documented: Vec<&str> = CODES.iter().map(|c| c.0).collect();
        for c in ["unresolved_anchor", "anchor_cycle", "overlap_same_track", "gap_in_main", "clip_past_media_end", "missing_media", "keyframe_outside_clip", "text_outside_title_safe", "content_outside_action_safe", "transition_not_adjacent", "transition_too_long", "empty_track", "duplicate_id", "zero_length_clip", "speed_out_of_range", "unknown_effect", "unknown_transition", "ducking_track_missing", "word_cut_out", "past_fixed_duration"] {
            assert!(documented.contains(&c), "{c}");
        }
        let mut p = project();
        p.clips.push(media_clip("c0", "v1", "mv", Time::At(0.0), 1.0, 0.0));
        for i in 1..10_000 {
            p.clips.push(media_clip(&format!("c{i}"), "v1", "mv", after(&format!("c{}", i - 1), 0.0), 0.05, 0.0));
        }
        let r = resolve(&p);
        let t = std::time::Instant::now();
        let f = validate(&p, &r);
        assert!(t.elapsed().as_millis() < 3000, "validate took {:?}", t.elapsed());
        assert!(f.iter().all(|f| f.severity != Severity::Error), "{:?}", f.iter().find(|f| f.severity == Severity::Error));
    }
}
