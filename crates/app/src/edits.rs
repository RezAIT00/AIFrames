//! Op builders for menu commands and shortcuts (pure: they read the project and return ops),
//! plus the clip clipboard and history navigation.

use aiframes_core::{Anchor, AnchorTarget, HistoryEntry, HistoryKind, Id, MediaKind, Op, Project, Resolved, Source, Time, TrackKind};
use serde_json::{json, Value as J};

/// Clipboard format marker.
pub const CLIP_MAGIC: &str = "aiframes/clips";

fn op(v: J) -> Option<Op> {
    Op::from_json(&v).ok()
}

/// True when `k` is the linked companion of `c` (detached audio anchored `with` it on the same media).
fn linked_to(p: &Project, k: &str, c: &str) -> bool {
    let (Some(kc), Some(cc)) = (p.clips.iter().find(|x| x.id == k), p.clips.iter().find(|x| x.id == c)) else { return false };
    let same_media = matches!((&kc.source, &cc.source), (Source::Media { asset: a, .. }, Source::Media { asset: b, .. }) if a == b);
    same_media && matches!(&kc.start, Time::Anchored(Anchor { to: AnchorTarget::With(t), offset }) if t == c && offset.abs() < 1e-9)
}

/// Selected clip ids minus clips that follow another selected clip as its linked companion
/// (ops on the leader already take them along).
fn leaders(p: &Project, ids: &[Id]) -> Vec<Id> {
    let clips: Vec<&Id> = ids.iter().filter(|i| p.clips.iter().any(|c| &c.id == *i)).collect();
    clips.iter().filter(|k| !clips.iter().any(|c| c != *k && linked_to(p, k, c))).map(|s| (*s).clone()).collect()
}

/// Copies clips as JSON with absolute starts relative to the earliest one.
pub fn copy_clips(p: &Project, r: &Resolved, ids: &[Id]) -> Option<String> {
    let mut items: Vec<(f64, J)> = Vec::new();
    for c in p.clips.iter().filter(|c| ids.contains(&c.id)) {
        let span = r.span(&c.id)?;
        let mut v = serde_json::to_value(c).ok()?;
        if let Some(o) = v.as_object_mut() {
            o.remove("id");
            o.insert("start".into(), json!(span.start));
        }
        items.push((span.start, v));
    }
    if items.is_empty() {
        return None;
    }
    let origin = items.iter().map(|(s, _)| *s).fold(f64::INFINITY, f64::min);
    let clips: Vec<J> = items
        .into_iter()
        .map(|(s, mut v)| {
            v["start"] = json!(s - origin);
            v
        })
        .collect();
    Some(json!({"format": CLIP_MAGIC, "clips": clips}).to_string())
}

/// `add_clip` ops pasting clipboard JSON at timeline time `at` (overwriting what is there).
/// Clips go back to their track when it still exists (and is unlocked), else to the first track of
/// the same kind.
pub fn paste_ops(p: &Project, text: &str, at: f64) -> Result<Vec<Op>, String> {
    let v: J = serde_json::from_str(text.trim()).map_err(|_| "the clipboard does not hold AIFrames clips".to_string())?;
    if v.get("format").and_then(J::as_str) != Some(CLIP_MAGIC) {
        return Err("the clipboard does not hold AIFrames clips".into());
    }
    let clips = v.get("clips").and_then(J::as_array).ok_or("no clips on the clipboard")?;
    let mut ops = Vec::new();
    for c in clips {
        let mut c = c.clone();
        let rel = c.get("start").and_then(J::as_f64).unwrap_or(0.0);
        let track = c.get("track").and_then(J::as_str).unwrap_or("").to_string();
        let wants_audio = p.tracks.iter().find(|t| t.id == track).map(|t| t.kind == TrackKind::Audio).unwrap_or_else(|| {
            // Unknown track: audio-only media goes to an audio track.
            c.pointer("/source/asset").and_then(J::as_str).and_then(|a| p.media.iter().find(|m| m.id == a)).is_some_and(|m| m.kind == MediaKind::Audio)
        });
        let ok = p.tracks.iter().any(|t| t.id == track && !t.locked);
        let dest = if ok {
            Some(track)
        } else {
            let kind = if wants_audio { TrackKind::Audio } else { TrackKind::Video };
            p.tracks.iter().find(|t| t.kind == kind && !t.locked).map(|t| t.id.clone())
        };
        let Some(dest) = dest else { return Err("no unlocked track to paste onto; add a track first".into()) };
        c["track"] = json!(dest);
        c["start"] = json!((at + rel).max(0.0));
        if let Some(o) = op(json!({"op": "add_clip", "clip": c, "overwrite": true})) {
            ops.push(o);
        }
    }
    if ops.is_empty() {
        return Err("nothing to paste".into());
    }
    Ok(ops)
}

