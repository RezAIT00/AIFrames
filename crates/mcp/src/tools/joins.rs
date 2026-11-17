//! `audit_joins`: every cut in the edit's sound, measured as it is heard.
//!
//! A join is a place on a track where one clip with sound ends and the next begins. For each side the
//! audit reads the recording's level at the edge (relative to its room tone), how long the quiet there
//! lasts (a real pause, or a dip inside running speech), and its transcript: a word the edge cuts
//! through, a speaker stopped mid-sentence or picked up mid-sentence, a word the join plays twice
//! ("the the"), and the pause the join leaves.

use super::*;
use aiframes_core::{source_time, MediaKind, Source, TrackKind, Word};
use aiframes_media::{classify, edge_level, ends_sentence, is_punctuated, level_db, quiet_before, quiet_run, room_floor, LevelClass, Waveform, MIN_PAUSE, SPEECH_ABOVE_FLOOR};
use std::collections::HashMap;
use std::path::PathBuf;

/// Margin (seconds) a word must reach past an edge to count as cut through.
const WORD_THROUGH: f64 = 0.015;
/// A fade (or a volume ramp to near nothing) at least this long (seconds) takes a voice away gently. A
/// shorter ramp is a de-click: it softens the click of a cut but still chops the word it lands in.
pub(crate) const MIN_FADE: f64 = 0.2;
/// The speaker is still talking within this long (seconds) of a cut: the cut stopped them mid-flow
/// (or picked them up mid-flow).
pub(crate) const CARRIES_ON: f64 = 0.35;
/// A clip that has already been silent this long (seconds) when it stops lets its speaker trail off: a
/// held pause, heard as a finished beat rather than a cut-off, even mid-sentence.
const HELD_PAUSE: f64 = 0.3;

/// One side of a join: a clip's audible edge in its source.
struct Side {
    clip: Id,
    asset: Id,
    /// Source seconds of the edge.
    src: f64,
    /// The clip's sound fades away at this edge (a fade or volume ramp of at least [`MIN_FADE`]): not a
    /// hard cut.
    faded: bool,
    /// Source seconds of the clip's other edge: words outside `src..far` are not heard in this clip.
    far: f64,
}

/// A place where one clip ends and the next begins on a track; a side is `None` when that clip plays no
/// sound (a held frame, a muted or silent clip), so only the other side's edge is heard.
struct Join {
    t: f64,
    track: Id,
    out: Option<Side>,
    inn: Option<Side>,
}

/// Volume at or below this fraction of the clip's level counts as (near) nothing.
const FADED: f64 = 0.1;

/// How long the clip's volume takes to come back from (near) nothing at an edge, going `inward` (+1 from
/// the start, -1 from the end): 0 when the edge is not ramped down at all.
fn ramp_len(c: &Clip, edge: f64, inward: f64) -> f64 {
    let reach = (c.duration / 2.0).min(1.0);
    let at = |d: f64| aiframes_core::sample(&c.volume, edge + inward * d).abs();
    let full = at(reach).max(1e-9);
    if at(0.0) > FADED * full {
        return 0.0;
    }
    let mut d = 0.0;
    while d < reach && at(d) < 0.9 * full {
        d += 0.005;
    }
    d
}

fn side(c: &Clip, asset: Id, local: f64, inward: f64) -> Option<Side> {
    let src = source_time(c, local)?;
    let fade = if inward > 0.0 { c.fade_in } else { c.fade_out };
    let faded = fade >= MIN_FADE || ramp_len(c, local, inward) >= MIN_FADE;
    let far = source_time(c, if inward > 0.0 { c.duration } else { 0.0 }).unwrap_or(src);
    Some(Side { clip: c.id.clone(), asset, src, faded, far })
}

/// How a cut sits in the speech around it, by the transcript (and the recording's level when known).
struct Flow<'a> {
    /// The speaker is mid-sentence at the edge and talking within [`CARRIES_ON`] of it on the far side.
    mid_sentence: bool,
    /// The word just across the edge (the next word after an outgoing edge, the one before an incoming
    /// edge).
    across: Option<&'a Word>,
}

