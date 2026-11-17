//! Op implementations (everything except transcript/caption ops, which live in `captions.rs`).

use super::prims::*;
use super::{fail, Cx, Edge, EditError, EffectArg, Op, SpeedKeep, MAX_SPEED, MIN_DURATION};
use crate::effects::{effect_spec, transition_spec, EFFECTS, TRANSITIONS};
use crate::eval::{consumed, sample, scale_param_time, source_time};
use crate::model::*;
use crate::resolve::{describe_anchor, resolve_raw as resolve, Resolved, EPS};
use crate::util;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

const CODECS: &[&str] = &["h264", "hevc", "av1", "prores", "gif", "png_sequence", "wav"];
const QUALITIES: &[&str] = &["draft", "standard", "high", "lossless"];

fn check_finite(name: &str, v: f64) -> Result<(), EditError> {
    if v.is_finite() {
        Ok(())
    } else {
        fail(format!("{name} must be a finite number (got {v})."))
    }
}

fn span_text(r: &Resolved, id: &str) -> String {
    match r.span(id) {
        Some(s) => format!("[{}–{}]", util::secs(s.start), util::secs(s.end)),
        None => "[unresolved]".into(),
    }
}

fn time_text(t: &Time) -> String {
    match t {
        Time::At(v) => format!("{} s", util::secs(*v)),
        Time::Anchored(a) => describe_anchor(a),
    }
}