/// Delete (or ripple delete) the selected clips.
pub fn delete_ops(p: &Project, ids: &[Id], ripple: bool) -> Vec<Op> {
    leaders(p, ids).into_iter().filter_map(|id| op(json!({"op": "delete_clip", "clip": id, "ripple": ripple}))).collect()
}

/// Split at `at`: the selected clips under the playhead, or every clip under it on unlocked
/// tracks when nothing under the playhead is selected.
pub fn split_ops(p: &Project, r: &Resolved, ids: &[Id], at: f64) -> Vec<Op> {
    let inside = |id: &str| r.span(id).is_some_and(|s| at > s.start + 1e-6 && at < s.end - 1e-6);
    let unlocked = |track: &str| p.tracks.iter().any(|t| t.id == track && !t.locked);
    let mut targets: Vec<Id> = ids.iter().filter(|i| inside(i)).cloned().collect();
    if targets.is_empty() {
        targets = p.clips.iter().filter(|c| inside(&c.id) && unlocked(&c.track)).map(|c| c.id.clone()).collect();
    }
    leaders(p, &targets).into_iter().filter_map(|id| op(json!({"op": "split_clip", "clip": id, "at": at}))).collect()
}

/// Every clip id (Select All).
pub fn all_clips(p: &Project) -> Vec<Id> {
    p.clips.iter().map(|c| c.id.clone()).collect()
}

/// Sorted cut points (clip starts and ends) for previous/next edit navigation.
pub fn edit_points(r: &Resolved) -> Vec<f64> {
    let mut v: Vec<f64> = r.clips.values().flat_map(|s| [s.start, s.end]).collect();
    v.push(0.0);
    v.sort_by(f64::total_cmp);
    v.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    v
}

pub fn prev_edit(points: &[f64], t: f64) -> Option<f64> {
    points.iter().rev().find(|p| **p < t - 1e-4).copied()
}

pub fn next_edit(points: &[f64], t: f64) -> Option<f64> {
    points.iter().find(|p| **p > t + 1e-4).copied()
}

/// A fresh track id with this prefix ("v", "a").
pub fn new_track_id(p: &Project, prefix: &str) -> Id {
    aiframes_core::new_id(p, prefix)
}

pub fn add_track_op(kind: TrackKind, id: Option<Id>) -> Option<Op> {
    let k = if kind == TrackKind::Audio { "audio" } else { "video" };
    match id {
        Some(id) => op(json!({"op": "add_track", "kind": k, "id": id})),
        None => op(json!({"op": "add_track", "kind": k})),
    }
}

/// Media dropped on the viewer: a new track on top with the clip at `at`.
pub fn drop_on_viewer_ops(p: &Project, asset: &str, at: f64) -> Vec<Op> {
    let Some(m) = p.media.iter().find(|m| m.id == asset) else { return Vec::new() };
    let audio = m.kind == MediaKind::Audio;
    let tid = new_track_id(p, if audio { "a" } else { "v" });
    let mut ops = Vec::new();
    ops.extend(add_track_op(if audio { TrackKind::Audio } else { TrackKind::Video }, Some(tid.clone())));
    ops.extend(op(json!({"op": "add_clip", "clip": {"track": tid, "asset": asset, "start": at.max(0.0)}})));
    ops
}