/// `out`: the edge ends a clip (the heard word is before it); otherwise it starts one.
fn flow<'a>(words: &'a [Word], punctuated: bool, edge: f64, far: f64, out: bool, talking: Option<bool>, held: Option<f64>) -> Flow<'a> {
    let before = words.iter().filter(|w| (w.start + w.end) / 2.0 < edge).last();
    let after = words.iter().find(|w| (w.start + w.end) / 2.0 >= edge);
    // A clip that plays no words of its own (a silent stare, room tone) stops or picks up no sentence.
    let plays_words = if out { before.is_some_and(|w| w.end > far) } else { after.is_some_and(|w| w.start < far) };
    if !plays_words {
        return Flow { mid_sentence: false, across: if out { after } else { before } };
    }
    let (across, last_said) = if out { (after, before) } else { (before, before) };
    let (Some(said), Some(other)) = (last_said, across) else { return Flow { mid_sentence: false, across } };
    if !punctuated || ends_sentence(&said.text) {
        return Flow { mid_sentence: false, across };
    }
    // Is the speaker talking just across the edge? The recording's level says so best (recognisers
    // misplace word edges); the word times otherwise.
    let near = if out { other.start - edge } else { edge - other.end };
    // An outgoing clip that already holds a pause after its last word lets the speaker trail off.
    if out && held.unwrap_or(edge - said.end) >= HELD_PAUSE {
        return Flow { mid_sentence: false, across };
    }
    let mid_sentence = talking.unwrap_or(near < CARRIES_ON);
    Flow { mid_sentence, across }
}

/// `s` followed by "…" unless it ends a sentence.
fn trailing(s: &str) -> String {
    if ends_sentence(s) {
        s.to_string()
    } else {
        format!("{s}…")
    }
}

/// A few words of a transcript from `i` going forward (or backward), for messages.
fn words_from(words: &[Word], i: usize, n: usize, forward: bool) -> String {
    let texts: Vec<&str> = if forward {
        words.iter().skip(i).take(n).map(|w| w.text.trim()).collect()
    } else {
        words[..=i.min(words.len().saturating_sub(1))].iter().rev().take(n).rev().map(|w| w.text.trim()).collect()
    };
    texts.join(" ")
}

fn audible(p: &Project, c: &Clip) -> Option<Id> {
    let Source::Media { asset, .. } = &c.source else { return None };
    let m = p.media.iter().find(|m| &m.id == asset)?;
    // A held frame (speed 0, e.g. from freeze_frame) plays no sound.
    let frozen = matches!(c.speed, aiframes_core::Param::Value(v) if v == 0.0);
    (c.enabled && !c.muted && !frozen && m.kind != MediaKind::Image && m.info.has_audio).then(|| asset.clone())
}

/// The joins on tracks that play sound, in time order, within `[from, to]`. Besides clips that butt
/// together, a clip whose sound starts out of silence or stops into it (nothing next to it on its track:
/// the first and last clip, a gap, a J- or L-cut voice on a track of its own) has its edge judged as a join
/// with a silent side: a word cut off there is heard just the same.
fn joins(p: &Project, r: &Resolved, track: Option<&str>, from: f64, to: f64) -> Vec<Join> {
    let tol = frame_dur(p) * 0.5;
    let in_range = |t: f64| t >= from - tol && t <= to + tol;
    let mut out = Vec::new();
    for tr in &p.tracks {
        if tr.muted || track.is_some_and(|t| t != tr.id) || !matches!(tr.kind, TrackKind::Audio | TrackKind::Video) {
            continue;
        }
        let mut clips: Vec<(&Clip, f64, f64)> = p.clips.iter().filter(|c| c.track == tr.id).filter_map(|c| r.span(&c.id).map(|s| (c, s.start, s.end))).collect();
        clips.sort_by(|a, b| a.1.total_cmp(&b.1));
        for (k, &(a, a_start, a_end)) in clips.iter().enumerate() {
            let touches_prev = k > 0 && (a_start - clips[k - 1].2).abs() <= tol;
            if !touches_prev && in_range(a_start) {
                if let Some(i) = audible(p, a).and_then(|asset| side(a, asset, 0.0, 1.0)) {
                    out.push(Join { t: a_start, track: tr.id.clone(), out: None, inn: Some(i) });
                }
            }
            let next = clips.get(k + 1).copied().filter(|&(_, b_start, _)| (b_start - a_end).abs() <= tol);
            let o = audible(p, a).and_then(|asset| side(a, asset, a.duration, -1.0));
            let Some((b, b_start, _)) = next else {
                if let Some(o) = o.filter(|_| in_range(a_end)) {
                    out.push(Join { t: a_end, track: tr.id.clone(), out: Some(o), inn: None });
                }
                continue;
            };
            if !in_range(b_start) {
                continue;
            }
            let i = audible(p, b).and_then(|asset| side(b, asset, 0.0, 1.0));
            if o.is_none() && i.is_none() {
                continue;
            }
            // A clip followed by the rest of the same recording (a split with nothing removed) is no cut.
            if let (Some(x), Some(y)) = (&o, &i) {
                if x.asset == y.asset && (x.src - y.src).abs() < 1e-3 {
                    continue;
                }
            }
            out.push(Join { t: b_start, track: tr.id.clone(), out: o, inn: i });
        }
    }
    out.sort_by(|a, b| a.t.total_cmp(&b.t));
    out
}

