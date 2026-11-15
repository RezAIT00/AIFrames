//! Transcript ops: setting transcripts, text-based editing (`delete_words`, `cut_source`) and
//! word-anchored karaoke captions (`generate_captions`).
//!
//! Captions are text clips whose start is a `word` anchor (the first word of the line) and whose
//! `highlight.words` are the clip-local windows of each word. When words are cut, the captions that
//! contained them are rebuilt from the transcript so text, timing and highlight stay in sync.

use super::prims::*;
use super::{fail, Cx, EditError, DEFAULT_WORD_PAD, MIN_DURATION};

/// How far (seconds) measured `delete_words` edges may lie from the words' own edges.
const MAX_EDGE_SHIFT: f64 = 0.5;
use crate::eval::{local_time_of_source, sample, source_span};
use crate::model::*;
use crate::resolve::{resolve_raw as resolve, Resolved, EPS};
use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

fn is_false(v: &bool) -> bool {
    !*v
}

/// Caption look for `generate_captions`. Every field is optional; defaults: bold (800) white
/// "Segoe UI" at 6% of the frame's short side, a 70%-black pill behind the line, the spoken word in
/// yellow (#FFD60A), centred at the bottom of the title-safe area, wrapped to the title-safe width.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptionStyle {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font: Option<String>,
    /// Pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<Color>,
    /// Colour of the word being spoken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlight: Option<Color>,
    /// Pill behind the line; `"transparent"` for none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<Color>,
    /// Vertical offset of the caption's centre from the frame centre (px, + = down).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
    /// Wrap width in px.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_width: Option<f64>,
    #[serde(skip_serializing_if = "is_false")]
    pub uppercase: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke: Option<Stroke>,
    /// Entrance of each caption; `spoken` types the line in word by word with the voice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reveal: Option<RevealKind>,
}

/// Tag put on generated caption clips.
pub const CAPTION_TAG: &str = "caption";

struct CapWord {
    clip: Id,
    /// The first transcript token of this caption word.
    index: usize,
    /// The last token (split tokens like "$2" ".11" read and highlight as one word).
    last: usize,
    text: String,
    t0: f64,
    t1: f64,
    /// (added) Timeline end of the clip the word is heard in: a caption never outlasts its voice clip.
    clip_end: f64,
}

/// For each transcript token, one past the last token of the word it reads as part of
/// (see [`crate::words::word_units`]).
fn unit_ends(words: &[Word]) -> Vec<usize> {
    let tokens: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
    let mut v: Vec<usize> = (1..=words.len()).collect();
    for r in crate::words::word_units(&tokens) {
        for i in r.clone() {
            v[i] = r.end;
        }
    }
    v
}

/// Caption text of tokens `range`: their caption words joined as they read ("$2" ".11" → "$2.11").
fn unit_text(words: &[Word], range: std::ops::Range<usize>) -> String {
    let parts: Vec<String> = words[range].iter().map(|w| caption_word(&w.text)).collect();
    crate::words::join_words(&parts)
}

/// A caption as found before an edit, with the transcript words it shows.
pub(crate) struct CapInfo {
    id: Id,
    offset: f64,
    anchor_clip: Id,
    voice_track: Id,
    asset: Id,
    indices: Vec<usize>,
    /// Timeline time of each word's start before the edit.
    times: Vec<f64>,
    texts: Option<Vec<String>>,
    /// Seconds the caption is shown after its last word ends (timeline, as shown).
    tail: f64,
    source: Source,
    /// No karaoke highlight (text written or edited by hand): it follows its words in time but keeps
    /// its text and look.
    plain: bool,
}

/// Shortest length a caption is shortened to when it would run into the next one.
const MIN_CAPTION: f64 = 0.25;

fn clean_word(s: &str) -> String {
    s.trim().to_string()
}

/// A transcript word as one caption word: inner spaces become U+00A0 (no-break space), so a word like
/// "three D" stays one highlight word.
pub(crate) fn caption_word(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join("\u{a0}")
}

/// The words of a caption text, as the renderer counts them for highlight windows: split on
/// whitespace except the no-break spaces U+00A0, U+2007 and U+202F (which join a word).
pub(crate) fn caption_words(s: &str) -> Vec<&str> {
    s.split(|c: char| c.is_whitespace() && !matches!(c, '\u{00A0}' | '\u{2007}' | '\u{202F}')).filter(|w| !w.is_empty()).collect()
}

/// Lower-case letters and digits only, for comparing word texts.
pub(crate) fn norm_word(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Tag of a generated caption a person has edited (kept by `generate_captions`).
pub const EDITED_TAG: &str = "edited";

/// The new word that best matches an old word by time: it must overlap the old word's span; a start
/// anchor takes the one whose start is nearest, an end anchor the one whose end is nearest.
pub(crate) fn best_word(old: &Word, new: &[Word], end: bool) -> Option<usize> {
    let (os, oe) = (old.start, old.end.max(old.start));
    let mut best: Option<(usize, f64, f64)> = None;
    for (j, w) in new.iter().enumerate() {
        let (ns, ne) = (w.start, w.end.max(w.start));
        let overlap = oe.min(ne) - os.max(ns);
        let point_in = |p: f64, a: f64, b: f64| p >= a - 1e-9 && p <= b + 1e-9;
        let touches = overlap > 1e-9 || ((oe - os) < 1e-9 && point_in(os, ns, ne)) || ((ne - ns) < 1e-9 && point_in(ns, os, oe));
        if !touches {
            continue;
        }
        let dist = if end { (ne - oe).abs() } else { (ns - os).abs() };
        if best.as_ref().is_none_or(|b| dist < b.1 - 1e-12 || ((dist - b.1).abs() <= 1e-12 && overlap > b.2)) {
            best = Some((j, dist, overlap));
        }
    }
    best.map(|b| b.0)
}

/// Clip ids an op names directly (`clip`, `clips`, `left`, `right`, `moves[].clip`).
fn named_clips(op: &super::Op) -> HashSet<String> {
    let v = serde_json::to_value(op).unwrap_or_default();
    let mut out = HashSet::new();
    for k in ["clip", "left", "right"] {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
            out.insert(s.to_string());
        }
    }
    if let Some(a) = v.get("clips").and_then(|x| x.as_array()) {
        out.extend(a.iter().filter_map(|x| x.as_str().map(str::to_string)));
    }
    if let Some(a) = v.get("moves").and_then(|x| x.as_array()) {
        out.extend(a.iter().filter_map(|m| m.get("clip").and_then(|x| x.as_str()).map(str::to_string)));
    }
    out
}

fn is_punct_only(s: &str) -> bool {
    !s.chars().any(|c| c.is_alphanumeric())
}

fn ends_sentence(s: &str) -> bool {
    s.ends_with(['.', '?', '!', '…'])
}

/// Where source second `s` of `asset` plays on `track`: preferring clip `prefer`, else the clip of that
/// asset on the track nearest to `near` (default: `prefer`'s start). Returns the clip and timeline time.
fn locate(p: &Project, r: &Resolved, track: &str, asset: &str, s: f64, prefer: &str, near: Option<f64>) -> Option<(Id, f64)> {
    locate_in(p, r, track, asset, s, prefer, near, false)
}

/// [`locate`] for the start of a word: a word that starts exactly where a clip ends (e.g. the cut of a
/// `delete_words` without padding) is not heard in that clip.
fn locate_start(p: &Project, r: &Resolved, track: &str, asset: &str, s: f64, prefer: &str, near: Option<f64>) -> Option<(Id, f64)> {
    locate_in(p, r, track, asset, s, prefer, near, true)
}