/// Media files dropped on the timeline (after import): one after another from `at`. When `target`
/// (the track under the pointer) is unlocked and of the right kind the files go there as an
/// overwrite edit (what the person pointed at); otherwise each goes to the topmost unlocked track
/// of its kind that is free over its span (a new track when none is), overwriting nothing.
pub fn place_ops(p: &Project, r: &Resolved, assets: &[(Id, MediaKind, f64)], at: f64, target: Option<&str>) -> Vec<Op> {
    let mut ops = Vec::new();
    let mut added: Vec<(Id, f64, f64)> = Vec::new();
    let mut created: Vec<(TrackKind, Id)> = Vec::new();
    let mut t = at.max(0.0);
    for (id, kind, dur) in assets {
        let len = if *dur > 0.0 { *dur } else { 3.0 };
        let tk = if *kind == MediaKind::Audio { TrackKind::Audio } else { TrackKind::Video };
        if let Some(tr) = target.and_then(|t| p.tracks.iter().find(|x| x.id == t && x.kind == tk && !x.locked)) {
            ops.extend(op(json!({"op": "add_clip", "clip": {"track": tr.id, "asset": id, "start": t}, "overwrite": true})));
            added.push((tr.id.clone(), t, t + len));
            t += len;
            continue;
        }
        let free = |track: &str| {
            r.on_track(p, track).iter().all(|(_, s)| s.end <= t + 1e-6 || s.start >= t + len - 1e-6)
                && added.iter().all(|(tr, a, b)| tr != track || *b <= t + 1e-6 || *a >= t + len - 1e-6)
        };
        // As shown: video topmost first, audio A1 first.
        let candidates: Vec<&aiframes_core::Track> = aiframes_core::tracks_in_display_order(p).into_iter().filter(|x| x.kind == tk && !x.locked).collect();
        let track = match candidates.iter().find(|x| free(&x.id)) {
            Some(x) => x.id.clone(),
            None => match created.iter().find(|(k, _)| *k == tk) {
                Some((_, id)) => id.clone(),
                None => {
                    let nid = new_track_id(p, if tk == TrackKind::Audio { "a" } else { "v" });
                    ops.extend(add_track_op(tk, Some(nid.clone())));
                    created.push((tk, nid.clone()));
                    nid
                }
            },
        };
        ops.extend(op(json!({"op": "add_clip", "clip": {"track": track, "asset": id, "start": t}})));
        added.push((track, t, t + len));
        t += len;
    }
    ops
}

/// Detach the selected video clips' sound to audio tracks that are free over each clip's span
/// (a new audio track is added when none is).
pub fn detach_audio_ops(p: &Project, r: &Resolved, ids: &[Id]) -> Vec<Op> {
    let mut ops = Vec::new();
    // Spans taken per audio track, including what this batch adds.
    let mut taken: Vec<(Id, Vec<aiframes_core::Span>)> = p
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Audio && !t.locked)
        .map(|t| (t.id.clone(), r.on_track(p, &t.id).into_iter().map(|(_, s)| s).collect()))
        .collect();
    let mut new_track: Option<Id> = None;
    for c in p.clips.iter().filter(|c| ids.contains(&c.id)) {
        let has_sound = match &c.source {
            Source::Media { asset, .. } => p.media.iter().find(|m| &m.id == asset).is_some_and(|m| m.kind == MediaKind::Video && m.info.has_audio),
            _ => false,
        };
        let Some(span) = r.span(&c.id) else { continue };
        if !has_sound || c.muted {
            continue;
        }
        let track = match taken.iter_mut().find(|(_, spans)| !spans.iter().any(|s| s.overlaps(&span))) {
            Some((id, spans)) => {
                spans.push(span);
                id.clone()
            }
            None => {
                let id = new_track.get_or_insert_with(|| new_track_id(p, "a")).clone();
                if !taken.iter().any(|(t, _)| *t == id) {
                    ops.extend(add_track_op(TrackKind::Audio, Some(id.clone())));
                    taken.push((id.clone(), Vec::new()));
                }
                if let Some((_, spans)) = taken.iter_mut().find(|(t, _)| *t == id) {
                    spans.push(span);
                }
                id
            }
        };
        ops.extend(op(json!({"op": "detach_audio", "clip": c.id, "track": track})));
    }
    ops
}

pub fn set_enabled_ops(p: &Project, ids: &[Id]) -> Vec<Op> {
    let sel: Vec<_> = p.clips.iter().filter(|c| ids.contains(&c.id)).collect();
    // Toggle: if any is enabled, disable all; else enable all.
    let enable = !sel.iter().any(|c| c.enabled);
    sel.iter().filter_map(|c| op(json!({"op": "set_clip", "clip": c.id, "patch": {"enabled": enable}}))).collect()
}

pub fn speed_ops(ids: &[Id], speed: f64, keep_duration: bool) -> Vec<Op> {
    ids.iter().filter_map(|id| op(json!({"op": "set_speed", "clip": id, "speed": speed, "keep": if keep_duration { "duration" } else { "source" }}))).collect()
}