/// The last word heard before an outgoing edge (most of it before the edge) and a word the edge cuts.
fn words_at_out(words: &[Word], edge: f64, far: f64) -> (Option<&Word>, Option<&Word>) {
    let heard = words.iter().filter(|w| (w.start + w.end) / 2.0 < edge).last().filter(|w| w.end > far);
    let through = words.iter().find(|w| w.start < edge - WORD_THROUGH && w.end > edge + WORD_THROUGH);
    (heard, through)
}

/// The first word heard after an incoming edge and a word the edge cuts.
fn words_at_in(words: &[Word], edge: f64, far: f64) -> (Option<&Word>, Option<&Word>) {
    let heard = words.iter().find(|w| (w.start + w.end) / 2.0 >= edge).filter(|w| w.start < far);
    let through = words.iter().find(|w| w.start < edge - WORD_THROUGH && w.end > edge + WORD_THROUGH);
    (heard, through)
}

fn side_j(s: &Side, lv: Option<(f64, LevelClass, f64)>, pause: Option<f64>) -> J {
    let mut j = json!({"clip": s.clip, "src": t3(s.src)});
    if let Some((db, class, floor)) = lv {
        j["level_db"] = json!(db.round());
        j["room_db"] = json!(floor.round());
        j["class"] = json!(class.as_str());
    }
    if let Some(p) = pause {
        j["pause_ms"] = json!((p * 1000.0).round());
    }
    if s.faded {
        j["faded"] = json!(true);
    }
    j
}

