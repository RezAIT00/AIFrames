//! Regressions from dogfood round 2.

mod common;

use aiframes_core::{Document, Project};
use aiframes_mcp::{AiframesBackend, Services};
use common::*;
use serde_json::{json, Value as J};
use std::sync::Arc;

fn backend() -> (Arc<Services>, Arc<AiframesBackend>) {
    let s = services();
    (s.clone(), AiframesBackend::new(s))
}

// ------------------------------------------------------------------------------- 1. changed on disk

#[test]
fn save_refuses_a_file_another_program_changed() {
    let dir = scratch("disk");
    let path = dir.join("p.aifr");
    Document::new(Project { name: "Mine".into(), ..Default::default() }).save_as(&path).unwrap();
    let (_, b) = backend();
    call(&b, "open_project", json!({"path": path_str(&path)}));
    call(&b, "edit", json!({"ops": [{"op": "add_marker", "time": 1.0, "name": "ai work"}]}));
    // Another program rewrites the file.
    let mut other = Document::open(&path).unwrap();
    other.apply("user", "elsewhere", vec![aiframes_core::Op::from_json(&json!({"op": "set_project", "name": "Theirs, a longer name"})).unwrap()]).unwrap();
    other.save().unwrap();
    let e = fails(&b, "save_project", json!({}));
    assert!(e.contains("changed by another program") && e.contains("overwrite_external") && e.contains("reload"), "{e}");
    assert!(std::fs::read_to_string(&path).unwrap().contains("Theirs"), "their file is intact");
    let e = fails(&b, "close_project", json!({"save": true}));
    assert!(e.contains("changed by another program"), "{e}");
    let o = call(&b, "open_project", json!({"path": path_str(&path), "show": false}));
    assert_eq!(o["disk_changed"], true, "{o}");
    // Reload takes their version (undoable); then saving is fine.
    let r = call(&b, "open_project", json!({"path": path_str(&path), "reload": true}));
    assert!(r["reloaded"].is_object(), "{r}");
    assert!(call(&b, "describe_project", json!({}))["description"][0].as_str().unwrap().contains("Theirs"));
    call(&b, "save_project", json!({}));
    // overwrite_external is the explicit way to replace their version.
    let mut other = Document::open(&path).unwrap();
    other.apply("user", "again", vec![aiframes_core::Op::from_json(&json!({"op": "set_project", "notes": "theirs again"})).unwrap()]).unwrap();
    other.save().unwrap();
    call(&b, "edit", json!({"ops": [{"op": "add_marker", "time": 2.0, "name": "mine"}]}));
    assert!(fails(&b, "save_project", json!({})).contains("another program"));
    call(&b, "save_project", json!({"overwrite_external": true}));
    assert!(std::fs::read_to_string(&path).unwrap().contains("\"mine\""));
}

// ------------------------------------------------------------------------------- 2. written spelling

#[test]
fn align_transcript_tool_reports_kept_written_forms() {
    let dir = scratch("spelling");
    let (_, b) = backend();
    call(&b, "new_project", json!({"path": path_str(&dir.join("p.aifr"))}));
    let r = call(&b, "import_media", json!({"paths": [path_str(&talk())], "sidecars": false, "place": "append"}));
    let asset = r["imported"][0]["id"].as_str().unwrap().to_string();
    let heard = json!({"words": [{"text": "make", "start": 0.0, "end": 0.2}, {"text": "a", "start": 0.2, "end": 0.3}, {"text": "3D", "start": 0.3, "end": 0.7}, {"text": "game", "start": 0.7, "end": 1.0}, {"text": "10", "start": 1.2, "end": 1.5}, {"text": "minutes", "start": 1.5, "end": 1.9}]});
    call(&b, "edit", json!({"ops": [{"op": "set_transcript", "asset": asset, "transcript": heard}]}));
    let r = call(&b, "align_transcript", json!({"asset": asset, "text": "Make a three D game. Ten minutes.", "plan": true}));
    assert_eq!(r["spelling"], "written");
    assert_eq!(r["kept_written"].as_array().unwrap().len(), 2, "{r}");
    assert_eq!(r["kept_written"][0]["written"], "3D");
    let r = call(&b, "align_transcript", json!({"asset": asset, "text": "Make a three D game. Ten minutes."}));
    assert_eq!(r["words_after"], 6, "{r}");
    let t = call(&b, "get_transcript", json!({"asset": asset, "format": "paragraphs"}));
    assert!(t["paragraphs"][0]["text"].as_str().unwrap().starts_with("Make a 3D game. 10 minutes."), "{t}");
    let r = call(&b, "align_transcript", json!({"asset": asset, "text": "Make a three D game. Ten minutes.", "spelling": "script"}));
    assert_eq!(r["kept_written"], json!([]));
    assert!(fails(&b, "align_transcript", json!({"asset": asset, "text": "x", "spelling": "phonetic"})).contains("written"));
}