pub fn freeze_ops(p: &Project, r: &Resolved, ids: &[Id], at: f64, duration: f64) -> Vec<Op> {
    let target = ids.iter().find(|i| r.span(i).is_some_and(|s| s.contains(at)) && p.clips.iter().any(|c| &c.id == *i && matches!(c.source, Source::Media { .. })));
    target.and_then(|id| op(json!({"op": "freeze_frame", "clip": id, "at": at, "duration": duration}))).into_iter().collect()
}

pub fn marker_op(at: f64) -> Option<Op> {
    op(json!({"op": "add_marker", "time": at}))
}

/// The clip a clip's start is anchored `with` (an overlay on a base clip).
fn with_base<'a>(p: &'a Project, c: &aiframes_core::Clip) -> Option<&'a aiframes_core::Clip> {
    match &c.start {
        Time::Anchored(Anchor { to: AnchorTarget::With(b), .. }) => p.clips.iter().find(|x| &x.id == b),
        _ => None,
    }
}

/// "Match length of <base>" for the selected overlays anchored `with` a base clip: each ends when
/// its base ends (`until` after the base). Returns the label of the (first) base and the ops; clips
/// that already match, or where it would make an anchor cycle, are left out.
pub fn match_length_ops(p: &Project, ids: &[Id]) -> Option<(String, Vec<Op>)> {
    let mut base_label = None;
    let mut ops = Vec::new();
    for c in p.clips.iter().filter(|c| ids.contains(&c.id)) {
        let Some(base) = with_base(p, c) else { continue };
        let until = Time::Anchored(Anchor { to: AnchorTarget::After(base.id.clone()), offset: 0.0 });
        if c.until.as_ref() == Some(&until) || !aiframes_ui::util::until_allowed(p, &aiframes_ui::util::end_dependants(p, &c.id), &c.id, &until) {
            continue;
        }
        base_label.get_or_insert_with(|| aiframes_ui::util::clip_label(p, base));
        ops.push(Op::SetUntil { clip: c.id.clone(), until: Some(Some(until)) });
    }
    base_label.map(|l| (l, ops))
}

/// "Fixed length": drops the end anchor (`until`) of the selected clips that have one.
pub fn fixed_length_ops(p: &Project, ids: &[Id]) -> Vec<Op> {
    p.clips.iter().filter(|c| ids.contains(&c.id) && c.until.is_some()).map(|c| Op::SetUntil { clip: c.id.clone(), until: Some(None) }).collect()
}

/// The track that "Set spacing" / "Close gaps" act on: the selected clips' track when they are all on
/// one, else the main track (or the first one with at least two clips). With `(track, selected clips
/// on it when there are at least two, clip count on the track)`.
pub fn gap_track(p: &Project, selection: &[Id]) -> Option<(Id, Vec<Id>, usize)> {
    let sel: Vec<&aiframes_core::Clip> = p.clips.iter().filter(|c| selection.contains(&c.id)).collect();
    let count = |t: &str| p.clips.iter().filter(|c| c.track == t).count();
    let track = match sel.first() {
        Some(first) if sel.iter().all(|c| c.track == first.track) => first.track.clone(),
        _ => {
            let shown = aiframes_core::tracks_in_display_order(p);
            shown.iter().find(|t| t.role == "main" && count(&t.id) >= 2).or_else(|| shown.iter().find(|t| count(&t.id) >= 2)).map(|t| t.id.clone())?
        }
    };
    let clips: Vec<Id> = if sel.len() >= 2 && sel.iter().all(|c| c.track == track) { sel.iter().map(|c| c.id.clone()).collect() } else { Vec::new() };
    let n = count(&track);
    Some((track, clips, n))
}