pub(crate) fn audit_joins(s: &Arc<Services>, args: &J) -> ToolResult {
    let from = arg_f64(args, "from")?.unwrap_or(f64::NEG_INFINITY);
    let to = arg_f64(args, "to")?.unwrap_or(f64::INFINITY);
    let issues_only = arg_bool(args, "issues_only", false)?;
    let track = arg_str(args, "track").map(String::from);
    let (doc, list, paths, transcripts) = with_doc(s, args, |id, g| {
        let (p, r) = (&g.doc.project, g.doc.resolved());
        if let Some(t) = &track {
            if !p.tracks.iter().any(|x| &x.id == t) {
                return Err(format!("unknown track '{t}'"));
            }
        }
        let list = joins(p, r, track.as_deref(), from, to);
        let mut paths: HashMap<Id, (PathBuf, u32)> = HashMap::new();
        let mut transcripts: HashMap<Id, Vec<Word>> = HashMap::new();
        for j in &list {
            for a in [&j.out, &j.inn].into_iter().flatten().map(|s| &s.asset) {
                if !transcripts.contains_key(a) {
                    if let Some(t) = p.transcripts.get(a) {
                        transcripts.insert(a.clone(), t.words.clone());
                    }
                }
                // Offline media has no levels; its joins are still checked against the transcript.
                // Levels come from the stream the words were heard in.
                if !paths.contains_key(a) {
                    if let Ok(path) = asset_path(&g.doc, a) {
                        paths.insert(a.clone(), (path, p.transcripts.get(a).map(|t| t.stream).unwrap_or(0)));
                    }
                }
            }
        }
        Ok((id, list, paths, transcripts))
    })?;
    // Levels are read outside the project lock (the first read of a file decodes it).
    let mut waves: HashMap<Id, std::sync::Arc<Waveform>> = HashMap::new();
    for (a, (path, stream)) in &paths {
        if let Ok(w) = s.pool.waveform_stream(path, *stream) {
            waves.insert(a.clone(), w);
        }
    }
    let none: Vec<Word> = Vec::new();
    let punctuated: HashMap<Id, bool> = transcripts.iter().map(|(a, w)| (a.clone(), is_punctuated(w))).collect();
    let (mut clean, mut flagged) = (0usize, 0usize);
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    let mut out: Vec<J> = Vec::new();
    for j in &list {
        let mut issues: Vec<J> = Vec::new();
        let mut note = |kind: &'static str, msg: String| {
            *counts.entry(kind).or_default() += 1;
            issues.push(json!({"kind": kind, "message": msg}));
        };
        let level = |side: &Side| -> Option<(f64, LevelClass, f64)> {
            let w = waves.get(&side.asset).filter(|w| !w.is_empty())?;
            let floor = room_floor(w, side.src - 1.0, side.src + 1.0);
            let db = edge_level(w, side.src);
            Some((db, classify(db, floor), floor))
        };
        let lo = j.out.as_ref().and_then(&level);
        let li = j.inn.as_ref().and_then(&level);
        let words_of = |s: &Side| transcripts.get(&s.asset).map(|v| v.as_slice()).unwrap_or(&none);
        let (last, out_through) = match &j.out {
            Some(o) => words_at_out(words_of(o), o.src, o.far),
            None => (None, None),
        };
        let (first, in_through) = match &j.inn {
            Some(i) => words_at_in(words_of(i), i.src, i.far),
            None => (None, None),
        };
        // The quiet each edge sits in, and whether the speaker is talking just across it.
        // (pause at the edge, talking just across it, quiet already held before it)
        let measure = |s: &Side, out: bool| -> (Option<f64>, Option<bool>, Option<f64>) {
            let Some(w) = waves.get(&s.asset).filter(|w| !w.is_empty()) else { return (None, None, None) };
            let floor = room_floor(w, s.src - 1.0, s.src + 1.0);
            let (a, b) = if out { (s.src, s.src + CARRIES_ON) } else { (s.src - CARRIES_ON, s.src) };
            (Some(quiet_run(w, s.src, floor)), Some(level_db(w, a, b) >= floor + SPEECH_ABOVE_FLOOR), Some(quiet_before(w, s.src, floor)))
        };
        let (po, to, ho) = j.out.as_ref().map(|o| measure(o, true)).unwrap_or((None, None, None));
        let (pi, ti, _) = j.inn.as_ref().map(|i| measure(i, false)).unwrap_or((None, None, None));
        let fo = j.out.as_ref().map(|o| flow(words_of(o), punctuated.get(&o.asset).copied().unwrap_or(false), o.src, o.far, true, to, ho));
        let fi = j.inn.as_ref().map(|i| flow(words_of(i), punctuated.get(&i.asset).copied().unwrap_or(false), i.src, i.far, false, ti, None));
        // Both sides mid-sentence in one recording: a cut inside a sentence (words taken out of it), which
        // is meant to run on. Its edges are judged by their sound only.
        let splice = matches!((&j.out, &j.inn, &fo, &fi), (Some(o), Some(i), Some(x), Some(y)) if o.asset == i.asset && x.mid_sentence && y.mid_sentence);
        let sides = [("end of", lo, po, out_through, j.out.as_ref()), ("start of", li, pi, in_through, j.inn.as_ref())];
        for (side, lv, pause, through, which) in sides {
            // A silent side has no edge to hear; a faded edge takes the sound away instead of cutting it.
            let Some(which) = which else { continue };
            if which.faded {
                continue;
            }
            let word = through.map(|w| w.text.trim().to_string()).unwrap_or_default();
            match lv {
                Some((db, LevelClass::Speech, floor)) => {
                    let what = through.map(|w| format!(" through \"{}\"", w.text.trim())).unwrap_or_default();
                    note("on_sound", format!("the {side} {} cuts on sound{what} ({} dB, room tone {} dB): a clipped word or breath is likely heard", which.clip, db.round(), floor.round()));
                }
                // Quieter than speech but not silent, and too brief for a pause: a dip inside running speech.
                Some((_, LevelClass::Low, _)) if !splice && pause.is_some_and(|p| p < MIN_PAUSE) => {
                    let what = if word.is_empty() { String::new() } else { format!(" (by the transcript, inside \"{word}\")") };
                    note(
                        "no_pause",
                        format!(
                            "the {side} {} is in running speech{what}: the quiet there lasts {} ms, a gap inside or between words rather than a pause, so the voice is chopped mid-flow. Move the edge to a real pause (at least {} ms of quiet), or fade it over at least {} s",
                            which.clip,
                            (pause.unwrap_or(0.0) * 1000.0).round(),
                            (MIN_PAUSE * 1000.0).round(),
                            MIN_FADE
                        ),
                    );
                }
                Some((_, LevelClass::Low, _)) if through.is_some() => {
                    note("cuts_word", format!("the {side} {} falls inside \"{word}\" by the transcript, at a low level: listen to it", which.clip));
                }
                None if through.is_some() => {
                    note("cuts_word", format!("the {side} {} falls inside \"{word}\" by the transcript (no audio levels to check)", which.clip));
                }
                _ => {}
            }
        }
        // What is said, whatever the fades: a fade softens a cut but does not finish the sentence.
        if !splice {
            if let (Some(o), Some(f)) = (&j.out, &fo) {
                if f.mid_sentence {
                    let ws = words_of(o);
                    let next = f.across.and_then(|a| ws.iter().position(|w| std::ptr::eq(w, a))).map(|k| words_from(ws, k, 6, true)).unwrap_or_default();
                    let said = last.map(|w| w.text.trim().to_string()).unwrap_or_default();
                    note(
                        "cut_off",
                        format!(
                            "{} stops its speaker mid-sentence (\"…{said}\"), and the recording goes straight on (\"{}\"): it is heard as a cut-off{}. End it where the sentence ends (trim_to_words names one), or keep the cut-off on purpose",
                            o.clip,
                            trailing(&next),
                            if o.faded { " even with the fade" } else { "" }
                        ),
                    );
                }
            }
            if let (Some(i), Some(f)) = (&j.inn, &fi) {
                if f.mid_sentence {
                    let ws = words_of(i);
                    let prev = f.across.and_then(|a| ws.iter().position(|w| std::ptr::eq(w, a))).map(|k| words_from(ws, k, 5, false)).unwrap_or_default();
                    let said = first.map(|w| w.text.trim().to_string()).unwrap_or_default();
                    note(
                        "starts_mid_sentence",
                        format!(
                            "{} picks its speaker up mid-sentence (the recording was saying \"…{prev}\" just before \"{said}…\"): the line may start abruptly or not make sense alone. Start it where a sentence starts, or check it by ear",
                            i.clip
                        ),
                    );
                }
            }
        }
        if let (Some(a), Some(b)) = (last, first) {
            let (na, nb) = (norm_word(&a.text), norm_word(&b.text));
            // "the | the" is a stutter; "…appreciate it. | It just…" is two sentences.
            if !na.is_empty() && na == nb && !ends_sentence(&a.text) {
                note("repeated_word", format!("plays \"{}\" twice across the join (\"{} | {}\")", a.text.trim(), a.text.trim(), b.text.trim()));
            }
        }
        let pause = match (last, first, &j.out, &j.inn) {
            (Some(a), Some(b), Some(o), Some(i)) => Some((o.src - a.end).max(0.0) + (b.start - i.src).max(0.0)),
            _ => None,
        };
        if issues.is_empty() {
            clean += 1;
        } else {
            flagged += 1;
        }
        if issues_only && issues.is_empty() {
            continue;
        }
        let mut e = json!({"t": t3(j.t), "track": j.track});
        e["out"] = j.out.as_ref().map(|o| side_j(o, lo, po)).unwrap_or(json!("silent"));
        e["in"] = j.inn.as_ref().map(|i| side_j(i, li, pi)).unwrap_or(json!("silent"));
        let heard_out = last.map(|w| format!("…{}", w.text.trim())).unwrap_or_else(|| if j.out.is_none() { "(silence)".into() } else { "…".into() });
        let heard_in = first.map(|w| format!("{}…", w.text.trim())).unwrap_or_else(|| if j.inn.is_none() { "(silence)".into() } else { "…".into() });
        if last.is_some() || first.is_some() {
            e["heard"] = json!(format!("{heard_out} | {heard_in}"));
        }
        if let Some(pz) = pause {
            e["pause"] = json!(t3(pz));
        }
        if !issues.is_empty() {
            e["issues"] = J::Array(issues);
        }
        out.push(e);
    }
    let mut res = json!({"project": doc, "joins": list.len(), "clean": clean, "flagged": flagged, "list": out});
    if !counts.is_empty() {
        res["by_kind"] = json!(counts);
        res["next"] = json!("fix a cut by trimming the clip edge onto a real pause (trim_to_words picks one and names the nearest sentence end), restoring a cut word, or re-cutting with delete_words (its edges snap into silence); a short volume ramp is not a fix (it still chops the word). Then audit again");
    }
    if list.is_empty() {
        res["note"] = json!("no cuts between clips with sound in this range");
    }
    Ok(res)
}

