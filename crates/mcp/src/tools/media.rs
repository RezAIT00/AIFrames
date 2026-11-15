//! Transcripts and text-based editing, media analysis (scenes, silences, beats, loudness), export and
//! background jobs.
//!
//! Analysis of long media runs as a background job (see [`analysis`]) so a tool call never hangs for
//! minutes; short media is analysed inside the call. Every analysis applies its edits (markers, cuts)
//! to the project the call named, resolved once by the dispatcher, never to whatever is active later.

use super::project::transcribe_options;
use super::*;
use crate::actions::{self, SilenceOptions};
use crate::{JobStatus, RenderInput};
use aiframes_core::ops::IdGen;
use aiframes_core::{local_time_of_source, source_span, Fit, MediaKind, Source, TrackKind};
use aiframes_media::LoudnessMeter;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Media up to this long (seconds) is analysed inside the tool call; longer media runs as a job.
pub(crate) const SYNC_MEDIA_SECONDS: f64 = 120.0;
/// How long an analysis call waits for its job by default before returning the job id.
const DEFAULT_ANALYSIS_WAIT: f64 = 25.0;

use crate::MAX_WAIT_SECONDS;

/// `wait_seconds` (capped at [`MAX_WAIT_SECONDS`]) and whether it was capped.
fn wait_arg(args: &J, default: f64) -> Result<(f64, bool), String> {
    let asked = arg_f64(args, "wait_seconds")?.unwrap_or(default).max(0.0);
    Ok((asked.min(MAX_WAIT_SECONDS), asked > MAX_WAIT_SECONDS))
}

fn still_running_note(j: &mut J, capped: bool) {
    if j["status"] == "running" {
        let job = j["job"].clone();
        let why = if capped { format!("waited the maximum {MAX_WAIT_SECONDS:.0} s for one call; ") } else { String::new() };
        j["note"] = json!(format!("{why}the job is still running — follow it with jobs {{\"job\": {job}, \"wait_seconds\": 60}} (export_status for exports)"));
    }
}

fn wait_job(s: &Arc<Services>, args: &J, job: u64) -> Result<Option<J>, String> {
    let (w, capped) = wait_arg(args, 0.0)?;
    if w <= 0.0 {
        return Ok(None);
    }
    Ok(s.jobs.wait(job, Duration::from_secs_f64(w)).map(|j| {
        let mut j = j.to_json();
        still_running_note(&mut j, capped);
        j
    }))
}

/// Runs `work` inside the call when `seconds` of media is short (and `background` is not set), else
/// as a background job of `kind` (cancellable; `work` checks the flag before editing) and waits up to
/// `wait_seconds` (default 25): the result when it finished, else the job id to follow with `jobs`.
fn analysis(
    s: &Arc<Services>,
    args: &J,
    label: String,
    doc: u64,
    seconds: f64,
    work: impl FnOnce(Arc<AtomicBool>) -> ToolResult + Send + 'static,
) -> ToolResult {
    let background = arg_bool(args, "background", false)?;
    let (wait, capped) = wait_arg(args, if background { 0.0 } else { DEFAULT_ANALYSIS_WAIT })?;
    if !background && seconds <= SYNC_MEDIA_SECONDS {
        return work(Arc::new(AtomicBool::new(false)));
    }
    let job = s.jobs.spawn("analyze", &label, Some(doc), move |ctx| {
        ctx.progress(0.0, "analysing");
        work(ctx.cancel_flag())
    });
    let info = s.jobs.wait(job, Duration::from_secs_f64(wait)).ok_or("the analysis job vanished")?;
    match info.status {
        JobStatus::Done => {
            let mut r = info.result.unwrap_or(J::Null);
            if r.is_object() {
                r["job"] = json!(job);
            }
            Ok(r)
        }
        JobStatus::Failed => Err(info.message),
        JobStatus::Cancelled => Err(format!("job {job} was cancelled")),
        JobStatus::Running => {
            let mut j = json!({
                "job": job,
                "project": doc,
                "status": "running",
                "label": label,
                "media_seconds": t3(seconds),
                "next": format!("long media is analysed in the background: jobs {{\"job\": {job}, \"wait_seconds\": 60}} returns the result (and any edits it made)"),
            });
            if capped {
                still_running_note(&mut j, true);
            }
            Ok(j)
        }
    }
}

