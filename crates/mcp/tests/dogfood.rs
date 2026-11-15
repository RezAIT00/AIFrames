//! Regressions from the dogfood run (a real Claude session editing real footage with the window open).

mod common;

use aiframes_core::{Document, Project};
use aiframes_mcp::{AiframesBackend, ProjectDoc, Services};
use common::*;
use serde_json::{json, Value as J};
use std::sync::Arc;
use std::time::Duration;
use suite_mcp::{client, Backend, IMAGE_KEY};

fn backend() -> (Arc<Services>, Arc<AiframesBackend>) {
    let s = services();
    (s.clone(), AiframesBackend::new(s))
}

fn in_session(b: &AiframesBackend, session: &str, name: &str, args: J) -> J {
    b.call_in(Some(session), name, &args).unwrap_or_else(|e| panic!("{name} {args} failed: {e}"))
}

// ------------------------------------------------------------------------------- 1. session-pinned project

#[test]
fn a_session_keeps_its_project_when_another_program_opens_one() {
    let dir = scratch("session");
    let (s, b) = backend();
    let mine = dir.join("coldopen.aifr");
    let theirs = dir.join("stranger.aifr");
    for (p, name) in [(&mine, "Cold open"), (&theirs, "Other cut")] {
        let mut d = Document::new(Project { name: name.into(), ..Default::default() });
        d.save_as(p).unwrap();
    }
    // The window has my project open; the AI session starts working on it (no project argument).
    s.hub.add(ProjectDoc::new(Document::open(&mine).unwrap()));
    let d = in_session(&b, "claude", "describe_project", json!({}));
    assert!(d["description"][0].as_str().unwrap().contains("Cold open"));
    assert_eq!(d["project_name"].as_str().unwrap(), "Cold open (coldopen.aifr)", "every result names the project");
    // Another program opens a different project in the window (single-instance forwarding: no session).
    let r = b.call("open_project", &json!({"path": path_str(&theirs), "show": true})).unwrap();
    let stranger = r["project"].as_u64().unwrap();
    assert_eq!(s.hub.active().unwrap().0, stranger, "the window shows the stranger's project");
    // The session still acts on its own project.
    let d = in_session(&b, "claude", "describe_project", json!({}));
    assert!(d["description"][0].as_str().unwrap().contains("Cold open"), "{d}");
    let e = in_session(&b, "claude", "edit", json!({"ops": [{"op": "add_marker", "time": 1.0, "name": "mine"}]}));
    assert_eq!(e["project"].as_u64(), Some(1), "{e}");
    let l = in_session(&b, "claude", "list_projects", json!({}));
    let flags: Vec<(u64, bool)> = l["projects"].as_array().unwrap().iter().map(|p| (p["project"].as_u64().unwrap(), p["session"].as_bool().unwrap())).collect();
    assert_eq!(flags, vec![(1, true), (stranger, false)]);
    // A new session without history falls back to the window's active project.
    let other = in_session(&b, "someone-else", "describe_project", json!({}));
    assert!(other["description"][0].as_str().unwrap().contains("Other cut"));
    // Naming a project switches the session to it.
    in_session(&b, "claude", "get_timeline", json!({"project": stranger}));
    let d = in_session(&b, "claude", "describe_project", json!({}));
    assert!(d["description"][0].as_str().unwrap().contains("Other cut"));
    // Closing it falls back to the active project again.
    in_session(&b, "claude", "close_project", json!({"discard": true}));
    let d = in_session(&b, "claude", "describe_project", json!({}));
    assert!(d["description"][0].as_str().unwrap().contains("Cold open"));
}