/// `export_otio`: the cut as an OpenTimelineIO file for DaVinci Resolve, Premiere and other editors.
pub(crate) fn export_otio(s: &Arc<Services>, args: &J) -> ToolResult {
    let overwrite = arg_bool(args, "overwrite", false)?;
    let (id, out, text, rep) = with_doc(s, args, |id, g| {
        let out = match arg_str(args, "path") {
            Some(p) => abs_path(p, Some(&g.doc.project_dir())),
            None => {
                let pp = g.doc.path.clone().ok_or("this project has never been saved; pass 'path' for the .otio file")?;
                pp.with_extension("otio")
            }
        };
        let out = if out.extension().is_none() { out.with_extension("otio") } else { out };
        let doc = &g.doc;
        let (v, rep) = aiframes_core::otio::to_otio(&doc.project, doc.resolved(), &|m| doc.media_path(m));
        let srt = aiframes_core::otio::captions_srt(&doc.project, doc.resolved());
        Ok((id, out, (serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?, srt), rep))
    })?;
    let (text, srt) = text;
    let srt_path = out.with_extension("srt");
    for f in [Some(&out), srt.as_ref().map(|_| &srt_path)].into_iter().flatten() {
        if f.exists() && !overwrite {
            return Err(format!("{} already exists; pass overwrite:true to replace it", f.display()));
        }
    }
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(&out, text).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    if let Some(s) = &srt {
        std::fs::write(&srt_path, s).map_err(|e| format!("cannot write {}: {e}", srt_path.display()))?;
    }
    let mut j = json!({
        "project": id,
        "path": path_str_of(&out),
        "tracks": rep.tracks,
        "clips": rep.clips,
        "transitions": rep.transitions,
        "markers": rep.markers,
        "next": "in DaVinci Resolve: File > Import > Timeline… and pick this file (media is linked by its full path)",
    });
    if rep.placeholders > 0 {
        j["placeholders"] = json!(rep.placeholders);
    }
    if srt.is_some() {
        j["captions"] = json!(rep.captions);
        j["subtitles"] = json!(path_str_of(&srt_path));
        j["next"] = json!("in DaVinci Resolve: File > Import > Timeline… and pick the .otio (media is linked by its full path); import the .srt next to it for the captions (File > Import > Subtitle)");
    }
    if !rep.notes.is_empty() {
        j["notes"] = json!(rep.notes);
    }
    Ok(j)
}