#[allow(clippy::too_many_arguments)]
fn locate_in(p: &Project, r: &Resolved, track: &str, asset: &str, s: f64, prefer: &str, near: Option<f64>, start: bool) -> Option<(Id, f64)> {
    let plays = |c: &Clip| matches!(&c.source, Source::Media { asset: a, .. } if a == asset);
    let heard = |c: &Clip, l: f64| !start || l < c.duration - 1e-9;
    if let Some(c) = p.clips.iter().find(|c| c.id == prefer) {
        if plays(c) {
            if let (Some(l), Some(sp)) = (local_time_of_source(c, s), r.span(&c.id)) {
                if heard(c, l) {
                    return Some((c.id.clone(), sp.start + l));
                }
            }
        }
    }
    let near = near.or_else(|| r.span(prefer).map(|s| s.start)).unwrap_or(0.0);
    let mut best: Option<(Id, f64, f64)> = None;
    for c in p.clips.iter().filter(|c| c.track == track && plays(c)) {
        let (Some(l), Some(sp)) = (local_time_of_source(c, s), r.span(&c.id)) else { continue };
        if !heard(c, l) {
            continue;
        }
        let d = (sp.start + l - near).abs();
        if best.as_ref().is_none_or(|b| d < b.2) {
            best = Some((c.id.clone(), sp.start + l, d));
        }
    }
    best.map(|(i, t, _)| (i, t))
}

fn is_caption(c: &Clip) -> bool {
    matches!(&c.source, Source::Text(t) if t.highlight.as_ref().is_some_and(|h| !h.words.is_empty()))
        && matches!(&c.start, Time::Anchored(Anchor { to: AnchorTarget::Word(_), .. }))
}

fn tagged_caption(c: &Clip) -> bool {
    matches!(c.source, Source::Text(_)) && c.tags.iter().any(|t| t == "caption")
}

/// A caption without karaoke windows (its text was written or edited by hand) that is still anchored
/// to a word: it follows its words in time, its text and look are kept.
fn is_plain_caption(c: &Clip) -> bool {
    tagged_caption(c)
        && matches!(&c.source, Source::Text(t) if t.highlight.as_ref().is_none_or(|h| h.words.is_empty()))
        && matches!(&c.start, Time::Anchored(Anchor { to: AnchorTarget::Word(_), .. }))
}

/// Any caption, for keeping captions on a track from running into each other.
fn is_caption_like(c: &Clip) -> bool {
    is_caption(c) || tagged_caption(c)
}

/// Every word-anchored karaoke caption with the transcript words it shows (recovered by matching its
/// highlight windows against the words' timeline times).
pub(crate) fn collect_caps(p: &Project, r: &Resolved) -> Vec<CapInfo> {
    let mut out = Vec::new();
    if !p.clips.iter().any(|c| is_caption(c) || is_plain_caption(c)) {
        return out;
    }
    // Tails count from the last word (as heard) to the caption's raw end, so a caption that moves with
    // its words keeps its raw end relative to them exactly (no rounding drift, neighbours stay apart).
    let raw = crate::resolve::resolve_raw(p);
    let raw_end = |c: &Clip| raw.span(&c.id).map(|s| s.end);
    let mut units: HashMap<Id, Vec<usize>> = HashMap::new();
    for c in p.clips.iter().filter(|c| is_caption(c)) {
        let Source::Text(t) = &c.source else { continue };
        let Some(h) = &t.highlight else { continue };
        let Time::Anchored(Anchor { to: AnchorTarget::Word(w), offset }) = &c.start else { continue };
        let Some(vc) = p.clips.iter().find(|x| x.id == w.clip) else { continue };
        let Source::Media { asset, .. } = &vc.source else { continue };
        let Some(words) = p.transcripts.get(asset).map(|t| &t.words) else { continue };
        let Some(cap_start) = r.span(&c.id).map(|s| s.start) else { continue };
        let mut indices = Vec::with_capacity(h.words.len());
        let mut times = Vec::with_capacity(h.words.len());
        let ends = units.entry(asset.clone()).or_insert_with(|| unit_ends(words));
        let mut j = w.index;
        let mut ok = true;
        for win in &h.words {
            let target = cap_start + win[0];
            // The nearest token to the window's start (first one on a tie).
            let mut found: Option<(usize, f64)> = None;
            for (cand, wd) in words.iter().enumerate().skip(j).take(8) {
                if let Some((_, tt)) = locate_start(p, r, &vc.track, asset, wd.start, &vc.id, Some(cap_start)) {
                    if (tt - target).abs() < 0.06 && found.is_none_or(|(_, f)| (tt - target).abs() < (f - target).abs() - 1e-9) {
                        found = Some((cand, tt));
                    }
                }
            }
            let hit = found.or_else(|| {
                let idx = j.min(words.len().saturating_sub(1));
                locate_start(p, r, &vc.track, asset, words.get(idx)?.start, &vc.id, Some(cap_start)).map(|(_, tt)| (idx, tt))
            });
            let Some((idx, tt)) = hit else {
                ok = false;
                break;
            };
            indices.push(idx);
            times.push(tt);
            // The next caption word starts after the tokens this one reads together with.
            j = ends.get(idx).copied().unwrap_or(idx + 1).max(idx + 1);
        }
        if !ok || indices.is_empty() {
            continue;
        }
        let texts: Vec<String> = caption_words(&t.text).into_iter().map(str::to_string).collect();
        let last_end = h.words.last().map(|w| w[1]).unwrap_or(0.0);
        let tail = raw_end(c).map(|e| e - (cap_start + last_end)).unwrap_or(c.duration - last_end);
        out.push(CapInfo {
            id: c.id.clone(),
            offset: *offset,
            anchor_clip: w.clip.clone(),
            voice_track: vc.track.clone(),
            asset: asset.clone(),
            texts: (texts.len() == indices.len()).then_some(texts),
            indices,
            times,
            tail: tail.max(0.0),
            source: c.source.clone(),
            plain: false,
        });
    }
    // Captions without karaoke windows: their words are the ones heard while they are shown.
    for c in p.clips.iter().filter(|c| is_plain_caption(c)) {
        let Time::Anchored(Anchor { to: AnchorTarget::Word(w), offset }) = &c.start else { continue };
        let Some(vc) = p.clips.iter().find(|x| x.id == w.clip) else { continue };
        let Source::Media { asset, .. } = &vc.source else { continue };
        let Some(words) = p.transcripts.get(asset).map(|t| &t.words) else { continue };
        let Some(sp) = r.span(&c.id) else { continue };
        let (mut indices, mut times, mut last_end) = (Vec::new(), Vec::new(), None);
        for (idx, wd) in words.iter().enumerate().skip(w.index) {
            let Some((_, tt)) = locate_start(p, r, &vc.track, asset, wd.start, &vc.id, Some(sp.start)) else { break };
            if idx > w.index && (tt >= sp.end - 1e-6 || tt < sp.start - 0.06) {
                break;
            }
            let te = locate(p, r, &vc.track, asset, wd.end, &vc.id, Some(tt)).map(|x| x.1).filter(|e| *e >= tt).unwrap_or(tt + (wd.end - wd.start));
            indices.push(idx);
            times.push(tt);
            last_end = Some(te);
        }
        let Some(last_end) = last_end else { continue };
        out.push(CapInfo {
            id: c.id.clone(),
            offset: *offset,
            anchor_clip: w.clip.clone(),
            voice_track: vc.track.clone(),
            asset: asset.clone(),
            texts: None,
            indices,
            times,
            tail: (raw_end(c).unwrap_or(sp.end) - last_end).max(0.0),
            source: c.source.clone(),
            plain: true,
        });
    }
    out
}

