//! The MCP backend driven headlessly end to end on lavfi fixtures.

mod common;

use aiframes_mcp::{AiframesBackend, JobStatus, Services};
use common::*;
use serde_json::{json, Value as J};
use std::time::Duration;
use suite_mcp::{Backend, IMAGE_KEY};

fn ids(v: &J) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

fn clip_ids_on(b: &AiframesBackend, track: &str) -> Vec<String> {
    let t = call(b, "get_timeline", json!({}));
    t["tracks"].as_array().unwrap().iter().find(|x| x["id"] == track).map(|x| x["clips"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap().to_string()).collect()).unwrap_or_default()
}

#[test]
fn edit_loop_end_to_end() {
    let dir = scratch("e2e");
    let s = services();
    let b = AiframesBackend::new(s.clone());

    // --- new project + import with placement
    let path = dir.join("edit.aifr");
    let r = call(&b, "new_project", json!({"preset": "youtube", "name": "Test", "path": path_str(&path)}));
    assert_eq!(r["project"], 1);
    assert!(path.exists());
    let r = call(&b, "import_media", json!({"paths": [path_str(&video()), path_str(&talk())], "place": "append"}));
    assert_eq!(r["imported"].as_array().unwrap().len(), 2, "{r}");
    assert_eq!(ids(&r["clips"]).len(), 2);
    assert_eq!(r["revision"], 1);
    let again = call(&b, "import_media", json!({"paths": [path_str(&video())]}));
    assert_eq!(again["note"], "nothing new to import", "{again}");
    let media = call(&b, "list_media", json!({}));
    let m = media["media"].as_array().unwrap();
    assert_eq!(m.len(), 2);
    let (vid, voice) = (m[0]["id"].as_str().unwrap().to_string(), m[1]["id"].as_str().unwrap().to_string());
    assert!(!m[0]["path"].as_str().unwrap().contains(':') || m[0]["path"].as_str().unwrap().starts_with(".."), "relative media path: {}", m[0]["path"]);
    let v1 = clip_ids_on(&b, "v1");
    let a1 = clip_ids_on(&b, "a1");
    assert_eq!((v1.len(), a1.len()), (1, 1));
    let (c_vid, c_voice) = (v1[0].clone(), a1[0].clone());

    // --- batch edit: split + anchored title + marker
    let r = call(
        &b,
        "edit",
        json!({"ops": [
            {"op": "split_clip", "clip": c_vid, "at": 3.0},
            {"op": "add_clip", "clip": {"id": "title", "track": "v2", "text": "Hello", "start": {"with": c_vid, "offset": 0.5}, "duration": 2}},
            {"op": "add_marker", "time": 1.0, "name": "Hook", "kind": "marker"},
        ]}),
    );
    assert_eq!(r["revision"], 2);
    assert!(ids(&r["created"]).contains(&"title".to_string()), "{r}");
    assert_eq!(r["done"].as_array().unwrap().len(), 3);
    assert!(r["open_errors"].is_number());
    let right = clip_ids_on(&b, "v1")[1].clone();

    // --- plan is a dry run with the same receipt shape
    let p = call(&b, "edit", json!({"plan": true, "ops": [{"op": "delete_clip", "clip": c_vid, "ripple": true}]}));
    assert_eq!(p["planned"], true);
    assert_eq!(p["revision"], 2, "a plan reports the current revision, never a future one");
    assert!(p["moved"].as_array().unwrap().iter().any(|m| m["id"] == right.as_str()), "{p}");
    let h = call(&b, "history", json!({}));
    assert_eq!(h["revision"], 2, "plan changed nothing");

    // --- errors: near-miss op names, unknown ids, nothing changes
    let e = fails(&b, "edit", json!({"ops": [{"op": "splt_clip", "clip": c_vid, "at": 1.0}]}));
    assert!(e.contains("Did you mean split_clip"), "{e}");
    let e = fails(&b, "edit", json!({"ops": [{"op": "trim_clip", "clip": c_vid, "edge": "end", "delta": -0.5}, {"op": "delete_clip", "clip": "nope"}]}));
    assert!(e.contains("op #1"), "{e}");
    assert_eq!(call(&b, "history", json!({}))["revision"], 2);
    let e = fails(&b, "get_clip", json!({"clip": "titel"}));
    assert!(e.contains("Did you mean title"), "{e}");
    let e = fails(&b, "describe_projekt", json!({}));
    assert!(e.contains("Did you mean describe_project"), "{e}");

    // --- undo / redo
    let u = call(&b, "undo", json!({}));
    assert_eq!(u["undone"][0]["by"], "ai");
    assert!(clip_ids_on(&b, "v2").is_empty());
    let r = call(&b, "redo", json!({}));
    assert!(r["redone"].is_array());
    assert_eq!(clip_ids_on(&b, "v2"), vec!["title".to_string()]);
    let rev = r["revision"].as_u64().unwrap();

    // --- the person edits; changes_since shows it
    {
        let (_, d) = s.hub.active().unwrap();
        let mut g = suite_hub::lock(&d);
        let op = aiframes_core::Op::from_json(&json!({"op": "set_clip", "clip": "title", "patch": {"name": "greeting"}})).unwrap();
        g.doc.apply("user", "Rename title", vec![op]).unwrap();
    }
    let c = call(&b, "changes_since", json!({"revision": rev, "exclude_author": "ai"}));
    let entries = c["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{c}");
    assert_eq!(entries[0]["author"], "user");
    assert!(entries[0]["ops"].is_array());
    let u = call(&b, "undo", json!({}));
    assert!(u["note"].as_str().unwrap().contains("by user"), "undoing the person's edit is flagged: {u}");
    call(&b, "redo", json!({}));

    // --- AI edits are recorded for the GUI and the AI log
    {
        let (_, d) = s.hub.active().unwrap();
        let g = suite_hub::lock(&d);
        assert!(g.live.ai_changed_revision > 0);
    }
    assert!(s.hub.take_notices().iter().any(|n| n.starts_with("AI: ")));

    // --- validate: a gap in the main track with a machine fix
    let r = call(&b, "edit", json!({"ops": [{"op": "delete_clip", "clip": c_vid, "ripple": false}]}));
    let nf = r["new_findings"].as_array().cloned().unwrap_or_default();
    assert!(nf.iter().any(|f| f["code"] == "gap_in_main"), "the receipt reports the new gap: {r}");
    let v = call(&b, "validate", json!({}));
    let gap = v["findings"].as_array().unwrap().iter().find(|f| f["code"] == "gap_in_main").cloned().expect("gap finding");
    assert!(gap["fix_ops"].is_array(), "{gap}");
    let v = call(&b, "validate", json!({"apply_fixes": true, "codes": ["gap_in_main"]}));
    assert_eq!(v["fix"]["fixed_findings"], 1, "{v}");
    assert!(!v["findings"].as_array().unwrap().iter().any(|f| f["code"] == "gap_in_main"), "{v}");

    // --- describe / timeline / clip
    let d = call(&b, "describe_project", json!({}));
    let lines: Vec<String> = d["description"].as_array().unwrap().iter().map(|l| l.as_str().unwrap().to_string()).collect();
    assert!(lines[0].starts_with("Project \"Test\""), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("title")), "{lines:?}");
    let short = call(&b, "describe_project", json!({"from": 0, "to": 0.2, "media": false}));
    assert!(short["description"].as_array().unwrap().len() < lines.len());
    let clip = call(&b, "get_clip", json!({"clip": "title"}));
    assert_eq!(clip["clip"]["source"]["text"], "Hello");
    assert!(clip["span"].is_array());

    // --- transcript tools on a synthetic transcript
    call(&b, "edit", json!({"ops": [{"op": "set_transcript", "asset": voice, "transcript": talk_transcript()}]}));
    let t = call(&b, "get_transcript", json!({"asset": voice, "format": "words"}));
    let words = t["words"].as_array().unwrap();
    assert_eq!(words.len(), 12);
    assert_eq!(words[3]["w"], "delta");
    assert!((words[3]["t"].as_f64().unwrap() - 2.75).abs() < 1e-6, "{}", words[3]);
    let para = call(&b, "get_transcript", json!({"clip": c_voice}));
    assert_eq!(para["paragraphs"].as_array().unwrap().len(), 4, "{para}");
    let idx = call(&b, "get_transcript", json!({"asset": voice, "format": "indexed", "from": 3, "to": 5}));
    assert!(idx["paragraphs"][0]["text"].as_str().unwrap().starts_with("[3]delta"), "{idx}");
    let found = call(&b, "search_transcript", json!({"query": "Delta, echo"}));
    assert_eq!(found["total"], 1);
    assert_eq!((found["matches"][0]["from"].as_u64(), found["matches"][0]["to"].as_u64()), (Some(3), Some(4)));
    let del = call(&b, "delete_words", json!({"from": 3, "to": 4, "asset": voice}));
    assert_eq!(del["deleted_text"][0], "delta echo");
    assert!(del["duration"].as_f64().is_some());
    let t = call(&b, "get_transcript", json!({"asset": voice, "format": "words"}));
    assert_eq!(t["words"][3]["out"], true, "{}", t["words"][3]);
    assert_eq!(t["not_on_timeline"], 2);
    let caps = call(&b, "generate_captions", json!({"max_words": 3}));
    let created = ids(&caps["created"]);
    assert!(created.len() >= 4, "a captions track and caption clips: {caps}");
    let tl = call(&b, "get_timeline", json!({}));
    assert!(tl["tracks"].as_array().unwrap().iter().any(|t| t["role"] == "captions" && !t["clips"].as_array().unwrap().is_empty()));

    // --- seeing (works with the stub renderer too)
    let snap = call(&b, "snapshot", json!({"at": [1.0]}));
    let png = snap[IMAGE_KEY].as_str().unwrap();
    assert!(png.starts_with("iVBORw0KGgo"), "base64 PNG");
    assert_eq!(snap["size"][0], 768, "small by default");
    assert!(snap["on_screen"].is_array());
    let snap2 = call(&b, "snapshot", json!({"at": "title,0.5", "scale": 0.25}));
    assert_eq!(snap2["times"].as_array().unwrap().len(), 2);
    let sheet = call(&b, "contact_sheet", json!({"count": 4}));
    assert!(sheet[IMAGE_KEY].is_string());
    let strip = call(&b, "filmstrip", json!({"cut": "title"}));
    assert_eq!(strip["times"].as_array().unwrap().len(), 6);
    let files = call(&b, "snapshot", json!({"at": [0.5, 1.5], "save_dir": path_str(&dir.join("frames")), "image": false}));
    assert_eq!(files["files"].as_array().unwrap().len(), 2);
    assert!(files.get(IMAGE_KEY).is_none());

    // --- live state (headless: requests are recorded)
    let st = call(&b, "get_state", json!({}));
    assert_eq!(st["window"], false);
    let sk = call(&b, "seek", json!({"time": "title"}));
    assert!(sk["playhead"].as_f64().unwrap() > 0.0);
    call(&b, "select", json!({"clips": ["title"]}));
    call(&b, "play", json!({"from": 0, "to": 2}));
    call(&b, "pause", json!({}));
    {
        let (_, d) = s.hub.active().unwrap();
        let g = suite_hub::lock(&d);
        assert_eq!(g.live.select_request.as_deref(), Some(&["title".to_string()][..]));
        assert_eq!(g.live.play_request, Some(false));
    }
    assert!(fails(&b, "screenshot_ui", json!({})).contains("launch_gui"));

    // --- references
    let ops = call(&b, "list_ops", json!({"op": "split_clip"}));
    assert_eq!(ops["ops"][0]["example"]["op"], "split_clip");
    assert!(call(&b, "list_effects", json!({}))["effects"].as_array().unwrap().len() > 10);
    assert!(call(&b, "list_transitions", json!({}))["transitions"].as_array().unwrap().len() > 10);
    let ch = call(&b, "chapters", json!({}));
    assert_eq!(ch["ok"], false);

    // --- save, reopen elsewhere: same edit, history continues
    call(&b, "save_project", json!({}));
    let n_clips = call(&b, "list_projects", json!({}))["projects"][0]["clips"].as_u64().unwrap();
    let rev = call(&b, "history", json!({}))["revision"].as_u64().unwrap();
    let b2 = AiframesBackend::new(services());
    let o = call(&b2, "open_project", json!({"path": path_str(&path)}));
    assert_eq!(o["clips"].as_u64().unwrap(), n_clips);
    assert_eq!(o["revision"].as_u64().unwrap(), rev);
    assert!(call(&b2, "list_media", json!({}))["media"].as_array().unwrap().iter().all(|m| m.get("offline").is_none()));
    let _ = vid;
}