/// RFC 7386 JSON merge patch.
fn merge_patch(target: &mut Value, patch: &Value) {
    if let Value::Object(pm) = patch {
        if !target.is_object() {
            *target = Value::Object(Map::new());
        }
        if let Value::Object(tm) = target {
            for (k, v) in pm {
                if v.is_null() {
                    tm.remove(k);
                } else {
                    merge_patch(tm.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
    } else {
        *target = patch.clone();
    }
}

impl Cx<'_> {
    pub(crate) fn run(&mut self, op: &Op) -> Result<(), EditError> {
        match op {
            Op::SetSettings { width, height, fps, sample_rate, background, duration, title_safe, action_safe } => {
                self.set_settings(*width, *height, *fps, *sample_rate, *background, *duration, *title_safe, *action_safe)
            }
            Op::SetProject { name, notes } => {
                if let Some(n) = name {
                    self.p.name = n.clone();
                }
                if let Some(n) = notes {
                    self.p.notes = n.clone();
                }
                self.summary = "Updated project name/notes".into();
                Ok(())
            }
            Op::SetExport { path, codec, quality, hardware, audio_bitrate_kbps, loudness_lufs, true_peak_db, range } => {
                self.set_export(path, codec, quality, *hardware, *audio_bitrate_kbps, *loudness_lufs, *true_peak_db, *range)
            }
            Op::AddMedia { id, path, kind, info, name, notes, tags } => self.add_media(id, path, *kind, info, name, notes, tags),
            Op::RemoveMedia { asset, force } => self.remove_media(asset, *force),
            Op::RelinkMedia { asset, path, info } => self.relink_media(asset, path, info),
            Op::SetMedia { asset, name, notes, tags } => {
                let i = media_ix(self.p, asset)?;
                let m = &mut self.p.media[i];
                if let Some(n) = name {
                    m.name = n.clone();
                }
                if let Some(n) = notes {
                    m.notes = n.clone();
                }
                if let Some(t) = tags {
                    m.tags = t.clone();
                }
                self.summary = format!("Updated media {asset}");
                Ok(())
            }
            Op::AddTrack { id, kind, name, role, index } => self.add_track(id, *kind, name, role, *index),
            Op::RemoveTrack { track, force } => self.remove_track(track, *force),
            Op::MoveTrack { track, index } => {
                let i = track_ix(self.p, track)?;
                if *index >= self.p.tracks.len() {
                    return fail(format!("index {index} is out of range: there are {} tracks (indices 0–{}).", self.p.tracks.len(), self.p.tracks.len() - 1));
                }
                let t = self.p.tracks.remove(i);
                self.p.tracks.insert(*index, t);
                self.summary = format!("Moved track {track} to index {index}");
                Ok(())
            }
            Op::SetTrack { track, name, role, muted, hidden, locked, solo, volume } => {
                let i = track_ix(self.p, track)?;
                if let Some(v) = volume {
                    if !(v.is_finite() && (0.0..=16.0).contains(v)) {
                        return fail(format!("track volume {v} must be a linear gain between 0 and 16 (1 = unchanged)."));
                    }
                }
                let t = &mut self.p.tracks[i];
                if let Some(v) = name {
                    t.name = v.clone();
                }
                if let Some(v) = role {
                    t.role = v.clone();
                }
                if let Some(v) = muted {
                    t.muted = *v;
                }
                if let Some(v) = hidden {
                    t.hidden = *v;
                }
                if let Some(v) = locked {
                    t.locked = *v;
                }
                if let Some(v) = solo {
                    t.solo = *v;
                }
                if let Some(v) = volume {
                    t.volume = *v;
                }
                self.summary = format!("Updated track {track}");
                Ok(())
            }
            Op::AddClip { clip, overwrite } => {
                let c = self.build_clip(clip, None, None, None)?;
                ensure_unlocked(self.p, &c.track)?;
                let id = c.id.clone();
                let track = c.track.clone();
                self.p.clips.push(c);
                if *overwrite {
                    self.overwrite_under(&id)?;
                }
                let r = resolve(self.p);
                self.summary = format!("Added {id} on {track} {}", span_text(&r, &id));
                Ok(())
            }
            Op::AppendClip { track, after, clip } => self.append_clip(track, after.as_deref(), clip),
            Op::InsertClip { track, at, clip, ripple } => self.insert_clip(track, *at, clip, *ripple),
            Op::MoveClip { clip, start, track, overwrite } => self.move_clip(clip, start, track.as_deref(), *overwrite),
            Op::MoveClips { moves, overwrite } => self.move_clips(moves, *overwrite),
            Op::SetUntil { clip, until } => self.set_until(clip, until),
            Op::SetSpacing { clips, track, gap, scale, min_gap, max_gap } => self.set_spacing(clips, track.as_deref(), *gap, *scale, *min_gap, *max_gap),
            Op::CloseGaps { track, max_gap } => {
                if let Some(m) = max_gap {
                    if !(m.is_finite() && *m >= 0.0) {
                        return fail("max_gap must be 0 or more seconds.");
                    }
                }
                let limit = max_gap.unwrap_or(f64::INFINITY);
                let ids = self.track_clip_ids(track)?;
                self.respace(&format!("Closed gaps on {track}"), ids, &|g| if g <= limit + EPS { 0.0 } else { g })
            }
            Op::SetExportSize { width, height, fit } => self.set_export_size(*width, *height, *fit),
            Op::TrimClip { clip, edge, to, delta, ripple } => self.trim_clip(clip, *edge, *to, *delta, *ripple),
            Op::SplitClip { clip, at, new_id, linked } => {
                let ci = editable(self.p, clip)?;
                check_finite("at", *at)?;
                // Splitting detached audio splits its video too (and the reverse), on a frame of the video.
                let lead = if *linked { leader_of(self.p, clip) } else { clip.clone() };
                let lead_track = self.p.clips[clip_ix(self.p, &lead).unwrap_or(ci)].track.clone();
                let at = &snap_for(self.p, &lead_track, *at);
                let right = self.split(&lead, *at, new_id.as_deref(), *linked)?;
                let mine = if &lead == clip {
                    right.clone()
                } else {
                    let track = self.p.clips[clip_ix(self.p, clip)?].track.clone();
                    linked_of(self.p, &right).into_iter().find(|k| self.p.clips.iter().any(|c| &c.id == k && c.track == track)).unwrap_or(right.clone())
                };
                let r = resolve(self.p);
                self.summary = format!("Split {clip} at {} s → {clip} {} + {mine} {}", util::secs(*at), span_text(&r, clip), span_text(&r, &mine));
                Ok(())
            }
            Op::DeleteClip { clip, ripple, linked } => {
                editable(self.p, clip)?;
                let links = if *linked { linked_of(self.p, clip) } else { vec![] };
                self.delete(clip, *ripple, *linked, true)?;
                self.summary = format!(
                    "Deleted {clip}{}{}",
                    if links.is_empty() { String::new() } else { format!(" and linked {}", links.join(", ")) },
                    if *ripple { " (ripple)" } else { "" }
                );
                Ok(())
            }
            Op::DuplicateClip { clip, start, track, new_id, overwrite } => self.duplicate_clip(clip, start.as_ref(), track.as_deref(), new_id.as_deref(), *overwrite),
            Op::SetClip { clip, patch } => self.set_clip(clip, patch),
            Op::SetParam { clip, path, value, keys } => self.set_param(clip, path, *value, keys.as_ref()),
            Op::AddKeyframe { clip, path, t, v, ease } => self.add_keyframe(clip, path, *t, *v, ease.clone().unwrap_or_default()),
            Op::RemoveKeyframe { clip, path, t } => self.remove_keyframe(clip, path, *t),
            Op::SlipClip { clip, delta, linked } => {
                editable(self.p, clip)?;
                let lead = if *linked { leader_of(self.p, clip) } else { clip.clone() };
                let clip = &lead;
                let i = editable(self.p, clip)?;
                check_finite("delta", *delta)?;
                if !matches!(self.p.clips[i].source, Source::Media { .. }) {
                    return fail(format!("slip moves the media under a clip, but {clip} is a {} clip.", kind_word(&self.p.clips[i])));
                }
                let mut all = vec![clip.clone()];
                if *linked {
                    all.extend(linked_of(self.p, clip));
                }
                for id in &all {
                    let j = clip_ix(self.p, id)?;
                    if let Source::Media { in_point, .. } = &mut self.p.clips[j].source {
                        *in_point += delta;
                    }
                }
                self.summary = format!("Slipped {} by {} s", all.join(", "), util::signed(*delta));
                Ok(())
            }
            Op::RollEdit { left, right, delta } => self.roll_edit(left, right, *delta),
            Op::SlideClip { clip, delta } => self.slide_clip(clip, *delta),
            Op::SetSpeed { clip, speed, keep, linked } => self.set_speed(clip, *speed, *keep, *linked),
            Op::FreezeFrame { clip, at, duration, new_id } => self.freeze_frame(clip, *at, *duration, new_id.as_deref()),
            Op::DetachAudio { clip, track, new_id, stream } => self.detach_audio(clip, track, new_id.as_deref(), *stream),
            Op::DetachStreams { clip, streams, tracks, new_ids } => self.detach_streams(clip, streams.as_deref(), tracks.as_deref(), new_ids.as_deref()),
            Op::SetAudioStream { clip, stream } => self.set_audio_stream(clip, *stream),
            Op::ReplaceMedia { clip, asset, in_point } => {
                let i = editable(self.p, clip)?;
                media_ix(self.p, asset)?;
                if let Some(ip) = in_point {
                    check_finite("in_point", *ip)?;
                }
                let new_asset = self.p.media[media_ix(self.p, asset)?].clone();
                let mut reset = None;
                match &mut self.p.clips[i].source {
                    Source::Media { asset: a, in_point: ip, stream } => {
                        *a = asset.clone();
                        if let Some(v) = in_point {
                            *ip = *v;
                        }
                        // The new file may have fewer sound streams: play its first one.
                        if stream_problem(&new_asset, *stream).is_some() {
                            reset = Some(*stream);
                            *stream = 0;
                        }
                    }
                    _ => return fail(format!("{clip} is not a media clip; replace_media swaps the media of video, audio and image clips.")),
                }
                if let Some(k) = reset {
                    self.warn(format!("{asset} has no sound stream {k}, so {clip} now plays its stream 0."));
                }
                self.summary = format!("{clip} now plays {asset}");
                Ok(())
            }
            Op::LiftRange { from, to, tracks } => self.range(*from, *to, tracks.as_ref(), false),
            Op::ExtractRange { from, to, tracks } => self.range(*from, *to, tracks.as_ref(), true),
            Op::Nudge { clips, delta } => {
                check_finite("delta", *delta)?;
                if clips.is_empty() {
                    return fail("`clips` is empty; list the clip ids to nudge.");
                }
                let r = resolve(self.p);
                let mut desired = Vec::new();
                for id in clips {
                    editable(self.p, id)?;
                    desired.push((id.clone(), span_of(&r, id)?.start + delta));
                }
                reposition(self.p, &desired)?;
                self.summary = format!("Nudged {} by {} s", clips.join(", "), util::signed(*delta));
                Ok(())
            }
            Op::AddEffect { clip, effect, index } => self.add_effect(clip, effect, *index),
            Op::RemoveEffect { clip, index } => {
                let i = editable(self.p, clip)?;
                let n = self.p.clips[i].effects.len();
                if *index >= n {
                    return fail(no_effect(clip, *index, n));
                }
                let e = self.p.clips[i].effects.remove(*index);
                self.summary = format!("Removed effect {index} ({}) from {clip}", e.kind);
                Ok(())
            }
            Op::SetEffect { clip, index, enabled, params, colors, strings } => {
                let i = editable(self.p, clip)?;
                let n = self.p.clips[i].effects.len();
                if *index >= n {
                    return fail(no_effect(clip, *index, n));
                }
                let e = &mut self.p.clips[i].effects[*index];
                if let Some(en) = enabled {
                    e.enabled = *en;
                }
                for (k, v) in params {
                    match v {
                        None => {
                            e.params.remove(k);
                        }
                        Some(Param::Animated { keys }) => {
                            e.params.insert(k.clone(), crate::eval::normalize_keys(clean_keys(keys.clone())?));
                        }
                        Some(p) => {
                            e.params.insert(k.clone(), p.clone());
                        }
                    }
                }
                for (k, v) in colors {
                    match v {
                        None => e.colors.remove(k),
                        Some(c) => e.colors.insert(k.clone(), *c),
                    };
                }
                for (k, v) in strings {
                    match v {
                        None => e.strings.remove(k),
                        Some(s) => e.strings.insert(k.clone(), s.clone()),
                    };
                }
                self.summary = format!("Updated effect {index} ({}) of {clip}", e.kind);
                Ok(())
            }
            Op::MoveEffect { clip, from, to } => {
                let i = editable(self.p, clip)?;
                let n = self.p.clips[i].effects.len();
                if *from >= n {
                    return fail(no_effect(clip, *from, n));
                }
                if *to >= n {
                    return fail(format!("target index {to} is out of range: {clip} has {n} effect(s) (indices 0–{}).", n - 1));
                }
                let e = self.p.clips[i].effects.remove(*from);
                self.p.clips[i].effects.insert(*to, e);
                self.summary = format!("Moved effect {from} of {clip} to {to}");
                Ok(())
            }
            Op::AddTransition { from, to, kind, duration, id, params } => self.add_transition(from, to, kind, *duration, id.as_deref(), params),
            Op::SetTransition { transition, kind, duration, params } => self.set_transition(transition, kind.as_deref(), *duration, params),
            Op::RemoveTransition { transition } => {
                let i = named_ix(&self.p.transitions, transition, "transition", |t| &t.id)?;
                let t = self.p.transitions.remove(i);
                self.summary = format!("Removed transition {transition} ({} {}→{})", t.kind, t.from, t.to);
                Ok(())
            }
            Op::SetTransitionColors { transition, colors } => {
                let i = named_ix(&self.p.transitions, transition, "transition", |t| &t.id)?;
                let t = &self.p.transitions[i];
                let spec = transition_spec(&t.kind).ok_or_else(|| EditError::new(format!("transition {transition} has unknown kind {:?}; set a known kind first.", t.kind)))?;
                if colors.is_empty() {
                    return fail("`colors` is empty; give e.g. {\"color\":\"#FF8A2B\"} (null resets one to the default).");
                }
                for k in colors.keys() {
                    if spec.color_default(k).is_none() {
                        let names: Vec<&str> = spec.colors.iter().map(|(n, _)| *n).collect();
                        return fail(format!("transition {} has no colour {k:?} (colours: {}).", spec.kind, if names.is_empty() { "none".into() } else { names.join(", ") }));
                    }
                }
                let t = &mut self.p.transitions[i];
                for (k, v) in colors {
                    match v {
                        Some(c) => t.colors.insert(k.clone(), *c),
                        None => t.colors.remove(k),
                    };
                }
                self.summary = format!("Updated colours of transition {transition}: {}", colors.keys().cloned().collect::<Vec<_>>().join(", "));
                Ok(())
            }
            Op::AddMarker { time, name, kind, id, color, note } => {
                if let Time::At(v) = time {
                    check_finite("time", *v)?;
                }
                let id = match id {
                    Some(i) => {
                        self.ids.claim(i)?;
                        i.clone()
                    }
                    None => self.ids.fresh("mk"),
                };
                self.made.push(id.clone());
                self.p.markers.push(Marker {
                    id: id.clone(),
                    time: time.clone(),
                    name: name.clone().unwrap_or_default(),
                    kind: kind.unwrap_or_default(),
                    color: *color,
                    note: note.clone().unwrap_or_default(),
                });
                self.summary = format!("Added marker {id} at {}", time_text(time));
                Ok(())
            }
            Op::SetMarker { marker, time, name, kind, color, note } => {
                let i = named_ix(&self.p.markers, marker, "marker", |m| &m.id)?;
                let m = &mut self.p.markers[i];
                if let Some(t) = time {
                    m.time = t.clone();
                }
                if let Some(v) = name {
                    m.name = v.clone();
                }
                if let Some(v) = kind {
                    m.kind = *v;
                }
                if let Some(v) = color {
                    m.color = *v;
                }
                if let Some(v) = note {
                    m.note = v.clone();
                }
                self.summary = format!("Updated marker {marker}");
                Ok(())
            }
            Op::AddSuggestion { id, label, reason, at, ops } => {
                if label.trim().is_empty() {
                    return fail("a suggestion needs a label: one sentence saying what applying it does.".to_string());
                }
                if ops.is_empty() {
                    return fail("a suggestion needs ops (the edit to propose).".to_string());
                }
                if let Some(o) = ops.iter().find(|o| matches!(o, Op::AddSuggestion { .. } | Op::RemoveSuggestion { .. })) {
                    return fail(format!("a suggestion's ops cannot contain {} (suggestions do not nest).", o.name()));
                }
                if let Some(t) = at {
                    check_finite("at", *t)?;
                }
                // It must apply to the project as it is now.
                let mut trial = self.p.clone();
                if let Err(e) = super::apply_batch_as(&mut trial, ops, &self.author) {
                    return fail(format!("the suggested ops do not apply to the project as it is: {e}"));
                }
                let id = match id {
                    Some(i) => {
                        self.ids.claim(i)?;
                        i.clone()
                    }
                    None => self.ids.fresh("sg"),
                };
                self.made.push(id.clone());
                self.p.suggestions.push(Suggestion {
                    id: id.clone(),
                    label: label.trim().to_string(),
                    reason: reason.clone().unwrap_or_default().trim().to_string(),
                    at: *at,
                    ops: ops.clone(),
                    author: self.author.clone(),
                });
                self.summary = format!("Suggested {id}: {}", util::short(label.trim(), 60));
                Ok(())
            }
            Op::RemoveSuggestion { suggestion } => {
                let i = named_ix(&self.p.suggestions, suggestion, "suggestion", |s| &s.id)?;
                self.p.suggestions.remove(i);
                self.summary = format!("Removed suggestion {suggestion}");
                Ok(())
            }
            Op::RemoveMarker { marker } => {
                let i = named_ix(&self.p.markers, marker, "marker", |m| &m.id)?;
                let name = self.p.markers[i].name.clone();
                let r = resolve(self.p);
                for n in dependants(self.p, marker) {
                    let nid = node_id(self.p, n).to_string();
                    let cur = node_value(self.p, &r, n);
                    if let Some(v) = cur {
                        *node_time(self.p, n) = Time::At(v);
                        let label = if name.is_empty() { String::new() } else { format!(" {name:?}") };
                        self.warn(format!("{nid} was anchored to the removed marker{label}; it keeps its time ({} s) as an absolute time.", util::secs(v)));
                    }
                }
                self.p.markers.remove(i);
                self.summary = format!("Removed marker {marker}");
                Ok(())
            }
            Op::AddDucking { target, trigger, amount_db, threshold_db, attack, release, id } => {
                track_ix(self.p, target)?;
                track_ix(self.p, trigger)?;
                if target == trigger {
                    return fail("a track cannot duck itself: `target` (lowered) and `trigger` (e.g. the voice track) must differ.");
                }
                let mut d = Ducking {
                    id: String::new(),
                    target: target.clone(),
                    trigger: trigger.clone(),
                    amount_db: -14.0,
                    threshold_db: -45.0,
                    attack: 0.12,
                    release: 0.45,
                };
                set_duck_values(&mut d, *amount_db, *threshold_db, *attack, *release)?;
                d.id = match id {
                    Some(i) => {
                        self.ids.claim(i)?;
                        i.clone()
                    }
                    None => self.ids.fresh("d"),
                };
                self.made.push(d.id.clone());
                self.summary = format!("Added ducking {}: {target} ducks {} dB while {trigger} has sound", d.id, util::num(d.amount_db));
                self.p.ducking.push(d);
                Ok(())
            }
            Op::SetDucking { ducking, target, trigger, amount_db, threshold_db, attack, release } => {
                let i = named_ix(&self.p.ducking, ducking, "ducking", |d| &d.id)?;
                if let Some(t) = target {
                    track_ix(self.p, t)?;
                }
                if let Some(t) = trigger {
                    track_ix(self.p, t)?;
                }
                let mut d = self.p.ducking[i].clone();
                if let Some(t) = target {
                    d.target = t.clone();
                }
                if let Some(t) = trigger {
                    d.trigger = t.clone();
                }
                if d.target == d.trigger {
                    return fail("a track cannot duck itself: `target` and `trigger` must differ.");
                }
                set_duck_values(&mut d, *amount_db, *threshold_db, *attack, *release)?;
                self.p.ducking[i] = d;
                self.summary = format!("Updated ducking {ducking}");
                Ok(())
            }
            Op::RemoveDucking { ducking } => {
                let i = named_ix(&self.p.ducking, ducking, "ducking", |d| &d.id)?;
                self.p.ducking.remove(i);
                self.summary = format!("Removed ducking {ducking}");
                Ok(())
            }
            Op::SetTranscript { asset, transcript, force } => self.set_transcript(asset, transcript, *force),
            Op::DeleteWords { clip, from, to, pad, edges } => self.delete_words(clip, *from, *to, *pad, *edges),
            Op::CutSource { clip, from, to, linked } => self.cut_source_op(clip, *from, *to, *linked),
            Op::GenerateCaptions { clips, track, style, max_words, max_chars, pause, replace } => {
                self.generate_captions(clips, track, style.as_ref(), *max_words, *max_chars, *pause, *replace)
            }
        }
    }

    // ----- project -----

    #[allow(clippy::too_many_arguments)]
    fn set_settings(
        &mut self,
        width: Option<u32>,
        height: Option<u32>,
        fps: Option<f64>,
        sample_rate: Option<u32>,
        background: Option<Color>,
        duration: Option<Option<f64>>,
        title_safe: Option<f64>,
        action_safe: Option<f64>,
    ) -> Result<(), EditError> {
        let mut s = self.p.settings.clone();
        for (name, v) in [("width", width), ("height", height)] {
            if let Some(v) = v {
                if v == 0 || v > 16384 {
                    return fail(format!("{name} must be 1–16384 px (got {v})."));
                }
                if v % 2 == 1 {
                    self.warn(format!("{name} {v} is odd; H.264/HEVC need even frame sizes."));
                }
            }
        }
        if let Some(w) = width {
            s.width = w;
        }
        if let Some(h) = height {
            s.height = h;
        }
        if let Some(f) = fps {
            if !(f.is_finite() && f > 0.0 && f <= 1000.0) {
                return fail(format!("fps must be between 0 and 1000 (got {f}); e.g. 24, 29.97, 30, 60."));
            }
            s.fps = f;
        }
        if let Some(r) = sample_rate {
            if !(8000..=384_000).contains(&r) {
                return fail(format!("sample_rate must be 8000–384000 Hz (got {r}); 48000 is standard for video."));
            }
            s.sample_rate = r;
        }
        if let Some(b) = background {
            s.background = b;
        }
        if let Some(d) = duration {
            if let Some(v) = d {
                if !(v.is_finite() && v > 0.0) {
                    return fail(format!("duration must be more than 0 s (got {v}); use null to end with the last clip."));
                }
            }
            s.duration = d;
        }
        for (name, v) in [("title_safe", title_safe), ("action_safe", action_safe)] {
            if let Some(v) = v {
                if !(v.is_finite() && v > 0.0 && v <= 1.0) {
                    return fail(format!("{name} is a fraction of the frame between 0 and 1 (got {v}); typical: title 0.8, action 0.9."));
                }
            }
        }
        if let Some(v) = title_safe {
            s.title_safe = v;
        }
        if let Some(v) = action_safe {
            s.action_safe = v;
        }
        self.summary = format!(
            "Settings: {}×{} @ {} fps, {} Hz{}",
            s.width,
            s.height,
            util::num(s.fps),
            s.sample_rate,
            s.duration.map(|d| format!(", fixed {} s", util::num(d))).unwrap_or_default()
        );
        self.p.settings = s;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn set_export(
        &mut self,
        path: &Option<Option<String>>,
        codec: &Option<String>,
        quality: &Option<String>,
        hardware: Option<bool>,
        audio_bitrate_kbps: Option<u32>,
        loudness_lufs: Option<Option<f64>>,
        true_peak_db: Option<f64>,
        range: Option<Option<[f64; 2]>>,
    ) -> Result<(), EditError> {
        let mut e = self.p.export.clone();
        if let Some(p) = path {
            e.path = p.clone();
        }
        if let Some(c) = codec {
            if !CODECS.contains(&c.as_str()) {
                return fail(format!("unknown codec {c:?}.{} Codecs: {}.", util::did_you_mean(c, CODECS.iter().copied()), CODECS.join(", ")));
            }
            e.codec = c.clone();
        }
        if let Some(q) = quality {
            if !QUALITIES.contains(&q.as_str()) {
                return fail(format!("unknown quality {q:?}; use one of {}.", QUALITIES.join(", ")));
            }
            e.quality = q.clone();
        }
        if let Some(h) = hardware {
            e.hardware = h;
        }
        if let Some(b) = audio_bitrate_kbps {
            if !(32..=1024).contains(&b) {
                return fail(format!("audio_bitrate_kbps must be 32–1024 (got {b})."));
            }
            e.audio_bitrate_kbps = b;
        }
        if let Some(l) = loudness_lufs {
            if let Some(v) = l {
                if !(v.is_finite() && (-70.0..=0.0).contains(&v)) {
                    return fail(format!("loudness_lufs must be between -70 and 0 (got {v}); YouTube uses -14."));
                }
            }
            e.loudness_lufs = l;
        }
        if let Some(t) = true_peak_db {
            if !(t.is_finite() && (-20.0..=0.0).contains(&t)) {
                return fail(format!("true_peak_db must be between -20 and 0 (got {t})."));
            }
            e.true_peak_db = t;
        }
        if let Some(r) = range {
            if let Some([a, b]) = r {
                if !(a.is_finite() && b.is_finite() && a >= 0.0 && b > a) {
                    return fail(format!("export range [{a}, {b}] must be [start, end] with 0 ≤ start < end."));
                }
            }
            e.range = r;
        }
        self.p.export = e;
        self.summary = "Updated export settings".into();
        Ok(())
    }

    // ----- media and tracks -----

    #[allow(clippy::too_many_arguments)]
    fn add_media(
        &mut self,
        id: &Option<Id>,
        path: &str,
        kind: MediaKind,
        info: &MediaInfo,
        name: &Option<String>,
        notes: &Option<String>,
        tags: &[String],
    ) -> Result<(), EditError> {
        if path.trim().is_empty() {
            return fail("`path` is empty; give the media file's path.");
        }
        if !(info.duration.is_finite() && info.duration >= 0.0) {
            return fail("info.duration must be 0 or more seconds.");
        }
        let id = match id {
            Some(i) => {
                self.ids.claim(i)?;
                i.clone()
            }
            None => self.ids.fresh("m"),
        };
        self.made.push(id.clone());
        let path = path.replace('\\', "/");
        if let Some(other) = self.p.media.iter().find(|m| m.path == path) {
            let other = other.id.clone();
            self.warn(format!("{other} already uses this file; {id} is a second asset for it."));
        }
        self.summary = format!("Added media {id} \"{}\"", util::file_name(&path));
        self.p.media.push(MediaAsset {
            id,
            name: name.clone().unwrap_or_default(),
            path,
            kind,
            info: info.clone(),
            tags: tags.to_vec(),
            notes: notes.clone().unwrap_or_default(),
        });
        Ok(())
    }

    fn remove_media(&mut self, asset: &str, force: bool) -> Result<(), EditError> {
        let mi = media_ix(self.p, asset)?;
        let users: Vec<Id> = self
            .p
            .clips
            .iter()
            .filter(|c| matches!(&c.source, Source::Media { asset: a, .. } if a == asset))
            .map(|c| c.id.clone())
            .collect();
        if !users.is_empty() && !force {
            return fail(format!(
                "{asset} is used by {} clip(s): {}. Delete those clips first, or pass force: true to delete them too.",
                users.len(),
                util::join_limited(&users, 12)
            ));
        }
        for u in &users {
            if clip_ix(self.p, u).is_ok() {
                self.delete(u, false, false, false)?;
            }
        }
        self.p.transcripts.remove(asset);
        self.p.media.remove(mi);
        self.summary = if users.is_empty() {
            format!("Removed media {asset}")
        } else {
            format!("Removed media {asset} and its {} clip(s)", users.len())
        };
        Ok(())
    }

    fn relink_media(&mut self, asset: &str, path: &str, info: &Option<MediaInfo>) -> Result<(), EditError> {
        let mi = media_ix(self.p, asset)?;
        if path.trim().is_empty() {
            return fail("`path` is empty; give the new file path.");
        }
        self.p.media[mi].path = path.replace('\\', "/");
        if let Some(i) = info {
            self.p.media[mi].info = i.clone();
        }
        let dur = self.p.media[mi].info.duration;
        if dur > 0.0 {
            let over: Vec<String> = self
                .p
                .clips
                .iter()
                .filter(|c| matches!(&c.source, Source::Media { asset: a, .. } if a == asset))
                .filter(|c| crate::eval::source_span(c).map(|(_, hi)| hi > dur + 1e-3).unwrap_or(false))
                .map(|c| c.id.clone())
                .collect();
            if !over.is_empty() {
                self.warn(format!("the new file is {} s long; {} now play past its end (validate reports clip_past_media_end).", util::num(dur), over.join(", ")));
            }
        }
        self.summary = format!("Relinked {asset} to \"{}\"", util::file_name(path));
        Ok(())
    }

    fn add_track(&mut self, id: &Option<Id>, kind: TrackKind, name: &Option<String>, role: &Option<String>, index: Option<usize>) -> Result<(), EditError> {
        let id = match id {
            Some(i) => {
                self.ids.claim(i)?;
                i.clone()
            }
            None => self.ids.fresh(if kind == TrackKind::Video { "v" } else { "a" }),
        };
        self.made.push(id.clone());
        let t = Track {
            id: id.clone(),
            name: name.clone().unwrap_or_default(),
            kind,
            role: role.clone().unwrap_or_default(),
            muted: false,
            hidden: false,
            locked: false,
            solo: false,
            volume: 1.0,
        };
        match index {
            Some(i) if i > self.p.tracks.len() => {
                return fail(format!("index {i} is out of range: there are {} tracks, so use 0–{}.", self.p.tracks.len(), self.p.tracks.len()));
            }
            Some(i) => self.p.tracks.insert(i, t),
            None => self.p.tracks.push(t),
        }
        self.summary = format!("Added {} track {id}", if kind == TrackKind::Video { "video" } else { "audio" });
        Ok(())
    }

    fn remove_track(&mut self, track: &str, force: bool) -> Result<(), EditError> {
        let ti = track_ix(self.p, track)?;
        ensure_unlocked(self.p, track)?;
        let clips: Vec<Id> = self.p.clips.iter().filter(|c| c.track == track).map(|c| c.id.clone()).collect();
        let ducks: Vec<Id> = self.p.ducking.iter().filter(|d| d.target == track || d.trigger == track).map(|d| d.id.clone()).collect();
        if !force && (!clips.is_empty() || !ducks.is_empty()) {
            let mut why = Vec::new();
            if !clips.is_empty() {
                why.push(format!("it has {} clip(s): {}", clips.len(), util::join_limited(&clips, 12)));
            }
            if !ducks.is_empty() {
                why.push(format!("ducking {} uses it", ducks.join(", ")));
            }
            return fail(format!("cannot remove track {track}: {}. Move or delete them first, or pass force: true.", why.join("; ")));
        }
        for c in &clips {
            if clip_ix(self.p, c).is_ok() {
                self.delete(c, false, false, false)?;
            }
        }
        self.p.ducking.retain(|d| d.target != track && d.trigger != track);
        self.p.tracks.remove(ti);
        self.summary = format!("Removed track {track}");
        Ok(())
    }

    // ----- clips -----

    fn append_clip(&mut self, track: &str, after: Option<&str>, clip: &Value) -> Result<(), EditError> {
        track_ix(self.p, track)?;
        ensure_unlocked(self.p, track)?;
        let r = resolve(self.p);
        let after_id = match after {
            Some(a) => {
                clip_ix(self.p, a)?;
                Some(a.to_string())
            }
            None => track_end(self.p, &r, track).1,
        };
        let start = match &after_id {
            Some(a) => Time::Anchored(Anchor { to: AnchorTarget::After(a.clone()), offset: 0.0 }),
            None => Time::At(0.0),
        };
        let c = self.build_clip(clip, Some(track), Some(start), Some("append_clip places the clip after `after` (or the last clip of the track)."))?;
        let id = c.id.clone();
        let mut moved = Vec::new();
        if let Some(a) = &after_id {
            for k in self.p.clips.iter_mut() {
                if k.track == track {
                    if let Time::Anchored(an) = &mut k.start {
                        if matches!(&an.to, AnchorTarget::After(x) if x == a) {
                            an.to = AnchorTarget::After(id.clone());
                            moved.push(k.id.clone());
                        }
                    }
                }
            }
        }
        self.p.clips.push(c);
        let r = resolve(self.p);
        self.summary = format!(
            "Appended {id} on {track} {}{}{}",
            span_text(&r, &id),
            after_id.map(|a| format!(" after {a}")).unwrap_or_default(),
            if moved.is_empty() { String::new() } else { format!("; {} now follow(s) it", moved.join(", ")) }
        );
        Ok(())
    }

    fn insert_clip(&mut self, track: &str, at: f64, clip: &Value, ripple: bool) -> Result<(), EditError> {
        track_ix(self.p, track)?;
        ensure_unlocked(self.p, track)?;
        check_finite("at", at)?;
        if at < 0.0 {
            return fail("`at` must be 0 or later.");
        }
        let at = snap_for(self.p, track, at);
        let mut c = self.build_clip(clip, Some(track), Some(Time::At(at)), Some("insert_clip places the clip at `at`."))?;
        let id = c.id.clone();
        let d = c.duration;
        let r = resolve(self.p);
        let under = r.on_track(self.p, track).into_iter().find(|(_, s)| s.start < at - MIN_DURATION && s.end > at + MIN_DURATION).map(|(c, _)| c.id.clone());
        if ripple {
            if let Some(u) = &under {
                self.split(u, at, None, true)?;
            }
            let r2 = resolve(self.p);
            let left = r2
                .on_track(self.p, track)
                .into_iter()
                .filter(|(_, s)| (s.end - at).abs() < 1e-6)
                .max_by(|a, b| a.1.start.total_cmp(&b.1.start))
                .map(|(c, _)| c.id.clone());
            if let Some(l) = &left {
                c.start = Time::Anchored(Anchor { to: AnchorTarget::After(l.clone()), offset: 0.0 });
            }
            let later: Vec<(Id, f64)> = r2.on_track(self.p, track).into_iter().filter(|(_, s)| s.start >= at - EPS).map(|(c, s)| (c.id.clone(), s.start + d)).collect();
            if let Some(l) = &left {
                for k in self.p.clips.iter_mut() {
                    if k.track == track {
                        if let Time::Anchored(an) = &mut k.start {
                            if matches!(&an.to, AnchorTarget::After(x) if x == l) {
                                an.to = AnchorTarget::After(id.clone());
                            }
                        }
                    }
                }
            }
            self.p.clips.push(c);
            reposition_rigid(self.p, &later, &r2)?;
            let r = resolve(self.p);
            self.summary = format!(
                "Inserted {id} on {track} {} (ripple: {} clip(s) moved {} s later{})",
                span_text(&r, &id),
                later.len(),
                util::num(d),
                under.map(|u| format!("; split {u}")).unwrap_or_default()
            );
        } else {
            let left = r.on_track(self.p, track).into_iter().filter(|(_, s)| (s.end - at).abs() < 1e-6).map(|(c, _)| c.id.clone()).next();
            if let Some(l) = left {
                c.start = Time::Anchored(Anchor { to: AnchorTarget::After(l), offset: 0.0 });
            }
            self.p.clips.push(c);
            self.overwrite_under(&id)?;
            let r = resolve(self.p);
            self.summary = format!("Inserted {id} on {track} {} (overwrite)", span_text(&r, &id));
        }
        Ok(())
    }

    fn move_clip(&mut self, clip: &str, start: &Time, track: Option<&str>, overwrite: bool) -> Result<(), EditError> {
        let i = editable(self.p, clip)?;
        if let Time::At(v) = start {
            check_finite("start", *v)?;
        }
        if let Some(t) = track {
            track_ix(self.p, t)?;
            ensure_unlocked(self.p, t)?;
            if self.p.clips[i].track != t {
                let n0 = self.p.transitions.len();
                self.p.transitions.retain(|tr| tr.from != clip && tr.to != clip);
                if self.p.transitions.len() < n0 {
                    self.warn(format!("removed the transition(s) of {clip}: it left its track."));
                }
                self.p.clips[i].track = t.to_string();
            }
        }
        self.p.clips[i].start = start.clone();
        if overwrite {
            self.overwrite_under(clip)?;
        }
        let r = resolve(self.p);
        self.summary = format!("Moved {clip} to {}{}", span_text(&r, clip), match start {
            Time::Anchored(a) => format!(" ({})", describe_anchor(a)),
            Time::At(_) => String::new(),
        });
        Ok(())
    }

    fn move_clips(&mut self, moves: &[super::ClipMove], overwrite: bool) -> Result<(), EditError> {
        if moves.is_empty() {
            return fail("`moves` is empty; list entries like {\"clip\":\"c3\",\"start\":12.0,\"track\":\"v2\"}.");
        }
        let mut seen: HashSet<&str> = HashSet::new();
        for m in moves {
            editable(self.p, &m.clip)?;
            if !seen.insert(m.clip.as_str()) {
                return fail(format!("{} is listed twice in `moves`; give each clip once.", m.clip));
            }
            if let Time::At(v) = m.start {
                check_finite("start", v)?;
            }
            if let Some(t) = &m.track {
                track_ix(self.p, t)?;
                ensure_unlocked(self.p, t)?;
            }
        }
        let old_track: std::collections::HashMap<Id, Id> = self.p.clips.iter().filter(|c| seen.contains(c.id.as_str())).map(|c| (c.id.clone(), c.track.clone())).collect();
        for m in moves {
            let i = clip_ix(self.p, &m.clip)?;
            if let Some(t) = &m.track {
                self.p.clips[i].track = t.clone();
            }
            self.p.clips[i].start = m.start.clone();
        }
        // Transitions survive only if both of their clips are still on one track.
        let track_of = |p: &Project, id: &str| p.clips.iter().find(|c| c.id == id).map(|c| c.track.clone());
        let n0 = self.p.transitions.len();
        let keep: Vec<bool> = self
            .p
            .transitions
            .iter()
            .map(|tr| {
                let moved = |id: &str| old_track.get(id).is_some_and(|old| track_of(self.p, id).as_deref() != Some(old.as_str()));
                let (mf, mt) = (moved(&tr.from), moved(&tr.to));
                (!mf && !mt) || (mf && mt && track_of(self.p, &tr.from) == track_of(self.p, &tr.to))
            })
            .collect();
        let mut k = keep.iter();
        self.p.transitions.retain(|_| *k.next().unwrap_or(&true));
        if self.p.transitions.len() < n0 {
            self.warn(format!("removed {} transition(s) whose clips no longer share a track.", n0 - self.p.transitions.len()));
        }
        if overwrite {
            let set: HashSet<Id> = seen.iter().map(|s| s.to_string()).collect();
            for m in moves {
                let r = resolve(self.p);
                let span = span_of(&r, &m.clip)?;
                let track = self.p.clips[clip_ix(self.p, &m.clip)?].track.clone();
                self.lift(span.start, span.end, &[track], &set, false)?;
            }
        }
        let r = resolve(self.p);
        let parts: Vec<String> = moves.iter().map(|m| format!("{} {}", m.clip, span_text(&r, &m.clip))).collect();
        self.summary = format!("Moved {}", util::join_limited(&parts, 8));
        Ok(())
    }

    fn set_until(&mut self, clip: &str, until: &Option<Option<Time>>) -> Result<(), EditError> {
        let ci = editable(self.p, clip)?;
        let Some(until) = until else {
            return fail("`until` is required: an end anchor like {\"after\":\"c3\"} (ends when c3 ends), {\"with\":\"c4\"} (when c4 starts), a marker, a word, seconds, or null to clear it.");
        };
        if let Some(Time::At(v)) = until {
            check_finite("until", *v)?;
        }
        if until.is_some() && matches!(&self.p.clips[ci].start, Time::Anchored(Anchor { to: AnchorTarget::Before(_), .. })) {
            return fail(format!("{clip} starts `before` another clip, so its length cannot also come from an end anchor; anchor its start `with`/`after` something first (move_clip)."));
        }
        let before = resolve(self.p).span(clip);
        self.p.clips[ci].until = until.clone();
        if until.is_none() {
            // Keep the length it had.
            if let Some(s) = before {
                self.p.clips[ci].duration = s.duration();
            }
        }
        self.normalize_until();
        let r = crate::resolve::resolve(self.p);
        self.summary = match until {
            Some(t) => format!("{clip} now runs until {} → {}", crate::describe::until_text(t), span_text(&r, clip)),
            None => format!("{clip} has no end anchor any more ({} s long)", util::num(self.p.clips[ci].duration)),
        };
        Ok(())
    }

    /// Keeps the stored length of clips with an end anchor equal to what their anchors give.
    pub(crate) fn normalize_until(&mut self) {
        if !self.p.clips.iter().any(|c| c.until.is_some()) {
            return;
        }
        let r = resolve(self.p);
        for c in self.p.clips.iter_mut().filter(|c| c.until.is_some()) {
            if let Some(s) = r.span(&c.id) {
                if (c.duration - s.duration()).abs() > 1e-12 {
                    c.duration = s.duration();
                }
            }
        }
    }

    /// Fills `WordRef::text` of word anchors this op created or changed.
    pub(crate) fn fill_word_texts(&mut self, before: &Project) {
        let word_text = |p: &Project, w: &WordRef| -> Option<String> {
            let c = p.clips.iter().find(|c| c.id == w.clip)?;
            let Source::Media { asset, .. } = &c.source else { return None };
            Some(p.transcripts.get(asset)?.words.get(w.index)?.text.trim().to_string())
        };
        let fill = |p: &Project, t: &Time| -> Option<Time> {
            let Time::Anchored(Anchor { to: AnchorTarget::Word(w), offset }) = t else { return None };
            if w.text.is_some() {
                return None;
            }
            let mut w2 = w.clone();
            w2.text = Some(word_text(p, w)?);
            Some(Time::Anchored(Anchor { to: AnchorTarget::Word(w2), offset: *offset }))
        };
        let old_clips: HashMap<&str, &Clip> = before.clips.iter().map(|c| (c.id.as_str(), c)).collect();
        let old_marks: HashMap<&str, &Marker> = before.markers.iter().map(|m| (m.id.as_str(), m)).collect();
        let mut updates: Vec<(usize, Option<Time>, Option<Time>)> = Vec::new();
        for (i, c) in self.p.clips.iter().enumerate() {
            let old = old_clips.get(c.id.as_str());
            let start = if old.is_none_or(|o| o.start != c.start) { fill(self.p, &c.start) } else { None };
            let until = match &c.until {
                Some(u) if old.is_none_or(|o| o.until.as_ref() != Some(u)) => fill(self.p, u),
                _ => None,
            };
            if start.is_some() || until.is_some() {
                updates.push((i, start, until));
            }
        }
        for (i, s, u) in updates {
            if let Some(s) = s {
                self.p.clips[i].start = s;
            }
            if let Some(u) = u {
                self.p.clips[i].until = Some(u);
            }
        }
        let mut mupd: Vec<(usize, Time)> = Vec::new();
        for (i, m) in self.p.markers.iter().enumerate() {
            if old_marks.get(m.id.as_str()).is_none_or(|o| o.time != m.time) {
                if let Some(t) = fill(self.p, &m.time) {
                    mupd.push((i, t));
                }
            }
        }
        for (i, t) in mupd {
            self.p.markers[i].time = t;
        }
    }

    fn track_clip_ids(&self, track: &str) -> Result<Vec<Id>, EditError> {
        track_ix(self.p, track)?;
        Ok(self.p.clips.iter().filter(|c| c.track == track).map(|c| c.id.clone()).collect())
    }

    #[allow(clippy::too_many_arguments)]
    fn set_spacing(&mut self, clips: &[Id], track: Option<&str>, gap: Option<f64>, scale: Option<f64>, min_gap: Option<f64>, max_gap: Option<f64>) -> Result<(), EditError> {
        for (name, v) in [("gap", gap), ("scale", scale), ("min_gap", min_gap), ("max_gap", max_gap)] {
            if let Some(v) = v {
                if !(v.is_finite() && v >= 0.0) {
                    return fail(format!("{name} must be 0 or more (got {v})."));
                }
            }
        }
        if let (Some(a), Some(b)) = (min_gap, max_gap) {
            if a > b {
                return fail(format!("min_gap ({}) is larger than max_gap ({}).", util::num(a), util::num(b)));
            }
        }
        if gap.is_none() && scale.is_none() && min_gap.is_none() && max_gap.is_none() {
            return fail("give `gap` (seconds), `scale` (factor), `min_gap` and/or `max_gap`, e.g. {\"op\":\"set_spacing\",\"track\":\"a1\",\"scale\":0.7}.");
        }
        let (ids, label) = match (clips.is_empty(), track) {
            (false, None) => {
                for id in clips {
                    clip_ix(self.p, id)?;
                }
                (clips.to_vec(), "Spacing".to_string())
            }
            (true, Some(t)) => (self.track_clip_ids(t)?, format!("Spacing on {t}")),
            (false, Some(_)) => return fail("give either `clips` or `track`, not both."),
            (true, None) => return fail("give `clips` (the clips whose gaps to change, in any order) or `track`."),
        };
        self.respace(&label, ids, &|g| {
            let mut n = match (gap, scale) {
                (Some(x), _) => x,
                (None, Some(s)) => g * s,
                (None, None) => g,
            };
            if let Some(m) = min_gap {
                n = n.max(m);
            }
            if let Some(m) = max_gap {
                n = n.min(m);
            }
            n
        })
    }

    /// Sets every gap between consecutive clips (time order) to `policy(gap)`; overlapping neighbours
    /// are left alone. Later clips move by the accumulated change, through their anchors.
    fn respace(&mut self, label: &str, ids: Vec<Id>, policy: &dyn Fn(f64) -> f64) -> Result<(), EditError> {
        let shown = crate::resolve::resolve(self.p);
        let raw = resolve(self.p);
        let mut list: Vec<(Id, crate::resolve::Span)> = Vec::new();
        for id in ids {
            editable(self.p, &id)?;
            list.push((id.clone(), span_of(&shown, &id)?));
        }
        list.sort_by(|a, b| a.1.start.total_cmp(&b.1.start).then(a.0.cmp(&b.0)));
        if list.len() < 2 {
            return fail("there are fewer than two clips, so there is no gap to change.");
        }
        let mut shift = 0.0;
        let mut end = list[0].1.end;
        let mut desired: Vec<(Id, f64)> = Vec::new();
        let mut gaps = Vec::new();
        let mut text = Vec::new();
        for k in 1..list.len() {
            let (id, s) = &list[k];
            let g = s.start - end;
            if g >= -EPS {
                let ng = policy(g.max(0.0));
                shift += ng - g.max(0.0);
                gaps.push(serde_json::json!({"from": list[k - 1].0, "to": id, "before": g.max(0.0), "after": ng}));
                if (ng - g).abs() > 1e-9 {
                    text.push(format!("{}→{} {}→{} s", list[k - 1].0, id, util::num(g.max(0.0)), util::num(ng)));
                }
            }
            if shift != 0.0 {
                desired.push((id.clone(), span_of(&raw, id)?.start + shift));
            }
            end = end.max(s.end);
        }
        reposition_rigid(self.p, &desired, &raw)?;
        self.details = serde_json::json!({ "gaps": gaps });
        self.summary = if text.is_empty() { format!("{label}: no gap changed") } else { format!("{label}: {}", util::join_limited(&text, 12)) };
        Ok(())
    }

    fn set_export_size(&mut self, width: Option<Option<u32>>, height: Option<Option<u32>>, fit: Option<Fit>) -> Result<(), EditError> {
        for (name, v) in [("width", width), ("height", height)] {
            if let Some(Some(v)) = v {
                if !(2..=16384).contains(&v) {
                    return fail(format!("export {name} must be 2–16384 px (got {v}); use null for the project's size."));
                }
                if v % 2 == 1 {
                    self.warn(format!("export {name} {v} is odd; H.264/HEVC need even frame sizes."));
                }
            }
        }
        let mut e = self.p.export.clone();
        if let Some(w) = width {
            e.width = w;
        }
        if let Some(h) = height {
            e.height = h;
        }
        if let Some(f) = fit {
            e.fit = f;
        }
        let s = &self.p.settings;
        let (ow, oh) = e.output_size(s);
        let fit_name = serde_json::to_value(e.fit).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
        if e.width.is_none() && e.height.is_none() {
            self.summary = format!("Export frame: the project's size ({}×{})", s.width, s.height);
        } else {
            self.summary = format!("Export frame: {ow}×{oh}, fit {fit_name} (project {}×{})", s.width, s.height);
            let (pa, oa) = (s.width as f64 / s.height.max(1) as f64, ow as f64 / oh.max(1) as f64);
            if (pa - oa).abs() / pa > 0.005 && e.fit == Fit::Contain {
                self.warn(format!(
                    "the output aspect differs from the project's ({}×{} vs {}×{}), so fit contain adds {} bars; use fit cover to fill by cropping.",
                    ow,
                    oh,
                    s.width,
                    s.height,
                    if oa > pa { "pillarbox (left/right)" } else { "letterbox (top/bottom)" }
                ));
            }
        }
        self.p.export = e;
        Ok(())
    }

    fn trim_clip(&mut self, clip: &str, edge: Edge, to: Option<f64>, delta: Option<f64>, ripple: Option<bool>) -> Result<(), EditError> {
        editable(self.p, clip)?;
        // Trimming detached audio trims its video too (and the reverse): work on the leader.
        let given = clip;
        let lead = leader_of(self.p, clip);
        let clip = lead.as_str();
        let ci = editable(self.p, clip)?;
        let r = resolve(self.p);
        let span = span_of(&r, clip)?;
        let delta = match (to, delta) {
            (Some(t), None) => {
                check_finite("to", t)?;
                match edge {
                    Edge::Start => t - span.start,
                    Edge::End => t - span.end,
                }
            }
            (None, Some(d)) => {
                check_finite("delta", d)?;
                d
            }
            (Some(_), Some(_)) => return fail("give either `to` (absolute seconds for the edge) or `delta` (seconds), not both."),
            (None, None) => return fail("give `to` (absolute seconds for the edge) or `delta` (seconds)."),
        };
        let c0 = self.p.clips[ci].clone();
        let asked = delta;
        let shown0 = crate::resolve::resolve(self.p).span(clip);
        let grid = self.p.tracks.iter().any(|t| t.id == c0.track && t.kind == TrackKind::Video).then_some(self.p.settings.fps);
        // On video tracks the trimmed edge lands on a frame of the project grid.
        let shown = shown0.unwrap_or(span);
        let delta = if grid.is_some() {
            let fps = self.p.settings.fps;
            let snap = |t: f64| crate::resolve::snap_to_frame(t, fps);
            match (edge, to) {
                // An absolute target: the edge lands on that frame.
                (Edge::Start, Some(t)) => snap(t) - span.start,
                (Edge::End, Some(t)) => snap(t) - span.end,
                // A change in seconds: whole frames, so the edge (and anything rippled) moves by exactly
                // that many frames on screen.
                (_, None) => whole_frames(self.p, &c0.track, delta),
            }
        } else {
            delta
        };
        let links = linked_of(self.p, clip);
        let mut all = vec![clip.to_string()];
        all.extend(links.iter().cloned());
        let exempt: HashSet<Id> = all.iter().cloned().collect();
        let tracks: HashSet<Id> = all.iter().filter_map(|id| self.p.clips.iter().find(|c| &c.id == id)).map(|c| c.track.clone()).collect();
        let later = |p: &Project, d: f64| -> Vec<(Id, f64)> {
            p.clips
                .iter()
                .filter(|c| tracks.contains(&c.track) && !exempt.contains(&c.id))
                .filter_map(|c| r.span(&c.id).filter(|s| s.start >= span.end - EPS).map(|s| (c.id.clone(), s.start + d)))
                .collect()
        };
        let (new_dur, which) = match edge {
            Edge::End => (c0.duration + delta, "end"),
            Edge::Start => (c0.duration - delta, "start"),
        };
        if new_dur < MIN_DURATION {
            return fail(format!(
                "trimming the {which} of {given} by {} s would leave {} s; {given} is {} s long ({}–{} s).",
                util::signed(delta),
                util::num(new_dur),
                util::num(c0.duration),
                util::secs(span.start),
                util::secs(span.end)
            ));
        }
        let chained = matches!(&c0.start, Time::Anchored(Anchor { to: AnchorTarget::After(_), .. }));
        let keep_start = match edge {
            Edge::End => true,
            Edge::Start => ripple.unwrap_or(chained),
        };
        for id in &all {
            let j = clip_ix(self.p, id)?;
            match edge {
                Edge::End => trim_back_content(&mut self.p.clips[j], new_dur),
                Edge::Start => trim_front_content(&mut self.p.clips[j], delta),
            }
            if let Some(w) = clamp_fades(&mut self.p.clips[j]) {
                self.warn(w);
            }
        }
        // Place the clip by its wanted start (this also handles `before` anchors, whose start depends on
        // the clip's own length).
        let want = if keep_start { span.start } else { span.start + delta };
        reposition(self.p, &[(clip.to_string(), want)])?;
        // An end anchor (`until`) moves with the end (`trim_back_content` already moved it for the end edge).
        let end_shift = match edge {
            Edge::End => 0.0,
            Edge::Start if keep_start => -delta,
            Edge::Start => 0.0,
        };
        if end_shift != 0.0 {
            for id in &all {
                let j = clip_ix(self.p, id)?;
                if let Some(u) = &mut self.p.clips[j].until {
                    crate::ops::prims::shift_time(u, end_shift);
                }
            }
        }
        match ripple {
            Some(true) => {
                // Later clips move by what the edge moved on screen (whole frames on video tracks), so
                // clips that touch keep touching.
                let moved = match (grid, to, edge) {
                    (Some(_), Some(_), Edge::End) => snap_for(self.p, &c0.track, span.end + delta) - shown.end,
                    (Some(_), Some(_), Edge::Start) => snap_for(self.p, &c0.track, span.start + delta) - shown.start,
                    _ => delta,
                };
                let shift = if edge == Edge::End { moved } else { -moved };
                let l = later(self.p, shift);
                reposition_rigid(self.p, &l, &r)?;
            }
            Some(false) => pin_all_except(self.p, &r, &exempt)?,
            None => {}
        }
        let r2 = crate::resolve::resolve(self.p);
        let others: Vec<Id> = all.iter().filter(|x| x.as_str() != given).cloned().collect();
        // What changed on screen, in the op's own sign (end: + = longer; start: + = later/shorter).
        let shown = match (shown0, r2.span(clip)) {
            (Some(a), Some(b)) => match edge {
                Edge::End => b.duration() - a.duration(),
                Edge::Start => a.duration() - b.duration(),
            },
            _ => delta,
        };
        self.summary = format!(
            "Trimmed {which} of {given} by {} → {}{}",
            util::change(shown, grid, Some(asked)),
            span_text(&r2, given),
            if others.is_empty() { String::new() } else { format!(" (with linked {})", others.join(", ")) }
        );
        Ok(())
    }

    fn duplicate_clip(&mut self, clip: &str, start: Option<&Time>, track: Option<&str>, new_id: Option<&str>, overwrite: bool) -> Result<(), EditError> {
        let ci = clip_ix(self.p, clip)?;
        let orig = self.p.clips[ci].clone();
        let track = track.map(str::to_string).unwrap_or_else(|| orig.track.clone());
        track_ix(self.p, &track)?;
        ensure_unlocked(self.p, &track)?;
        let nid = match new_id {
            Some(n) => {
                self.ids.claim(n)?;
                n.to_string()
            }
            None => self.ids.fresh("c"),
        };
        self.made.push(nid.clone());
        let start = match start {
            Some(t) => t.clone(),
            None => {
                let r = resolve(self.p);
                match track_end(self.p, &r, &track).1 {
                    Some(last) => Time::Anchored(Anchor { to: AnchorTarget::After(last), offset: 0.0 }),
                    None => Time::At(0.0),
                }
            }
        };
        let mut c = orig;
        c.id = nid.clone();
        c.track = track.clone();
        c.start = start;
        self.p.clips.push(c);
        if overwrite {
            self.overwrite_under(&nid)?;
        }
        let r = resolve(self.p);
        self.summary = format!("Duplicated {clip} as {nid} on {track} {}", span_text(&r, &nid));
        Ok(())
    }

    fn set_clip(&mut self, clip: &str, patch: &Value) -> Result<(), EditError> {
        let ci = editable(self.p, clip)?;
        let Value::Object(pm) = patch else {
            return fail("`patch` must be a JSON object of clip fields to change, e.g. {\"name\":\"hook\",\"opacity\":0.8}.");
        };
        check_fields(pm)?;
        let old = self.p.clips[ci].clone();
        if let Some(v) = pm.get("id") {
            if v.as_str() != Some(clip) {
                return fail("clip ids are stable and cannot be changed; use duplicate_clip with new_id and delete the old clip if you need another id.");
            }
        }
        let mut v = serde_json::to_value(&old).map_err(|e| EditError::new(e.to_string()))?;
        // Anchors and a changed source type are replaced whole, not merged.
        let mut patch = pm.clone();
        if let (Some(Value::Object(new_src)), Some(Value::Object(old_src))) = (patch.get("source"), v.get("source")) {
            if new_src.get("type").is_some() && new_src.get("type") != old_src.get("type") {
                v["source"] = Value::Null;
            }
        }
        if let Some(st) = patch.remove("start") {
            v["start"] = st;
        }
        merge_patch(&mut v, &Value::Object(patch));
        let new: Clip = serde_json::from_value(v).map_err(|e| EditError::new(format!("the patched clip is invalid: {e}.")))?;
        if new.track != old.track {
            track_ix(self.p, &new.track)?;
            ensure_unlocked(self.p, &new.track)?;
            let n0 = self.p.transitions.len();
            self.p.transitions.retain(|tr| tr.from != clip && tr.to != clip);
            if self.p.transitions.len() < n0 {
                self.warn(format!("removed the transition(s) of {clip}: it left its track."));
            }
        }
        self.p.clips[ci] = new;
        let keys: Vec<&String> = pm.keys().collect();
        self.summary = format!("Updated {clip}: {}", keys.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", "));
        Ok(())
    }

    fn set_param(&mut self, clip: &str, path: &str, value: Option<f64>, keys: Option<&Vec<Key>>) -> Result<(), EditError> {
        let ci = editable(self.p, clip)?;
        let partners = if path == "speed" { super::linked_partners(self.p, clip) } else { vec![] };
        let r = resolve(self.p);
        let start = r.span(clip).map(|s| s.start).unwrap_or(0.0);
        let dur = self.p.clips[ci].duration;
        let c_snapshot = self.p.clips[ci].clone();
        let mut warn = None;
        let text = {
            let c = &mut self.p.clips[ci];
            match (value, keys) {
                (Some(v), None) => {
                    check_finite("value", v)?;
                    match param_at(c, path)? {
                        Target::Param(p) => *p = Param::Value(v),
                        Target::Scalar(s) => {
                            if path.starts_with("fade") && v < 0.0 {
                                return fail(format!("{path} must be 0 or more seconds."));
                            }
                            *s = v;
                        }
                    }
                    format!("= {}", util::num(v))
                }
                (None, Some(k)) => {
                    let ks = clean_keys(k.clone())?;
                    if ks.iter().any(|k| k.t < -1e-9 || k.t > dur + 1e-9) {
                        let outside: Vec<String> = ks.iter().filter(|k| k.t < 0.0 || k.t > dur).map(|k| util::num(k.t)).collect();
                        warn = Some(format!(
                            "{clip}.{path}: key time(s) {} are outside the clip (clip-local 0–{} s){}",
                            outside.join(", "),
                            util::num(dur),
                            if ks.iter().any(|k| k.t >= start && k.t <= start + dur && k.t > dur) {
                                format!("; key times are clip-local, so subtract the clip start ({} s).", util::num(start))
                            } else {
                                ".".into()
                            }
                        ));
                    }
                    let n = ks.len();
                    match param_at(c, path)? {
                        Target::Param(p) => *p = crate::eval::normalize_keys(ks),
                        Target::Scalar(_) => return fail(format!("{path} cannot be animated; set it with `value`.")),
                    }
                    format!("animated ({n} keys)")
                }
                (Some(_), Some(_)) => return fail("give either `value` (a constant) or `keys` (animation), not both."),
                (None, None) => return fail("give `value` (a constant) or `keys` ([{\"t\":0,\"v\":1},…] in clip-local seconds)."),
            }
        };
        if path == "fade_in" || path == "fade_out" {
            let c = &self.p.clips[ci];
            if c.fade_in + c.fade_out > c.duration + 1e-9 {
                self.p.clips[ci] = c_snapshot;
                return fail(format!("fade_in + fade_out would exceed {clip}'s {} s length.", util::num(dur)));
            }
        }
        if let Some(w) = warn {
            self.warn(w);
        }
        let linked = self.copy_speed_to(clip, &partners)?;
        self.summary = format!("{clip}.{path} {text}{linked}");
        Ok(())
    }

    /// Keeps linked clips (detached audio and its video) on the same speed curve.
    fn copy_speed_to(&mut self, clip: &str, partners: &[Id]) -> Result<String, EditError> {
        if partners.is_empty() {
            return Ok(String::new());
        }
        let sp = self.p.clips[clip_ix(self.p, clip)?].speed.clone();
        for k in partners {
            let j = clip_ix(self.p, k)?;
            ensure_unlocked(self.p, &self.p.clips[j].track.clone())?;
            self.p.clips[j].speed = sp.clone();
        }
        Ok(format!(" (and linked {})", partners.join(", ")))
    }

    fn add_keyframe(&mut self, clip: &str, path: &str, t: f64, v: f64, ease: Ease) -> Result<(), EditError> {
        let ci = editable(self.p, clip)?;
        let partners = if path == "speed" { super::linked_partners(self.p, clip) } else { vec![] };
        let r = resolve(self.p);
        let start = r.span(clip).map(|s| s.start).unwrap_or(0.0);
        check_local_time(&self.p.clips[ci], start, t)?;
        check_finite("v", v)?;
        let c = &mut self.p.clips[ci];
        match param_at(c, path)? {
            Target::Scalar(_) => return fail(format!("{path} cannot be animated; use set_param with `value`.")),
            Target::Param(p) => {
                let key = Key { t, v, ease };
                match p {
                    Param::Value(_) => *p = Param::Animated { keys: vec![key] },
                    Param::Animated { keys } => {
                        if let Some(k) = keys.iter_mut().find(|k| (k.t - t).abs() < 1e-6) {
                            *k = key;
                        } else {
                            let pos = keys.partition_point(|k| k.t < t);
                            keys.insert(pos, key);
                        }
                    }
                }
            }
        }
        let linked = self.copy_speed_to(clip, &partners)?;
        self.summary = format!("{clip}.{path}: key at {} s = {}{linked}", util::num(t), util::num(v));
        Ok(())
    }

    fn remove_keyframe(&mut self, clip: &str, path: &str, t: f64) -> Result<(), EditError> {
        let ci = editable(self.p, clip)?;
        let partners = if path == "speed" { super::linked_partners(self.p, clip) } else { vec![] };
        let tol = (0.5 / self.p.settings.fps.max(1.0)).max(1e-3);
        let c = &mut self.p.clips[ci];
        match param_at(c, path)? {
            Target::Scalar(_) => return fail(format!("{path} has no keyframes.")),
            Target::Param(p) => match p {
                Param::Value(v) => return fail(format!("{clip}.{path} is not animated (it is the constant {}).", util::num(*v))),
                Param::Animated { keys } => {
                    let best = keys.iter().enumerate().min_by(|a, b| (a.1.t - t).abs().total_cmp(&(b.1.t - t).abs())).map(|(i, k)| (i, (k.t - t).abs()));
                    match best {
                        Some((i, d)) if d <= tol => {
                            let removed = keys.remove(i);
                            if keys.len() <= 1 {
                                let v = keys.first().map(|k| k.v).unwrap_or(removed.v);
                                *p = Param::Value(v);
                            }
                        }
                        _ => {
                            let times: Vec<String> = keys.iter().map(|k| util::num(k.t)).collect();
                            return fail(format!("{clip}.{path} has no key at {} s (keys at {} s, clip-local).", util::num(t), times.join(", ")));
                        }
                    }
                }
            },
        }
        let linked = self.copy_speed_to(clip, &partners)?;
        self.summary = format!("{clip}.{path}: removed key at {} s{linked}", util::num(t));
        Ok(())
    }

    fn roll_edit(&mut self, left: &str, right: &str, delta: f64) -> Result<(), EditError> {
        editable(self.p, left)?;
        editable(self.p, right)?;
        let (left, right) = (leader_of(self.p, left), leader_of(self.p, right));
        let (left, right) = (left.as_str(), right.as_str());
        let li = editable(self.p, left)?;
        let ri = editable(self.p, right)?;
        check_finite("delta", delta)?;
        if self.p.clips[li].track != self.p.clips[ri].track {
            return fail(format!("{left} and {right} are on different tracks; roll edits move the cut between two touching clips on one track."));
        }
        let r = resolve(self.p);
        let (sl, sr) = (span_of(&r, left)?, span_of(&r, right)?);
        let asked = delta;
        let cut0 = crate::resolve::resolve(self.p).span(left).map(|s| s.end).unwrap_or(sl.end);
        let track = self.p.clips[li].track.clone();
        let grid = self.p.tracks.iter().any(|t| t.id == track && t.kind == TrackKind::Video).then_some(self.p.settings.fps);
        let delta = whole_frames(self.p, &track, delta);
        if (sl.end - sr.start).abs() > 1e-3 {
            return fail(format!(
                "{left} ends at {} s but {right} starts at {} s; a roll edit needs touching clips (make {right} start {{\"after\":\"{left}\"}} first).",
                util::secs(sl.end),
                util::secs(sr.start)
            ));
        }
        let (dl, dr) = (self.p.clips[li].duration + delta, self.p.clips[ri].duration - delta);
        if dl < MIN_DURATION || dr < MIN_DURATION {
            return fail(format!(
                "rolling by {} s would leave {left} {} s and {right} {} s long; the cut can move between {} and {} s.",
                util::signed(delta),
                util::num(dl),
                util::num(dr),
                util::secs(sl.start),
                util::secs(sr.end)
            ));
        }
        let links_l = linked_of(self.p, left);
        let links_r = linked_of(self.p, right);
        let mut exempt: HashSet<Id> = [left.to_string(), right.to_string()].into_iter().collect();
        for id in std::iter::once(left.to_string()).chain(links_l.iter().cloned()) {
            let j = clip_ix(self.p, &id)?;
            trim_back_content(&mut self.p.clips[j], dl);
            exempt.insert(id);
        }
        for id in std::iter::once(right.to_string()).chain(links_r.iter().cloned()) {
            let j = clip_ix(self.p, &id)?;
            trim_front_content(&mut self.p.clips[j], delta);
            exempt.insert(id);
        }
        // The left keeps its start, the right starts at the new cut, everything else stays put.
        let mut desired: Vec<(Id, f64)> = self.p.clips.iter().filter(|c| !exempt.contains(&c.id)).filter_map(|c| Some((c.id.clone(), r.span(&c.id)?.start))).collect();
        desired.push((left.to_string(), sl.start));
        desired.push((right.to_string(), sl.end + delta));
        reposition_rigid(self.p, &desired, &r)?;
        let cut1 = crate::resolve::resolve(self.p).span(left).map(|s| s.end).unwrap_or(sl.end + delta);
        self.summary = format!("Rolled the cut {left}|{right} by {} to {} s", util::change(cut1 - cut0, grid, Some(asked)), util::tsecs(cut1));
        Ok(())
    }

    fn slide_clip(&mut self, clip: &str, delta: f64) -> Result<(), EditError> {
        editable(self.p, clip)?;
        let lead = leader_of(self.p, clip);
        let clip = lead.as_str();
        let ci = editable(self.p, clip)?;
        check_finite("delta", delta)?;
        let r = resolve(self.p);
        let span = span_of(&r, clip)?;
        let track = self.p.clips[ci].track.clone();
        let asked = delta;
        let start0 = crate::resolve::resolve(self.p).span(clip).map(|s| s.start).unwrap_or(span.start);
        let grid = self.p.tracks.iter().any(|t| t.id == track && t.kind == TrackKind::Video).then_some(self.p.settings.fps);
        let delta = whole_frames(self.p, &track, delta);
        let on = r.on_track(self.p, &track);
        let prev = on.iter().find(|(c, s)| c.id != clip && (s.end - span.start).abs() < 1e-3).map(|(c, s)| (c.id.clone(), s.start));
        let next = on.iter().find(|(c, s)| c.id != clip && (s.start - span.end).abs() < 1e-3).map(|(c, _)| c.id.clone());
        let mut exempt: HashSet<Id> = HashSet::new();
        exempt.insert(clip.to_string());
        // Things attached to the slid clip move with it (as a whole).
        let mut movers: Vec<Id> = Vec::new();
        let mut stack = vec![clip.to_string()];
        while let Some(x) = stack.pop() {
            for n in dependants(self.p, &x) {
                if let Node::Clip(i) = n {
                    let id = self.p.clips[i].id.clone();
                    if Some(&id) != next.as_ref() && prev.as_ref().is_none_or(|(pv, _)| pv != &id) && exempt.insert(id.clone()) {
                        movers.push(id.clone());
                        stack.push(id);
                    }
                }
            }
        }
        let mut desired: Vec<(Id, f64)> = Vec::new();
        if let Some((pv, pstart)) = &prev {
            editable(self.p, pv)?;
            let pi = clip_ix(self.p, pv)?;
            let nd = self.p.clips[pi].duration + delta;
            if nd < MIN_DURATION {
                return fail(format!("sliding {clip} by {} s would shrink {pv} to {} s.", util::signed(delta), util::num(nd)));
            }
            for id in std::iter::once(pv.clone()).chain(linked_of(self.p, pv)) {
                let j = clip_ix(self.p, &id)?;
                trim_back_content(&mut self.p.clips[j], nd);
                exempt.insert(id);
            }
            desired.push((pv.clone(), *pstart));
        }
        if let Some(nx) = &next {
            editable(self.p, nx)?;
            let ni = clip_ix(self.p, nx)?;
            let nd = self.p.clips[ni].duration - delta;
            if nd < MIN_DURATION {
                return fail(format!("sliding {clip} by {} s would shrink {nx} to {} s.", util::signed(delta), util::num(nd)));
            }
            for id in std::iter::once(nx.clone()).chain(linked_of(self.p, nx)) {
                let j = clip_ix(self.p, &id)?;
                trim_front_content(&mut self.p.clips[j], delta);
                exempt.insert(id);
            }
            desired.push((nx.clone(), span.end + delta));
        }
        desired.push((clip.to_string(), span.start + delta));
        for m in &movers {
            if let Some(s) = r.span(m) {
                desired.push((m.clone(), s.start + delta));
            }
        }
        let mut all: Vec<(Id, f64)> = self.p.clips.iter().filter(|c| !exempt.contains(&c.id)).filter_map(|c| Some((c.id.clone(), r.span(&c.id)?.start))).collect();
        all.extend(desired);
        reposition_rigid(self.p, &all, &r)?;
        let start1 = crate::resolve::resolve(self.p).span(clip).map(|s| s.start).unwrap_or(span.start + delta);
        self.summary = format!(
            "Slid {clip} by {}{}{}",
            util::change(start1 - start0, grid, Some(asked)),
            prev.map(|(p, _)| format!("; {p} absorbs it")).unwrap_or_default(),
            next.map(|n| format!("; {n} adjusts")).unwrap_or_default()
        );
        Ok(())
    }

    fn set_speed(&mut self, clip: &str, speed: f64, keep: SpeedKeep, linked: bool) -> Result<(), EditError> {
        editable(self.p, clip)?;
        let lead = if linked { leader_of(self.p, clip) } else { clip.to_string() };
        let clip = lead.as_str();
        let ci = editable(self.p, clip)?;
        if !(speed.is_finite() && (0.0..=MAX_SPEED).contains(&speed)) {
            return fail(format!("speed {speed} is out of range; use 0 (freeze) to {MAX_SPEED} (1 = normal, 2 = double, 0.5 = slow motion)."));
        }
        if !matches!(self.p.clips[ci].source, Source::Media { .. }) {
            return fail(format!("{clip} is a {} clip; speed applies to media clips.", kind_word(&self.p.clips[ci])));
        }
        let old_start = resolve(self.p).span(clip).map(|s| s.start);
        let mut all = vec![clip.to_string()];
        if linked {
            all.extend(linked_of(self.p, clip));
        }
        let c = &self.p.clips[ci];
        let cons = consumed(c);
        let old_dur = c.duration;
        match keep {
            SpeedKeep::Source => {
                if speed == 0.0 {
                    return fail("speed 0 cannot keep the source range (it would last forever); use keep: \"duration\" for a freeze, or freeze_frame.");
                }
                if cons <= 1e-9 {
                    return fail(format!("{clip} currently plays no source time (it is frozen); use keep: \"duration\"."));
                }
                let new_dur = cons / speed;
                let factor = new_dur / old_dur;
                for id in &all {
                    let j = clip_ix(self.p, id)?;
                    let x = &mut self.p.clips[j];
                    map_params(x, |p| scale_param_time(p, factor));
                    x.speed = Param::Value(speed);
                    if let Some(u) = &mut x.until {
                        crate::ops::prims::shift_time(u, new_dur - old_dur);
                    }
                    x.duration = new_dur;
                    if let Some(w) = clamp_fades(x) {
                        self.warnings.push(w);
                    }
                }
                if let Some(s) = old_start {
                    reposition(self.p, &[(clip.to_string(), s)])?;
                }
                self.summary = format!("{clip} at {}× speed → {} s long (same source)", util::num(speed), util::num(new_dur));
            }
            SpeedKeep::Duration => {
                for id in &all {
                    let j = clip_ix(self.p, id)?;
                    self.p.clips[j].speed = Param::Value(speed);
                }
                self.summary = format!("{clip} at {}× speed (same {} s length)", util::num(speed), util::num(old_dur));
            }
        }
        Ok(())
    }

    fn freeze_frame(&mut self, clip: &str, at: f64, duration: f64, new_id: Option<&str>) -> Result<(), EditError> {
        editable(self.p, clip)?;
        let lead = leader_of(self.p, clip);
        let clip = lead.as_str();
        let ci = editable(self.p, clip)?;
        check_finite("at", at)?;
        if !(duration.is_finite() && duration >= MIN_DURATION) {
            return fail("freeze `duration` must be a positive number of seconds.");
        }
        let c = self.p.clips[ci].clone();
        let Some(asset) = media_of(self.p, &c) else {
            return fail(format!("{clip} is a {} clip; freeze_frame needs a video clip.", kind_word(&c)));
        };
        if asset.kind != MediaKind::Video {
            return fail(format!("{clip} plays {} media; freeze_frame needs video (stills and audio cannot freeze).", if asset.kind == MediaKind::Image { "still image" } else { "audio" }));
        }
        let media_dur = asset.info.duration;
        let asset_id = asset.id.clone();
        let r = resolve(self.p);
        let span = span_of(&r, clip)?;
        if at < span.start - 1e-6 || at > span.end + 1e-6 {
            return fail(format!("{} s is outside {clip} ({}–{} s); freeze a time inside the clip.", util::secs(at), util::secs(span.start), util::secs(span.end)));
        }
        let at = snap_for(self.p, &c.track, at).clamp(span.start, span.end);
        // On video tracks the freeze lasts whole frames, so everything it pushes moves by whole frames.
        let duration = match frame_len(self.p, &c.track) {
            Some(f) => (duration / f).round().max(1.0) * f,
            None => duration,
        };
        let local = (at - span.start).clamp(0.0, c.duration);
        let mut src = source_time(&c, local).unwrap_or(0.0);
        if media_dur > 0.0 {
            src = src.min(media_dur - 1e-3).max(0.0);
        }
        let fid = match new_id {
            Some(n) => {
                self.ids.claim(n)?;
                n.to_string()
            }
            None => self.ids.fresh("c"),
        };
        self.made.push(fid.clone());
        let mut f = c.clone();
        f.id = fid.clone();
        map_params(&mut f, |p| Param::Value(sample(p, local)));
        f.speed = Param::Value(0.0);
        f.reverse = false;
        f.duration = duration;
        f.until = None;
        let stream = match &c.source {
            Source::Media { stream, .. } => *stream,
            _ => 0,
        };
        f.source = Source::Media { asset: asset_id, in_point: src, stream };
        f.muted = true;
        f.fade_in = 0.0;
        f.fade_out = 0.0;
        f.name = if c.name.is_empty() { "freeze".into() } else { format!("{} (freeze)", c.name) };
        let track = c.track.clone();
        let after_c = Time::Anchored(Anchor { to: AnchorTarget::After(clip.to_string()), offset: 0.0 });
        let later_from;
        let mut right = None;
        if local <= MIN_DURATION {
            // Freeze the first frame: the freeze takes the clip's place, the clip follows it.
            f.start = c.start.clone();
            self.p.clips[ci].start = Time::Anchored(Anchor { to: AnchorTarget::After(fid.clone()), offset: 0.0 });
            later_from = span.start;
        } else if local >= c.duration - MIN_DURATION {
            f.start = after_c;
            for k in self.p.clips.iter_mut() {
                if k.track == track {
                    if let Time::Anchored(an) = &mut k.start {
                        if matches!(&an.to, AnchorTarget::After(x) if x == clip) {
                            an.to = AnchorTarget::After(fid.clone());
                        }
                    }
                }
            }
            later_from = span.end;
        } else {
            let rid = self.split(clip, at, None, true)?;
            f.start = after_c;
            let ri = clip_ix(self.p, &rid)?;
            self.p.clips[ri].start = Time::Anchored(Anchor { to: AnchorTarget::After(fid.clone()), offset: 0.0 });
            right = Some(rid);
            later_from = at;
        }
        self.p.clips.push(f);
        // Everything on the track from the freeze point on moves later by the freeze's length.
        let mut desired: Vec<(Id, f64)> = self
            .p
            .clips
            .iter()
            .filter(|k| k.track == track && k.id != fid)
            .filter_map(|k| {
                let old = if right.as_deref() == Some(k.id.as_str()) { Some(at) } else { r.span(&k.id).map(|s| s.start) }?;
                (old >= later_from - EPS).then(|| (k.id.clone(), old + duration))
            })
            .collect();
        // The freeze sits exactly where the frame was (its own anchor may depend on its length).
        desired.push((fid.clone(), later_from));
        reposition_rigid(self.p, &desired, &r)?;
        self.summary = format!("Froze {clip} at {} s for {} s as {fid}", util::secs(at), util::num(duration));
        Ok(())
    }

    fn detach_audio(&mut self, clip: &str, track: &str, new_id: Option<&str>, stream: Option<u32>) -> Result<(), EditError> {
        let ci = editable(self.p, clip)?;
        let ti = track_ix(self.p, track)?;
        ensure_unlocked(self.p, track)?;
        let c = self.p.clips[ci].clone();
        let Some(asset) = media_of(self.p, &c) else {
            return fail(format!("{clip} is a {} clip; only video clips have sound to detach.", kind_word(&c)));
        };
        if asset.kind != MediaKind::Video {
            let more = if asset.kind == MediaKind::Audio && asset.info.audio_stream_count() > 1 {
                format!(" To give its other sound streams their own clips, use {{\"op\":\"detach_streams\",\"clip\":\"{clip}\"}}.")
            } else {
                String::new()
            };
            return fail(format!(
                "{clip} plays {} media; detach_audio separates the sound of a video clip.{more}",
                if asset.kind == MediaKind::Audio { "audio-only" } else { "still image" }
            ));
        }
        if asset.info.duration > 0.0 && !asset.info.has_audio {
            return fail(format!("{} has no audio stream, so {clip} has no sound to detach.", asset.id));
        }
        if self.p.tracks[ti].kind != TrackKind::Audio {
            let auds: Vec<&str> = self.p.tracks.iter().filter(|t| t.kind == TrackKind::Audio).map(|t| t.id.as_str()).collect();
            return fail(format!(
                "{track} is a video track; detach the sound onto an audio track ({}).",
                if auds.is_empty() { "add one with {\"op\":\"add_track\",\"kind\":\"audio\"}".to_string() } else { auds.join(", ") }
            ));
        }
        let stream = stream.unwrap_or_else(|| clip_stream(&c));
        if let Some(why) = stream_problem(asset, stream) {
            return fail(why);
        }
        let multi = asset.info.audio_stream_count() > 1;
        let followers = linked_of(self.p, clip);
        if let Some(k) = followers.iter().find(|k| self.p.clips.iter().any(|x| &x.id == *k && clip_stream(x) == stream)) {
            return fail(if multi { format!("{clip}'s sound stream {stream} is already detached as {k}.") } else { format!("{clip}'s sound is already detached as {k}.") });
        }
        let was_muted = c.muted && followers.is_empty();
        let aid = self.detach_one(ci, track, new_id, stream)?;
        if was_muted {
            self.warn(format!("{clip} was muted; its detached audio {aid} plays (mute {aid} if you want silence)."));
        }
        self.summary = if multi {
            format!("Detached sound stream {stream} of {clip} to {aid} on {track} (linked, anchored with {clip})")
        } else {
            format!("Detached the sound of {clip} to {aid} on {track} (linked, anchored with {clip})")
        };
        Ok(())
    }

    /// Adds a linked audio clip of clip `ci` playing `stream` on `track` and mutes a video clip's own
    /// sound. Checks are the caller's.
    fn detach_one(&mut self, ci: usize, track: &str, new_id: Option<&str>, stream: u32) -> Result<Id, EditError> {
        let c = self.p.clips[ci].clone();
        let aid = match new_id {
            Some(n) => {
                self.ids.claim(n)?;
                n.to_string()
            }
            None => self.ids.fresh("c"),
        };
        self.made.push(aid.clone());
        let m = media_of(self.p, &c);
        let title = m.filter(|m| m.info.audio_stream_count() > 1).and_then(|m| m.info.audio_stream(stream)).map(|s| s.title).filter(|t| !t.is_empty());
        let is_video = m.is_some_and(|m| m.kind == MediaKind::Video);
        let mut a = c.clone();
        a.id = aid.clone();
        a.track = track.to_string();
        a.start = Time::Anchored(Anchor { to: AnchorTarget::With(c.id.clone()), offset: 0.0 });
        a.transform = Transform::default();
        a.opacity = Param::Value(1.0);
        a.blend = BlendMode::Normal;
        a.effects.clear();
        a.muted = false;
        a.enabled = true;
        a.notes = String::new();
        if let Source::Media { stream: s, .. } = &mut a.source {
            *s = stream;
        }
        a.name = match (c.name.is_empty(), title) {
            (true, None) => String::new(),
            (true, Some(t)) => t,
            (false, None) => format!("{} (audio)", c.name),
            (false, Some(t)) => format!("{} ({t})", c.name),
        };
        if is_video {
            self.p.clips[ci].muted = true;
        }
        self.p.clips.push(a);
        Ok(aid)
    }

    fn detach_streams(&mut self, clip: &str, streams: Option<&[u32]>, tracks: Option<&[Id]>, new_ids: Option<&[Id]>) -> Result<(), EditError> {
        let ci = editable(self.p, clip)?;
        let c = self.p.clips[ci].clone();
        let Some(asset) = media_of(self.p, &c).cloned() else {
            return fail(format!("{clip} is a {} clip; only video and audio clips have sound streams.", kind_word(&c)));
        };
        let n = asset.info.audio_stream_count();
        if asset.kind == MediaKind::Image || n == 0 {
            return fail(format!("{} \"{}\" has no sound, so {clip} has no sound streams to detach.", asset.id, util::file_name(&asset.path)));
        }
        let is_video = asset.kind == MediaKind::Video;
        let own = clip_stream(&c);
        let taken: Vec<(Id, u32)> = linked_of(self.p, clip).into_iter().filter_map(|k| self.p.clips.iter().find(|x| x.id == k).map(|x| (k, clip_stream(x)))).collect();
        let list: Vec<u32> = match streams {
            Some(s) => {
                if s.is_empty() {
                    return fail("`streams` is empty; omit it to detach every stream.");
                }
                let mut seen = HashSet::new();
                for k in s {
                    if !seen.insert(*k) {
                        return fail(format!("stream {k} is listed twice in `streams`."));
                    }
                    if let Some(why) = stream_problem(&asset, *k) {
                        return fail(why);
                    }
                    if let Some((id, _)) = taken.iter().find(|(_, x)| x == k) {
                        return fail(format!("{clip}'s sound stream {k} is already detached as {id}."));
                    }
                    if !is_video && *k == own {
                        return fail(format!("{clip} plays stream {k} itself; list only its other streams."));
                    }
                }
                s.to_vec()
            }
            None => (0..n as u32).filter(|k| !taken.iter().any(|(_, x)| x == k) && (is_video || *k != own)).collect(),
        };
        if list.is_empty() {
            let have: Vec<String> = taken.iter().map(|(id, k)| format!("stream {k} → {id}")).collect();
            return fail(format!(
                "every sound stream of {clip} already has its own clip ({}); nothing to detach.",
                if have.is_empty() { format!("{clip} plays its only stream itself") } else { have.join(", ") }
            ));
        }
        if let Some(ids) = new_ids {
            if ids.len() != list.len() {
                return fail(format!("`new_ids` has {} id(s) but {} stream(s) are being detached ({list:?}); give one id per stream.", ids.len(), list.len()));
            }
        }
        let r = resolve(self.p);
        let span = span_of(&r, clip)?;
        let chosen: Vec<Id> = match tracks {
            Some(ts) => {
                if ts.len() != list.len() {
                    return fail(format!(
                        "`tracks` has {} track(s) but {} stream(s) are being detached ({list:?}); give one audio track per stream, or omit `tracks`.",
                        ts.len(),
                        list.len()
                    ));
                }
                let mut seen = HashSet::new();
                for t in ts {
                    let ti = track_ix(self.p, t)?;
                    ensure_unlocked(self.p, t)?;
                    if self.p.tracks[ti].kind != TrackKind::Audio {
                        return fail(format!("{t} is a video track; each stream goes on an audio track."));
                    }
                    if !seen.insert(t.as_str()) {
                        return fail(format!("track {t} is listed twice in `tracks`; each stream needs its own track."));
                    }
                }
                ts.to_vec()
            }
            None => {
                let free: Vec<Id> = self
                    .p
                    .tracks
                    .iter()
                    .filter(|t| t.kind == TrackKind::Audio && !t.locked && t.id != c.track)
                    .filter(|t| !self.p.clips.iter().any(|x| x.track == t.id && r.span(&x.id).is_some_and(|s| s.start < span.end - EPS && s.end > span.start + EPS)))
                    .map(|t| t.id.clone())
                    .collect();
                let mut free = free.into_iter();
                let mut out: Vec<Id> = Vec::new();
                for k in &list {
                    match free.next() {
                        Some(t) => out.push(t),
                        None => {
                            let id = self.ids.fresh("a");
                            self.made.push(id.clone());
                            let name = asset.info.audio_stream(*k).map(|s| s.title).filter(|t| !t.is_empty()).unwrap_or_default();
                            self.p.tracks.push(Track {
                                id: id.clone(),
                                name,
                                kind: TrackKind::Audio,
                                role: String::new(),
                                muted: false,
                                hidden: false,
                                locked: false,
                                solo: false,
                                volume: 1.0,
                            });
                            out.push(id);
                        }
                    }
                }
                out
            }
        };
        let mut done = Vec::new();
        for (i, (k, t)) in list.iter().zip(&chosen).enumerate() {
            let id = self.detach_one(ci, t, new_ids.map(|ids| ids[i].as_str()), *k)?;
            let title = asset.info.audio_stream(*k).map(|s| s.title).filter(|t| !t.is_empty()).map(|t| format!(" {t:?}")).unwrap_or_default();
            done.push(format!("stream {k}{title} → {id} on {t}"));
        }
        self.summary = format!(
            "Gave {} sound stream(s) of {clip} their own linked clips: {}{}",
            list.len(),
            done.join(", "),
            if is_video { format!(" ({clip}'s own sound is muted)") } else { String::new() }
        );
        Ok(())
    }

    fn set_audio_stream(&mut self, clip: &str, stream: u32) -> Result<(), EditError> {
        let ci = editable(self.p, clip)?;
        let c = &self.p.clips[ci];
        let Some(asset) = media_of(self.p, c) else {
            return fail(format!("{clip} is a {} clip; only media clips play a sound stream.", kind_word(c)));
        };
        if asset.kind == MediaKind::Image {
            return fail(format!("{clip} shows a still image; it has no sound stream."));
        }
        if let Some(why) = stream_problem(asset, stream) {
            return fail(why);
        }
        let label = asset.info.audio_stream(stream).map(|s| s.describe()).unwrap_or_default();
        let before = clip_stream(c);
        let also: Vec<Id> = super::linked_partners(self.p, clip)
            .into_iter()
            .filter(|k| self.p.clips.iter().any(|x| &x.id == k && clip_stream(x) == stream && !x.muted && x.enabled))
            .collect();
        if let Source::Media { stream: s, .. } = &mut self.p.clips[ci].source {
            *s = stream;
        }
        if !also.is_empty() && !self.p.clips[ci].muted && before != stream {
            self.warn(format!("{} already play(s) stream {stream} too, so it is heard twice; mute one of them or pick another stream.", also.join(", ")));
        }
        self.summary = if before == stream {
            format!("{clip} already plays sound stream {stream}")
        } else {
            format!("{clip} now plays sound stream {stream}{}", if label.is_empty() { String::new() } else { format!(" ({label})") })
        };
        Ok(())
    }

    fn range(&mut self, from: f64, to: f64, tracks: Option<&Vec<Id>>, close: bool) -> Result<(), EditError> {
        check_finite("from", from)?;
        check_finite("to", to)?;
        // Ranges cut on frames of the project grid.
        let fps = self.p.settings.fps;
        let (from, to) = (crate::resolve::snap_to_frame(from, fps), crate::resolve::snap_to_frame(to, fps));
        if from < 0.0 || to <= from + MIN_DURATION {
            return fail(format!("the range must satisfy 0 ≤ from < to (got {}–{}).", util::num(from), util::num(to)));
        }
        let list: Vec<Id> = match tracks {
            Some(ts) => {
                if ts.is_empty() {
                    return fail("`tracks` is empty; omit it for all unlocked tracks.");
                }
                for t in ts {
                    track_ix(self.p, t)?;
                    ensure_unlocked(self.p, t)?;
                }
                ts.clone()
            }
            None => self.p.tracks.iter().filter(|t| !t.locked).map(|t| t.id.clone()).collect(),
        };
        let r0 = resolve(self.p);
        let affected = self.p.clips.iter().filter(|c| list.contains(&c.track)).filter(|c| r0.span(&c.id).is_some_and(|s| s.start < to - EPS && s.end > from + EPS)).count();
        self.lift(from, to, &list, &HashSet::new(), close)?;
        if close && tracks.is_none() {
            let len = to - from;
            let mut moved = 0;
            for m in &mut self.p.markers {
                if let Time::At(v) = &mut m.time {
                    if *v >= to - EPS {
                        *v -= len;
                        moved += 1;
                    } else if *v > from {
                        *v = from;
                        moved += 1;
                    }
                }
            }
            if moved > 0 {
                self.warn(format!("{moved} marker(s) after the range moved left with the timeline."));
            }
        }
        self.summary = format!(
            "{} {}–{} s on {} ({affected} clip(s) affected)",
            if close { "Extracted" } else { "Lifted" },
            util::secs(from),
            util::secs(to),
            list.join(", ")
        );
        Ok(())
    }

    fn add_effect(&mut self, clip: &str, effect: &EffectArg, index: Option<usize>) -> Result<(), EditError> {
        let ci = editable(self.p, clip)?;
        let unknown = |k: &str| {
            EditError::new(format!(
                "unknown effect {k:?}.{} Effects: {}.",
                util::did_you_mean(k, EFFECTS.iter().map(|e| e.kind)),
                EFFECTS.iter().map(|e| e.kind).collect::<Vec<_>>().join(", ")
            ))
        };
        let e = match effect {
            EffectArg::Kind(k) => effect_spec(k).ok_or_else(|| unknown(k))?.instantiate(),
            EffectArg::Full(e) => {
                effect_spec(&e.kind).ok_or_else(|| unknown(&e.kind))?;
                let mut e = e.clone();
                for v in e.params.values_mut() {
                    if let Param::Animated { keys } = v {
                        *v = crate::eval::normalize_keys(clean_keys(keys.clone())?);
                    }
                }
                e
            }
        };
        let n = self.p.clips[ci].effects.len();
        let i = index.unwrap_or(n);
        if i > n {
            return fail(format!("index {i} is out of range: {clip} has {n} effect(s), so use 0–{n}."));
        }
        let kind = e.kind.clone();
        self.p.clips[ci].effects.insert(i, e);
        self.summary = format!("Added {kind} to {clip} (effect {i})");
        Ok(())
    }

    fn add_transition(&mut self, from: &str, to: &str, kind: &str, duration: Option<f64>, id: Option<&str>, params: &std::collections::BTreeMap<String, f64>) -> Result<(), EditError> {
        let fi = clip_ix(self.p, from)?;
        let ti = clip_ix(self.p, to)?;
        let track = self.p.clips[fi].track.clone();
        if self.p.clips[ti].track != track {
            return fail(format!("transitions join two clips on the same track; {from} is on {track} and {to} is on {}.", self.p.clips[ti].track));
        }
        ensure_unlocked(self.p, &track)?;
        let r = resolve(self.p);
        let (sf, st) = (span_of(&r, from)?, span_of(&r, to)?);
        if (sf.end - st.start).abs() > 1e-3 {
            return fail(format!(
                "{from} ends at {} s but {to} starts at {} s; a transition needs touching clips with {from} first (e.g. move {to} to {{\"after\":\"{from}\"}}).",
                util::secs(sf.end),
                util::secs(st.start)
            ));
        }
        let spec = transition_spec(kind).ok_or_else(|| {
            EditError::new(format!(
                "unknown transition {kind:?}.{} Transitions: {}.",
                util::did_you_mean(kind, TRANSITIONS.iter().map(|t| t.kind)),
                TRANSITIONS.iter().map(|t| t.kind).collect::<Vec<_>>().join(", ")
            ))
        })?;
        let d = duration.unwrap_or(spec.default_duration);
        let max = sf.duration().min(st.duration());
        if !(d.is_finite() && d > 0.0) {
            return fail("transition duration must be more than 0 s.");
        }
        if d > max + 1e-9 {
            return fail(format!("a {} s transition is longer than the shorter clip ({} s); use at most {} s.", util::num(d), util::num(max), util::num(max)));
        }
        if let Some(t) = self.p.transitions.iter().find(|t| t.from == from && t.to == to) {
            return fail(format!("there is already a transition {} on this cut; change it with set_transition.", t.id));
        }
        check_transition_params(spec, params.iter().map(|(k, v)| (k, Some(*v))))?;
        let tid = match id {
            Some(i) => {
                self.ids.claim(i)?;
                i.to_string()
            }
            None => self.ids.fresh("tr"),
        };
        self.made.push(tid.clone());
        self.p.transitions.push(Transition { id: tid.clone(), from: from.into(), to: to.into(), kind: kind.into(), duration: d, params: params.clone(), colors: Default::default() });
        self.summary = format!("Added {kind} {tid} ({} s) {from}→{to} at {} s", util::num(d), util::secs(sf.end));
        Ok(())
    }

    fn set_transition(&mut self, id: &str, kind: Option<&str>, duration: Option<f64>, params: &std::collections::BTreeMap<String, Option<f64>>) -> Result<(), EditError> {
        let i = named_ix(&self.p.transitions, id, "transition", |t| &t.id)?;
        let mut t = self.p.transitions[i].clone();
        if let Some(k) = kind {
            if transition_spec(k).is_none() {
                return fail(format!(
                    "unknown transition {k:?}.{} Transitions: {}.",
                    util::did_you_mean(k, TRANSITIONS.iter().map(|t| t.kind)),
                    TRANSITIONS.iter().map(|t| t.kind).collect::<Vec<_>>().join(", ")
                ));
            }
            t.kind = k.to_string();
            // Parameters and colours of the old kind that the new kind lacks are dropped.
            let spec = transition_spec(k).expect("checked");
            t.params.retain(|n, _| spec.param(n).is_some());
            t.colors.retain(|n, _| spec.color_default(n).is_some());
        }
        if let Some(d) = duration {
            if !(d.is_finite() && d > 0.0) {
                return fail("transition duration must be more than 0 s.");
            }
            let len = |id: &str| self.p.clips.iter().find(|c| c.id == id).map(|c| c.duration);
            if let (Some(a), Some(b)) = (len(&t.from), len(&t.to)) {
                let max = a.min(b);
                if d > max + 1e-9 {
                    return fail(format!("a {} s transition is longer than the shorter clip ({} s); use at most {} s.", util::num(d), util::num(max), util::num(max)));
                }
            }
            t.duration = d;
        }
        let spec = transition_spec(&t.kind).ok_or_else(|| EditError::new(format!("transition {id} has unknown kind {:?}; set a known kind.", t.kind)))?;
        check_transition_params(spec, params.iter().map(|(k, v)| (k, *v)))?;
        for (k, v) in params {
            match v {
                None => t.params.remove(k),
                Some(x) => t.params.insert(k.clone(), *x),
            };
        }
        self.p.transitions[i] = t;
        self.summary = format!("Updated transition {id}");
        Ok(())
    }
}

fn check_transition_params<'a>(spec: &crate::effects::TransitionSpec, params: impl Iterator<Item = (&'a String, Option<f64>)>) -> Result<(), EditError> {
    for (k, v) in params {
        let Some(ps) = spec.param(k) else {
            let names: Vec<&str> = spec.params.iter().map(|p| p.name).collect();
            return fail(format!(
                "transition {} has no parameter {k:?} (parameters: {}).",
                spec.kind,
                if names.is_empty() { "none".into() } else { names.join(", ") }
            ));
        };
        if let Some(v) = v {
            if !(v.is_finite() && v >= ps.min && v <= ps.max) {
                return fail(format!("{}.{k} = {} is outside {}–{}.", spec.kind, util::num(v), util::num(ps.min), util::num(ps.max)));
            }
        }
    }
    Ok(())
}

fn no_effect(clip: &str, index: usize, n: usize) -> String {
    if n == 0 {
        format!("{clip} has no effects, so there is no effect {index}.")
    } else {
        format!("{clip} has {n} effect(s) (indices 0–{}), so there is no effect {index}.", n - 1)
    }
}

fn set_duck_values(d: &mut Ducking, amount: Option<f64>, threshold: Option<f64>, attack: Option<f64>, release: Option<f64>) -> Result<(), EditError> {
    if let Some(a) = amount {
        if !(a.is_finite() && (-60.0..=0.0).contains(&a)) {
            return fail(format!("amount_db is the gain reduction, -60 to 0 dB (got {a}); e.g. -12."));
        }
        d.amount_db = a;
    }
    if let Some(t) = threshold {
        if !(t.is_finite() && (-100.0..=0.0).contains(&t)) {
            return fail(format!("threshold_db must be -100 to 0 dBFS (got {t})."));
        }
        d.threshold_db = t;
    }
    for (name, v, slot) in [("attack", attack, &mut d.attack), ("release", release, &mut d.release)] {
        if let Some(v) = v {
            if !(v.is_finite() && (0.0..=10.0).contains(&v)) {
                return fail(format!("{name} must be 0–10 s (got {v})."));
            }
            *slot = v;
        }
    }
    Ok(())
}