impl Cx<'_> {
    pub(crate) fn set_transcript(&mut self, asset: &str, transcript: &Option<Option<Transcript>>, force: bool) -> Result<(), EditError> {
        media_ix(self.p, asset)?;
        let Some(t) = transcript else {
            return fail("`transcript` is required: a transcript object {\"words\":[{\"text\":\"Hi\",\"start\":0.2,\"end\":0.5}]}, or null to remove it.");
        };
        if let Some(t) = t {
            for (i, w) in t.words.iter().enumerate() {
                if !(w.start.is_finite() && w.end.is_finite() && w.start >= 0.0 && w.end >= w.start) {
                    return fail(format!("word {i} {:?} has invalid times {}–{}; words need 0 ≤ start ≤ end (source seconds).", w.text, w.start, w.end));
                }
                if i > 0 && w.start < t.words[i - 1].start - 1e-9 {
                    return fail(format!(
                        "word {i} {:?} starts at {} s, before word {} ({} s); words must be in time order.",
                        w.text,
                        util::num(w.start),
                        i - 1,
                        util::num(t.words[i - 1].start)
                    ));
                }
            }
        }
        let before = resolve(self.p);
        // Word anchors point at word indices; re-map each to the new word that best overlaps the old
        // word's time, so anchors stay on what is said even when words split or merge.
        let old_words = self.p.transcripts.get(asset).map(|t| t.words.clone()).unwrap_or_default();
        let new_words = t.as_ref().map(|t| t.words.clone()).unwrap_or_default();
        let plays: HashSet<Id> = self.p.clips.iter().filter(|c| matches!(&c.source, Source::Media { asset: a, .. } if a == asset)).map(|c| c.id.clone()).collect();
        let mut remaps: Vec<(Node, Option<WordRef>)> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut slots: Vec<(Node, WordRef)> = Vec::new();
        for (i, c) in self.p.clips.iter().enumerate() {
            if let Time::Anchored(Anchor { to: AnchorTarget::Word(w), .. }) = &c.start {
                slots.push((Node::Clip(i), w.clone()));
            }
            if let Some(Time::Anchored(Anchor { to: AnchorTarget::Word(w), .. })) = &c.until {
                slots.push((Node::ClipEnd(i), w.clone()));
            }
        }
        for (i, m) in self.p.markers.iter().enumerate() {
            if let Time::Anchored(Anchor { to: AnchorTarget::Word(w), .. }) = &m.time {
                slots.push((Node::Marker(i), w.clone()));
            }
        }
        for (n, w) in slots {
            if !plays.contains(&w.clip) {
                continue;
            }
            let Some(old) = old_words.get(w.index) else { continue };
            let who = match n {
                Node::ClipEnd(_) => format!("the end of {}", node_id(self.p, n)),
                _ => node_id(self.p, n).to_string(),
            };
            match best_word(old, &new_words, w.end) {
                Some(j) => {
                    let new_text = new_words[j].text.trim().to_string();
                    if norm_word(&new_text) != norm_word(&old.text) {
                        notes.push(format!("{who} was anchored to {:?} (word {}); the new transcript has {:?} there (word {j}).", old.text.trim(), w.index, new_text));
                    }
                    if j != w.index || w.text.as_deref() != Some(new_text.as_str()) {
                        remaps.push((n, Some(WordRef { clip: w.clip.clone(), index: j, end: w.end, text: Some(new_text) })));
                    }
                }
                None => {
                    notes.push(format!("{who} was anchored to {:?} (word {}), which the new transcript no longer has; it keeps its time as an absolute time.", old.text.trim(), w.index));
                    remaps.push((n, None));
                }
            }
        }
        for (n, w) in remaps {
            let now = node_value(self.p, &before, n);
            let t = node_time(self.p, n);
            match (w, t.clone()) {
                (Some(w), Time::Anchored(a)) => *t = Time::Anchored(Anchor { to: AnchorTarget::Word(w), offset: a.offset }),
                (None, _) => {
                    if let Some(v) = now {
                        *t = Time::At(v);
                    }
                }
                _ => {}
            }
        }
        for n in notes {
            self.warn(n);
        }
        let _ = force;
        match t {
            Some(t) => {
                self.p.transcripts.insert(asset.to_string(), t.clone());
                self.summary = format!("Set the transcript of {asset} ({} words)", t.words.len());
            }
            None => {
                self.p.transcripts.remove(asset);
                self.summary = format!("Removed the transcript of {asset}");
            }
        }
        let after = resolve(self.p);
        let old: HashSet<&str> = before.errors.iter().map(|e| e.id.as_str()).collect();
        let broken: Vec<&str> = after.errors.iter().filter(|e| !old.contains(e.id.as_str())).map(|e| e.id.as_str()).collect();
        if !broken.is_empty() {
            {
                // Items whose re-mapped word is cut out of their clip keep their time as an absolute time.
                let mut fixed: Vec<String> = Vec::new();
                for _ in 0..8 {
                    let r = resolve(self.p);
                    let roots: Vec<Id> = r
                        .errors
                        .iter()
                        .filter(|e| !old.contains(e.id.as_str()) && e.kind != crate::resolve::ResolveErrorKind::Blocked)
                        .map(|e| e.id.clone())
                        .collect();
                    if roots.is_empty() {
                        break;
                    }
                    for id in roots {
                        let t = before.span(&id).map(|s| s.start).or_else(|| before.marker(&id));
                        let Some(t) = t else { continue };
                        if let Some(c) = self.p.clips.iter_mut().find(|c| c.id == id) {
                            c.start = Time::At(t);
                        } else if let Some(m) = self.p.markers.iter_mut().find(|m| m.id == id) {
                            m.time = Time::At(t);
                        }
                        fixed.push(format!("{id} ({} s)", util::secs(t)));
                    }
                }
                if !fixed.is_empty() {
                    self.warn(format!("{} are anchored to words outside their clip now; they keep their times as absolute times.", util::join_limited(&fixed, 10)));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn cut_source_op(&mut self, clip: &str, from: f64, to: f64, linked: bool) -> Result<(), EditError> {
        editable(self.p, clip)?;
        if !(from.is_finite() && to.is_finite() && to > from) {
            return fail(format!("cut_source needs from < to in source seconds (got {from}–{to})."));
        }
        let removed = self.cut_source(clip, from, to, linked)?;
        self.summary = format!("Cut source {}–{} s out of {clip} ({} s of timeline removed)", util::secs(from), util::secs(to), util::num(removed));
        Ok(())
    }

    /// Removes source `[a, b)` from a clip (split + ripple delete). Returns the timeline seconds removed.
    pub(crate) fn cut_source(&mut self, id: &str, a: f64, b: f64, linked: bool) -> Result<f64, EditError> {
        // Cutting detached audio cuts its video too (and the reverse).
        let lead = if linked { leader_of(self.p, id) } else { id.to_string() };
        let id = lead.as_str();
        editable(self.p, id)?;
        let ci = clip_ix(self.p, id)?;
        let c = self.p.clips[ci].clone();
        let Some((smin, smax)) = source_span(&c) else {
            return fail(format!("{id} is a {} clip; cutting source time needs a media clip.", kind_word(&c)));
        };
        let lo = a.max(smin);
        let hi = b.min(smax);
        if hi - lo <= 1e-9 {
            return fail(format!(
                "source {}–{} s is not in {id}, which plays source {}–{} s.",
                util::secs(a),
                util::secs(b),
                util::secs(smin),
                util::secs(smax)
            ));
        }
        let (Some(la), Some(lb)) = (local_time_of_source(&c, lo), local_time_of_source(&c, hi)) else {
            return fail(format!("cannot map source {}–{} s into {id} (is it frozen?).", util::secs(lo), util::secs(hi)));
        };
        let (la, lb) = (la.min(lb), la.max(lb));
        // A picture (and the sound linked to it) is cut on frame boundaries, so its pieces start and end
        // on frames and linked sound (never rounded itself) stays exactly as long as its picture.
        let dur = c.duration;
        let (la, lb) = match resolve(self.p).span(id) {
            Some(s) => ((snap_for(self.p, &c.track, s.start + la) - s.start).clamp(0.0, dur), (snap_for(self.p, &c.track, s.start + lb) - s.start).clamp(0.0, dur)),
            None => (la, lb),
        };
        if lb - la < MIN_DURATION {
            return Ok(0.0);
        }
        if la <= MIN_DURATION && lb >= dur - MIN_DURATION {
            self.delete(id, true, linked, true)?;
            return Ok(dur);
        }
        let mid = if la > MIN_DURATION { self.split_local(id, la, None, linked)? } else { id.to_string() };
        if lb < dur - MIN_DURATION {
            self.split_local(&mid, lb - la, None, linked)?;
        }
        self.delete(&mid, true, linked, true)?;
        Ok(lb - la)
    }

    pub(crate) fn delete_words(&mut self, clip: &str, from: usize, to: usize, pad: Option<f64>, edges: Option<[f64; 2]>) -> Result<(), EditError> {
        let ci = editable(self.p, clip)?;
        let c = self.p.clips[ci].clone();
        let Source::Media { asset, .. } = &c.source else {
            return fail(format!("{clip} is a {} clip; delete_words needs a media clip whose media has a transcript.", kind_word(&c)));
        };
        let asset = asset.clone();
        let Some(tr) = self.p.transcripts.get(&asset) else {
            return fail(format!("{asset} (the media of {clip}) has no transcript; transcribe it first (set_transcript)."));
        };
        let words = tr.words.clone();
        if words.is_empty() {
            return fail(format!("the transcript of {asset} has no words."));
        }
        if from > to || to >= words.len() {
            return fail(format!("word range {from}–{to} is invalid: the transcript of {asset} has words 0–{} and from must be ≤ to.", words.len() - 1));
        }
        let pad = pad.unwrap_or(DEFAULT_WORD_PAD);
        if !(pad.is_finite() && (0.0..=1.0).contains(&pad)) {
            return fail(format!("pad must be 0–1 s (got {pad}); the default is {DEFAULT_WORD_PAD}."));
        }
        let ws = words[from].start;
        let we = words[from..=to].iter().map(|w| w.end).fold(f64::MIN, f64::max);
        let prev_end = if from > 0 { words[from - 1].end } else { f64::NEG_INFINITY };
        let next_start = if to + 1 < words.len() { words[to + 1].start } else { f64::INFINITY };
        // Remove a margin around the deleted words, but keep `pad` of breathing room around the words
        // that stay (never cutting into them).
        let (cut_a, cut_b) = match edges {
            None => ((ws - pad).max(prev_end + pad).min(ws).max(0.0), (we + pad).min(next_start - pad).max(we)),
            Some([a, b]) => {
                if !(a.is_finite() && b.is_finite() && a >= 0.0 && b > a) {
                    return fail(format!("edges must be source seconds [start, end] with 0 ≤ start < end (got [{a}, {b}])."));
                }
                if (a - ws).abs() > MAX_EDGE_SHIFT || (b - we).abs() > MAX_EDGE_SHIFT {
                    return fail(format!(
                        "edges [{}, {}] are too far from words {from}–{to} (source {}–{} s): each edge must be within {MAX_EDGE_SHIFT} s of the words' own edges.",
                        util::secs(a),
                        util::secs(b),
                        util::secs(ws),
                        util::secs(we)
                    ));
                }
                (a, b)
            }
        };
        let r = resolve(self.p);
        let mut pieces = self.pieces(clip, &asset, cut_a, cut_b, &r);
        if pieces.is_empty() {
            // The words may be in another piece of the same recording (after earlier cuts).
            let near = r.span(clip).map(|s| s.start).unwrap_or(0.0);
            let other = self
                .p
                .clips
                .iter()
                .filter(|o| o.id != clip && o.track == c.track && matches!(&o.source, Source::Media { asset: a, .. } if *a == asset))
                .filter(|o| source_span(o).is_some_and(|(lo, hi)| hi > cut_a + 1e-9 && lo < cut_b - 1e-9))
                .filter_map(|o| Some((o.id.clone(), (r.span(&o.id)?.start - near).abs())))
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(id, _)| id);
            if let Some(o) = other {
                editable(self.p, &o)?;
                pieces = self.pieces(&o, &asset, cut_a, cut_b, &r);
            }
        }
        if pieces.is_empty() {
            let (a, b) = source_span(&c).unwrap_or((0.0, 0.0));
            return fail(format!(
                "words {from}–{to} (source {}–{} s) are not in {clip}, which plays source {}–{} s.",
                util::secs(ws),
                util::secs(we),
                util::secs(a),
                util::secs(b)
            ));
        }
        // Captions that showed these words are rebuilt by the caption pass that runs after every op.
        let mut removed = 0.0;
        for piece in &pieces {
            if clip_ix(self.p, piece).is_ok() {
                removed += self.cut_source(piece, cut_a, cut_b, true)?;
            }
        }
        let text: Vec<String> = words[from..=to].iter().map(|w| clean_word(&w.text)).collect();
        self.summary = format!(
            "Deleted words {from}–{to} \"{}\" from {clip}: cut source {}–{} s ({} s)",
            util::short(&crate::words::join_words(&text), 60),
            util::secs(cut_a),
            util::secs(cut_b),
            util::num(removed)
        );
        Ok(())
    }

    /// The clip plus touching neighbours on its track that play the same asset and intersect
    /// source `[a, b]` (a cut range can straddle pieces of an earlier cut).
    fn pieces(&self, clip: &str, asset: &str, a: f64, b: f64, r: &Resolved) -> Vec<Id> {
        let Some(c0) = self.p.clips.iter().find(|c| c.id == clip) else { return vec![] };
        let intersects = |c: &Clip| source_span(c).is_some_and(|(lo, hi)| hi > a + 1e-9 && lo < b - 1e-9);
        let same = |c: &Clip| c.track == c0.track && matches!(&c.source, Source::Media { asset: x, .. } if x == asset);
        let mut out: Vec<Id> = Vec::new();
        if !intersects(c0) {
            return out;
        }
        out.push(clip.to_string());
        let mut spans: Vec<crate::resolve::Span> = r.span(clip).into_iter().collect();
        loop {
            let mut grew = false;
            let cands: Vec<(Id, crate::resolve::Span)> = self
                .p
                .clips
                .iter()
                .filter(|c| same(c) && !out.contains(&c.id) && intersects(c))
                .filter_map(|c| Some((c.id.clone(), r.span(&c.id)?)))
                .collect();
            for (id, s) in cands {
                if spans.iter().any(|x| (x.end - s.start).abs() < 1e-3 || (s.end - x.start).abs() < 1e-3) {
                    out.push(id);
                    spans.push(s);
                    grew = true;
                }
            }
            if !grew || out.len() > 64 {
                break;
            }
        }
        out
    }

    /// Runs after every op: a caption whose words now play at different times relative to each other (or
    /// not at all, or whose word anchor was replaced) is rebuilt from the transcript — re-anchored to its
    /// first word still heard, highlight windows recomputed, missing words dropped — and removed when no
    /// word is left. Then caption tails that run into the next caption are shortened instead of
    /// overlapping. Captions this op edited directly, detached (absolute) or whose transcript it replaced
    /// are left alone. Returns the ids re-timed and removed.
    pub(crate) fn resync_captions(&mut self, before: &Project, rb: &Resolved, caps: &[CapInfo]) -> Result<(Vec<Id>, Vec<Id>), EditError> {
        let mut retimed: Vec<Id> = Vec::new();
        let mut removed: Vec<Id> = Vec::new();
        if caps.is_empty() {
            return Ok((retimed, removed));
        }
        let r = resolve(self.p);
        let shown = crate::resolve::resolve(self.p);
        let mut tracks: HashSet<Id> = HashSet::new();
        for cap in caps {
            let Ok(ci) = clip_ix(self.p, &cap.id) else { continue };
            let c = &self.p.clips[ci];
            if c.source != cap.source || before.transcripts.get(&cap.asset) != self.p.transcripts.get(&cap.asset) {
                continue;
            }
            let Some(words) = self.p.transcripts.get(&cap.asset).map(|t| t.words.clone()) else { continue };
            let (offset, prefer, word_ok) = match &c.start {
                Time::At(_) => continue,
                Time::Anchored(Anchor { to: AnchorTarget::Word(w), offset }) => (*offset, w.clip.clone(), w.index == cap.indices[0]),
                Time::Anchored(_) => (cap.offset, cap.anchor_clip.clone(), false),
            };
            let near = shown.span(&cap.id).map(|s| s.start);
            // Word times as they are heard (shown timeline, like the highlight windows).
            let now: Vec<Option<(Id, f64)>> = cap.indices.iter().map(|&i| locate_start(self.p, &shown, &cap.voice_track, &cap.asset, words[i].start, &prefer, near)).collect();
            // Only captions this op affected: their anchor word, their (frame-accurate) start or a word time
            // changed. Such a caption is fine when every highlight still starts exactly when its word is heard.
            let (s_now, s_before) = (shown.span(&cap.id).map(|s| s.start), rb.span(&cap.id).map(|s| s.start));
            let moved = !word_ok
                || now.iter().any(Option::is_none)
                || s_now.zip(s_before).is_none_or(|(a, b)| (a - b).abs() > 1e-9)
                || now.iter().zip(&cap.times).any(|(n, t)| n.as_ref().is_none_or(|x| (x.1 - t).abs() > 1e-9));
            if !moved {
                continue;
            }
            let wins: Vec<[f64; 2]> = match &c.source {
                Source::Text(t) => t.highlight.as_ref().map(|h| h.words.clone()).unwrap_or_default(),
                _ => vec![],
            };
            let in_sync = if cap.plain {
                // No windows to keep in sync: fine while its words keep their spacing.
                word_ok
                    && now.iter().all(Option::is_some)
                    && now.iter().zip(&cap.times).all(|(n, t)| n.as_ref().is_some_and(|x| ((x.1 - now[0].as_ref().map_or(0.0, |y| y.1)) - (t - cap.times[0])).abs() <= 1e-6))
            } else {
                word_ok
                    && wins.len() == now.len()
                    && s_now.is_some_and(|s| now.iter().zip(&wins).all(|(n, w)| n.as_ref().is_some_and(|x| (s + w[0] - x.1).abs() <= 1e-6)))
            };
            let track = c.track.clone();
            if in_sync {
                continue;
            }
            self.notes.retain(|(i, _)| i != &cap.id);
            let keep: Vec<(usize, usize, Id, f64)> = now.iter().enumerate().filter_map(|(k, n)| n.as_ref().map(|(pc, t)| (k, cap.indices[k], pc.clone(), *t))).collect();
            if keep.is_empty() {
                let text = match &c.source {
                    Source::Text(t) => util::short(&t.text, 40),
                    _ => String::new(),
                };
                self.delete(&cap.id, false, false, false)?;
                self.warn(format!("a caption ({text:?}) lost all of its words and was removed."));
                removed.push(cap.id.clone());
                continue;
            }
            let (_, first, piece, t0) = keep[0].clone();
            // The anchor resolves on the raw timeline; windows count from where the caption is shown
            // (its frame-accurate start) and are matched to when the words are heard.
            let t0_raw = locate_start(self.p, &r, &cap.voice_track, &cap.asset, words[first].start, &piece, None).map(|x| x.1).unwrap_or(t0);
            let raw_start = t0_raw + offset;
            let start = snap_for(self.p, &track, raw_start);
            let mut wins = Vec::with_capacity(keep.len());
            let mut texts = Vec::with_capacity(keep.len());
            let ends = unit_ends(&words);
            for (n, (k, i, pc, t)) in keep.iter().enumerate() {
                // A caption word may be several tokens ("$2" ".11"): up to the next caption word.
                let stop = keep.get(n + 1).map(|x| x.1).unwrap_or(usize::MAX);
                let last_tok = ends.get(*i).copied().unwrap_or(i + 1).min(stop).max(i + 1) - 1;
                let w = &words[last_tok];
                let e = locate(self.p, &shown, &cap.voice_track, &cap.asset, w.end, pc, Some(*t)).map(|x| x.1).filter(|e| *e >= *t).unwrap_or(*t + (w.end - words[*i].start));
                wins.push([t - start, e - start]);
                texts.push(cap.texts.as_ref().map(|x| x[*k].clone()).unwrap_or_else(|| unit_text(&words, *i..last_tok + 1)));
            }
            let last = wins.last().map(|w| w[1]).unwrap_or(0.0);
            let lost = cap.indices.len() - keep.len();
            let c = &mut self.p.clips[ci];
            c.start = Time::Anchored(Anchor { to: AnchorTarget::Word(WordRef { clip: piece, index: first, end: false, text: None }), offset });
            // Shown until `tail` after its last word; the duration counts from the anchor (the raw start).
            if c.until.is_none() {
                c.duration = (start + last + cap.tail - raw_start).max(0.1);
            }
            if cap.plain {
                if lost > 0 {
                    self.warnings.push(format!("caption {} lost {lost} of its words; its text was written by hand, so it was kept: check it.", cap.id));
                }
            } else if let Source::Text(t) = &mut c.source {
                t.text = texts.join(" ");
                if let Some(h) = &mut t.highlight {
                    h.words = wins;
                }
            }
            if let Some(w) = clamp_fades(c) {
                self.warn(w);
            }
            retimed.push(cap.id.clone());
            tracks.insert(track);
        }
        // Any caption that moved (karaoke or not) may now run into its neighbour.
        let after = crate::resolve::resolve(self.p);
        for c in self.p.clips.iter().filter(|c| is_caption_like(c)) {
            if rb.span(&c.id) != after.span(&c.id) {
                tracks.insert(c.track.clone());
            }
        }
        self.fit_captions(&tracks)?;
        Ok((retimed, removed))
    }

    /// Shortens captions on `tracks` that run into the next caption (never below [`MIN_CAPTION`]); their
    /// text and look stay. Reported in the summary.
    fn fit_captions(&mut self, tracks: &HashSet<Id>) -> Result<(), EditError> {
        if tracks.is_empty() {
            return Ok(());
        }
        let r = resolve(self.p);
        let mut by_track: HashMap<Id, Vec<(Id, crate::resolve::Span)>> = HashMap::new();
        for c in self.p.clips.iter().filter(|c| tracks.contains(&c.track) && is_caption_like(c)) {
            if let Some(s) = r.span(&c.id) {
                by_track.entry(c.track.clone()).or_default().push((c.id.clone(), s));
            }
        }
        let mut done: Vec<String> = Vec::new();
        let mut keys: Vec<Id> = by_track.keys().cloned().collect();
        keys.sort();
        for t in keys {
            let mut v = by_track.remove(&t).unwrap_or_default();
            v.sort_by(|a, b| a.1.start.total_cmp(&b.1.start).then(a.0.cmp(&b.0)));
            for k in 0..v.len().saturating_sub(1) {
                let (a, b) = (v[k].1, v[k + 1].1);
                let len = b.start - a.start;
                if a.end > b.start + EPS && len >= MIN_DURATION.max(MIN_CAPTION.min(a.duration())) {
                    let ci = clip_ix(self.p, &v[k].0)?;
                    let cut = a.end - b.start;
                    let c = &mut self.p.clips[ci];
                    match &mut c.until {
                        Some(u) => shift_time(u, -cut),
                        None => c.duration = len,
                    }
                    if let Some(w) = clamp_fades(&mut self.p.clips[ci]) {
                        self.warn(w);
                    }
                    if !self.made.contains(&v[k].0) {
                        done.push(format!("{} by {} s (it ran into {})", v[k].0, util::num(cut), v[k + 1].0));
                    }
                }
            }
        }
        if !done.is_empty() {
            self.summary.push_str(&format!("; shortened caption(s) {}", util::join_limited(&done, 8)));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn generate_captions(
        &mut self,
        clips: &[Id],
        track: &str,
        style: Option<&CaptionStyle>,
        max_words: Option<usize>,
        max_chars: Option<usize>,
        pause: Option<f64>,
        replace: bool,
    ) -> Result<(), EditError> {
        let ti = track_ix(self.p, track)?;
        if self.p.tracks[ti].kind != TrackKind::Video {
            return fail(format!("{track} is an audio track; captions are pictures, so put them on a video track (add one with {{\"op\":\"add_track\",\"kind\":\"video\",\"role\":\"captions\"}})."));
        }
        ensure_unlocked(self.p, track)?;
        if clips.is_empty() {
            return fail("`clips` is empty; list the voice clips to caption.");
        }
        let mut seen: HashSet<&str> = HashSet::new();
        let clips: Vec<Id> = clips.iter().filter(|c| seen.insert(c.as_str())).cloned().collect();
        if let Some(st) = style {
            let h = self.p.settings.height as f64;
            let checks: [(&str, Option<f64>, f64, f64); 4] = [
                ("size", st.size, 1.0, 2000.0),
                ("y", st.y, -10.0 * h, 10.0 * h),
                ("max_width", st.max_width, 1.0, 100_000.0),
                ("stroke.width", st.stroke.as_ref().map(|s| s.width), 0.0, 500.0),
            ];
            for (name, v, lo, hi) in checks {
                if let Some(v) = v {
                    if !(v.is_finite() && v >= lo && v <= hi) {
                        return fail(format!("caption style {name} = {v} is out of range; use {}–{}.", util::num(lo), util::num(hi)));
                    }
                }
            }
            if let Some(w) = st.weight {
                if !(1..=1000).contains(&w) {
                    return fail(format!("caption style weight = {w} is out of range; use 100–900 (700 = bold)."));
                }
            }
        }
        let (w, h) = (self.p.settings.width as f64, self.p.settings.height as f64);
        let portrait = h > w;
        let max_words = max_words.unwrap_or(5);
        let max_chars = max_chars.unwrap_or(if portrait { 20 } else { 32 });
        let pause = pause.unwrap_or(0.5);
        if max_words == 0 || max_chars < 4 {
            return fail("max_words must be at least 1 and max_chars at least 4.");
        }
        if !(pause.is_finite() && pause >= 0.0) {
            return fail("pause must be 0 or more seconds.");
        }
        let mut assets: HashSet<Id> = HashSet::new();
        let mut sources: Vec<Clip> = Vec::new();
        for id in &clips {
            let ci = clip_ix(self.p, id)?;
            let c = self.p.clips[ci].clone();
            let Source::Media { asset, .. } = &c.source else {
                return fail(format!("{id} is a {} clip; captions come from the transcript of a media clip.", kind_word(&c)));
            };
            if !self.p.transcripts.contains_key(asset) {
                return fail(format!("{asset} (the media of {id}) has no transcript; transcribe it first."));
            }
            if c.reverse {
                self.warn(format!("{id} plays backwards; it was skipped."));
                continue;
            }
            assets.insert(asset.clone());
            sources.push(c);
        }
        // Existing captions of these clips on the target track. Ownership is per voice clip (the
        // caption's anchor clip): a media file used by another clip keeps that clip's captions.
        let mine: HashSet<&str> = clips.iter().map(String::as_str).collect();
        let on_track: Vec<Clip> = self.p.clips.iter().filter(|c| c.track == track && is_caption(c)).cloned().collect();
        let existing: Vec<Clip> = on_track
            .iter()
            .filter(|c| matches!(&c.start, Time::Anchored(Anchor { to: AnchorTarget::Word(w), .. }) if mine.contains(w.clip.as_str())))
            .cloned()
            .collect();
        // Their look: these clips' captions first, else any captions on the track.
        let look_from = if existing.is_empty() { &on_track } else { &existing };
        let template = if style.is_some_and(|s| *s != CaptionStyle::default()) || look_from.is_empty() { None } else { most_common_look(look_from) };
        let template = template.or_else(|| if style.is_none() { None } else { most_common_look(look_from) });
        let r0 = resolve(self.p);
        let caps_now = collect_caps(self.p, &r0);
        let mut kept: Vec<Id> = Vec::new();
        let mut replaced: Vec<Id> = Vec::new();
        // Source ranges not to caption again: kept captions' words and ranges a person deleted.
        let mut blocked: Vec<(Id, f64, f64)> = Vec::new();
        let mut kept_spans: Vec<crate::resolve::Span> = Vec::new();
        for c in &existing {
            let edited = c.tags.iter().any(|t| t == EDITED_TAG);
            if edited && !replace {
                kept.push(c.id.clone());
                if let Some(s) = r0.span(&c.id) {
                    kept_spans.push(s);
                }
                if let Some(cap) = caps_now.iter().find(|x| x.id == c.id) {
                    if let (Some(ws), Some(first), Some(last)) = (self.p.transcripts.get(&cap.asset), cap.indices.first(), cap.indices.last()) {
                        let last = unit_ends(&ws.words).get(*last).copied().unwrap_or(last + 1).max(last + 1) - 1;
                        if let (Some(a), Some(b)) = (ws.words.get(*first), ws.words.get(last)) {
                            blocked.push((cap.anchor_clip.clone(), a.start, b.end));
                        }
                    }
                }
            } else {
                replaced.push(c.id.clone());
            }
        }
        for id in &replaced {
            if clip_ix(self.p, id).is_ok() {
                self.delete(id, false, false, false)?;
            }
        }
        // Words whose captions a person deleted, per voice clip (older projects keyed them by media:
        // those apply to every clip of that media).
        let excluded = |p: &Project, c: &Clip| -> Vec<[f64; 2]> {
            let mut v = p.caption_exclusions.get(&c.id).cloned().unwrap_or_default();
            if let Source::Media { asset, .. } = &c.source {
                if p.media.iter().any(|m| &m.id == asset) && !p.clips.iter().any(|x| &x.id == asset) {
                    v.extend(p.caption_exclusions.get(asset).cloned().unwrap_or_default());
                }
            }
            v
        };
        let mut person_deleted: Vec<(Id, f64, f64)> = Vec::new();
        if replace {
            for c in &sources {
                self.p.caption_exclusions.remove(&c.id);
            }
            // A media-keyed (older) entry goes once every clip of that media is regenerated.
            for a in &assets {
                if self.p.clips.iter().filter(|c| matches!(&c.source, Source::Media { asset, .. } if asset == a)).all(|c| mine.contains(c.id.as_str())) {
                    self.p.caption_exclusions.remove(a);
                }
            }
        } else {
            for c in &sources {
                for r in excluded(self.p, c) {
                    blocked.push((c.id.clone(), r[0], r[1]));
                    person_deleted.push((c.id.clone(), r[0], r[1]));
                }
            }
        }
        let r = resolve(self.p);
        let mut cw: Vec<CapWord> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        let mut skipping = false;
        for c in &sources {
            let Some(span) = r.span(&c.id) else { continue };
            let Source::Media { asset, .. } = &c.source else { continue };
            let words = &self.p.transcripts[asset].words;
            let Some((lo, hi)) = source_span(c) else { continue };
            let speed_end = sample(&c.speed, c.duration).max(1e-6);
            let ends = unit_ends(words);
            let mut mine: Vec<CapWord> = Vec::new();
            for (i, wd) in words.iter().enumerate() {
                if wd.start < lo - 1e-9 || wd.start > hi + 1e-9 {
                    continue;
                }
                let Some(l0) = local_time_of_source(c, wd.start) else { continue };
                if l0 >= c.duration - 1e-9 {
                    continue; // starts where the clip ends: not heard in it
                }
                let t0 = span.start + l0;
                let t1 = local_time_of_source(c, wd.end).map(|l| span.start + l).unwrap_or_else(|| (t0 + (wd.end - wd.start) / speed_end).min(span.end));
                let text = caption_word(&wd.text);
                if text.is_empty() {
                    continue;
                }
                let mid = 0.5 * (wd.start + wd.end.max(wd.start));
                if blocked.iter().any(|(k, x, y)| k == &c.id && mid >= *x - 1e-9 && mid <= *y + 1e-9) {
                    // Kept or deleted by a person: not captioned again.
                    let by_person = person_deleted.iter().any(|(k, x, y)| k == &c.id && mid >= *x - 1e-9 && mid <= *y + 1e-9);
                    if by_person {
                        if skipping {
                            if let Some(last) = skipped.last_mut() {
                                *last = crate::words::join_words(&[last.as_str(), text.as_str()]);
                            }
                        } else {
                            skipped.push(text.clone());
                        }
                        skipping = true;
                    }
                    continue;
                }
                skipping = false;
                let text = if style.is_some_and(|s| s.uppercase) { text.to_uppercase() } else { text };
                // Tokens that read as one word ("$2" ".11", "four" "-hour", "times" ".") become one caption
                // word with one highlight window, never split across caption lines.
                if let Some(prev) = mine.last_mut() {
                    if ends.get(prev.last) == ends.get(i) && prev.last < i {
                        prev.text = crate::words::join_words(&[prev.text.as_str(), text.as_str()]);
                        prev.t1 = prev.t1.max(t1);
                        prev.last = i;
                        continue;
                    }
                }
                // A lone mark that reads with nothing (e.g. a dash the voice clip starts with) is not a word.
                if is_punct_only(&text) && ends.get(i).is_none_or(|e| *e <= i + 1) {
                    continue;
                }
                mine.push(CapWord { clip: c.id.clone(), index: i, last: i, text, t0, t1: t1.max(t0), clip_end: span.end });
            }
            cw.extend(mine);
        }
        if cw.is_empty() && kept.is_empty() && skipped.is_empty() {
            return fail("no transcript words fall inside those clips (check their in-points and the transcript).");
        }
        cw.sort_by(|a, b| a.t0.total_cmp(&b.t0));
        let mut groups: Vec<Vec<CapWord>> = Vec::new();
        for wd in cw {
            let new_group = match groups.last() {
                None => true,
                Some(g) => {
                    let last = g.last().expect("groups are never empty");
                    let chars: usize = g.iter().map(|x| x.text.chars().count() + 1).sum::<usize>() + wd.text.chars().count();
                    // A kept caption between two words also splits the line.
                    let kept_between = kept_spans.iter().any(|s| s.start >= last.t0 - 1e-9 && s.start <= wd.t0 + 1e-9);
                    last.clip != wd.clip || g.len() >= max_words || chars > max_chars || wd.t0 - last.t1 >= pause || ends_sentence(&last.text) || kept_between
                }
            };
            if new_group {
                groups.push(vec![wd]);
            } else if let Some(g) = groups.last_mut() {
                g.push(wd);
            }
        }
        // Look: the most common look of the captions already on the track, else the default style; fields
        // given in `style` win.
        let st = style.cloned().unwrap_or_default();
        let (base_text, base_tf) = match template {
            Some((t, tf)) => {
                let (mut t, mut tf) = (t, tf);
                apply_style(&mut t, &mut tf, &st, self.p);
                (t, tf)
            }
            None => default_look(&st, self.p),
        };
        let hl = base_text.highlight.as_ref().map(|h| h.color).unwrap_or(Color::rgba8(0xFF, 0xD6, 0x0A, 0xFF));
        let hl_bg = base_text.highlight.as_ref().and_then(|h| h.background);
        let mut made = Vec::new();
        for gi in 0..groups.len() {
            let g = &groups[gi];
            let start_t = g[0].t0;
            if kept_spans.iter().any(|s| start_t >= s.start - 1e-9 && start_t < s.end - 1e-9) {
                continue;
            }
            let next_kept = kept_spans.iter().map(|s| s.start).filter(|s| *s > start_t + 1e-9).fold(f64::INFINITY, f64::min);
            let next_start = groups.get(gi + 1).map(|n| n[0].t0).unwrap_or(f64::INFINITY).min(next_kept);
            let last_end = g.last().map(|x| x.t1).unwrap_or(start_t);
            let end = if next_start - last_end <= pause { next_start } else { (last_end + 0.3).min(next_start) };
            // (added) The caption leaves with its voice: never past the end of the clip its last word is heard
            // in (on a cut to someone else, their caption should not wait under the next person's face).
            let clip_end = g.last().map(|x| x.clip_end).unwrap_or(f64::INFINITY);
            let end = if clip_end > start_t + MIN_DURATION { end.min(clip_end) } else { end };
            let mut dur = (end - start_t).max(0.1);
            if next_start.is_finite() {
                dur = dur.min(next_start - start_t);
            }
            if dur < MIN_DURATION {
                continue;
            }
            let id = self.ids.fresh("c");
            let mut text_src = base_text.clone();
            text_src.text = g.iter().map(|x| x.text.as_str()).collect::<Vec<_>>().join(" ");
            // Highlight windows count from where the caption is shown (its frame-accurate start).
            let shown_start = snap_for(self.p, track, start_t);
            text_src.highlight = Some(WordHighlight { color: hl, words: g.iter().map(|x| [x.t0 - shown_start, x.t1 - shown_start]).collect(), background: hl_bg });
            let clip = Clip {
                id: id.clone(),
                track: track.to_string(),
                name: String::new(),
                start: Time::Anchored(Anchor { to: AnchorTarget::Word(WordRef { clip: g[0].clip.clone(), index: g[0].index, end: false, text: None }), offset: 0.0 }),
                duration: dur,
                until: None,
                source: Source::Text(text_src),
                speed: Param::Value(1.0),
                reverse: false,
                transform: base_tf.clone(),
                opacity: Param::Value(1.0),
                volume: Param::Value(1.0),
                fade_in: 0.0,
                fade_out: 0.0,
                muted: false,
                blend: BlendMode::Normal,
                effects: vec![],
                enabled: true,
                tags: vec![CAPTION_TAG.to_string()],
                notes: String::new(),
                motion_blur: None,
                frame_blend: false,
                matte: None,
            };
            self.p.clips.push(clip);
            self.made.push(id.clone());
            made.push(id);
        }
        let mut s = format!(
            "Generated {} caption(s) on {track} from {}{}",
            made.len(),
            clips.join(", "),
            made.first().map(|f| format!(" ({f}–{})", made.last().unwrap_or(f))).unwrap_or_default()
        );
        if !replaced.is_empty() {
            s.push_str(&format!("; replaced {} earlier caption(s){}", replaced.len(), if replace { "" } else { " nobody had edited" }));
        }
        if !kept.is_empty() {
            s.push_str(&format!("; kept {} caption(s) a person edited: {} (replace: true overrides)", kept.len(), util::join_limited(&kept, 10)));
        }
        if !skipped.is_empty() {
            s.push_str(&format!(
                "; skipped words whose captions a person deleted: {} (replace: true recreates them)",
                skipped.iter().map(|t| format!("{:?}", util::short(t, 40))).collect::<Vec<_>>().join(", ")
            ));
        }
        self.summary = s;
        self.details = serde_json::json!({"created": made, "replaced": replaced, "kept": kept, "skipped": skipped});
        Ok(())
    }

    /// After an op by a person: generated captions they edited get the `edited` tag (kept by
    /// `generate_captions`), and the words of generated captions they deleted are remembered so they
    /// are not captioned again.
    pub(crate) fn note_human_caption_changes(&mut self, op: &super::Op, before: &Project, caps: &[CapInfo], dropped: &[Id]) {
        let named = named_clips(op);
        let old: HashMap<&str, &Clip> = before.clips.iter().map(|c| (c.id.as_str(), c)).collect();
        for c in self.p.clips.iter_mut() {
            if c.tags.iter().any(|t| t == CAPTION_TAG) && named.contains(&c.id) && old.get(c.id.as_str()).is_some_and(|o| *o != c) && !c.tags.iter().any(|t| t == EDITED_TAG) {
                c.tags.push(EDITED_TAG.to_string());
            }
        }
        if op.name() == "generate_captions" {
            return;
        }
        let alive: HashSet<&str> = self.p.clips.iter().map(|c| c.id.as_str()).collect();
        let mut add: Vec<(Id, [f64; 2])> = Vec::new();
        for cap in caps {
            if alive.contains(cap.id.as_str()) || dropped.contains(&cap.id) {
                continue;
            }
            let Some(ws) = before.transcripts.get(&cap.asset) else { continue };
            // Up to the last token of its last caption word ("$2" ".11" reads as one).
            let last = cap.indices.last().map(|i| unit_ends(&ws.words).get(*i).copied().unwrap_or(i + 1).max(i + 1) - 1);
            if let (Some(a), Some(b)) = (cap.indices.first().and_then(|i| ws.words.get(*i)), last.and_then(|i| ws.words.get(i))) {
                // Remembered for the caption's voice clip only (another clip may play the same media).
                add.push((cap.anchor_clip.clone(), [a.start, b.end]));
            }
        }
        for (clip, r) in add {
            let v = self.p.caption_exclusions.entry(clip).or_default();
            if !v.iter().any(|x| (x[0] - r[0]).abs() < 1e-9 && (x[1] - r[1]).abs() < 1e-9) {
                v.push(r);
            }
        }
    }
}

/// The look of a caption: its text style (no text, no word windows) and placement.
fn look_of(c: &Clip) -> Option<(TextSource, Transform)> {
    let Source::Text(t) = &c.source else { return None };
    let mut t = t.clone();
    t.text.clear();
    if let Some(h) = &mut t.highlight {
        h.words.clear();
    }
    // Timed reveals belong to one caption's length; the word-driven `spoken` reveal is part of the look.
    if t.reveal.kind != RevealKind::Spoken {
        t.reveal = TextReveal::default();
    }
    Some((t, c.transform.clone()))
}

/// The look most captions share (ties: the earliest).
fn most_common_look(caps: &[Clip]) -> Option<(TextSource, Transform)> {
    let mut counts: Vec<((TextSource, Transform), usize)> = Vec::new();
    for c in caps {
        let Some(l) = look_of(c) else { continue };
        match counts.iter_mut().find(|(x, _)| *x == l) {
            Some((_, n)) => *n += 1,
            None => counts.push((l, 1)),
        }
    }
    let max = counts.iter().map(|(_, n)| *n).max()?;
    counts.into_iter().find(|(_, n)| *n == max).map(|(l, _)| l)
}

/// The default caption look with the given style fields applied.
fn default_look(st: &CaptionStyle, p: &Project) -> (TextSource, Transform) {
    let (w, h) = (p.settings.width as f64, p.settings.height as f64);
    let size = st.size.unwrap_or((w.min(h) * 0.06).round()).max(1.0);
    let pad = (size * 0.3).round();
    let box_h = size * 1.15 + 2.0 * pad;
    let ts = p.settings.title_safe;
    let bg = st.background.unwrap_or(Color::rgba8(0, 0, 0, 0xB3));
    let hl = st.highlight.unwrap_or(Color::rgba8(0xFF, 0xD6, 0x0A, 0xFF));
    let y = st.y.unwrap_or((h * ts / 2.0 - box_h / 2.0 - size * 0.25).round());
    let max_width = st.max_width.unwrap_or((w * ts - 2.0 * pad).round());
    let text = TextSource {
        font: st.font.clone().unwrap_or_else(|| TextSource::default().font),
        size,
        weight: st.weight.unwrap_or(800),
        color: st.color.unwrap_or(Color::WHITE),
        max_width: Some(max_width),
        background: (bg.a > 0.0).then_some(TextBox { color: bg, padding: pad, radius: (box_h / 2.0).round() }),
        stroke: st.stroke.clone(),
        highlight: Some(WordHighlight { color: hl, words: vec![], background: None }),
        reveal: TextReveal { kind: st.reveal.unwrap_or_default(), duration: 0.0 },
        ..Default::default()
    };
    let tf = Transform { y: Param::Value(y), ..Default::default() };
    (text, tf)
}

/// Applies the fields set in a caption style to an inherited look.
fn apply_style(t: &mut TextSource, tf: &mut Transform, st: &CaptionStyle, p: &Project) {
    if let Some(f) = &st.font {
        t.font = f.clone();
    }
    if let Some(s) = st.size {
        t.size = s.max(1.0);
        if let Some(b) = &mut t.background {
            b.padding = (s * 0.3).round();
            b.radius = ((s * 1.15 + 2.0 * b.padding) / 2.0).round();
        }
    }
    if let Some(w) = st.weight {
        t.weight = w;
    }
    if let Some(c) = st.color {
        t.color = c;
    }
    if let Some(c) = st.highlight {
        t.highlight.get_or_insert(WordHighlight { color: c, words: vec![], background: None }).color = c;
    }
    if let Some(bg) = st.background {
        if bg.a > 0.0 {
            let pad = (t.size * 0.3).round();
            let b = t.background.get_or_insert(TextBox { color: bg, padding: pad, radius: ((t.size * 1.15 + 2.0 * pad) / 2.0).round() });
            b.color = bg;
        } else {
            t.background = None;
        }
    }
    if let Some(y) = st.y {
        tf.y = Param::Value(y);
    }
    if let Some(m) = st.max_width {
        t.max_width = Some(m);
    }
    if let Some(r) = st.reveal {
        t.reveal = TextReveal { kind: r, duration: 0.0 };
    }
    if let Some(s) = &st.stroke {
        t.stroke = Some(s.clone());
    }
    let _ = p;
}
