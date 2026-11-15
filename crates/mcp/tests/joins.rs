//! Cuts measured on the audio: `delete_words` snapping its edges into silence, and `audit_joins`
//! reporting how each cut sounds. Also the cut-quality eval: word times shifted the way recognisers
//! shift them, scored by where the cut edges land.

mod common;
use common::*;
use serde_json::{json, Value as J};

/// 6 s: eight "words" (a 220 Hz tone) at 0.5 + 0.6·k, each 0.45 s long, 0.15 s of silence between.
fn words_audio() -> std::path::PathBuf {
    let out = root().join("media").join("eight-words.wav");
    if !out.exists() {
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        let tmp = out.with_file_name(format!("eight-words-{}-{:?}.wav", std::process::id(), std::thread::current().id()).replace(['(', ')'], ""));
        ffmpeg(&["-f", "lavfi", "-i", "aevalsrc='0.4*sin(2*PI*220*t)*gte(t,0.5)*lt(t,5.3)*lt(mod(t-0.5,0.6),0.45)':s=48000:d=6", "-c:a", "pcm_s16le", "-f", "wav", tmp.to_str().unwrap()]);
        // Tests run in parallel: another may have made it meanwhile.
        if std::fs::rename(&tmp, &out).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    out
}

/// Real word k plays 0.5 + 0.6·k .. +0.45. The transcript is `late` seconds late on every word and runs
/// each word on until the next starts (no gaps), as recognisers often do.
fn transcript(names: &[&str], late: f64) -> J {
    let words: Vec<J> = names
        .iter()
        .enumerate()
        .map(|(k, w)| {
            let start = 0.5 + 0.6 * k as f64 + late;
            let end = if k + 1 < names.len() { start + 0.6 } else { start + 0.45 };
            json!({"text": format!(" {w}"), "start": start, "end": end})
        })
        .collect();
    json!({"language": "en", "words": words})
}

fn project(names: &[&str], late: f64) -> aiframes_mcp::AiframesBackend {
    let (_, b) = {
        let s = services();
        let b = aiframes_mcp::AiframesBackend::new(s.clone());
        (s, b)
    };
    let b = std::sync::Arc::try_unwrap(b).ok().expect("one backend");
    call(&b, "new_project", json!({"name": "joins"}));
    call(&b, "import_media", json!({"paths": [path_str(&words_audio())], "place": "none"}));
    let media = call(&b, "list_media", json!({}));
    let asset = media["media"][0]["id"].as_str().unwrap().to_string();
    call(&b, "edit", json!({"ops": [
        {"op": "add_clip", "clip": {"id": "vo", "track": "a1", "asset": asset, "start": 0}},
        {"op": "set_transcript", "asset": asset, "transcript": transcript(names, late)},
    ]}));
    b
}

const NAMES: &[&str] = &["one", "two", "three", "four", "five", "six", "seven", "eight"];

/// The cuts between two clips with sound (the audit also lists each clip edge that meets silence).
fn cuts(audit: &J) -> Vec<J> {
    audit["list"].as_array().unwrap().iter().filter(|j| j["out"].is_object() && j["in"].is_object()).cloned().collect()
}

fn kinds(audit: &J) -> Vec<String> {
    audit["list"].as_array().unwrap().iter().flat_map(|j| j["issues"].as_array().cloned().unwrap_or_default()).map(|i| i["kind"].as_str().unwrap().to_string()).collect()
}

#[test]
fn snapped_cuts_land_in_silence_and_word_time_cuts_are_flagged() {
    // Word times alone: deleting "three" cuts at the transcript's edges, 60 ms late, which ends inside
    // the real "four" (and starts inside the real "three").
    let b = project(NAMES, 0.06);
    let r = call(&b, "delete_words", json!({"clip": "vo", "from": 2, "to": 2, "snap": false}));
    assert!(r.get("cut_edges").is_none(), "{r}");
    let a = call(&b, "audit_joins", json!({}));
    assert_eq!(cuts(&a).len(), 1, "{a}");
    assert!(kinds(&a).iter().any(|k| k == "on_sound"), "the late cut is heard: {a}");
    assert_eq!(cuts(&a)[0]["heard"], "…two | four…", "{a}");

    // Snapped: both edges move into the real 0.15 s pauses and the audit is clean.
    let b = project(NAMES, 0.06);
    let r = call(&b, "delete_words", json!({"clip": "vo", "from": 2, "to": 2}));
    let e = &r["cut_edges"][0];
    assert_eq!((e["start"]["class"].as_str(), e["end"]["class"].as_str()), (Some("silence"), Some("silence")), "{r}");
    let (s, t) = (e["start"]["t"].as_f64().unwrap(), e["end"]["t"].as_f64().unwrap());
    assert!(s > 1.55 && s < 1.7, "start edge in the pause 1.55–1.7 after \"two\": {e}");
    assert!(t > 2.15 && t < 2.3, "end edge in the pause 2.15–2.3 before \"four\": {e}");
    assert!(r.get("warnings").is_none(), "{r}");
    let a = call(&b, "audit_joins", json!({}));
    assert_eq!((cuts(&a).len(), a["flagged"].as_u64()), (1, Some(0)), "{a}");
    assert_eq!(cuts(&a)[0]["out"]["class"], "silence", "{a}");
}

#[test]
fn audit_finds_repeated_words_and_respects_filters() {
    let b = project(&["so", "the", "um", "the", "plan", "is", "simple", "now"], 0.0);
    call(&b, "delete_words", json!({"clip": "vo", "from": 2, "to": 2}));
    let a = call(&b, "audit_joins", json!({}));
    assert!(kinds(&a).iter().any(|k| k == "repeated_word"), "{a}");
    assert_eq!(cuts(&a)[0]["heard"], "…the | the…", "{a}");
    assert!(cuts(&a)[0]["pause"].as_f64().unwrap() < 0.3, "{a}");
    let only = call(&b, "audit_joins", json!({"issues_only": true, "from": 100}));
    assert_eq!(only["joins"], 0, "{only}");
    let e = fails(&b, "audit_joins", json!({"track": "nope"}));
    assert!(e.contains("unknown track"), "{e}");
}

/// The cut-quality eval: for transcripts shifted early and late by up to 80 ms (typical recogniser error),
/// delete each inner word and count how many cut edges land in silence, with and without snapping.
/// Snapping must put every edge in silence; word times alone miss many.
#[test]
fn eval_edge_quality_across_word_time_errors() {
    let mut rows = Vec::new();
    let (mut plain_ok, mut snap_ok, mut total) = (0, 0, 0);
    for late in [-0.08, -0.04, 0.0, 0.04, 0.08] {
        for (snap, ok) in [(false, &mut plain_ok), (true, &mut snap_ok)] {
            let mut clean = 0;
            for k in 1..NAMES.len() - 1 {
                let b = project(NAMES, late);
                call(&b, "delete_words", json!({"clip": "vo", "from": k, "to": k, "snap": snap}));
                let a = call(&b, "audit_joins", json!({}));
                let j = &cuts(&a)[0];
                clean += [&j["out"]["class"], &j["in"]["class"]].iter().filter(|c| c.as_str() == Some("silence")).count();
                if snap {
                    total += 2;
                }
            }
            *ok += clean;
            rows.push(format!("late {:+.2} s  snap {snap:5}  edges in silence {clean}/{}", late, 2 * (NAMES.len() - 2)));
        }
    }
    println!("cut-quality eval:\n  {}", rows.join("\n  "));
    println!("  total: word times {plain_ok}/{total}, snapped {snap_ok}/{total}");
    assert_eq!(snap_ok, total, "every snapped edge lands in silence");
    assert!(plain_ok < total, "the eval must exercise cuts that word times alone get wrong");
}

#[test]
fn export_otio_writes_a_timeline_file() {
    let b = project(NAMES, 0.0);
    call(&b, "delete_words", json!({"clip": "vo", "from": 2, "to": 2}));
    call(&b, "edit", json!({"ops": [
        {"op": "add_clip", "clip": {"id": "title", "track": "v1", "text": "Eight words", "start": 0.5, "duration": 2}},
        {"op": "add_marker", "time": 1.0, "name": "Start", "kind": "chapter"},
    ]}));
    let dir = scratch("otio");
    let out = dir.join("cut.otio");
    let r = call(&b, "export_otio", json!({"path": path_str(&out)}));
    assert_eq!((r["clips"].as_u64(), r["placeholders"].as_u64(), r["markers"].as_u64()), (Some(3), Some(1), Some(1)), "{r}");
    let v: J = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(v["OTIO_SCHEMA"], "Timeline.1");
    let e = fails(&b, "export_otio", json!({"path": path_str(&out)}));
    assert!(e.contains("overwrite"), "{e}");
    call(&b, "export_otio", json!({"path": path_str(&out), "overwrite": true}));
    if let Ok(keep) = std::env::var("AIFRAMES_OTIO_SAMPLE") {
        std::fs::copy(&out, keep).unwrap();
    }
}

#[test]
fn show_panel_and_suggest_bring_the_review_tab_forward() {
    let s = services();
    let b = aiframes_mcp::AiframesBackend::new(s.clone());
    let r = call(&b, "new_project", json!({"name": "panels"}));
    let id = r["project"].as_u64().unwrap();
    let r = call(&b, "show_panel", json!({"panel": "Review"}));
    assert_eq!(r["panel"], "review", "{r}");
    let d = s.hub.get(id).unwrap();
    assert_eq!(aiframes_mcp::suite_hub::lock(&d).live.panel_request.as_deref(), Some("review"));
    let e = fails(&b, "show_panel", json!({"panel": "timeline-ish"}));
    assert!(e.contains("unknown panel") && e.contains("review"), "{e}");
    // Headless (no window): suggest records the suggestion but has nothing to show.
    aiframes_mcp::suite_hub::lock(&d).live.panel_request = None;
    let r = call(&b, "suggest", json!({"label": "Mark the start", "ops": [{"op": "add_marker", "time": 0.5}]}));
    assert!(r.get("shown").is_none(), "{r}");
    // Applying a suggestion can be planned first.
    let r = call(&b, "resolve_suggestion", json!({"suggestion": "all", "plan": true}));
    assert_eq!(r["planned"], true, "{r}");
    assert_eq!(call(&b, "list_suggestions", json!({}))["pending"], 1);
}

#[test]
fn trim_to_words_assembles_clips_with_edges_in_silence() {
    // Transcript 60 ms late with words run together: word times alone would cut into the neighbours.
    let b = project(NAMES, 0.06);
    let media = call(&b, "list_media", json!({}));
    let asset = media["media"][0]["id"].as_str().unwrap().to_string();
    call(&b, "edit", json!({"ops": [{"op": "add_clip", "clip": {"id": "vo2", "track": "a1", "asset": asset, "start": {"after": "vo"}, "in_point": 0, "duration": 2}}]}));
    let r = call(&b, "trim_to_words", json!({"clip": "vo", "from": 1, "to": 2}));
    assert_eq!(r["text"], "two three", "{r}");
    assert_eq!((r["edges"]["start"]["class"].as_str(), r["edges"]["end"]["class"].as_str()), (Some("silence"), Some("silence")), "{r}");
    let r = call(&b, "trim_to_words", json!({"clip": "vo2", "from": 5, "to": 6}));
    assert_eq!(r["text"], "six seven", "{r}");
    let a = call(&b, "audit_joins", json!({}));
    assert_eq!((cuts(&a).len(), a["flagged"].as_u64()), (1, Some(0)), "{a}");
    assert_eq!(cuts(&a)[0]["heard"], "…three | six…", "{a}");
    // The first clip's start and the last clip's end meet silence: they are judged too (cleanly here).
    assert_eq!(a["joins"], 3, "{a}");
    assert_eq!(a["list"][0]["heard"], "(silence) | two…", "{a}");
    // Shot limits hold an edge in place (and say so); bad ranges are refused.
    let r = call(&b, "trim_to_words", json!({"clip": "vo2", "from": 5, "to": 6, "not_after": 4.3}));
    assert!(r["src"][1].as_f64().unwrap() <= 4.3 + 1e-6, "{r}");
    assert!(r["warnings"].to_string().contains("inside the shot"), "{r}");
    let e = fails(&b, "trim_to_words", json!({"clip": "vo", "from": 6, "to": 2}));
    assert!(e.contains("invalid"), "{e}");
}

#[test]
fn a_speaker_stopped_or_picked_up_mid_sentence_is_flagged() {
    // A punctuated transcript: two sentences, said without long pauses (0.15 s between words).
    const SAID: &[&str] = &["Is", "my", "job", "safe?", "Well,", "we", "think", "so."];
    let b = project(SAID, 0.0);
    let media = call(&b, "list_media", json!({}));
    let asset = media["media"][0]["id"].as_str().unwrap().to_string();
    call(&b, "edit", json!({"ops": [
        {"op": "add_clip", "clip": {"id": "b", "track": "a1", "asset": asset, "start": {"after": "vo"}, "in_point": 0, "duration": 1}},
        {"op": "add_clip", "clip": {"id": "c", "track": "a1", "asset": asset, "start": {"after": "b"}, "in_point": 0, "duration": 1}},
        {"op": "add_clip", "clip": {"id": "d", "track": "a1", "asset": asset, "start": {"after": "c"}, "in_point": 0, "duration": 1}},
    ]}));
    // Whole sentences raise nothing.
    let r = call(&b, "trim_to_words", json!({"clip": "vo", "from": 0, "to": 3}));
    assert!(r.get("warnings").is_none(), "{r}");
    // "Well, we" stops before "think so.": trim says so and names where the sentence ends.
    let r = call(&b, "trim_to_words", json!({"clip": "b", "from": 4, "to": 5}));
    assert!(r["warnings"].to_string().contains("mid-sentence"), "{r}");
    assert_eq!(r["suggest"]["to"]["to"], 7, "{r}");
    call(&b, "trim_to_words", json!({"clip": "c", "from": 0, "to": 3}));
    // "think so." starts after "we": trim names where the sentence starts.
    let r = call(&b, "trim_to_words", json!({"clip": "d", "from": 6, "to": 7}));
    assert!(r["warnings"].to_string().contains("mid-sentence"), "{r}");
    assert_eq!(r["suggest"]["from"]["from"], 4, "{r}");
    // Every cut is in silence, yet two of them are heard as cut-offs.
    let a = call(&b, "audit_joins", json!({}));
    let by = |clip: &str, side: &str| a["list"].as_array().unwrap().iter().find(|j| j[side]["clip"] == clip).cloned().unwrap_or_else(|| panic!("{clip}: {a}"));
    // The question runs straight on into "Well, we" (the rest of the same recording): no cut there.
    assert!(a["list"].as_array().unwrap().iter().all(|j| j["in"]["clip"] != "b"), "{a}");
    let j = by("b", "out");
    assert_eq!(j["issues"][0]["kind"], "cut_off", "{j}");
    assert!(j["issues"][0]["message"].as_str().unwrap().contains("think"), "names what was cut off: {j}");
    let j = by("d", "in");
    assert_eq!(j["issues"][0]["kind"], "starts_mid_sentence", "{j}");
    assert_eq!(a["flagged"], 2, "{a}");
    // A fade softens the cut but does not finish the sentence.
    call(&b, "edit", json!({"ops": [{"op": "set_clip", "clip": "b", "patch": {"fade_out": 0.3}}]}));
    let a = call(&b, "audit_joins", json!({}));
    assert!(kinds(&a).contains(&"cut_off".to_string()), "{a}");
}

#[test]
fn a_word_repeated_across_a_sentence_break_is_not_a_stutter() {
    let b = project(&["It", "works.", "I", "like", "it.", "So", "we", "ship."], 0.0);
    let media = call(&b, "list_media", json!({}));
    let asset = media["media"][0]["id"].as_str().unwrap().to_string();
    call(&b, "edit", json!({"ops": [{"op": "add_clip", "clip": {"id": "b", "track": "a1", "asset": asset, "start": {"after": "vo"}, "in_point": 0, "duration": 1}}]}));
    call(&b, "trim_to_words", json!({"clip": "vo", "from": 2, "to": 4}));
    call(&b, "trim_to_words", json!({"clip": "b", "from": 0, "to": 1}));
    let a = call(&b, "audit_joins", json!({}));
    assert_eq!(cuts(&a)[0]["heard"], "…it. | It…", "{a}");
    assert!(!kinds(&a).contains(&"repeated_word".to_string()), "{a}");
}

#[test]
fn a_clip_that_stops_mid_word_into_silence_is_judged() {
    // A voice on a track of its own (a J- or L-cut) with nothing after it: its end is still heard.
    let b = project(NAMES, 0.0);
    // Real "three" plays 1.7–2.15: stop the clip in the middle of it.
    call(&b, "edit", json!({"ops": [{"op": "set_clip", "clip": "vo", "patch": {"source": {"in_point": 1.05}, "duration": 0.85}}]}));
    let a = call(&b, "audit_joins", json!({}));
    let end = a["list"].as_array().unwrap().iter().find(|j| j["out"]["clip"] == "vo").cloned().unwrap_or_else(|| panic!("{a}"));
    assert_eq!(end["in"], "silent", "{end}");
    assert!(end["issues"].to_string().contains("on_sound"), "the word is chopped going into silence: {end}");
    // Trimmed onto the pause after the word, it is clean.
    call(&b, "trim_to_words", json!({"clip": "vo", "from": 1, "to": 2}));
    let a = call(&b, "audit_joins", json!({}));
    assert_eq!(a["flagged"], 0, "{a}");
}

#[test]
fn audit_counts_fades_as_handled_and_held_frames_as_silent() {
    // A cut by late word times lands on sound at both edges...
    let b = project(NAMES, 0.06);
    call(&b, "delete_words", json!({"clip": "vo", "from": 2, "to": 2, "snap": false}));
    let a = call(&b, "audit_joins", json!({}));
    assert_eq!(a["flagged"], 1, "{a}");
    let right = cuts(&a)[0]["in"]["clip"].as_str().unwrap().to_string();
    // ...a 30 ms de-click ramp on each side softens the click but still chops the words: still flagged...
    let gc = call(&b, "get_clip", json!({"clip": "vo"}));
    let d = gc.get("clip").unwrap_or(&gc)["duration"].as_f64().unwrap_or_else(|| panic!("{gc}"));
    let ramps = |len: f64| {
        json!({"ops": [
            {"op": "set_param", "clip": "vo", "path": "volume", "keys": [{"t": 0, "v": 1}, {"t": d - len, "v": 1}, {"t": d, "v": 0}]},
            {"op": "set_param", "clip": right, "path": "volume", "keys": [{"t": 0, "v": 0}, {"t": len, "v": 1}]},
        ]})
    };
    call(&b, "edit", ramps(0.03));
    let a = call(&b, "audit_joins", json!({}));
    assert_eq!(a["flagged"], 1, "a de-click is not a fade: {a}");
    assert!(cuts(&a)[0]["out"].get("faded").is_none(), "{a}");
    // ...while a real fade (a quarter second) takes the voice away instead of cutting it.
    call(&b, "edit", ramps(0.25));
    let a = call(&b, "audit_joins", json!({}));
    assert_eq!(a["flagged"], 0, "{a}");
    assert_eq!((cuts(&a)[0]["out"]["faded"].as_bool(), cuts(&a)[0]["in"]["faded"].as_bool()), (Some(true), Some(true)), "{a}");
    // A held frame before a clip is silence: only the clip's own start is judged.
    let media = call(&b, "list_media", json!({}));
    let asset = media["media"][0]["id"].as_str().unwrap().to_string();
    call(&b, "edit", json!({"ops": [{"op": "add_clip", "clip": {"id": "hold", "track": "a1", "asset": asset, "in_point": 1.0, "speed": 0, "duration": 1.0, "start": {"after": right}}},
                                    {"op": "add_clip", "clip": {"id": "late", "track": "a1", "asset": asset, "in_point": 3.15, "duration": 0.5, "start": {"after": "hold"}}}]}));
    let a = call(&b, "audit_joins", json!({}));
    let j = a["list"].as_array().unwrap().iter().find(|j| j["in"]["clip"] == "late").expect("the held frame's join").clone();
    assert_eq!(j["out"], "silent", "{j}");
    assert!(j["issues"].to_string().contains("on_sound"), "late starts mid-word and is still judged: {j}");
}