#[test]
fn ai_edits_record_their_session_for_the_window() {
    let (s, b) = backend();
    let id = s.add_project(Project::default());
    // Two MCP clients: one introduced itself (stdio), one did not (HTTP).
    b.client_info(Some("stdio-1-1-ab"), &json!({"name": "claude-code", "title": "Claude Code", "version": "2"}));
    let e1 = in_session(&b, "stdio-1-1-ab", "edit", json!({"ops": [{"op": "add_marker", "time": 1.0}]}));
    assert_eq!(e1["session"]["label"], "Claude Code (stdio #1)", "{e1}");
    let e2 = in_session(&b, "f00dfeed", "edit", json!({"ops": [{"op": "add_marker", "time": 2.0}]}));
    assert_eq!(e2["session"]["label"], "AI session #2 (HTTP)");
    let d = s.hub.get(id).unwrap();
    {
        let g = suite_hub::lock(&d);
        assert_eq!(g.live.ai_session.as_ref().map(|x| x.id.as_str()), Some("f00dfeed"), "the latest AI edit's session");
        let by_rev: Vec<(u64, String)> = g.live.ai_sessions.iter().map(|(r, x)| (*r, x.label.clone())).collect();
        assert_eq!(by_rev, vec![(1, "Claude Code (stdio #1)".to_string()), (2, "AI session #2 (HTTP)".to_string())]);
        assert_eq!(g.doc.history()[0].author, "ai", "the history author stays \"ai\"");
    }
    // Undo by a session is attributed too; session-less calls (CLI) record none.
    in_session(&b, "stdio-1-1-ab", "undo", json!({}));
    assert_eq!(suite_hub::lock(&d).live.ai_sessions.get(&3).map(|x| x.label.clone()).as_deref(), Some("Claude Code (stdio #1)"));
    b.call("edit", &json!({"ops": [{"op": "add_marker", "time": 3.0}]})).unwrap();
    let g = suite_hub::lock(&d);
    assert!(g.live.ai_session.is_none() && !g.live.ai_sessions.contains_key(&4));
    // The internal session argument never leaks into the strict argument check.
    drop(g);
    assert!(b.call("edit", &json!({"ops": [], "__session": {}})).unwrap_err().contains("unknown argument '__session'"));
}

#[test]
fn sessions_over_http_and_stdio() {
    let (s, _) = backend();
    let a = s.add_project(Project { name: "A".into(), ..Default::default() });
    let port = aiframes_mcp::start_http_on(s.clone(), 0, 1, false).unwrap();
    let t = Duration::from_secs(30);
    let call = |name: &str, args: J, session: Option<&str>| -> J {
        let h: Vec<(&str, &str)> = session.map(|x| vec![("Mcp-Session-Id", x)]).unwrap_or_default();
        let r = client::call_tool_with(port, name, args, t, &h).unwrap();
        assert_eq!(r["isError"], false, "{r}");
        r["structuredContent"].clone()
    };
    assert_eq!(call("describe_project", json!({}), Some("s1"))["project"], a);
    let b = call("new_project", json!({"name": "B"}), None)["project"].as_u64().unwrap();
    assert_eq!(s.hub.active().unwrap().0, b);
    assert_eq!(call("describe_project", json!({}), Some("s1"))["project"], a, "the HTTP session kept A");
    assert_eq!(call("describe_project", json!({}), None)["project"], b, "session-less calls use the active project");
    // A headless stdio server is one session.
    let mut srv = aiframes_mcp::headless_stdio(services());
    let line = |srv: &mut suite_mcp::stdio::StdioServer<AiframesBackend>, name: &str, args: J| -> J {
        let out = srv.handle_line(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string()).unwrap();
        serde_json::from_str::<J>(&out).unwrap()["result"]["structuredContent"].clone()
    };
    let first = line(&mut srv, "new_project", json!({"name": "first"}))["project"].as_u64().unwrap();
    line(&mut srv, "new_project", json!({"name": "second"}));
    let d = line(&mut srv, "describe_project", json!({}));
    assert_eq!(d["project"].as_u64(), Some(first + 1), "the session's last created project");
}

// ------------------------------------------------------------------------------- 2. CLI with the window open

