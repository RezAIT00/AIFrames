//! Stale-edit detection: was a batch of ops planned against an older revision still safe to apply?
//!
//! An AI plans an edit from what it last read (a receipt or a description at some revision). A person
//! may edit in between. Ops that name clips, anchors or transcript words follow those edits or fail
//! loudly when their target is gone, but ops with absolute timeline seconds (a split at 12.4 s, a
//! range, a marker time) would silently land in the wrong place. [`Document::check_base`] compares a
//! batch with the history entries after the revision it was planned against and refuses it when
//!
//! - the batch uses absolute timeline seconds and a newer change moved, trimmed, added or removed
//!   clips (or otherwise changed what plays when), or
//! - the batch references an id (clip, track, marker, media, ...) that a newer change modified or
//!   removed, or words of a transcript that was replaced, or
//! - a newer change cannot be inspected (the whole project was replaced or reloaded, or the history
//!   does not hold its ops).
//!
//! Everything else (unrelated newer changes, id- and anchor-based ops) is fine to apply on top.
//! Keyframe times are clip-local and source ranges (`cut_source`, word indices) are media-local:
//! neither is an absolute timeline time.

use crate::document::{Document, HistoryEntry, HistoryKind};
use crate::model::{AnchorTarget, Id, Source, Time};
use crate::ops::{EditError, Op};
use serde_json::Value;
use std::collections::BTreeMap;

/// What one op refers to and does, as far as stale-edit detection cares.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OpUse {
    /// Ids the op modifies or removes.
    pub targets: Vec<Id>,
    /// Every id the op references (its targets, anchor targets, tracks, media).
    pub refs: Vec<Id>,
    /// Clips whose transcript words the op addresses (word anchors, `delete_words`, captions).
    pub word_clips: Vec<Id>,
    /// Fields holding absolute timeline seconds: (field, seconds).
    pub absolute: Vec<(&'static str, f64)>,
    /// The op moves, trims, adds or removes clips, or otherwise changes what plays when.
    pub retimes: bool,
}

impl OpUse {
    fn target(&mut self, id: &Id) {
        push(&mut self.targets, id);
        push(&mut self.refs, id);
    }

    fn uses(&mut self, id: &Id) {
        push(&mut self.refs, id);
    }

    fn time(&mut self, field: &'static str, t: &Time) {
        match t {
            Time::At(v) => self.absolute.push((field, *v)),
            Time::Anchored(a) => match &a.to {
                AnchorTarget::After(id) | AnchorTarget::With(id) | AnchorTarget::Before(id) | AnchorTarget::Marker(id) => self.uses(id),
                AnchorTarget::Word(w) => {
                    self.uses(&w.clip);
                    push(&mut self.word_clips, &w.clip);
                }
            },
        }
    }

    /// A partial clip (`add_clip`) or a clip patch (`set_clip`): its track, media and times.
    fn clip_json(&mut self, v: &Value, times: bool) {
        for key in ["track", "asset"] {
            if let Some(id) = v.get(key).and_then(Value::as_str) {
                self.uses(&id.to_string());
            }
        }
        if let Some(Ok(Source::Media { asset, .. })) = v.get("source").map(|s| serde_json::from_value::<Source>(s.clone())) {
            self.uses(&asset);
        }
        if times {
            for key in ["start", "until"] {
                if let Some(Ok(t)) = v.get(key).filter(|t| !t.is_null()).map(|t| serde_json::from_value::<Time>(t.clone())) {
                    self.time(key, &t);
                }
            }
        }
    }
}

fn push(v: &mut Vec<Id>, id: &Id) {
    if !v.contains(id) {
        v.push(id.clone());
    }
}

/// Clip fields whose change (through `set_clip`) changes what plays when.
const TIMING_FIELDS: &[&str] = &["start", "duration", "until", "track", "source", "speed", "reverse"];