/// Undo/redo steps that bring the document to just after edit `target` (`None` when the target
/// is not reachable: not an edit, or undone history that was replaced by a newer edit).
pub fn restore_plan(history: &[HistoryEntry], target: u64) -> Option<(usize, usize)> {
    let mut applied: Vec<u64> = Vec::new();
    let mut redo: Vec<u64> = Vec::new();
    for e in history {
        match e.kind {
            HistoryKind::Edit => {
                applied.push(e.revision);
                redo.clear();
            }
            HistoryKind::Undo => {
                if let Some(r) = applied.pop() {
                    redo.push(r);
                }
            }
            HistoryKind::Redo => {
                if let Some(r) = redo.pop() {
                    applied.push(r);
                }
            }
        }
    }
    if let Some(pos) = applied.iter().position(|r| *r == target) {
        return Some((applied.len() - 1 - pos, 0));
    }
    if let Some(pos) = redo.iter().position(|r| *r == target) {
        return Some((0, redo.len() - pos));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiframes_core::{resolve, Document};

    fn project() -> Project {
        let v = json!({
            "format": 1,
            "settings": {"width": 1920, "height": 1080, "fps": 30},
            "media": [
                {"id": "m1", "path": "a.mp4", "kind": "video", "info": {"duration": 20.0, "width": 1920, "height": 1080, "fps": 30, "has_video": true, "has_audio": true}},
                {"id": "m2", "path": "b.wav", "kind": "audio", "info": {"duration": 30.0, "has_audio": true}}
            ],
            "tracks": [{"id": "v1", "kind": "video"}, {"id": "a1", "kind": "audio"}],
            "clips": [
                {"id": "c1", "track": "v1", "start": 0.0, "duration": 4.0, "source": {"type": "media", "asset": "m1"}},
                {"id": "c2", "track": "v1", "start": {"after": "c1"}, "duration": 3.0, "source": {"type": "media", "asset": "m1", "in_point": 5.0}},
                {"id": "c3", "track": "a1", "start": 1.0, "duration": 5.0, "source": {"type": "media", "asset": "m2"}}
            ]
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn copy_paste_roundtrip() {
        let p = project();
        let r = resolve(&p);
        let text = copy_clips(&p, &r, &["c2".into(), "c3".into()]).unwrap();
        let v: J = serde_json::from_str(&text).unwrap();
        assert_eq!(v["clips"][0]["start"], json!(3.0), "c2 is 3 s after c3 (c3 at 1, c2 at 4)");
        assert_eq!(v["clips"][1]["start"], json!(0.0));
        assert!(v["clips"][0].get("id").is_none());
        let ops = paste_ops(&p, &text, 10.0).unwrap();
        assert_eq!(ops.len(), 2);
        let mut doc = Document::new(p);
        doc.apply("user", "Paste", ops).expect("paste applies");
        assert_eq!(doc.project.clips.len(), 5);
        let new: Vec<_> = doc.project.clips.iter().filter(|c| !["c1", "c2", "c3"].contains(&c.id.as_str())).collect();
        let starts: Vec<f64> = new.iter().map(|c| doc.resolved().span(&c.id).unwrap().start).collect();
        assert!(starts.contains(&13.0) && starts.contains(&10.0), "{starts:?}");
        assert!(paste_ops(&doc.project, "hello", 0.0).is_err());
    }

    #[test]
    fn delete_and_split() {
        let p = project();
        let r = resolve(&p);
        let ops = split_ops(&p, &r, &[], 2.0);
        assert_eq!(ops.len(), 2, "c1 and c3 are under 2.0");
        let ops = split_ops(&p, &r, &["c3".into()], 2.0);
        assert_eq!(ops.len(), 1);
        assert!(split_ops(&p, &r, &[], 4.0).iter().all(|o| !matches!(o, Op::SplitClip { clip, .. } if clip == "c1")), "not at an edge");
        let mut doc = Document::new(p.clone());
        doc.apply("user", "Split", split_ops(&p, &r, &[], 2.0)).unwrap();
        assert_eq!(doc.project.clips.len(), 5);
        let ops = delete_ops(&p, &["c1".into(), "c9".into()], true);
        assert_eq!(ops.len(), 1);
        let mut doc = Document::new(p);
        doc.apply("user", "Ripple delete", ops).unwrap();
        assert_eq!(doc.resolved().span("c2").unwrap().start, 0.0, "c2 follows magnetically");
    }

    #[test]
    fn edits_points() {
        let r = resolve(&project());
        let pts = edit_points(&r);
        assert_eq!(pts, vec![0.0, 1.0, 4.0, 6.0, 7.0]);
        assert_eq!(prev_edit(&pts, 4.0), Some(1.0));
        assert_eq!(next_edit(&pts, 4.0), Some(6.0));
        assert_eq!(next_edit(&pts, 7.0), None);
    }

    #[test]
    fn drop_and_place() {
        let p = project();
        let ops = drop_on_viewer_ops(&p, "m1", 2.0);
        assert_eq!(ops.len(), 2);
        let mut doc = Document::new(p.clone());
        doc.apply("user", "Drop", ops).unwrap();
        assert_eq!(doc.project.tracks.len(), 3);
        assert_eq!(doc.project.tracks.last().unwrap().kind, TrackKind::Video);
        let r = resolve(&p);
        let ops = place_ops(&p, &r, &[("m2".into(), MediaKind::Audio, 30.0)], 20.0, None);
        let mut doc = Document::new(p.clone());
        doc.apply("user", "Place", ops).unwrap();
        assert!(doc.project.clips.iter().any(|c| c.track == "a1" && doc.resolved().span(&c.id).unwrap().start == 20.0));
        // Over existing clips: a new track instead of overwriting; two files share it in sequence.
        let ops = place_ops(&p, &r, &[("m1".into(), MediaKind::Video, 20.0), ("m1".into(), MediaKind::Video, 20.0)], 1.0, None);
        assert_eq!(ops.iter().filter(|o| matches!(o, Op::AddTrack { .. })).count(), 1);
        let mut doc = Document::new(p.clone());
        doc.apply("user", "Place", ops).unwrap();
        assert_eq!(doc.project.clips.len(), 5, "nothing was overwritten");
        // Pointing at a track of the right kind: an overwrite edit there; the wrong kind falls back.
        let ops = place_ops(&p, &r, &[("m1".into(), MediaKind::Video, 20.0)], 1.0, Some("v1"));
        assert!(matches!(&ops[..], [Op::AddClip { overwrite: true, .. }]));
        let ops = place_ops(&p, &r, &[("m1".into(), MediaKind::Video, 20.0)], 1.0, Some("a1"));
        assert!(ops.iter().any(|o| matches!(o, Op::AddTrack { .. })));
    }

    #[test]
    fn detach_enable_speed_freeze() {
        let p = project();
        let r = resolve(&p);
        // a1 is busy at 1..6 (c3), so c1 (0..4) goes to a new audio track.
        let ops = detach_audio_ops(&p, &r, &["c1".into()]);
        assert_eq!(ops.len(), 2, "{ops:?}");
        assert!(detach_audio_ops(&p, &r, &["c3".into()]).is_empty(), "audio clips have nothing to detach");
        let mut doc = Document::new(p.clone());
        doc.apply("user", "Detach", ops).unwrap();
        assert!(doc.project.clips.iter().find(|c| c.id == "c1").unwrap().muted);
        // Both video clips: c2 (4..7) still overlaps c3 on a1, so both share the new track.
        let ops = detach_audio_ops(&p, &r, &["c1".into(), "c2".into()]);
        assert_eq!(ops.iter().filter(|o| matches!(o, Op::AddTrack { .. })).count(), 1);
        let mut doc = Document::new(p.clone());
        doc.apply("user", "Detach both", ops).unwrap();
        let ops = set_enabled_ops(&p, &["c1".into()]);
        let mut doc = Document::new(p.clone());
        doc.apply("user", "Disable", ops).unwrap();
        assert!(!doc.project.clips[0].enabled);
        assert_eq!(speed_ops(&["c1".into()], 2.0, false).len(), 1);
        assert_eq!(freeze_ops(&p, &r, &["c1".into()], 2.0, 1.0).len(), 1);
        assert!(freeze_ops(&p, &r, &["c1".into()], 9.0, 1.0).is_empty());
        assert!(marker_op(3.0).is_some());
    }

    #[test]
    fn restore_plans() {
        let p = project();
        let mut doc = Document::new(p);
        for i in 0..3 {
            doc.apply("user", &format!("m{i}"), vec![marker_op(i as f64).unwrap()]).unwrap();
        }
        // revisions 1, 2, 3 applied
        assert_eq!(restore_plan(doc.history(), 1), Some((2, 0)));
        assert_eq!(restore_plan(doc.history(), 3), Some((0, 0)));
        doc.undo();
        doc.undo(); // revisions 4 (undo 3), 5 (undo 2)
        assert_eq!(restore_plan(doc.history(), 3), Some((0, 2)));
        assert_eq!(restore_plan(doc.history(), 2), Some((0, 1)));
        assert_eq!(restore_plan(doc.history(), 1), Some((0, 0)));
        assert_eq!(restore_plan(doc.history(), 4), None, "undo entries are not targets");
        doc.apply("user", "new", vec![marker_op(9.0).unwrap()]).unwrap();
        assert_eq!(restore_plan(doc.history(), 3), None, "replaced by a newer edit");
        // Following a plan really lands on the target.
        let (u, r) = restore_plan(doc.history(), 1).unwrap();
        for _ in 0..u {
            doc.undo();
        }
        for _ in 0..r {
            doc.redo();
        }
        assert_eq!(doc.project.markers.len(), 1);
    }
}