#[test]
fn cli_edits_go_into_the_open_window() {
    let dir = scratch("cliwin");
    let path = dir.join("p.aifr");
    let mut d = Document::new(Project { name: "P".into(), ..Default::default() });
    d.save_as(&path).unwrap();
    // The window has the project open.
    let win = services();
    let id = win.open_project(&path).unwrap();
    let port = aiframes_mcp::start_http_on(win.clone(), 0, 1, false).unwrap();
    let ops = dir.join("ops.json");
    std::fs::write(&ops, r#"[{"op":"add_marker","time":2.0,"name":"from the CLI"}]"#).unwrap();
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = aiframes_mcp::cli_with_window(services(), Some(port), &args, &mut out, &mut err);
        (code, String::from_utf8_lossy(&out).into_owned(), String::from_utf8_lossy(&err).into_owned())
    };
    let (code, out, err) = run(&["apply", &path_str(&path), &path_str(&ops), "--author", "user"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("Applied r1"), "{out}");
    assert!(err.contains("ran in the open AIFrames window"), "{err}");
    {
        let d = win.hub.get(id).unwrap();
        let g = suite_hub::lock(&d);
        assert_eq!(g.doc.revision(), 1, "the window's document has the edit");
        assert_eq!(g.doc.history()[0].author, "user");
        assert!(g.doc.dirty(), "not saved behind the person's back");
    }
    assert!(!std::fs::read_to_string(&path).unwrap().contains("from the CLI"), "the file is untouched");
    // Reads see the live state.
    let (_, out, _) = run(&["describe", &path_str(&path)]);
    assert!(out.contains("from the CLI"), "{out}");
    // --save saves in the window.
    let (code, _, _) = run(&["apply", &path_str(&path), &path_str(&ops), "--save"]);
    assert_eq!(code, 0);
    assert!(std::fs::read_to_string(&path).unwrap().contains("from the CLI"));
    // new over the open file is refused.
    let (code, _, err) = run(&["new", &path_str(&path), "--force"]);
    assert_eq!(code, 1);
    assert!(err.contains("open in the AIFrames window"), "{err}");
    // A project the window does not have open is edited locally and saved.
    let other = dir.join("other.aifr");
    Document::new(Project::default()).save_as(&other).unwrap();
    let (code, _, err) = run(&["apply", &path_str(&other), &path_str(&ops)]);
    assert_eq!(code, 0);
    assert!(!err.contains("window"), "{err}");
    assert!(std::fs::read_to_string(&other).unwrap().contains("from the CLI"));
}

// ------------------------------------------------------------------------------- 3. source previews + strict args

fn project_with_footage(b: &AiframesBackend, dir: &std::path::Path) -> (String, J) {
    call(b, "new_project", json!({"path": path_str(&dir.join("p.aifr"))}));
    let r = call(b, "import_media", json!({"paths": [path_str(&cut_video())]}));
    (r["imported"][0]["id"].as_str().unwrap().to_string(), call(b, "get_timeline", json!({})))
}

#[test]
fn contact_sheets_of_source_media_never_touch_the_timeline() {
    let dir = scratch("preview");
    let (_, b) = backend();
    let (asset, before) = project_with_footage(&b, &dir);
    let rev = call(&b, "history", json!({}))["revision"].clone();
    let r = call(&b, "contact_sheet", json!({"asset": asset, "count": 4, "width": 160}));
    assert!(r[IMAGE_KEY].is_string());
    assert_eq!(r["source_times"].as_array().unwrap().len(), 4, "{r}");
    assert!((r["duration"].as_f64().unwrap() - 4.0).abs() < 0.1);
    assert_eq!(r["times_are"], "source seconds of the media (not timeline)");
    let r = call(&b, "contact_sheet", json!({"asset": asset, "from": 1.0, "to": 3.0, "every": 0.5}));
    assert_eq!(r["source_times"].as_array().unwrap().len(), 4);
    let r = call(&b, "snapshot", json!({"asset": asset, "at": [0.5, 2.5], "scale": 0.25}));
    assert_eq!(r["source_times"], json!([0.5, 2.5]));
    assert!(r[IMAGE_KEY].is_string());
    let r = call(&b, "filmstrip", json!({"asset": asset, "at": 2.0, "frames": 4}));
    assert_eq!(r["source_times"].as_array().unwrap().len(), 4);
    // A file that is not imported.
    let r = call(&b, "snapshot", json!({"file": path_str(&video()), "at": 1.0, "scale": 0.25}));
    assert!(r[IMAGE_KEY].is_string() && r.get("asset").is_none(), "{r}");
    let m = call(&b, "media_info", json!({"file": path_str(&video())}));
    assert_eq!(m["kind"], "video");
    assert!(m["next"].as_str().unwrap().contains("contact_sheet"));
    let m = call(&b, "media_info", json!({"asset": asset}));
    assert_eq!(m["used_by"], json!([]));
    // Nothing changed in the project.
    assert_eq!(call(&b, "get_timeline", json!({}))["tracks"], before["tracks"]);
    assert_eq!(call(&b, "history", json!({}))["revision"], rev);
    // Sound has no picture; "m3@100" on the timeline explains the asset form.
    let talk_id = call(&b, "import_media", json!({"paths": [path_str(&talk())]}))["imported"][0]["id"].as_str().unwrap().to_string();
    assert!(fails(&b, "contact_sheet", json!({"asset": talk_id})).contains("no picture"));
    let e = fails(&b, "snapshot", json!({"at": format!("{asset}@100")}));
    assert!(e.contains(&format!("\"asset\":\"{asset}\"")), "{e}");
}

#[test]
fn unknown_arguments_are_rejected_with_suggestions() {
    let (_, b) = backend();
    call(&b, "new_project", json!({}));
    let e = fails(&b, "snapshot", json!({"at": [1], "scale": 0.2, "bogus": true}));
    assert!(e.contains("unknown argument 'bogus' for snapshot") && e.contains("Valid arguments:"), "{e}");
    let e = fails(&b, "contact_sheet", json!({"cout": 4}));
    assert!(e.contains("Did you mean count?"), "{e}");
    let e = fails(&b, "describe_project", json!({"max_clip": 3}));
    assert!(e.contains("Did you mean max_clips?"), "{e}");
    assert!(fails(&b, "list_projects", json!({"x": 1})).contains("Valid arguments: none"));
    // Every tool accepts what its schema lists (spot checks across kinds).
    call(&b, "history", json!({"by": "ai", "limit": 2, "ops": false}));
    call(&b, "edit", json!({"ops": [{"op": "add_marker", "time": 0}], "plan": true, "label": "x", "author": "ai"}));
}

// ------------------------------------------------------------------------------- 4. transcripts from files

const WORDS_JSON: &str = r#"[
{"word": "I", "start": 0.0, "end": 0.16},
{"word": "gave", "start": 0.16, "end": 0.36},
{"word": "Claude", "start": 0.36, "end": 0.72},
{"word": "10", "start": 7.34, "end": 7.72},
{"word": "minutes.", "start": 7.72, "end": 8.1}
]"#;