fn path_str_of(p: &std::path::Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// `trim_to_words`: makes a clip play exactly words `from..=to` of its media's transcript, with both edges
/// moved onto the quietest point near the word times (as `delete_words` does for cuts). The clip keeps
/// its place on the timeline; only its in-point and length change. `not_before` / `not_after` (source
/// seconds) keep the edges inside a shot when the footage cuts away right next to the words.
pub(crate) fn trim_to_words(s: &Arc<Services>, args: &J) -> ToolResult {
    let clip = req_str(args, "clip")?.to_string();
    let from = arg_usize(args, "from")?.ok_or("pass 'from' (the first word index to keep)")?;
    let to = arg_usize(args, "to")?.unwrap_or(from);
    let pad = arg_f64(args, "pad")?.unwrap_or(aiframes_core::ops::DEFAULT_WORD_PAD);
    let not_before = arg_f64(args, "not_before")?;
    let not_after = arg_f64(args, "not_after")?;
    let (path, words, media_len) = with_doc(s, args, |_, g| {
        let p = &g.doc.project;
        let c = clip_of(p, &clip)?;
        let Source::Media { asset, .. } = &c.source else {
            return Err(format!("{clip} is not a media clip; trim_to_words needs a clip whose media has a transcript"));
        };
        if c.reverse || !matches!(c.speed, aiframes_core::Param::Value(v) if (v - 1.0).abs() < 1e-9) {
            return Err(format!("{clip} does not play at normal speed; trim_to_words keeps words at their real timing"));
        }
        let t = p.transcripts.get(asset).ok_or_else(|| format!("{asset} (the media of {clip}) has no transcript; call transcribe first"))?;
        if from > to || to >= t.words.len() {
            return Err(format!("word range {from}–{to} is invalid: the transcript of {asset} has words 0–{} and from must be ≤ to", t.words.len().saturating_sub(1)));
        }
        let len = p.media.iter().find(|m| &m.id == asset).map(|m| m.info.duration).unwrap_or(f64::INFINITY);
        let path: Option<PathBuf> = if crate::actions::has_sound(p, asset) { asset_path(&g.doc, asset).ok() } else { None };
        Ok((path.map(|x| (x, t.stream)), t.words.clone(), len))
    })?;
    let wave = path.and_then(|(p, k)| s.pool.waveform_stream(&p, k).ok()).filter(|w| !w.is_empty());
    let ws = words[from].start;
    let we = words[from..=to].iter().map(|w| w.end).fold(f64::MIN, f64::max);
    let measured = wave.as_ref().and_then(|w| aiframes_media::cut_edges(w, &words, from, to, pad));
    let (mut a, mut b) = match &measured {
        Some(e) => (e.start.t, e.end.t),
        None => {
            let prev_end = if from > 0 { words[from - 1].end } else { f64::NEG_INFINITY };
            let next_start = words.get(to + 1).map(|w| w.start).unwrap_or(f64::INFINITY);
            ((ws - pad).max(prev_end + pad).min(ws).max(0.0), (we + pad).min(next_start - pad).max(we))
        }
    };
    if let Some(lo) = not_before {
        a = a.max(lo);
    }
    if let Some(hi) = not_after {
        b = b.min(hi);
    }
    b = b.min(media_len);
    if b - a < 0.05 {
        return Err(format!("words {from}–{to} leave nothing to play between {} s and {} s (check not_before/not_after)", secs(a), secs(b)));
    }
    let texts: Vec<&str> = words[from..=to].iter().map(|w| w.text.as_str()).collect();
    let text = aiframes_core::join_words(&texts);
    let op = crate::actions::op(json!({"op": "set_clip", "clip": clip, "patch": {"source": {"in_point": a}, "duration": b - a}}))?;
    let mut warnings: Vec<String> = Vec::new();
    if let Some(e) = &measured {
        for (side, edge, clamped) in [("start", &e.start, not_before.is_some_and(|lo| lo > e.start.t)), ("end", &e.end, not_after.is_some_and(|hi| hi < e.end.t))] {
            if clamped {
                warnings.push(format!("the {side} was held inside the shot (not_before/not_after), away from the quiet point the audio measurement chose; listen to it"));
            } else if edge.class == LevelClass::Speech {
                warnings.push(format!("the {side} still lands on sound ({} dB, room tone {} dB): no quiet point within reach", edge.db.round(), e.floor_db.round()));
            } else if edge.in_running_speech() {
                warnings.push(format!(
                    "the {side} is in running speech: only {} ms of quiet there (a pause is at least {} ms), so the voice is chopped mid-flow",
                    (edge.pause * 1000.0).round(),
                    (MIN_PAUSE * 1000.0).round()
                ));
            }
        }
    }
    // Whole sentences: a line that stops (or starts) while its speaker is still talking is heard as cut off,
    // however clean the cut itself is.
    let mut suggest = serde_json::Map::new();
    if is_punctuated(&words) {
        let floor = wave.as_ref().map(|w| room_floor(w, ws, we));
        let talking = |t0: f64, t1: f64| -> Option<bool> { Some(level_db(wave.as_ref()?, t0, t1) >= floor? + SPEECH_ABOVE_FLOOR) };
        if let Some(next) = words.get(to + 1).filter(|_| !ends_sentence(&words[to].text)) {
            if talking(b, b + CARRIES_ON).unwrap_or(next.start - words[to].end < CARRIES_ON) {
                let later = (to + 1..words.len()).take_while(|&k| words[k].end - we <= 12.0).find(|&k| ends_sentence(&words[k].text));
                let earlier = (from..to).rev().find(|&k| ends_sentence(&words[k].text));
                let mut m = format!("this stops the speaker mid-sentence: the recording goes straight on (\"{}\"), so it is heard as a cut-off", trailing(&words_from(&words, to + 1, 6, true)));
                if let Some(k) = later {
                    m += &format!("; the sentence ends at word {k} (\"…{}\", {} s longer)", words_from(&words, k, 5, false), secs(words[k].end - we));
                    suggest.insert("to".into(), json!({"to": k, "ends": format!("…{}", words_from(&words, k, 6, false)), "adds_s": t3(words[k].end - we)}));
                }
                if let Some(k) = earlier {
                    m += &format!("{} the last full sentence before it ends at word {k} (\"…{}\")", if later.is_some() { ", or" } else { ";" }, words_from(&words, k, 5, false));
                    suggest.insert("to_earlier".into(), json!({"to": k, "ends": format!("…{}", words_from(&words, k, 6, false))}));
                }
                warnings.push(m);
            }
        }
        if let Some(prev) = from.checked_sub(1).map(|i| &words[i]).filter(|p| !ends_sentence(&p.text)) {
            if talking(a - CARRIES_ON, a).unwrap_or(words[from].start - prev.end < CARRIES_ON) {
                let earlier = (0..from).rev().take_while(|&k| ws - words[k].start <= 12.0).find(|&k| k == 0 || ends_sentence(&words[k - 1].text));
                let later = (from + 1..=to).find(|&k| ends_sentence(&words[k - 1].text));
                let mut m = format!("this picks the speaker up mid-sentence (the recording was saying \"…{}\" just before)", words_from(&words, from - 1, 5, false));
                if let Some(k) = earlier {
                    m += &format!("; the sentence starts at word {k} (\"{}…\")", words_from(&words, k, 5, true));
                    suggest.insert("from".into(), json!({"from": k, "starts": format!("{}…", words_from(&words, k, 6, true))}));
                }
                if let Some(k) = later {
                    m += &format!("{} the next sentence starts at word {k} (\"{}…\")", if earlier.is_some() { ", or" } else { ";" }, words_from(&words, k, 5, true));
                    suggest.insert("from_later".into(), json!({"from": k, "starts": format!("{}…", words_from(&words, k, 6, true))}));
                }
                warnings.push(m);
            }
        }
    }
    commit_with(s, args, &format!("Trim {clip} to \"{}\"", short(&text, 50)), vec![op], |j| {
        j["words"] = json!([from, to]);
        j["text"] = json!(text);
        j["src"] = span_j(a, b);
        if let Some(e) = &measured {
            let ej = |x: &aiframes_media::Edge| json!({"moved_ms": x.moved_ms().round(), "level_db": x.db.round(), "class": x.class.as_str(), "pause_ms": (x.pause * 1000.0).round()});
            j["edges"] = json!({"start": ej(&e.start), "end": ej(&e.end), "room_db": e.floor_db.round()});
        } else {
            j["note"] = json!("edges from word times only (no audio levels for this media)");
        }
        if !warnings.is_empty() {
            j["warnings"] = json!(warnings);
        }
        if !suggest.is_empty() {
            j["suggest"] = J::Object(suggest);
        }
    })
}