#[test]
fn silences_scenes_beats_and_audio() {
    let dir = scratch("analysis");
    let s = services();
    let b = AiframesBackend::new(s.clone());
    call(&b, "new_project", json!({"path": path_str(&dir.join("a.aifr"))}));
    call(&b, "import_media", json!({"paths": [path_str(&talk()), path_str(&cut_video()), path_str(&music())], "place": "none"}));
    let media = call(&b, "list_media", json!({}));
    let id = |file: &str| media["media"].as_array().unwrap().iter().find(|m| m["file"] == file).unwrap()["id"].as_str().unwrap().to_string();
    let (talk_id, cut_id, music_id) = (id("talk.wav"), id("cut.mp4"), id("music.wav"));
    call(
        &b,
        "edit",
        json!({"ops": [
            {"op": "add_clip", "clip": {"id": "vo", "track": "a1", "asset": talk_id, "start": 0}},
            {"op": "add_clip", "clip": {"id": "shot", "track": "v1", "asset": cut_id, "start": 0}},
            {"op": "add_clip", "clip": {"id": "song", "track": "a2", "asset": music_id, "start": 0}},
        ]}),
    );

    // silence detection and removal (plan, then apply)
    let sil = call(&b, "detect_silence", json!({"clip": "vo"}));
    assert!(sil["count"].as_u64().unwrap() >= 3, "{sil}");
    let plan = call(&b, "remove_silences", json!({"clip": "vo", "plan": true, "min_length": 0.5, "padding": 0.1}));
    assert_eq!(plan["planned"], true);
    let cuts = plan["cuts"].as_array().unwrap();
    assert!(cuts.len() >= 3, "{plan}");
    assert!((cuts[0]["src"][0].as_f64().unwrap() - 1.8).abs() < 0.05, "first internal silence 1.7–2.7 padded: {}", cuts[0]);
    let before = call(&b, "history", json!({}))["revision"].as_u64().unwrap();
    let applied = call(&b, "remove_silences", json!({"clip": "vo", "min_length": 0.5, "padding": 0.1}));
    assert_eq!(applied["revision"].as_u64().unwrap(), before + 1, "one undoable step");
    assert!(applied["removed_seconds"].as_f64().unwrap() > 2.0, "{applied}");
    assert!(clip_ids_on(&b, "a1").len() >= 4, "the voice clip is now several pieces");

    // scene detection with markers
    let sc = call(&b, "detect_scenes", json!({"clip": "shot", "markers": true}));
    let c = sc["cuts"].as_array().unwrap();
    assert_eq!(c.len(), 1, "{sc}");
    assert!((c[0]["t"].as_f64().unwrap() - 2.0).abs() < 0.1, "{sc}");
    assert_eq!(ids(&sc["created"]).len(), 1, "one marker: {sc}");
    let sp = call(&b, "detect_scenes", json!({"clip": "shot", "split": true}));
    assert_eq!(ids(&sp["created"]).len(), 1, "split into two: {sp}");

    // beats
    let bt = call(&b, "detect_beats", json!({"clip": "song", "markers": true, "every": 2}));
    let bpm = bt["bpm"].as_f64().unwrap();
    assert!((bpm - 120.0).abs() < 8.0 || (bpm - 60.0).abs() < 4.0 || (bpm - 240.0).abs() < 15.0, "bpm {bpm}: {bt}");
    assert!(!ids(&bt["created"]).is_empty(), "{bt}");

    // ducking + audio analysis (the mix comes from the renderer; the stub mixes silence)
    let d = call(&b, "auto_ducking", json!({"amount_db": -15}));
    assert_eq!(ids(&d["created"]).len(), 1, "{d}");
    assert!(fails(&b, "auto_ducking", json!({})).contains("already ducks"));
    let an = call(&b, "analyze_audio", json!({}));
    assert!(an["loudness"]["integrated_lufs"].is_number(), "{an}");
    assert!(an["short_term"]["lufs"].as_array().is_some());
    let file = call(&b, "analyze_audio", json!({"asset": talk_id}));
    assert!(file["loudness"]["integrated_lufs"].as_f64().unwrap() > -30.0, "{file}");
}