/// Classifies one op. The match is exhaustive on purpose: a new op must decide what it is here.
pub fn op_use(op: &Op) -> OpUse {
    let mut u = OpUse::default();
    match op {
        Op::SetSettings { .. } | Op::SetProject { .. } | Op::SetExportSize { .. } | Op::AddMedia { .. } | Op::AddTrack { .. } => {}
        Op::SetExport { range, .. } => {
            if let Some(Some([a, b])) = range {
                u.absolute.push(("range", *a));
                u.absolute.push(("range", *b));
            }
        }
        Op::RemoveMedia { asset, force } => {
            u.target(asset);
            u.retimes = *force;
        }
        Op::RelinkMedia { asset, .. } | Op::SetMedia { asset, .. } => u.target(asset),
        Op::RemoveTrack { track, force } => {
            u.target(track);
            u.retimes = *force;
        }
        Op::MoveTrack { track, .. } | Op::SetTrack { track, .. } => u.target(track),
        Op::AddClip { clip, .. } => {
            u.clip_json(clip, true);
            u.retimes = true;
        }
        Op::AppendClip { track, after, clip } => {
            u.uses(track);
            if let Some(a) = after {
                u.uses(a);
            }
            u.clip_json(clip, false);
            u.retimes = true;
        }
        Op::InsertClip { track, at, clip, .. } => {
            u.uses(track);
            u.absolute.push(("at", *at));
            u.clip_json(clip, false);
            u.retimes = true;
        }
        Op::MoveClip { clip, start, track, .. } => {
            u.target(clip);
            u.time("start", start);
            if let Some(t) = track {
                u.uses(t);
            }
            u.retimes = true;
        }
        Op::MoveClips { moves, .. } => {
            for m in moves {
                u.target(&m.clip);
                u.time("start", &m.start);
                if let Some(t) = &m.track {
                    u.uses(t);
                }
            }
            u.retimes = true;
        }
        Op::SetUntil { clip, until } => {
            u.target(clip);
            if let Some(Some(t)) = until {
                u.time("until", t);
            }
            u.retimes = true;
        }
        Op::TrimClip { clip, to, .. } => {
            u.target(clip);
            if let Some(t) = to {
                u.absolute.push(("to", *t));
            }
            u.retimes = true;
        }
        Op::SplitClip { clip, at, .. } => {
            u.target(clip);
            u.absolute.push(("at", *at));
            u.retimes = true;
        }
        Op::DeleteClip { clip, .. } => {
            u.target(clip);
            u.retimes = true;
        }
        Op::DuplicateClip { clip, start, track, .. } => {
            u.uses(clip);
            if let Some(t) = start {
                u.time("start", t);
            }
            if let Some(t) = track {
                u.uses(t);
            }
            u.retimes = true;
        }
        Op::SetClip { clip, patch } => {
            u.target(clip);
            u.clip_json(patch, true);
            u.retimes = TIMING_FIELDS.iter().any(|k| patch.get(k).is_some());
        }
        // Keyframe times are clip-local; only the speed curve changes what plays when.
        Op::SetParam { clip, path, .. } | Op::AddKeyframe { clip, path, .. } | Op::RemoveKeyframe { clip, path, .. } => {
            u.target(clip);
            u.retimes = path == "speed";
        }
        Op::SlipClip { clip, .. } | Op::SlideClip { clip, .. } | Op::SetSpeed { clip, .. } | Op::CutSource { clip, .. } => {
            u.target(clip);
            u.retimes = true;
        }
        Op::RollEdit { left, right, .. } => {
            u.target(left);
            u.target(right);
            u.retimes = true;
        }
        Op::FreezeFrame { clip, at, .. } => {
            u.target(clip);
            u.absolute.push(("at", *at));
            u.retimes = true;
        }
        Op::DetachAudio { clip, track, .. } => {
            u.target(clip);
            u.uses(track);
            u.retimes = true;
        }
        Op::DetachStreams { clip, tracks, .. } => {
            u.target(clip);
            for t in tracks.iter().flatten() {
                u.uses(t);
            }
            u.retimes = true;
        }
        Op::SetAudioStream { clip, .. } => {
            u.target(clip);
        }
        Op::ReplaceMedia { clip, asset, .. } => {
            u.target(clip);
            u.uses(asset);
            u.retimes = true;
        }
        Op::LiftRange { from, to, tracks } | Op::ExtractRange { from, to, tracks } => {
            u.absolute.push(("from", *from));
            u.absolute.push(("to", *to));
            for t in tracks.iter().flatten() {
                u.uses(t);
            }
            u.retimes = true;
        }
        Op::Nudge { clips, .. } => {
            for c in clips {
                u.target(c);
            }
            u.retimes = true;
        }
        Op::SetSpacing { clips, track, .. } => {
            for c in clips {
                u.target(c);
            }
            if let Some(t) = track {
                u.uses(t);
            }
            u.retimes = true;
        }
        Op::CloseGaps { track, .. } => {
            u.uses(track);
            u.retimes = true;
        }
        Op::AddEffect { clip, .. } | Op::RemoveEffect { clip, .. } | Op::SetEffect { clip, .. } | Op::MoveEffect { clip, .. } => u.target(clip),
        Op::AddTransition { from, to, .. } => {
            u.target(from);
            u.target(to);
        }
        Op::SetTransition { transition, .. } | Op::RemoveTransition { transition } | Op::SetTransitionColors { transition, .. } => u.target(transition),
        Op::AddMarker { time, .. } => u.time("time", time),
        Op::SetMarker { marker, time, .. } => {
            u.target(marker);
            if let Some(t) = time {
                u.time("time", t);
                // Whatever is anchored to the marker moves with it.
                u.retimes = true;
            }
        }
        Op::RemoveMarker { marker } => u.target(marker),
        // A suggestion changes nothing until it is applied, but what it proposes must still fit.
        Op::AddSuggestion { at, ops, .. } => {
            if let Some(t) = at {
                u.absolute.push(("at", *t));
            }
            for inner in ops {
                let i = op_use(inner);
                for id in &i.refs {
                    u.uses(id);
                }
                for id in &i.word_clips {
                    push(&mut u.word_clips, id);
                }
                u.absolute.extend(i.absolute);
            }
        }
        Op::RemoveSuggestion { suggestion } => u.target(suggestion),
        Op::AddDucking { target, trigger, .. } => {
            u.uses(target);
            u.uses(trigger);
        }
        Op::SetDucking { ducking, target, trigger, .. } => {
            u.target(ducking);
            for t in [target, trigger].into_iter().flatten() {
                u.uses(t);
            }
        }
        Op::RemoveDucking { ducking } => u.target(ducking),
        Op::SetTranscript { asset, .. } => {
            u.target(asset);
            // Word anchors of this media resolve to other times.
            u.retimes = true;
        }
        Op::DeleteWords { clip, .. } => {
            u.target(clip);
            push(&mut u.word_clips, clip);
            u.retimes = true;
        }
        Op::GenerateCaptions { clips, track, .. } => {
            for c in clips {
                u.uses(c);
                push(&mut u.word_clips, c);
            }
            u.uses(track);
            u.retimes = true;
        }
    }
    u
}