#[test]
fn transcripts_import_from_sidecars_and_files() {
    let dir = scratch("sidecar");
    let (_, b) = backend();
    call(&b, "new_project", json!({"path": path_str(&dir.join("p.aifr"))}));
    let wav = dir.join("01.wav");
    std::fs::copy(talk(), &wav).unwrap();
    std::fs::write(dir.join("01.words.json"), WORDS_JSON).unwrap();
    let r = call(&b, "import_media", json!({"paths": [path_str(&wav)], "place": "append", "transcribe": true}));
    assert_eq!(r["transcripts"][0]["words"], 5, "{r}");
    assert_eq!(r["transcripts"][0]["format"], "words_json");
    assert!(r["transcribe_jobs"].as_array().unwrap().is_empty(), "the sidecar replaces transcription: {r}");
    let asset = r["imported"][0]["id"].as_str().unwrap().to_string();
    let t = call(&b, "get_transcript", json!({"asset": asset, "format": "words"}));
    assert_eq!(t["words"][2]["w"], "Claude");
    // Explicit files: SRT, VTT and whisper JSON.
    std::fs::write(dir.join("a.srt"), "1\n00:00:00,500 --> 00:00:01,500\nHello there world\n").unwrap();
    let r = call(&b, "import_transcript", json!({"asset": asset, "path": "a.srt"}));
    assert_eq!((r["format"].as_str(), r["words"].as_u64()), (Some("srt"), Some(3)), "{r}");
    std::fs::write(dir.join("a.vtt"), "WEBVTT\n\n00:00.500 --> 00:01.500\nOne two\n").unwrap();
    assert_eq!(call(&b, "import_transcript", json!({"asset": asset, "path": "a.vtt"}))["words"], 2);
    std::fs::write(dir.join("w.json"), r#"{"language":"en","segments":[{"start":0,"end":1,"text":" hi","words":[{"word":" hi","start":0.1,"end":0.4,"probability":0.9}]}]}"#).unwrap();
    let r = call(&b, "import_transcript", json!({"asset": asset, "path": "w.json", "plan": true}));
    assert_eq!(r["format"], "whisper_json");
    assert_eq!(r["planned"], true);
    // Without path: the sidecar next to the media.
    let r = call(&b, "import_transcript", json!({"asset": asset}));
    assert!(r["file"].as_str().unwrap().ends_with("01.words.json"));
    // sidecars:false skips them.
    let wav2 = dir.join("02.wav");
    std::fs::copy(talk(), &wav2).unwrap();
    std::fs::write(dir.join("02.srt"), "1\n00:00:00,000 --> 00:00:01,000\nx\n").unwrap();
    let r = call(&b, "import_media", json!({"paths": [path_str(&wav2)], "sidecars": false}));
    assert!(r.get("transcripts").is_none(), "{r}");
}

#[test]
fn align_transcript_takes_the_scripts_spelling() {
    let dir = scratch("align");
    let (_, b) = backend();
    call(&b, "new_project", json!({"path": path_str(&dir.join("p.aifr"))}));
    let r = call(&b, "import_media", json!({"paths": [path_str(&talk())], "place": "append", "sidecars": false}));
    let asset = r["imported"][0]["id"].as_str().unwrap().to_string();
    let heard = json!({"language": "en", "words": [
        {"text": "i", "start": 0.0, "end": 0.16}, {"text": "gave", "start": 0.16, "end": 0.36}, {"text": "clawed", "start": 0.36, "end": 0.72},
        {"text": "10", "start": 1.0, "end": 1.3}, {"text": "minutes", "start": 1.3, "end": 1.7}, {"text": "1", "start": 2.0, "end": 2.2}, {"text": "hour", "start": 2.2, "end": 2.6}]});
    call(&b, "edit", json!({"ops": [{"op": "set_transcript", "asset": asset, "transcript": heard}]}));
    let plan = call(&b, "align_transcript", json!({"asset": asset, "text": "I gave Claude the prompt. Ten minutes. One hour.", "plan": true}));
    assert_eq!(plan["planned"], true);
    let r = call(&b, "align_transcript", json!({"asset": asset, "text": "I gave Claude the prompt. Ten minutes. One hour."}));
    assert_eq!(r["words_after"], 9, "{r}");
    let low = r["low_confidence"].to_string();
    assert!(low.contains("Claude") && low.contains("clawed") && low.contains("the prompt."), "{low}");
    let t = call(&b, "get_transcript", json!({"asset": asset, "format": "words"}));
    let words: Vec<&str> = t["words"].as_array().unwrap().iter().map(|w| w["w"].as_str().unwrap()).collect();
    assert_eq!(words, vec!["I", "gave", "Claude", "the", "prompt.", "10", "minutes.", "1", "hour."], "written forms kept (default spelling)");
    assert_eq!(t["words"][5]["s"], 1.0, "Ten keeps the timing of 10");
}

// ------------------------------------------------------------------------------- 5. schema

#[test]
fn schema_tool_and_strict_json_hints() {
    let (_, b) = backend();
    let idx = call(&b, "schema", json!({}));
    assert!(idx["types"].as_array().unwrap().iter().any(|t| t["name"] == "clip"), "{idx}");
    assert!(idx["forms"].as_array().unwrap().iter().any(|t| t["name"] == "time"));
    let t = call(&b, "schema", json!({"type": "TextSource"}));
    assert_eq!(t["name"], "text");
    assert!(t["doc"]["fields"]["background"].is_object(), "{t}");
    assert_eq!(call(&b, "schema", json!({"type": "TextBox"}))["name"], "text_box");
    assert_eq!(call(&b, "schema", json!({"type": "time"}))["kind"], "form");
    assert!(call(&b, "schema", json!({"type": "all"}))["types"].is_object());
    assert!(fails(&b, "schema", json!({"type": "clp"})).contains("Did you mean clip?"));
    // A strict-JSON edit error points to it (the dogfood's text background given as a colour string).
    call(&b, "new_project", json!({}));
    call(&b, "edit", json!({"ops": [{"op": "add_clip", "clip": {"id": "t", "track": "v2", "text": "x", "duration": 1}}]}));
    let e = fails(&b, "edit", json!({"ops": [{"op": "set_clip", "clip": "t", "patch": {"source": {"background": "#000000B0"}}}]}));
    assert!(e.contains("schema"), "{e}");
    let e = fails(&b, "edit", json!({"ops": [{"op": "trim_clip", "clip": "t", "edge": "end", "to": 7.0, "foo": 1}]}));
    assert!(e.contains("unknown field") && e.contains("schema"), "{e}");
}

// ------------------------------------------------------------------------------- 6. smaller ones

#[test]
fn live_state_and_result_consistency() {
    let (s, b) = backend();
    call(&b, "new_project", json!({}));
    call(&b, "edit", json!({"ops": [{"op": "add_clip", "clip": {"id": "c9", "track": "v1", "text": "x", "duration": 2}}]}));
    s.hub.set_waker(|| {}); // a window is attached
    call(&b, "select", json!({"clips": ["c9"]}));
    let st = call(&b, "get_state", json!({}));
    assert_eq!(st["selection"], json!(["c9"]), "the selection shows at once: {st}");
    call(&b, "seek", json!({"time": 1.5}));
    assert_eq!(call(&b, "get_state", json!({}))["playhead"], 1.5);
    // image:false returns no picture.
    let r = call(&b, "snapshot", json!({"at": [0.5, 1.0], "image": false}));
    assert!(r.get(IMAGE_KEY).is_none() && r["on_screen"].is_array(), "{r}");
    // The on-screen order (core): video from the top layer down to V1, then audio with A1 first.
    let d = call(&b, "describe_project", json!({"media": false}));
    let tracks: Vec<String> = d["description"].as_array().unwrap().iter().filter_map(|l| l.as_str()).filter(|l| l.starts_with("  ") && !l.starts_with("    ")).map(|l| l.trim().split(' ').next().unwrap().to_string()).collect();
    assert_eq!(tracks, vec!["v2", "v1", "a1", "a2"], "{d}");
    let tl = call(&b, "get_timeline", json!({}));
    let kinds: Vec<&str> = tl["tracks"].as_array().unwrap().iter().map(|t| t["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, vec!["video", "video", "audio", "audio"]);
    // Every project-scoped result carries the project, its name and the revision.
    for (name, args) in [("get_state", json!({})), ("list_media", json!({})), ("validate", json!({})), ("chapters", json!({})), ("get_clip", json!({"clip": "c9"}))] {
        let r = call(&b, name, args);
        assert!(r["project"].is_u64() && r["project_name"].is_string() && r["revision"].is_u64(), "{name}: {r}");
    }
}