#[test]
fn export_job_runs_or_reports_the_stub() {
    let dir = scratch("export");
    let s = services();
    let b = AiframesBackend::new(s.clone());
    call(&b, "new_project", json!({"path": path_str(&dir.join("x.aifr")), "preset": "720p"}));
    call(&b, "import_media", json!({"paths": [path_str(&video())], "place": "append"}));
    call(&b, "edit", json!({"ops": [{"op": "add_clip", "clip": {"track": "v2", "text": "Export test", "start": 0.5, "duration": 2}}]}));
    let out = dir.join("out.mp4");
    let r = call(&b, "export", json!({"path": path_str(&out), "range": [0, 2], "preset": "draft", "wait_seconds": 300}));
    let job = r["job"].as_u64().unwrap();
    let st = call(&b, "export_status", json!({"job": job}));
    match st["status"].as_str().unwrap() {
        "done" => {
            assert!(out.exists() && std::fs::metadata(&out).unwrap().len() > 1000, "exported file: {st}");
            assert!(st["result"]["frames"].as_u64().unwrap() >= 59, "{st}");
            println!("export ran against the real renderer: {}", st["result"]);
        }
        "failed" if render_is_stub(st["error"].as_str().unwrap_or("")) => println!("render crate is still a stub: {}", st["error"]),
        other => panic!("export {other}: {st}"),
    }
    let jobs = call(&b, "jobs", json!({}));
    assert!(jobs["jobs"].as_array().unwrap().iter().any(|j| j["job"] == job));
    assert!(fails(&b, "export", json!({"path": path_str(&video())})).contains("refusing"), "never export over the project's media");
    let _ = s.jobs.wait(job, Duration::from_secs(1)).map(|j| assert_ne!(j.status, JobStatus::Running));
}