// ------------------------------------------------------------------------------- 3. docs

#[test]
fn docs_describe_the_session_project_and_caption_regeneration() {
    let (_, b) = backend();
    let tools = suite_mcp::Backend::tools(&*b, false);
    let get = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap().clone();
    let p = get("describe_project")["inputSchema"]["properties"]["project"]["description"].as_str().unwrap().to_string();
    assert!(p.contains("session") && !p.contains("active project"), "{p}");
    let c = get("generate_captions");
    assert!(c["description"].as_str().unwrap().contains("captions they edited stay"), "{c}");
    assert!(c["inputSchema"]["properties"]["replace"]["description"].as_str().unwrap().contains("default false"));
    let instr = suite_mcp::Backend::instructions(&*b, "headlessly");
    assert!(instr.contains("Project rule:") && instr.contains("re-check the brief") && instr.contains("overwrite_external"));
}

// ------------------------------------------------------------------------------- 4. CLI messages

#[test]
fn cli_messages_say_what_happened() {
    let dir = scratch("climsg");
    let path = dir.join("p.aifr");
    Document::new(Project::default()).save_as(&path).unwrap();
    let win = services();
    win.open_project(&path).unwrap();
    let port = aiframes_mcp::start_http_on(win, 0, 1, false).unwrap();
    let run = |window: Option<u16>, args: &[&str]| {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = aiframes_mcp::cli_with_window(services(), window, &args, &mut out, &mut err);
        (code, String::from_utf8_lossy(&out).into_owned(), String::from_utf8_lossy(&err).into_owned())
    };
    let p = path_str(&path);
    for cmd in [vec!["describe", &p], vec!["history", &p], vec!["validate", &p]] {
        let (_, _, err) = run(Some(port), &cmd);
        assert!(!err.contains("window"), "{cmd:?} only reads: {err}");
    }
    let ops = dir.join("ops.json");
    std::fs::write(&ops, r#"[{"op":"add_marker","time":1.0}]"#).unwrap();
    let (code, _, err) = run(Some(port), &["apply", &p, &path_str(&ops)]);
    assert_eq!(code, 0);
    assert!(err.contains("unsaved"), "{err}");
    let (code, _, err) = run(Some(port), &["apply", &p, &path_str(&ops), "--save"]);
    assert_eq!(code, 0);
    assert!(err.contains("saved there") && !err.contains("unsaved"), "{err}");
    // import reports a sidecar transcript (here: locally).
    let local = dir.join("local.aifr");
    Document::new(Project::default()).save_as(&local).unwrap();
    let wav = dir.join("line.wav");
    std::fs::copy(talk(), &wav).unwrap();
    std::fs::write(dir.join("line.words.json"), r#"[{"word":"Hello","start":0.2,"end":0.6}]"#).unwrap();
    let (code, out, _) = run(None, &["import", &path_str(&local), &path_str(&wav), "--append"]);
    assert_eq!(code, 0);
    assert!(out.contains("transcript for m1 from") && out.contains("line.words.json"), "{out}");
    // describe --from/--to lists only the media used there.
    std::fs::copy(video(), dir.join("v.mp4")).unwrap();
    run(None, &["import", &path_str(&local), &path_str(&dir.join("v.mp4"))]);
    let (_, out, _) = run(None, &["describe", &path_str(&local), "--from", "0", "--to", "1"]);
    assert!(out.contains("Media used in this range (1 of 2):") && out.contains("line.wav") && !out.contains("v.mp4"), "{out}");
    let (_, out, _) = run(None, &["describe", &path_str(&local)]);
    assert!(out.contains("Media (2):"), "{out}");
    let _: J = json!(null);
}

// ------------------------------------------------------------------------------- split tokens

/// Word-timing tools split "$2.11" into "$2" ".11" and "four-hour" into "four" "-hour": transcript
/// text reads them as one word (like the captions), searches find them, indices still address tokens.
#[test]
fn split_tokens_read_and_search_as_one_word() {
    let dir = scratch("split-tokens");
    let (_, b) = backend();
    call(&b, "new_project", json!({"path": path_str(&dir.join("p.aifr"))}));
    let r = call(&b, "import_media", json!({"paths": [path_str(&talk())], "sidecars": false, "place": "append"}));
    let asset = r["imported"][0]["id"].as_str().unwrap().to_string();
    let toks = ["It", "costs", "$2", ".11", "for", "a", "four", "-hour", "run."];
    let mut words: Vec<J> = toks.iter().enumerate().map(|(i, w)| json!({"text": w, "start": i as f64 * 0.5, "end": i as f64 * 0.5 + 0.4})).collect();
    // Long pauses inside "$2.11" (must not split it across paragraphs) and after it.
    for (i, w) in words.iter_mut().enumerate().skip(3) {
        let shift = if i == 3 { 1.5 } else { 3.0 };
        w["start"] = json!(i as f64 * 0.5 + shift);
        w["end"] = json!(i as f64 * 0.5 + shift + 0.4);
    }
    call(&b, "edit", json!({"ops": [{"op": "set_transcript", "asset": asset, "transcript": {"words": words}}]}));

    let t = call(&b, "get_transcript", json!({"asset": asset, "format": "paragraphs"}));
    assert_eq!(t["paragraphs"][0]["text"], "It costs $2.11", "{t}");
    assert_eq!(t["paragraphs"][1]["text"], "for a four-hour run.", "{t}");
    let t = call(&b, "get_transcript", json!({"asset": asset, "format": "indexed"}));
    assert_eq!(t["paragraphs"][0]["text"], "[0]It [1]costs [2–3]$2.11", "{t}");
    assert_eq!(t["paragraphs"][1]["text"], "[4]for [5]a [6–7]four-hour [8]run.", "{t}");
    let t = call(&b, "get_transcript", json!({"asset": asset, "format": "words"}));
    assert_eq!(t["words"].as_array().unwrap().len(), 9, "tokens keep their own indices: {t}");

    for (query, from, to, text) in [("$2.11", 2, 3, "$2.11"), ("2.11", 2, 3, "$2.11"), ("four-hour", 6, 7, "four-hour"), ("four hour", 6, 7, "four-hour"), ("costs $2.11 for", 1, 4, "costs $2.11 for"), ("a four-hour run", 5, 8, "a four-hour run.")] {
        let r = call(&b, "search_transcript", json!({"query": query}));
        assert_eq!(r["total"], 1, "{query}: {r}");
        let m = &r["matches"][0];
        assert_eq!((m["from"].as_u64().unwrap(), m["to"].as_u64().unwrap()), (from, to), "{query}: {r}");
        assert_eq!(m["text"], text, "{query}: {r}");
    }
    let r = call(&b, "search_transcript", json!({"query": "four-hour", "context": 2}));
    assert_eq!(r["matches"][0]["context"], "for a [four-hour] run.", "{r}");
    // Whole words only: "hour" is part of "four-hour", ".11" of "$2.11".
    assert_eq!(call(&b, "search_transcript", json!({"query": "hour"}))["total"], 0);

    let r = call(&b, "delete_words", json!({"asset": asset, "from": 2, "to": 3, "plan": true}));
    assert!(r.to_string().contains("$2.11"), "{r}");
}