/// A batch that was planned against an older revision but is safe to apply on top of the newer changes.
#[derive(Clone, Debug, PartialEq)]
pub struct AppliedOver {
    pub base_revision: u64,
    /// History entries after `base_revision`.
    pub newer_changes: usize,
    /// How many of them each author made.
    pub by_author: BTreeMap<String, usize>,
}

impl AppliedOver {
    /// "applied on top of 2 newer change(s) since revision 7 (by user) ...".
    pub fn note(&self) -> String {
        let by: Vec<String> = self.by_author.iter().map(|(a, n)| if self.by_author.len() == 1 { a.clone() } else { format!("{a}: {n}") }).collect();
        format!(
            "applied on top of {} newer change(s) since revision {} (by {}); they do not touch what this edit references. changes_since {{\"revision\": {}}} lists them.",
            self.newer_changes,
            self.base_revision,
            by.join(", "),
            self.base_revision
        )
    }
}

const LIST_LIMIT: usize = 8;

fn listed(items: Vec<String>, sep: &str) -> String {
    let more = items.len().saturating_sub(LIST_LIMIT);
    let mut s = items.into_iter().take(LIST_LIMIT).collect::<Vec<_>>().join(sep);
    if more > 0 {
        s.push_str(&format!("{sep}(+{more} more)"));
    }
    s
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

/// "r12 by user".
fn who(e: &HistoryEntry) -> String {
    format!("r{} by {}", e.revision, e.author)
}

impl Document {
    /// Checks a batch that was planned against `base_revision` (see the module docs).
    ///
    /// `Ok(None)`: the project is still at that revision. `Ok(Some(_))`: it has moved on, but nothing
    /// newer can invalidate the batch. `Err`: stale; the message says what changed since, which ops
    /// are affected and why, and how to continue. Nothing is changed in any case.
    pub fn check_base(&self, base_revision: u64, ops: &[Op]) -> Result<Option<AppliedOver>, EditError> {
        let current = self.revision();
        if base_revision == current {
            return Ok(None);
        }
        if base_revision > current {
            return Err(EditError::new(format!(
                "base_revision {base_revision} is ahead of the project (revision {current}): pass the `revision` of your last receipt or describe_project for this project, or leave base_revision out. Nothing was changed."
            )));
        }
        let newer = self.changes_since(base_revision);
        let fix = format!(
            "Fix: call changes_since {{\"revision\": {base_revision}}}, re-read what you are editing (describe_project), and resend with base_revision: {current} (clip ids and anchors survive the person's edits better than absolute seconds)."
        );

        // What the newer changes did: the ones that retimed the timeline, the ids they touched, and
        // the ones that cannot be inspected.
        let mut retimed: Vec<&HistoryEntry> = Vec::new();
        let mut touched: BTreeMap<Id, &HistoryEntry> = BTreeMap::new();
        let mut opaque: Vec<&HistoryEntry> = Vec::new();
        let mut changes: Vec<String> = Vec::new();
        for &e in &newer {
            // An undo/redo reverts/repeats the ops of the edit it targets.
            let entry_ops: &[Op] = match e.kind {
                HistoryKind::Edit => &e.ops,
                _ => self.history().iter().find(|h| Some(h.revision) == e.target && h.kind == HistoryKind::Edit).map(|h| h.ops.as_slice()).unwrap_or(&[]),
            };
            let what = if e.summary.is_empty() || e.summary == e.label { e.label.clone() } else { format!("{}: {}", e.label, e.summary) };
            let mut line = format!("{} \"{}\"", who(e), short(&what, 140));
            if entry_ops.is_empty() {
                opaque.push(e);
                line.push_str(" [the whole project may have changed]");
            } else {
                let mut ids: Vec<Id> = Vec::new();
                let mut retimes = false;
                for o in entry_ops {
                    let u = op_use(o);
                    retimes |= u.retimes;
                    for id in u.targets {
                        push(&mut ids, &id);
                    }
                }
                let mut tags = Vec::new();
                if retimes {
                    retimed.push(e);
                    tags.push("moved/trimmed/added/removed clips".to_string());
                }
                if !ids.is_empty() {
                    tags.push(format!("changed {}", listed(ids.clone(), ", ")));
                }
                if !tags.is_empty() {
                    line.push_str(&format!(" [{}]", tags.join("; ")));
                }
                for id in ids {
                    touched.insert(id, e);
                }
            }
            changes.push(line);
        }
        let head = format!(
            "stale edit: it was planned against base_revision {base_revision}, but the project is now at revision {current}. Nothing was changed. Since {base_revision}: {}.",
            if changes.is_empty() { "the history holds no entries for the newer revisions".to_string() } else { listed(changes, "; ") }
        );
        if newer.is_empty() || !opaque.is_empty() {
            let why = match opaque.first() {
                Some(e) => format!("{} replaced, reloaded or undid the project in a way the history does not itemise", who(e)),
                None => "what changed is not in the history".to_string(),
            };
            return Err(EditError::new(format!("{head} Affected: every op ({why}, so nothing planned against revision {base_revision} can be trusted). {fix}")));
        }

        let mut affected: Vec<String> = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            let u = op_use(op);
            let mut why: Vec<String> = Vec::new();
            if let (false, Some(first)) = (u.absolute.is_empty(), retimed.first()) {
                let mut fields: Vec<String> = Vec::new();
                for (f, v) in &u.absolute {
                    let s = format!("{f} {v}");
                    if !fields.contains(&s) {
                        fields.push(s);
                    }
                }
                let by = if retimed.len() == 1 { who(first) } else { format!("{} and {} more", who(first), retimed.len() - 1) };
                why.push(format!("{} is absolute timeline seconds, and {by} changed what is where on the timeline", listed(fields, ", ")));
            }
            for id in &u.refs {
                if let Some(e) = touched.get(id) {
                    why.push(format!("it references {id}, which {} changed", who(e)));
                }
            }
            for c in &u.word_clips {
                let asset = self.project.clips.iter().find(|x| &x.id == c).and_then(|x| match &x.source {
                    Source::Media { asset, .. } => Some(asset),
                    _ => None,
                });
                if let Some((a, e)) = asset.and_then(|a| touched.get(a).map(|e| (a, e))) {
                    why.push(format!("it addresses words of {c}, and {} changed its media {a} (word indices may differ)", who(e)));
                }
            }
            if !why.is_empty() {
                affected.push(format!("op #{i} ({}): {}", op.name(), why.join("; ")));
            }
        }
        if affected.is_empty() {
            let mut by_author = BTreeMap::new();
            for e in &newer {
                *by_author.entry(e.author.clone()).or_insert(0) += 1;
            }
            return Ok(Some(AppliedOver { base_revision, newer_changes: newer.len(), by_author }));
        }
        Err(EditError::new(format!("{head} Affected: {}. {fix}", listed(affected, " | "))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn op(v: Value) -> Op {
        Op::from_json(&v).unwrap()
    }

    #[test]
    fn absolute_fields_are_timeline_seconds_only() {
        let abs = |v: Value| op_use(&op(v)).absolute.iter().map(|(f, _)| *f).collect::<Vec<_>>();
        assert_eq!(abs(json!({"op": "split_clip", "clip": "c1", "at": 2.0})), ["at"]);
        assert_eq!(abs(json!({"op": "trim_clip", "clip": "c1", "edge": "end", "to": 2.0})), ["to"]);
        assert_eq!(abs(json!({"op": "lift_range", "from": 1.0, "to": 2.0})), ["from", "to"]);
        assert_eq!(abs(json!({"op": "move_clip", "clip": "c1", "start": 3.0})), ["start"]);
        assert_eq!(abs(json!({"op": "add_marker", "time": 3.0})), ["time"]);
        assert_eq!(abs(json!({"op": "add_clip", "clip": {"track": "v1", "text": "Hi", "start": 4.0}})), ["start"]);
        assert_eq!(abs(json!({"op": "add_suggestion", "label": "x", "ops": [{"op": "split_clip", "clip": "c1", "at": 2.0}]})), ["at"]);
        // Relative, anchored, clip-local and source-local values are not.
        assert!(abs(json!({"op": "trim_clip", "clip": "c1", "edge": "end", "delta": -1.0})).is_empty());
        assert!(abs(json!({"op": "move_clip", "clip": "c1", "start": {"after": "c2"}})).is_empty());
        assert!(abs(json!({"op": "add_keyframe", "clip": "c1", "path": "opacity", "t": 0.5, "v": 1.0})).is_empty());
        assert!(abs(json!({"op": "cut_source", "clip": "c1", "from": 1.0, "to": 2.0})).is_empty());
        assert!(abs(json!({"op": "nudge", "clips": ["c1"], "delta": 0.5})).is_empty());
    }

    #[test]
    fn refs_and_targets() {
        let u = op_use(&op(json!({"op": "move_clip", "clip": "c1", "start": {"after": "c2"}, "track": "v2"})));
        assert_eq!(u.targets, ["c1"]);
        assert_eq!(u.refs, ["c1", "c2", "v2"]);
        assert!(u.retimes);
        let u = op_use(&op(json!({"op": "add_clip", "clip": {"track": "v2", "text": "Hi", "start": {"word": {"clip": "c3", "index": 4}}}})));
        assert!(u.targets.is_empty());
        assert_eq!(u.refs, ["v2", "c3"]);
        assert_eq!(u.word_clips, ["c3"]);
        let u = op_use(&op(json!({"op": "set_param", "clip": "c1", "path": "opacity", "value": 0.5})));
        assert!(!u.retimes);
        // Proposing an edit changes nothing by itself.
        let u = op_use(&op(json!({"op": "add_suggestion", "label": "x", "ops": [{"op": "delete_clip", "clip": "c1"}]})));
        assert!(u.targets.is_empty() && !u.retimes);
        assert_eq!(u.refs, ["c1"]);
    }

    fn doc() -> Document {
        use crate::testutil::*;
        let mut p = project();
        p.clips.push(media_clip("c1", "v1", "mv", Time::At(0.0), 4.0, 0.0));
        p.clips.push(media_clip("c2", "v1", "mv", after("c1", 0.0), 4.0, 10.0));
        p.clips.push(media_clip("c3", "v1", "mv", after("c2", 0.0), 4.0, 20.0));
        Document::new(p)
    }

    #[test]
    fn check_base_follows_the_history() {
        let mut d = doc();
        let base = d.revision();
        let split = [op(json!({"op": "split_clip", "clip": "c3", "at": 9.0}))];
        let fade = [op(json!({"op": "set_param", "clip": "c3", "path": "opacity", "value": 0.5}))];
        assert_eq!(d.check_base(base, &split), Ok(None), "same revision");
        assert!(d.check_base(base + 5, &split).unwrap_err().message.contains("ahead of the project"));

        // An unrelated change: both still apply.
        d.apply("user", "", vec![op(json!({"op": "set_param", "clip": "c2", "path": "opacity", "value": 0.8}))]).unwrap();
        let over = d.check_base(base, &split).unwrap().unwrap();
        assert_eq!((over.newer_changes, over.by_author.get("user")), (1, Some(&1)));
        assert!(over.note().contains("1 newer change(s) since revision 0 (by user)"), "{}", over.note());

        // The person trims c1: everything after it moves. Absolute seconds are stale, ids are not.
        d.apply("user", "", vec![op(json!({"op": "trim_clip", "clip": "c1", "edge": "end", "delta": -1.0}))]).unwrap();
        let e = d.check_base(base, &split).unwrap_err().message;
        assert!(e.starts_with("stale edit") && e.contains("Nothing was changed"), "{e}");
        assert!(e.contains("r2 by user") && e.contains("op #0 (split_clip): at 9 is absolute timeline seconds"), "{e}");
        assert!(e.contains("changes_since {\"revision\": 0}") && e.contains("base_revision: 2"), "{e}");
        assert!(d.check_base(base, &fade).unwrap().is_some());
        let on_c1 = [op(json!({"op": "add_effect", "clip": "c1", "effect": "blur"}))];
        let e = d.check_base(base, &on_c1).unwrap_err().message;
        assert!(e.contains("op #0 (add_effect): it references c1, which r2 by user changed"), "{e}");
        // Planned after the trim: fine again.
        assert_eq!(d.check_base(2, &split), Ok(None));
    }

    #[test]
    fn undo_counts_as_the_edit_it_reverts_and_replacements_are_opaque() {
        let mut d = doc();
        d.apply("ai", "", vec![op(json!({"op": "trim_clip", "clip": "c1", "edge": "end", "delta": -1.0}))]).unwrap();
        let base = d.revision();
        d.undo().unwrap();
        let split = [op(json!({"op": "split_clip", "clip": "c3", "at": 9.0}))];
        let e = d.check_base(base, &split).unwrap_err().message;
        assert!(e.contains("r2 by user") && e.contains("Undo"), "{e}");
        let fade = [op(json!({"op": "set_param", "clip": "c3", "path": "opacity", "value": 0.5}))];
        assert!(d.check_base(base, &fade).unwrap().is_some());

        let base = d.revision();
        let whole = d.project.clone();
        d.replace_project("user", "", whole).unwrap();
        let e = d.check_base(base, &fade).unwrap_err().message;
        assert!(e.contains("Affected: every op"), "{e}");
    }
}