#[test]
fn waits_are_capped_and_long_tools_get_a_long_proxy_timeout() {
    let dir = scratch("waitcap");
    let s = services();
    let b = AiframesBackend::new(s.clone());
    call(&b, "new_project", json!({"path": path_str(&dir.join("p.aifr"))}));
    // A job that outlives the call: a capped wait returns "still running" with the job id.
    let job = s.jobs.spawn("export", "slow export", Some(1), |ctx| {
        while !ctx.cancelled() {
            std::thread::sleep(Duration::from_millis(20));
        }
        Err("stopped".into())
    });
    let t = std::time::Instant::now();
    let r = call(&b, "jobs", json!({"job": job, "wait_seconds": 0.3}));
    assert_eq!(r["status"], "running", "{r}");
    assert!(t.elapsed() < Duration::from_secs(5));
    s.jobs.cancel(job);
    assert_eq!(s.jobs.wait(job, Duration::from_secs(5)).unwrap().status, JobStatus::Cancelled);
    assert_eq!(aiframes_mcp::MAX_WAIT_SECONDS, 300.0);
    // The stdio proxy's per-tool hint.
    assert_eq!(b.proxy_timeout("export", &json!({})), Some(Duration::from_secs(900)));
    assert_eq!(b.proxy_timeout("describe_project", &json!({})), None);
}