fn check_cancel(c: &AtomicBool) -> Result<(), String> {
    if c.load(Ordering::Relaxed) {
        Err("cancelled; nothing was changed".into())
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Transcripts
// ---------------------------------------------------------------------------------------------

pub(crate) fn transcribe(s: &Arc<Services>, args: &J) -> ToolResult {
    let (id, d) = find_doc(s, args)?;
    let (asset, file, had) = {
        let g = suite_hub::lock(&d);
        let (asset, _) = asset_arg(&g.doc.project, args)?;
        let file = g.doc.project.media.iter().find(|m| m.id == asset).map(|m| file_name(&m.path).to_string()).unwrap_or_default();
        (asset.clone(), file, g.doc.project.transcripts.contains_key(&asset))
    };
    let job = actions::start_transcribe_by(s, id, &asset, &author_of(args), transcribe_options(args)?, session_of(args))?;
    s.hub.notify(format!("{}: transcribing {file}", who(&author_of(args))));
    let mut res = json!({"job": job, "project": id, "asset": asset, "status": "running", "next": "follow it with jobs (or wait_seconds); then get_transcript / search_transcript"});
    if had {
        res["note"] = json!("this asset already had a transcript; it will be replaced");
    }
    if let Some(j) = wait_job(s, args, job)? {
        res["status"] = j["status"].clone();
        res["job_state"] = j;
    }
    Ok(res)
}

fn no_transcript(s: &Arc<Services>, doc: u64, asset: &str) -> String {
    let running = s.jobs.list().into_iter().any(|j| j.kind == "transcribe" && j.doc == Some(doc) && j.status == JobStatus::Running);
    if running {
        format!("{asset} has no transcript yet; a transcription job is running (see jobs)")
    } else {
        format!("{asset} has no transcript; call transcribe with asset \"{asset}\" first")
    }
}

fn place_j(pl: &actions::WordPlace) -> J {
    json!({"clip": pl.clip, "t": t3(pl.start)})
}

pub(crate) fn get_transcript(s: &Arc<Services>, args: &J) -> ToolResult {
    let format = arg_str(args, "format").unwrap_or("paragraphs").to_ascii_lowercase();
    if !matches!(format.as_str(), "paragraphs" | "words" | "indexed") {
        return Err(format!("'format' must be paragraphs, words or indexed (got {format:?})"));
    }
    let pause = arg_f64(args, "pause")?.unwrap_or(1.0).max(0.1);
    let (id, d) = find_doc(s, args)?;
    let g = suite_hub::lock(&d);
    let (p, r) = (&g.doc.project, g.doc.resolved());
    let (asset, clip) = asset_arg(p, args)?;
    let Some(t) = p.transcripts.get(&asset) else {
        drop(g);
        return Err(no_transcript(s, id, &asset));
    };
    let n = t.words.len();
    if n == 0 {
        return Ok(json!({"project": id, "asset": asset, "words": 0, "note": "the transcript is empty (no speech found)"}));
    }
    let places = actions::word_places_all(p, r, &asset, clip.as_deref());
    // With a clip, default to the words it plays (a piece of a longer recording after cuts).
    let (mut lo, mut hi) = (0, n - 1);
    if clip.is_some() {
        if let (Some(a), Some(b)) = (places.iter().position(|v| !v.is_empty()), places.iter().rposition(|v| !v.is_empty())) {
            (lo, hi) = (a, b);
        }
    }
    let from = arg_usize(args, "from")?.unwrap_or(lo).min(n - 1);
    let mut to = arg_usize(args, "to")?.unwrap_or(hi).min(n - 1);
    if to < from {
        return Err(format!("'to' ({to}) is before 'from' ({from})"));
    }
    let max_words = arg_int_clamped(args, "max_words", 1, 100_000)?.map(|v| v as usize).unwrap_or(if format == "paragraphs" { 5000 } else { 400 });
    let mut truncated = None;
    if to - from + 1 > max_words {
        to = from + max_words - 1;
        truncated = Some(to + 1);
    }
    let multi = clip.is_none() && p.clips.iter().filter(|c| matches!(&c.source, Source::Media { asset: a, .. } if *a == asset)).count() > 1;
    let mut res = json!({"project": id, "asset": asset, "language": t.language, "total_words": n, "from": from, "to": to});
    if let Some(c) = &clip {
        res["clip"] = json!(c);
    }
    let out_words = places[from..=to].iter().filter(|p| p.is_empty()).count();
    if out_words > 0 {
        res["not_on_timeline"] = json!(out_words);
    }
    let repeated = places[from..=to].iter().filter(|p| p.len() > 1).count();
    if repeated > 0 {
        res["played_more_than_once"] = json!(repeated);
    }
    if format == "words" {
        let words: Vec<J> = (from..=to)
            .map(|i| {
                let w = &t.words[i];
                let mut j = json!({"i": i, "w": w.text.trim(), "s": t3(w.start), "e": t3(w.end)});
                match places[i].first() {
                    Some(pl) => {
                        j["t"] = json!(t3(pl.start));
                        if multi {
                            j["c"] = json!(pl.clip);
                        }
                        if places[i].len() > 1 {
                            j["also"] = J::Array(places[i][1..].iter().map(place_j).collect());
                        }
                    }
                    None => j["out"] = json!(true),
                }
                if w.conf < 0.5 {
                    j["conf"] = json!(crate::util::r(w.conf as f64, 2));
                }
                j
            })
            .collect();
        res["words"] = J::Array(words);
    } else {
        let indexed = format == "indexed";
        let mut paras = Vec::new();
        let mut start = from;
        for i in from..=to {
            let w = &t.words[i];
            let last = i == to;
            let gap = if last { f64::MAX } else { t.words[i + 1].start - w.end };
            let sentence_end = w.text.trim_end().ends_with(['.', '?', '!']);
            let len = i + 1 - start;
            // Never break inside a word ("$2" | ".11").
            let mid_word = !last && aiframes_core::words::joins(&w.text, &t.words[i + 1].text);
            if last || ((!mid_word || len >= 120) && (gap >= pause || (sentence_end && (gap >= pause * 0.5 || len >= 40)) || len >= 90)) {
                let text = units_text(&t.words, start, i, indexed, |k| places[k].is_empty());
                let mut pj = json!({"from": start, "to": i, "src": span_j(t.words[start].start, w.end), "text": text});
                if let Some(first) = places[start..=i].iter().find(|v| !v.is_empty()) {
                    pj["t"] = json!(t3(first[0].start));
                    if first.len() > 1 {
                        pj["also"] = J::Array(first[1..].iter().map(place_j).collect());
                    }
                }
                paras.push(pj);
                start = i + 1;
            }
        }
        res["paragraphs"] = J::Array(paras);
        if out_words > 0 {
            res["legend"] = json!("~word = cut out of the timeline (not heard)");
        }
    }
    if repeated > 0 {
        res["note"] = json!("this media plays in more than one place: t is the first, also lists the others (pass clip to see one)");
    }
    if let Some(next) = truncated {
        res["next_from"] = json!(next);
    }
    Ok(res)
}

/// Tokens `from..=to` as text people read: split tokens join into one word (`"$2" ".11"` → `$2.11`,
/// `"four" "-hour"` → `four-hour`, core's `join_words`). `indexed` prefixes each word with its token
/// index (`[12]`, or `[12–13]` for a word made of two tokens); `cut(k)` marks words cut out with `~`.
fn units_text(words: &[aiframes_core::Word], from: usize, to: usize, indexed: bool, cut: impl Fn(usize) -> bool) -> String {
    let toks: Vec<&str> = words[from..=to].iter().map(|w| w.text.as_str()).collect();
    let parts: Vec<String> = aiframes_core::word_units(&toks)
        .into_iter()
        .map(|u| {
            let (a, b) = (from + u.start, from + u.end - 1);
            let text = aiframes_core::join_words(&toks[u.clone()]);
            let mark = if (a..=b).all(&cut) { "~" } else { "" };
            match (indexed, a == b) {
                (false, _) => format!("{mark}{text}"),
                (true, true) => format!("[{a}]{mark}{text}"),
                (true, false) => format!("[{a}–{b}]{mark}{text}"),
            }
        })
        .collect();
    parts.join(" ")
}

/// Tokens `from..=to` (clamped) as readable text (core's `join_words`).
fn joined(words: &[aiframes_core::Word], from: usize, to_excl: usize) -> String {
    let to_excl = to_excl.min(words.len());
    if from >= to_excl {
        return String::new();
    }
    aiframes_core::join_words(&words[from..to_excl].iter().map(|w| w.text.as_str()).collect::<Vec<_>>())
}

pub(crate) fn search_transcript(s: &Arc<Services>, args: &J) -> ToolResult {
    let query = req_str(args, "query")?;
    let q: Vec<String> = query.split_whitespace().map(norm_word).filter(|w| !w.is_empty()).collect();
    if q.is_empty() {
        return Err("'query' has no words".into());
    }
    let max = arg_int_clamped(args, "max", 1, 1000)?.unwrap_or(30) as usize;
    let ctx = arg_int_clamped(args, "context", 0, 50)?.unwrap_or(5) as usize;
    with_doc(s, args, |id, g| {
        let (p, r) = (&g.doc.project, g.doc.resolved());
        let (assets, clip): (Vec<Id>, Option<Id>) = if arg_str(args, "clip").is_some() || arg_str(args, "asset").is_some() {
            let (a, c) = asset_arg(p, args)?;
            (vec![a], c)
        } else {
            (p.transcripts.keys().cloned().collect(), None)
        };
        if assets.iter().all(|a| !p.transcripts.contains_key(a)) {
            return Err(if assets.is_empty() { "no media has a transcript yet; call transcribe first".to_string() } else { format!("{} has no transcript; call transcribe first", assets[0]) });
        }
        let mut matches = Vec::new();
        let mut total = 0;
        for a in &assets {
            let Some(t) = p.transcripts.get(a) else { continue };
            // Match readable words (a run of split tokens like "$2" ".11" is one word "$2.11"), report
            // token indices.
            let toks: Vec<&str> = t.words.iter().map(|w| w.text.as_str()).collect();
            let units = aiframes_core::word_units(&toks);
            let norm: Vec<String> = units.iter().map(|u| norm_word(&aiframes_core::join_words(&toks[u.clone()]))).collect();
            let tok_norm: Vec<String> = toks.iter().map(|t| norm_word(t)).collect();
            let mut unit_of = vec![usize::MAX; toks.len()];
            for (k, u) in units.iter().enumerate() {
                unit_of[u.clone()].fill(k);
            }
            // The last unit of a match starting at unit `ui`: whole words ("$2.11", "four-hour"), or the
            // tokens' own words ("four hour" still finds "four" "-hour"), ending on a word boundary.
            let match_at = |ui: usize| -> Option<usize> {
                if ui + q.len() <= norm.len() && (0..q.len()).all(|k| norm[ui + k] == q[k]) {
                    return Some(ui + q.len() - 1);
                }
                let mut k = 0;
                for ti in units[ui].start..toks.len() {
                    if tok_norm[ti].is_empty() {
                        continue;
                    }
                    if tok_norm[ti] != q[k] {
                        return None;
                    }
                    k += 1;
                    if k == q.len() {
                        let uj = unit_of[ti];
                        return (uj != usize::MAX && tok_norm[ti + 1..units[uj].end].iter().all(String::is_empty)).then_some(uj);
                    }
                }
                None
            };
            let places = actions::word_places_all(p, r, a, clip.as_deref());
            let mut ui = 0;
            while ui < norm.len() {
                let Some(uj) = match_at(ui) else {
                    ui += 1;
                    continue;
                };
                total += 1;
                let (i, j) = (units[ui].start, units[uj].end - 1);
                if matches.len() < max {
                    let text = joined(&t.words, i, j + 1);
                    let before = joined(&t.words, units[ui.saturating_sub(ctx)].start, i);
                    let after = joined(&t.words, j + 1, units.get((uj + ctx).min(units.len() - 1)).map(|u| u.end).unwrap_or(j + 1));
                    let mut m = json!({
                        "asset": a,
                        "from": i,
                        "to": j,
                        "text": text,
                        "context": format!("{before} [{text}] {after}").trim().to_string(),
                        "src": span_j(t.words[i].start, t.words[j].end),
                    });
                    // Every place the whole phrase plays (start and end word in the same clip).
                    let whole: Vec<J> = places[i]
                        .iter()
                        .filter_map(|a0| places[j].iter().find(|b0| b0.clip == a0.clip && b0.start >= a0.start).map(|b0| json!({"clip": a0.clip, "t": span_j(a0.start, b0.end)})))
                        .collect();
                    if let Some(first) = whole.first() {
                        m["t"] = first["t"].clone();
                        m["clip"] = first["clip"].clone();
                        if whole.len() > 1 {
                            m["places"] = J::Array(whole);
                        }
                    } else if let Some(a0) = places[i].first().or_else(|| places[j].first()) {
                        m["t"] = json!(t3(a0.start));
                        m["clip"] = json!(a0.clip);
                        m["note"] = json!("partly cut out or spans two clips");
                    } else {
                        m["out"] = json!(true);
                    }
                    matches.push(m);
                }
                ui = uj + 1;
            }
        }
        let mut res = json!({"project": id, "query": query, "total": total, "matches": matches});
        if total > max {
            res["more"] = json!(total - max);
        }
        if total == 0 {
            res["note"] = json!("no match (search ignores case and punctuation; try fewer words)");
        }
        Ok(res)
    })
}

/// Stores `t` as the transcript of `asset` (one undoable step, plan-able). Word anchors that would
/// break are refused unless `force`.
fn store_transcript(s: &Arc<Services>, args: &J, asset: &str, t: aiframes_core::Transcript, label: &str, extra: J) -> ToolResult {
    let force = arg_bool(args, "force", false)?;
    let op = actions::op(json!({"op": "set_transcript", "asset": asset, "transcript": t, "force": force}))?;
    commit_with(s, args, label, vec![op], |j| merge(j, extra)).map_err(|e| {
        if e.contains("word anchors") {
            format!("{e} (pass force: true to store it anyway; validate then lists the broken anchors)")
        } else {
            e
        }
    })
}

/// A file argument: absolute, else relative to the project folder, else to the media file's folder.
fn resolve_file(g: &crate::ProjectDoc, asset: &str, p: &str) -> PathBuf {
    let first = abs_path(p, Some(&g.doc.project_dir()));
    if first.exists() || std::path::Path::new(p).is_absolute() {
        return first;
    }
    g.doc
        .project
        .media
        .iter()
        .find(|m| m.id == asset)
        .and_then(|m| g.doc.media_path(m).parent().map(|d| abs_path(p, Some(d))))
        .filter(|x| x.exists())
        .unwrap_or(first)
}

pub(crate) fn import_transcript(s: &Arc<Services>, args: &J) -> ToolResult {
    let (asset, path) = with_doc(s, args, |_, g| {
        let (asset, _) = asset_arg(&g.doc.project, args)?;
        let m = g.doc.project.media.iter().find(|m| m.id == asset).ok_or_else(|| format!("unknown media asset '{asset}'"))?;
        let path = match arg_str(args, "path") {
            Some(p) => resolve_file(g, &asset, p),
            None => {
                let media = g.doc.media_path(m);
                crate::transcripts::find_sidecar(&media)
                    .map(|(p, _)| p)
                    .ok_or_else(|| format!("no 'path' given and no sidecar next to {} (<name>.words.json, <name>.json, <name>.srt or <name>.vtt)", media.display()))?
            }
        };
        Ok((asset, path))
    })?;
    let imp = crate::transcripts::read_transcript(&path)?;
    let words = imp.transcript.words.len();
    let file = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let label = format!("Import transcript {file}");
    let extra = json!({"asset": asset, "file": path.to_string_lossy(), "format": imp.format, "words": words, "first_words": aiframes_core::join_words(&imp.transcript.words.iter().take(12).map(|w| w.text.as_str()).collect::<Vec<_>>())});
    store_transcript(s, args, &asset, imp.transcript, &label, extra)
}

pub(crate) fn align_transcript(s: &Arc<Services>, args: &J) -> ToolResult {
    let (asset, mut t, script) = with_doc(s, args, |_, g| {
        let (asset, _) = asset_arg(&g.doc.project, args)?;
        let t = g.doc.project.transcripts.get(&asset).cloned().ok_or_else(|| format!("{asset} has no transcript to align yet; transcribe it (or import_transcript) first"))?;
        let script = match (arg_str(args, "text"), arg_str(args, "path")) {
            (Some(t), _) => t.to_string(),
            (None, Some(p)) => {
                let f = resolve_file(g, &asset, p);
                std::fs::read_to_string(&f).map_err(|e| format!("cannot read {}: {e}", f.display()))?
            }
            (None, None) => return Err("pass 'text' (the script the voice reads) or 'path' (a text file with it)".into()),
        };
        Ok((asset, t, script))
    })?;
    let n = t.words.len();
    if n == 0 {
        return Err(format!("the transcript of {asset} has no words"));
    }
    let from = arg_usize(args, "from")?.unwrap_or(0);
    let to = arg_usize(args, "to")?.unwrap_or(n - 1);
    if from > to || to >= n {
        return Err(format!("word range {from}–{to} is invalid: the transcript of {asset} has words 0–{}", n - 1));
    }
    let spelling = match arg_str(args, "spelling").unwrap_or("written").to_ascii_lowercase().as_str() {
        "written" => crate::transcripts::Spelling::Written,
        "script" => crate::transcripts::Spelling::Script,
        other => return Err(format!("'spelling' must be \"written\" or \"script\" (got {other:?})")),
    };
    let a = crate::transcripts::align_with(&t.words[from..=to], &script, spelling)?;
    let kept: Vec<J> = a
        .kept_written
        .iter()
        .take(60)
        .map(|k| {
            let mut j = crate::transcripts::kept_json(k);
            j["from"] = json!(k.from + from);
            j["to"] = json!(k.to + from);
            j
        })
        .collect();
    let low: Vec<J> = a
        .low_confidence
        .iter()
        .take(40)
        .map(|r| {
            let mut j = crate::transcripts::region_json(r);
            j["from"] = json!(r.from + from);
            j["to"] = json!(r.to + from);
            j
        })
        .collect();
    let new_len = a.words.len();
    let extra = json!({
        "asset": asset,
        "words_before": to - from + 1,
        "words_after": new_len,
        "matched": a.matched,
        "substituted": a.substituted,
        "inserted": a.inserted,
        "dropped": a.dropped,
        "low_confidence": low,
        "kept_written": kept,
        "spelling": if spelling == crate::transcripts::Spelling::Written { "written" } else { "script" },
        "range": [from, from + new_len.saturating_sub(1)],
        "note": "timings come from the recognition; words, case and punctuation from the script, except written forms kept where the script only spells them out (kept_written; spelling:\"script\" uses the script verbatim). Check the low_confidence regions (script words heard differently or not at all).",
    });
    t.words.splice(from..=to, a.words);
    store_transcript(s, args, &asset, t, &format!("Align the transcript of {asset} to the script"), extra)
}

/// `[from, to]` word index ranges from `ranges` and/or `from`/`to` (whole numbers; `3.0` is fine).
fn word_ranges(args: &J) -> Result<Vec<(usize, usize)>, String> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    if let Some(v) = args.get("ranges").filter(|v| !v.is_null()) {
        let arr = v.as_array().ok_or("'ranges' must be [[from, to], ...] word indices")?;
        for x in arr {
            let items: Vec<&J> = match x {
                J::Array(a) => a.iter().collect(),
                other => vec![other],
            };
            let bad = || format!("bad range {x}: use [from, to] with whole-number word indices and to ≥ from");
            let a = items.first().and_then(|v| int_of(v)).ok_or_else(bad)?;
            let b = match items.get(1) {
                Some(v) => int_of(v).ok_or_else(bad)?,
                None => a,
            };
            if items.len() > 2 || b < a {
                return Err(bad());
            }
            ranges.push((a as usize, b as usize));
        }
    }
    if let Some(f) = arg_usize(args, "from")? {
        let t = arg_usize(args, "to")?.unwrap_or(f);
        if t < f {
            return Err(format!("'to' ({t}) is before 'from' ({f})"));
        }
        ranges.push((f, t));
    }
    if ranges.is_empty() {
        return Err("pass 'from' and 'to' (word indices from get_transcript/search_transcript) or 'ranges'".into());
    }
    ranges.sort_by_key(|r| std::cmp::Reverse(r.0));
    Ok(ranges)
}

/// `{"t", "planned", "moved_ms", "level_db", "class", "pause_ms"}` of a measured cut edge.
fn edge_j(e: &aiframes_media::Edge) -> J {
    json!({"t": t3(e.t), "planned": t3(e.planned), "moved_ms": e.moved_ms().round(), "level_db": e.db.round(), "class": e.class.as_str(), "pause_ms": (e.pause * 1000.0).round()})
}

pub(crate) fn delete_words(s: &Arc<Services>, args: &J) -> ToolResult {
    let ranges = word_ranges(args)?;
    let pad = arg_f64(args, "pad")?;
    let snap = arg_bool(args, "snap", true)?;
    // The recording's levels, read outside the project lock (the first read of a file decodes it).
    // Offline media (or no sound) falls back to word times: measuring never blocks a cut.
    let wave = if snap {
        let path = with_doc(s, args, |_, g| {
            let (asset, _) = asset_arg(&g.doc.project, args)?;
            Ok(actions::has_sound(&g.doc.project, &asset).then(|| asset_path(&g.doc, &asset).ok()).flatten())
        })
        .ok()
        .flatten();
        path.and_then(|p| s.pool.waveform(&p).ok()).filter(|w| !w.is_empty())
    } else {
        None
    };
    let mut measured: Vec<J> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let (ops, text) = with_doc(s, args, |_, g| {
        let (p, r) = (&g.doc.project, g.doc.resolved());
        let (asset, clip) = asset_arg(p, args)?;
        let t = p.transcripts.get(&asset).ok_or_else(|| format!("{asset} has no transcript; call transcribe first"))?;
        let places = actions::word_places_all(p, r, &asset, None);
        let mut ops = Vec::new();
        let mut text = Vec::new();
        for (f, to) in &ranges {
            if *to >= t.words.len() {
                return Err(format!("word {to} does not exist: the transcript of {asset} has words 0–{}", t.words.len().saturating_sub(1)));
            }
            let c = match &clip {
                // A picture and its detached sound are cut together through their leader.
                Some(c) => actions::link_leader(p, c),
                None => {
                    if let Some(i) = (*f..=*to).find(|i| places[*i].len() > 1) {
                        let where_: Vec<String> = places[i].iter().map(|pl| format!("{} at {} s", pl.clip, secs(pl.start))).collect();
                        return Err(format!("words {f}–{to} play in more than one place ({}); pass 'clip' to say which to cut", where_.join(", ")));
                    }
                    let first = (*f..=*to).find_map(|i| places[i].first()).ok_or_else(|| format!("words {f}–{to} are not on the timeline (already cut out?)"))?;
                    actions::link_leader(p, &first.clip)
                }
            };
            let mut o = json!({"op": "delete_words", "clip": c, "from": f, "to": to});
            if let Some(pd) = pad {
                o["pad"] = json!(pd);
            }
            let w = joined(&t.words, *f, *to + 1);
            if let Some(e) = wave.as_ref().and_then(|wf| aiframes_media::cut_edges(wf, &t.words, *f, *to, pad.unwrap_or(aiframes_core::ops::DEFAULT_WORD_PAD))) {
                o["edges"] = json!([e.start.t, e.end.t]);
                for (side, edge) in [("start", &e.start), ("end", &e.end)] {
                    if edge.class == aiframes_media::LevelClass::Speech {
                        warnings.push(format!(
                            "the {side} of the cut for \"{}\" (source {} s) still lands on sound ({} dB, room tone {} dB): no quiet point within reach; check it with audit_joins or by ear",
                            short(&w, 40),
                            secs(edge.t),
                            edge.db.round(),
                            e.floor_db.round()
                        ));
                    } else if edge.in_running_speech() {
                        warnings.push(format!(
                            "the {side} of the cut for \"{}\" (source {} s) is in running speech: only {} ms of quiet there, no pause; it may sound chopped, so listen to it",
                            short(&w, 40),
                            secs(edge.t),
                            (edge.pause * 1000.0).round()
                        ));
                    }
                }
                measured.push(json!({"words": [f, to], "start": edge_j(&e.start), "end": edge_j(&e.end), "room_db": e.floor_db.round()}));
            }
            ops.push(actions::op(o)?);
            text.push(w);
        }
        text.reverse();
        measured.reverse();
        Ok((ops, text))
    })?;
    let label = format!("Delete words: \"{}\"", short(&text.join(" … "), 60));
    commit_with(s, args, &label, ops, |j| {
        j["deleted_text"] = json!(text);
        if !measured.is_empty() {
            j["cut_edges"] = json!(measured);
        } else if snap {
            j["note"] = json!("cut by word times only (no audio levels for this media)");
        }
        if !warnings.is_empty() {
            j["warnings"] = json!(warnings);
        }
    })
}

pub(crate) fn generate_captions(s: &Arc<Services>, args: &J) -> ToolResult {
    let ops = with_doc(s, args, |_, g| {
        let p = &g.doc.project;
        let clips: Vec<Id> = match arg_strs(args, "clips")? {
            Some(c) if !c.is_empty() => c,
            _ => {
                let with_t = |c: &&aiframes_core::Clip| matches!(&c.source, Source::Media { asset, .. } if p.transcripts.contains_key(asset));
                let audio: HashSet<&str> = p.tracks.iter().filter(|t| t.kind == TrackKind::Audio).map(|t| t.id.as_str()).collect();
                let mut v: Vec<Id> = p.clips.iter().filter(with_t).filter(|c| audio.contains(c.track.as_str())).map(|c| c.id.clone()).collect();
                if v.is_empty() {
                    v = p.clips.iter().filter(with_t).map(|c| c.id.clone()).collect();
                }
                if v.is_empty() {
                    return Err("no clip has a transcript; transcribe the voice first (transcribe), or pass 'clips'".into());
                }
                v
            }
        };
        let mut ops = Vec::new();
        let track = match arg_str(args, "track") {
            Some(t) => t.to_string(),
            None => match p.tracks.iter().find(|t| t.kind == TrackKind::Video && (t.role == "captions" || t.name.eq_ignore_ascii_case("captions"))) {
                Some(t) => t.id.clone(),
                None => {
                    let id = IdGen::new(p).fresh("v");
                    ops.push(actions::op(json!({"op": "add_track", "id": id, "kind": "video", "role": "captions", "name": "Captions"}))?);
                    id
                }
            },
        };
        let mut o = json!({"op": "generate_captions", "clips": clips, "track": track});
        for k in ["style", "max_words", "max_chars", "pause", "replace"] {
            if let Some(v) = args.get(k).filter(|v| !v.is_null()) {
                o[k] = v.clone();
            }
        }
        ops.push(actions::op(o)?);
        Ok(ops)
    })?;
    commit(s, args, "Generate captions", ops)
}

// ---------------------------------------------------------------------------------------------
// Analysis of source media
// ---------------------------------------------------------------------------------------------

/// The media a tool analyses: the pinned project, the asset (and clip), its file, name and length.
struct Located {
    doc: u64,
    asset: Id,
    clip: Option<Id>,
    path: PathBuf,
    file: String,
    seconds: f64,
    has_sound: bool,
    has_video: bool,
}

fn locate(s: &Arc<Services>, args: &J) -> Result<Located, String> {
    with_doc(s, args, |id, g| {
        let (asset, clip) = asset_arg(&g.doc.project, args)?;
        let path = asset_path(&g.doc, &asset)?;
        let m = g.doc.project.media.iter().find(|m| m.id == asset).ok_or_else(|| format!("unknown media asset '{asset}'"))?;
        Ok(Located {
            doc: id,
            file: file_name(&m.path).to_string(),
            seconds: m.info.duration,
            has_sound: actions::has_sound(&g.doc.project, &asset),
            has_video: m.kind == MediaKind::Video && m.info.has_video,
            asset,
            clip,
            path,
        })
    })
}

/// `{"t", "clip"}` of the first place and `places` when there are more.
fn places_j(v: &[(Id, f64)], src: f64) -> J {
    let mut j = json!({"src": t3(src)});
    if let Some((c, t)) = v.first() {
        j["t"] = json!(t3(*t));
        j["clip"] = json!(c);
    }
    if v.len() > 1 {
        j["places"] = J::Array(v.iter().map(|(c, t)| json!({"clip": c, "t": t3(*t)})).collect());
    }
    j
}

/// Merges the fields of `extra` into the receipt `j`.
fn merge(j: &mut J, extra: J) {
    if let J::Object(m) = extra {
        for (k, v) in m {
            j[k] = v;
        }
    }
}

pub(crate) fn detect_scenes(s: &Arc<Services>, args: &J) -> ToolResult {
    let th = arg_f64(args, "threshold")?.unwrap_or(0.3);
    let markers = arg_bool(args, "markers", false)?;
    let split = arg_bool(args, "split", false)?;
    let loc = locate(s, args)?;
    if split && loc.clip.is_none() {
        return Err("split needs 'clip' (the clip to cut at each scene change)".into());
    }
    if !loc.has_video {
        return Err(format!("{} ({}) has no video to detect scene changes in", loc.asset, loc.file));
    }
    let (s2, a2) = (s.clone(), args.clone());
    let label = format!("Detect scenes in {}", loc.file);
    analysis(s, args, label, loc.doc, loc.seconds, move |cancel| {
        let cuts = aiframes_media::detect_scenes(&loc.path, th).map_err(|e| e.to_string())?;
        check_cancel(&cancel)?;
        let (list, ops) = with_doc(&s2, &a2, |_, g| {
            let (p, r) = (&g.doc.project, g.doc.resolved());
            let fd = frame_dur(p);
            let placed = actions::source_places(p, r, &loc.asset, loc.clip.as_deref(), &cuts, fd * 1.5);
            let list: Vec<J> = cuts.iter().zip(&placed).map(|(c, pl)| places_j(pl, *c)).collect();
            let mut ops = Vec::new();
            let mut times: Vec<(Id, f64)> = placed.iter().flatten().cloned().collect();
            times.sort_by(|a, b| a.1.total_cmp(&b.1));
            times.dedup_by(|a, b| (a.1 - b.1).abs() < 1e-6);
            if markers {
                for (k, (_, t)) in times.iter().enumerate() {
                    ops.push(actions::op(json!({"op": "add_marker", "time": t3(*t), "name": format!("Scene {}", k + 2), "kind": "marker"}))?);
                }
            }
            if split {
                // Only through the given clip (source_places was limited to it); last cut first.
                for (c, t) in times.iter().rev() {
                    ops.push(actions::op(json!({"op": "split_clip", "clip": c, "at": t3(*t)}))?);
                }
            }
            Ok((list, ops))
        })?;
        let mut base = json!({"asset": loc.asset, "threshold": th, "scene_changes": list.len(), "cuts": list});
        if ops.is_empty() {
            base["project"] = json!(loc.doc);
            if !list.is_empty() && !markers && !split {
                base["next"] = json!("markers:true adds a marker at each change; split:true (with clip) cuts the clip there");
            }
            return Ok(base);
        }
        let label = if split { format!("Split {} at {} scene changes", loc.clip.clone().unwrap_or_default(), ops.len()) } else { format!("Add {} scene markers", ops.len()) };
        commit_with(&s2, &a2, &label, ops, |j| merge(j, base))
    })
}

pub(crate) fn detect_silence(s: &Arc<Services>, args: &J) -> ToolResult {
    let noise = arg_f64(args, "noise_db")?.unwrap_or(-35.0);
    let min = arg_f64(args, "min_length")?.unwrap_or(0.5).max(0.05);
    let loc = locate(s, args)?;
    if !loc.has_sound {
        return Err(format!("{} ({}) has no sound: there is nothing to detect silence in (it is a picture)", loc.asset, loc.file));
    }
    let (s2, a2) = (s.clone(), args.clone());
    let label = format!("Detect silence in {}", loc.file);
    analysis(s, args, label, loc.doc, loc.seconds, move |_| {
        let sil = aiframes_media::detect_silence(&loc.path, noise, min).map_err(|e| e.to_string())?;
        with_doc(&s2, &a2, |id, g| {
            let (p, r) = (&g.doc.project, g.doc.resolved());
            let clip = loc.clip.as_ref().and_then(|c| p.clips.iter().find(|x| &x.id == c));
            let range = clip.and_then(source_span);
            let mut total = 0.0;
            let list: Vec<J> = sil
                .iter()
                .filter_map(|s2| {
                    let (a, b) = match range {
                        Some((lo, hi)) => (s2[0].max(lo), s2[1].min(hi)),
                        None => (s2[0], s2[1]),
                    };
                    if b - a < min {
                        return None;
                    }
                    total += b - a;
                    let mut j = json!({"src": span_j(a, b), "duration": t3(b - a)});
                    if let (Some(c), Some(sp)) = (clip, clip.and_then(|c| r.span(&c.id))) {
                        if let (Some(la), Some(lb)) = (local_time_of_source(c, a), local_time_of_source(c, b)) {
                            j["t"] = span_j(sp.start + la.min(lb), sp.start + la.max(lb));
                        }
                    }
                    Some(j)
                })
                .collect();
            let mut res = json!({"project": id, "asset": loc.asset, "noise_db": noise, "min_length": min, "count": list.len(), "total_seconds": t3(total), "silences": list});
            if let Some(c) = &loc.clip {
                res["clip"] = json!(c);
                res["next"] = json!("remove_silences cuts them out of the clip (try plan:true first)");
            }
            Ok(res)
        })
    })
}

pub(crate) fn remove_silences(s: &Arc<Services>, args: &J) -> ToolResult {
    let o = SilenceOptions {
        noise_db: arg_f64(args, "noise_db")?.unwrap_or(-35.0),
        min_len: arg_f64(args, "min_length")?.unwrap_or(0.6).max(0.1),
        padding: arg_f64(args, "padding")?.unwrap_or(0.12).clamp(0.0, 2.0),
    };
    // Which clips (each once, in order) and which files to analyse: only media with sound.
    let (doc, clips, files, seconds) = with_doc(s, args, |id, g| {
        let p = &g.doc.project;
        let mut clips: Vec<Id> = Vec::new();
        let mut push = |c: &str| {
            if !clips.iter().any(|x| x == c) {
                clips.push(c.to_string());
            }
        };
        for c in arg_strs(args, "clips")?.unwrap_or_default() {
            push(&c);
        }
        if let Some(c) = arg_str(args, "clip") {
            push(c);
        }
        if let Some(t) = arg_str(args, "track") {
            if !p.tracks.iter().any(|x| x.id == t) {
                return Err(format!("unknown track '{t}'"));
            }
            for c in p.clips.iter().filter(|c| c.track == t && matches!(c.source, Source::Media { .. })) {
                push(&c.id);
            }
        }
        if clips.is_empty() {
            return Err("pass 'clip', 'clips' or 'track' (the media clips to remove silences from)".into());
        }
        let mut files: HashMap<Id, PathBuf> = HashMap::new();
        let mut seconds = 0.0;
        for c in &clips {
            if let Source::Media { asset, .. } = &clip_of(p, c)?.source {
                if actions::has_sound(p, asset) && !files.contains_key(asset) {
                    files.insert(asset.clone(), asset_path(&g.doc, asset)?);
                    seconds += p.media.iter().find(|m| &m.id == asset).map(|m| m.info.duration).unwrap_or(0.0);
                }
            }
        }
        Ok((id, clips, files, seconds))
    })?;
    let (s2, a2) = (s.clone(), args.clone());
    let label = format!("Remove silences from {} clip(s)", clips.len());
    analysis(s, args, label, doc, seconds, move |cancel| {
        let mut silences: HashMap<Id, Vec<[f64; 2]>> = HashMap::new();
        for (asset, path) in &files {
            silences.insert(asset.clone(), aiframes_media::detect_silence(path, o.noise_db, o.min_len).map_err(|e| e.to_string())?);
            check_cancel(&cancel)?;
        }
        let (ops, cuts, notes) = with_doc(&s2, &a2, |_, g| actions::silence_cut_ops(&g.doc.project, g.doc.resolved(), &clips, &silences, &o))?;
        let cut_j: Vec<J> = cuts.iter().map(|c| json!({"clip": c.clip, "src": span_j(c.source[0], c.source[1]), "t": span_j(c.timeline[0], c.timeline[1])})).collect();
        let total: f64 = cuts.iter().map(|c| c.timeline[1] - c.timeline[0]).sum();
        if ops.is_empty() {
            let mut res = json!({"project": doc, "cuts": [], "note": format!("no silence of at least {} s below {} dB inside the clips", num(o.min_len), num(o.noise_db))});
            if !notes.is_empty() {
                res["notes"] = json!(notes);
            }
            return Ok(res);
        }
        let label = format!("Remove {} silence(s) ({} s)", cuts.len(), secs(total));
        commit_with(&s2, &a2, &label, ops, |j| {
            j["cuts"] = J::Array(cut_j);
            j["removed_seconds"] = json!(t3(total));
            j["settings"] = json!({"noise_db": o.noise_db, "min_length": o.min_len, "padding": o.padding});
            if !notes.is_empty() {
                j["notes"] = json!(notes);
            }
        })
    })
}

pub(crate) fn detect_beats(s: &Arc<Services>, args: &J) -> ToolResult {
    let markers = arg_bool(args, "markers", false)?;
    let every = arg_int_clamped(args, "every", 1, 1000)?.unwrap_or(1) as usize;
    let from = arg_f64(args, "from")?;
    let to = arg_f64(args, "to")?;
    let loc = locate(s, args)?;
    if !loc.has_sound {
        return Err(format!("{} ({}) has no sound to detect beats in", loc.asset, loc.file));
    }
    let (s2, a2) = (s.clone(), args.clone());
    let label = format!("Detect beats in {}", loc.file);
    analysis(s, args, label, loc.doc, loc.seconds, move |cancel| {
        let b = aiframes_media::analyze_beats(&loc.path).map_err(|e| e.to_string())?;
        check_cancel(&cancel)?;
        let (placed, unplaced) = with_doc(&s2, &a2, |_, g| {
            let pl = actions::source_places(&g.doc.project, g.doc.resolved(), &loc.asset, loc.clip.as_deref(), &b.beats, 0.0);
            let mut times: Vec<f64> = pl.iter().flatten().map(|(_, t)| *t).filter(|t| from.is_none_or(|f| *t >= f) && to.is_none_or(|x| *t <= x)).collect();
            times.sort_by(f64::total_cmp);
            times.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
            Ok((times, pl.iter().filter(|x| x.is_empty()).count()))
        })?;
        let mut base = json!({"asset": loc.asset, "bpm": r(b.bpm, 1), "beats": b.beats.len(), "onsets": b.onsets.len()});
        if placed.is_empty() {
            base["src_times"] = json!(b.beats.iter().take(200).map(|t| t3(*t)).collect::<Vec<_>>());
            base["note"] = json!("the music is not on the timeline (or outside the range): times are source seconds");
        } else {
            base["times"] = json!(placed.iter().step_by(every).take(400).map(|t| t3(*t)).collect::<Vec<_>>());
            if unplaced > 0 {
                base["not_on_timeline"] = json!(unplaced);
            }
        }
        if !markers || placed.is_empty() {
            base["project"] = json!(loc.doc);
            return Ok(base);
        }
        let ops = actions::beat_marker_ops(&placed, every)?;
        let label = format!("Add {} beat markers", ops.len());
        commit_with(&s2, &a2, &label, ops, |j| merge(j, base))
    })
}

// ---------------------------------------------------------------------------------------------
// Loudness of the mix
// ---------------------------------------------------------------------------------------------

/// Loudness and level statistics of a rendered mix.
struct MixStats {
    loudness: aiframes_media::Loudness,
    sample_peak_db: f64,
    /// Short-term (3 s) loudness every `window` seconds.
    series: Vec<f64>,
    silences: Vec<[f64; 2]>,
}

fn db(v: f64) -> f64 {
    if v <= 1e-9 {
        -144.0
    } else {
        20.0 * v.log10()
    }
}

struct Meter {
    rate: u32,
    from: f64,
    to: f64,
    window: f64,
    silence_db: f64,
    silence_min: f64,
}

/// Renders the block starting at `t0` into `out`.
type RenderBlock<'a> = dyn FnMut(f64, &mut [f32]) -> Result<(), String> + 'a;

/// Measures `[from, to)` rendered block by block by `render(t0, out)`.
fn mix_stats(m: &Meter, render: &mut RenderBlock) -> Result<MixStats, String> {
    let rate = m.rate;
    let mut meter = LoudnessMeter::new(rate);
    let block = (rate / 10) as usize; // 100 ms
    let sub = (rate / 100) as usize; // 10 ms silence windows
    let mut buf = vec![0f32; block * 2];
    let mut series = Vec::new();
    let mut next_mark = m.from + m.window;
    let mut peak = 0f64;
    let mut silences = Vec::new();
    let mut quiet_since: Option<f64> = None;
    let total = ((m.to - m.from) * rate as f64).round() as u64;
    let mut done: u64 = 0;
    while done < total {
        let n = ((total - done) as usize).min(block);
        let t = m.from + done as f64 / rate as f64;
        let b = &mut buf[..n * 2];
        render(t, b).map_err(|e| format!("mixing failed at {} s: {e}", secs(t)))?;
        meter.push(b);
        for (k, w) in b.chunks(sub * 2).enumerate() {
            let (mut sl, mut sr) = (0f64, 0f64);
            for f in w.as_chunks::<2>().0 {
                sl += (f[0] as f64).powi(2);
                sr += (f[1] as f64).powi(2);
                peak = peak.max(f[0].abs() as f64).max(f[1].abs() as f64);
            }
            let frames = (w.len() / 2).max(1) as f64;
            let level = db((sl.max(sr) / frames).sqrt());
            let wt = t + (k * sub) as f64 / rate as f64;
            if level < m.silence_db {
                quiet_since.get_or_insert(wt);
            } else if let Some(q) = quiet_since.take() {
                if wt - q >= m.silence_min {
                    silences.push([q, wt]);
                }
            }
        }
        done += n as u64;
        let now = m.from + done as f64 / rate as f64;
        while now + 1e-9 >= next_mark && next_mark <= m.to + 1e-9 {
            series.push(meter.short_term().max(-70.0));
            next_mark += m.window;
        }
    }
    if let Some(q) = quiet_since {
        if m.to - q >= m.silence_min {
            silences.push([q, m.to]);
        }
    }
    Ok(MixStats { loudness: meter.result(), sample_peak_db: db(peak), series, silences })
}

fn loud_j(l: &aiframes_media::Loudness) -> J {
    json!({"integrated_lufs": r(l.integrated_lufs, 1), "true_peak_db": r(l.true_peak_db, 1), "lra": r(l.lra, 1), "max_momentary_lufs": r(l.max_momentary_lufs, 1)})
}

/// Several tracks' short-term loudness combined (energy sum).
fn sum_series(list: &[&Vec<f64>]) -> Vec<f64> {
    let n = list.iter().map(|v| v.len()).min().unwrap_or(0);
    (0..n).map(|i| 10.0 * list.iter().map(|v| 10f64.powf(v[i] / 10.0)).sum::<f64>().max(1e-12).log10()).collect()
}

/// Decodes the audio of every sounding media file the range uses completely (progressive decoding
/// would otherwise mix silence where it has not arrived yet). Problems become warnings.
fn complete_audio(s: &Services, inp: &RenderInput, from: f64, to: f64) -> Vec<String> {
    let p = &inp.project;
    let rate = p.settings.sample_rate;
    let mut seen = HashSet::new();
    let mut warnings = Vec::new();
    for c in &p.clips {
        let Source::Media { asset, .. } = &c.source else { continue };
        let Some(sp) = inp.resolved.span(&c.id) else { continue };
        if sp.end <= from || sp.start >= to || !seen.insert(asset.clone()) {
            continue;
        }
        let Some(m) = p.media.iter().find(|m| &m.id == asset).filter(|m| m.info.has_audio && m.kind != MediaKind::Image) else { continue };
        let path = crate::util::abs_path(&m.path, Some(&inp.dir));
        if let Err(e) = s.pool.audio_complete(&path, rate) {
            warnings.push(format!("{} ({}): {e}", m.id, file_name(&m.path)));
        }
    }
    warnings
}

pub(crate) fn analyze_audio(s: &Arc<Services>, args: &J) -> ToolResult {
    let silence_db = arg_f64(args, "silence_db")?.unwrap_or(-50.0);
    let silence_min = arg_f64(args, "silence_min")?.unwrap_or(1.0).max(0.1);
    if arg_str(args, "asset").is_some() || (arg_str(args, "clip").is_some() && arg_str(args, "track").is_none()) {
        // A source file (streamed by ffmpeg: no cache, always complete).
        let loc = locate(s, args)?;
        if !loc.has_sound {
            return Err(format!("{} ({}) has no sound", loc.asset, loc.file));
        }
        let label = format!("Analyse the sound of {}", loc.file);
        return analysis(s, args, label, loc.doc, loc.seconds, move |_| {
            let l = aiframes_media::loudness_of_file(&loc.path).map_err(|e| e.to_string())?;
            let sil = aiframes_media::detect_silence(&loc.path, silence_db, silence_min).map_err(|e| e.to_string())?;
            Ok(json!({"project": loc.doc, "asset": loc.asset, "loudness": loud_j(&l), "silences": sil.iter().map(|x| span_j(x[0], x[1])).collect::<Vec<_>>()}))
        });
    }
    let (id, inp) = with_doc(s, args, |id, g| Ok((id, g.render_input())))?;
    let dur = inp.resolved.duration;
    if dur <= 0.0 {
        return Err("the timeline is empty".into());
    }
    let from = arg_f64(args, "from")?.unwrap_or(0.0).clamp(0.0, dur);
    let to = arg_f64(args, "to")?.unwrap_or(dur).clamp(from, dur);
    if to - from < 0.1 {
        return Err("the range is shorter than 0.1 s".into());
    }
    // At most ~2000 readings, whatever the range.
    let window = arg_f64(args, "window")?.unwrap_or(((to - from) / 60.0).clamp(1.0, 10.0)).max(0.5).max((to - from) / 2000.0);
    let p = &inp.project;
    let sounding: Vec<(Id, String)> = p
        .tracks
        .iter()
        .filter(|t| p.clips.iter().any(|c| c.track == t.id && matches!(&c.source, Source::Media { asset, .. } if actions::has_sound(p, asset))))
        .map(|t| (t.id.clone(), t.role.clone()))
        .collect();
    let target = match arg_str(args, "track") {
        Some(t) => {
            if !p.tracks.iter().any(|x| x.id == t) {
                return Err(format!("unknown track '{t}'"));
            }
            Some(t.to_string())
        }
        None => None,
    };
    let per_track = target.is_none() && arg_bool(args, "per_track", (to - from) * sounding.len() as f64 <= 3600.0)?;
    let passes = 1 + if per_track { sounding.len() } else { 0 };
    let s2 = s.clone();
    let label = format!("Analyse the mix ({}–{} s)", secs(from), secs(to));
    // Mixing runs far faster than real time: the cost estimate is media-seconds / 5.
    analysis(s, args, label, id, (to - from) * passes as f64 / 5.0, move |cancel| {
        let mut warnings = complete_audio(&s2, &inp, from, to);
        check_cancel(&cancel)?;
        let (p, res_, dir) = (&inp.project, &inp.resolved, &inp.dir);
        let meter = Meter { rate: p.settings.sample_rate.clamp(8000, 192_000), from, to, window, silence_db, silence_min };
        let m = match &target {
            Some(t) => mix_stats(&meter, &mut |t0, out| s2.renderer.mix_track(dir, p, res_, t, t0, out).map_err(|e| e.to_string()))?,
            None => mix_stats(&meter, &mut |t0, out| {
                let w = s2.renderer.mix_checked(dir, p, res_, t0, out).map_err(|e| e.to_string())?;
                for x in w {
                    if !warnings.contains(&x) && warnings.len() < 20 {
                        warnings.push(x);
                    }
                }
                Ok(())
            })?,
        };
        let mut res = json!({
            "project": id,
            "what": target.clone().map(|t| format!("track {t}")).unwrap_or_else(|| "the full mix".into()),
            "range": span_j(from, to),
            "loudness": loud_j(&m.loudness),
            "sample_peak_db": r(m.sample_peak_db, 1),
            "short_term": {"window_s": r(window, 2), "lufs": m.series.iter().map(|v| r(*v, 1)).collect::<Vec<_>>()},
            "silences": m.silences.iter().map(|x| span_j(x[0], x[1])).collect::<Vec<_>>(),
        });
        let mut advice = Vec::new();
        if m.loudness.integrated_lufs <= -69.0 {
            advice.push("the mix is silent in this range".to_string());
        } else {
            if m.loudness.true_peak_db > -1.0 {
                advice.push(format!("true peak {:.1} dBTP is near clipping; export normalisation limits it, but lower hot clips if they distort", m.loudness.true_peak_db));
            }
            advice.push(format!(
                "integrated {:.1} LUFS; export normalises to the project's loudness target (YouTube: -14 LUFS) unless loudness_lufs is null",
                m.loudness.integrated_lufs
            ));
        }
        if per_track {
            let mut tracks = Vec::new();
            let mut series: Vec<(String, Vec<f64>)> = Vec::new();
            for (tid, role) in &sounding {
                check_cancel(&cancel)?;
                let st = mix_stats(&meter, &mut |t0, out| s2.renderer.mix_track(dir, p, res_, tid, t0, out).map_err(|e| e.to_string()))?;
                tracks.push(json!({"track": tid, "role": role, "integrated_lufs": r(st.loudness.integrated_lufs, 1), "true_peak_db": r(st.loudness.true_peak_db, 1)}));
                series.push((role.clone(), st.series));
            }
            res["tracks"] = J::Array(tracks);
            let of_role = |role: &str| -> Vec<&Vec<f64>> { series.iter().filter(|(r, _)| r == role).map(|(_, s)| s).collect() };
            let (voice, music) = (of_role("voice"), of_role("music"));
            if !voice.is_empty() && !music.is_empty() {
                let (v, mu) = (sum_series(&voice), sum_series(&music));
                let mut diffs: Vec<f64> = v.iter().zip(&mu).filter(|(a, b)| **a > -45.0 && **b > -69.0).map(|(a, b)| a - b).collect();
                diffs.sort_by(f64::total_cmp);
                if let Some(med) = diffs.get(diffs.len() / 2) {
                    res["voice_over_music_lu"] = json!(r(*med, 1));
                    if *med < 10.0 {
                        advice.push(format!("while the voice speaks, music is only {med:.1} LU below it (aim for 12–20 LU): lower the music track or add ducking (auto_ducking)"));
                    } else {
                        advice.push(format!("music sits {med:.1} LU under the voice while it speaks: good separation"));
                    }
                }
            }
        }
        if !m.silences.is_empty() {
            advice.push(format!("{} silent stretch(es) of at least {} s (see silences)", m.silences.len(), num(silence_min)));
        }
        res["advice"] = json!(advice);
        if !warnings.is_empty() {
            res["warnings"] = json!(warnings);
        }
        Ok(res)
    })
}

// ---------------------------------------------------------------------------------------------
// Export and jobs
// ---------------------------------------------------------------------------------------------

/// Applies an export preset: codec/quality/loudness, and for the size presets the output frame size.
fn apply_export_preset(e: &mut aiframes_core::ExportSettings, name: &str) -> Result<(), String> {
    let key = name.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    let size: Option<(u32, u32)> = match key.as_str() {
        "1080p" | "youtube_1080p" | "hd" => Some((1920, 1080)),
        "4k" | "youtube_4k" | "uhd" | "2160p" => Some((3840, 2160)),
        "720p" => Some((1280, 720)),
        "shorts" | "youtube_shorts" | "vertical" | "reels" | "tiktok" => Some((1080, 1920)),
        "square" | "instagram" => Some((1080, 1080)),
        _ => None,
    };
    let (codec, quality, lufs): (&str, &str, Option<f64>) = match key.as_str() {
        _ if size.is_some() => ("h264", "high", Some(-14.0)),
        "youtube" | "web" | "mp4" => ("h264", "high", Some(-14.0)),
        "hevc" | "h265" => ("hevc", "high", Some(-14.0)),
        "draft" | "preview" => ("h264", "draft", Some(-14.0)),
        "master" | "prores" => ("prores", "high", None),
        "gif" => ("gif", "standard", None),
        "wav" | "audio" => ("wav", "high", Some(-14.0)),
        other => return Err(format!("unknown export preset '{other}'. Presets: {EXPORT_PRESETS}")),
    };
    e.codec = codec.into();
    e.quality = quality.into();
    e.loudness_lufs = lufs;
    if let Some((w, h)) = size {
        e.width = Some(w);
        e.height = Some(h);
    }
    Ok(())
}

pub(crate) const EXPORT_PRESETS: &str = "youtube (H.264, project size), 1080p, 4k, 720p, shorts (1080x1920), square (1080x1080), hevc, draft, prores, gif, wav";

fn fit_of(s: &str) -> Result<Fit, String> {
    serde_json::from_value(json!(s.trim().to_ascii_lowercase())).map_err(|_| format!("unknown fit '{s}' (contain, cover, stretch, none)"))
}

/// True when `out` was written by an export job of this session (re-exporting it is expected).
fn our_export(s: &Services, out: &std::path::Path) -> bool {
    let key = actions::path_key(out);
    s.jobs.list().iter().any(|j| j.kind == "export" && j.result.as_ref().and_then(|r| r["path"].as_str()).is_some_and(|p| actions::path_key(std::path::Path::new(p)) == key))
}

pub(crate) fn export(s: &Arc<Services>, args: &J) -> ToolResult {
    let (id, d) = find_doc(s, args)?;
    let overwrite = arg_bool(args, "overwrite", false)?;
    let (settings, out, errors, out_size, project_size) = {
        let g = suite_hub::lock(&d);
        let mut e = g.doc.project.export.clone();
        if let Some(pr) = arg_str(args, "preset") {
            apply_export_preset(&mut e, pr)?;
        }
        if let Some(c) = arg_str(args, "codec") {
            if !c.eq_ignore_ascii_case("wav") && aiframes_media::VideoCodec::parse(c).is_none() {
                return Err(format!("unknown codec '{c}' (h264, hevc, av1, prores, gif, png_sequence, wav)"));
            }
            e.codec = c.to_ascii_lowercase();
        }
        if let Some(q) = arg_str(args, "quality") {
            if aiframes_media::Quality::parse(q).is_none() {
                return Err(format!("unknown quality '{q}' (draft, standard, high, lossless)"));
            }
            e.quality = q.to_ascii_lowercase();
        }
        if args.get("hardware").is_some() {
            e.hardware = arg_bool(args, "hardware", true)?;
        }
        if let Some(v) = args.get("loudness_lufs") {
            e.loudness_lufs = if v.is_null() { None } else { Some(num_of(v).ok_or("'loudness_lufs' must be a number (LUFS) or null")?) };
        }
        if let Some(tp) = arg_f64(args, "true_peak_db")? {
            e.true_peak_db = tp;
        }
        if let Some(rg) = arg_range(args, "range")? {
            e.range = Some(rg);
        }
        if let Some(w) = arg_int(args, "width", 16, 16384)? {
            e.width = Some(w as u32);
            if args.get("height").is_none() && arg_str(args, "preset").is_none() {
                e.height = None;
            }
        }
        if let Some(h) = arg_int(args, "height", 16, 16384)? {
            e.height = Some(h as u32);
            if args.get("width").is_none() && arg_str(args, "preset").is_none() {
                e.width = None;
            }
        }
        if let Some(f) = arg_str(args, "fit") {
            e.fit = fit_of(f)?;
        }
        let out = match arg_str(args, "path") {
            Some(p) => {
                let mut pb = abs_path(p, Some(&g.doc.project_dir()));
                let ext = actions::export_extension(&e.codec);
                if pb.extension().is_none() && !ext.is_empty() {
                    pb.set_extension(ext);
                }
                pb
            }
            None => actions::default_export_path(&g.doc, &e).ok_or("this project has never been saved; pass 'path' for the exported file (e.g. C:\\Users\\me\\Videos\\video.mp4)")?,
        };
        let errors: Vec<String> = g.doc.validate().into_iter().filter(|f| f.severity == Severity::Error).take(5).map(|f| format!("{}: {}", f.code, f.message)).collect();
        let st = &g.doc.project.settings;
        (e.clone(), out, errors, e.output_size(st), (st.width, st.height))
    };
    // Existing files are only replaced on request (or when this session exported them).
    let replace = overwrite || our_export(s, &out);
    if let Some(why) = actions::protected_target(s, &out) {
        return Err(format!("refusing to export over {}: {why}. Choose another path.", out.display()));
    }
    if out.is_file() && !replace {
        return Err(format!("{} already exists; pass overwrite: true to replace it, or choose another path", out.display()));
    }
    let job = actions::start_export(s, id, settings.clone(), out.clone())?;
    s.hub.notify(format!("{}: exporting {}", who(&author_of(args)), out.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()));
    let mut res = json!({
        "job": job,
        "project": id,
        "path": out.to_string_lossy(),
        "codec": settings.codec,
        "quality": settings.quality,
        "loudness_lufs": settings.loudness_lufs,
        "output_size": [out_size.0, out_size.1],
        "status": "running",
        "next": "follow it with export_status (or pass wait_seconds)",
    });
    if out_size != project_size {
        res["fit"] = json!(settings.fit);
        let (pa, oa) = (project_size.0 as f64 / project_size.1.max(1) as f64, out_size.0 as f64 / out_size.1.max(1) as f64);
        if (pa - oa).abs() > 0.01 && settings.fit == Fit::Contain {
            res["note"] = json!(format!(
                "the output ({}x{}) has another aspect than the project ({}x{}): fit contain letterboxes; pass fit: \"cover\" to crop to fill, or make the edit in that size (new_project preset)",
                out_size.0, out_size.1, project_size.0, project_size.1
            ));
        }
    }
    if let Some(rg) = settings.range {
        res["range"] = span_j(rg[0], rg[1]);
    }
    if !errors.is_empty() {
        res["validation_errors"] = json!(errors);
        res["warning"] = json!("the project has validation errors; the export may not look as intended (see validate)");
    }
    if let Some(j) = wait_job(s, args, job)? {
        res["status"] = j["status"].clone();
        res["job_state"] = j;
    }
    Ok(res)
}

pub(crate) fn export_status(s: &Arc<Services>, args: &J) -> ToolResult {
    let j = match arg_usize(args, "job")? {
        Some(id) => s.jobs.get(id as u64).ok_or_else(|| format!("no job {id} (jobs lists them)"))?,
        None => s.jobs.latest("export", None).ok_or("no export has run yet in this session")?,
    };
    if let Some(j) = wait_job(s, args, j.id)? {
        return Ok(j);
    }
    Ok(j.to_json())
}

pub(crate) fn jobs(s: &Arc<Services>, args: &J) -> ToolResult {
    if let Some(id) = arg_usize(args, "job")? {
        let j = s.jobs.get(id as u64).ok_or_else(|| format!("no job {id}"))?;
        return Ok(wait_job(s, args, j.id)?.unwrap_or_else(|| j.to_json()));
    }
    let all = arg_bool(args, "all", true)?;
    let mut list: Vec<J> = s.jobs.list().iter().rev().filter(|j| all || j.status == JobStatus::Running).map(|j| j.to_json()).collect();
    list.truncate(arg_int_clamped(args, "limit", 1, 1000)?.unwrap_or(20) as usize);
    Ok(json!({"running": s.jobs.running().len(), "jobs": list}))
}

pub(crate) fn cancel_job(s: &Arc<Services>, args: &J) -> ToolResult {
    let id = arg_usize(args, "job")?.ok_or("missing 'job' (an id from jobs)")? as u64;
    let j = s.jobs.get(id).ok_or_else(|| format!("no job {id}"))?;
    if j.status != JobStatus::Running {
        return Ok(json!({"job": id, "status": j.status.name(), "note": "the job is not running"}));
    }
    s.jobs.cancel(id);
    let after = s.jobs.wait(id, Duration::from_secs(5)).map(|j| j.status.name()).unwrap_or("unknown");
    Ok(json!({"job": id, "cancel_requested": true, "status": after}))
}