#[test]
fn jobs_report_progress_and_cancel() {
    let s = Services::with_pool(aiframes_media::MediaPool::with_cache_dir(1 << 20, root().join("cache")));
    let id = s.jobs.spawn("analyze", "spin", None, |ctx| {
        for i in 0..200 {
            if ctx.cancelled() {
                return Err("stopped".into());
            }
            ctx.progress(i as f32 / 200.0, "spinning");
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(json!({"ok": true}))
    });
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(s.jobs.get(id).unwrap().status, JobStatus::Running);
    assert!(s.jobs.cancel(id));
    let j = s.jobs.wait(id, Duration::from_secs(5)).unwrap();
    assert_eq!(j.status, JobStatus::Cancelled);
    assert_eq!(j.to_json()["status"], "cancelled");
    let ok = s.jobs.spawn("analyze", "quick", None, |_| Ok(json!({"n": 1})));
    let j = s.jobs.wait(ok, Duration::from_secs(5)).unwrap();
    assert_eq!(j.to_json()["result"]["n"], 1);
    let bad = s.jobs.spawn("analyze", "panics", None, |_| panic!("boom"));
    assert_eq!(s.jobs.wait(bad, Duration::from_secs(5)).unwrap().status, JobStatus::Failed);
    for _ in 0..60 {
        s.jobs.spawn("analyze", "x", None, |_| Ok(J::Null));
    }
    std::thread::sleep(Duration::from_millis(300));
    s.jobs.spawn("analyze", "last", None, |_| Ok(J::Null));
    assert!(s.jobs.list().len() <= aiframes_mcp::KEEP_FINISHED_JOBS + 2, "finished jobs are pruned: {}", s.jobs.list().len());
}

/// Real Whisper transcription (slow; needs the whisper CLI): `cargo test -- --ignored`.
#[test]
#[ignore]
fn whisper_transcription_job() {
    let dir = scratch("whisper");
    let b = AiframesBackend::new(services());
    call(&b, "new_project", json!({"path": path_str(&dir.join("w.aifr"))}));
    let r = call(&b, "import_media", json!({"paths": [path_str(&talk())], "place": "append", "transcribe": true, "language": "en"}));
    let job = r["transcribe_jobs"][0]["job"].as_u64().unwrap();
    let st = call(&b, "jobs", json!({"job": job, "wait_seconds": 600}));
    assert_eq!(st["status"], "done", "{st}");
}
