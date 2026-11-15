//! Regression tests for review round 1 (ported from the reviewer's repros) and the round-2 additions.

mod common;

use aiframes_mcp::{actions, AiframesBackend, Services};
use common::*;
use serde_json::{json, Value as J};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn backend() -> (Arc<Services>, Arc<AiframesBackend>) {
    let s = services();
    (s.clone(), AiframesBackend::new(s))
}

fn try_call(b: &AiframesBackend, name: &str, args: J) -> Result<J, String> {
    suite_mcp::Backend::call(b, name, &args)
}

/// An offline audio asset m1 used by c1 (0–5 s on a1) with a 10-word transcript.
fn offline_project(b: &AiframesBackend) {
    call(b, "new_project", json!({"name": "P"}));
    let words: Vec<J> = (0..10).map(|i| json!({"text": format!("w{i}"), "start": 0.5 * i as f64, "end": 0.5 * i as f64 + 0.4})).collect();
    call(
        b,
        "edit",
        json!({"ops": [
            {"op": "add_media", "id": "m1", "path": "Z:/definitely/missing/voice.wav", "kind": "audio", "info": {"duration": 20.0, "has_audio": true}},
            {"op": "add_clip", "clip": {"id": "c1", "track": "a1", "asset": "m1", "start": 0, "duration": 5}},
            {"op": "set_transcript", "asset": "m1", "transcript": {"language": "en", "words": words}},
        ]}),
    );
}

fn clips_on(b: &AiframesBackend, project: u64, track: &str) -> Vec<(String, f64, f64)> {
    let t = call(b, "get_timeline", json!({"project": project}));
    t["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|x| x["id"] == track)
        .flat_map(|x| x["clips"].as_array().unwrap().clone())
        .map(|c| (c["id"].as_str().unwrap().to_string(), c["start"].as_f64().unwrap(), c["end"].as_f64().unwrap()))
        .collect()
}

// ------------------------------------------------------------------------------- bug 6: plans

#[test]
fn plan_and_apply_report_the_same_findings_label_and_open_errors() {
    let (_, b) = backend();
    offline_project(&b);
    let op = json!([{"op": "add_marker", "time": 1.0, "name": "x"}]);
    let planned = call(&b, "edit", json!({"plan": true, "ops": op}));
    let applied = call(&b, "edit", json!({"ops": op}));
    assert_eq!(planned["open_errors"], applied["open_errors"], "plan: {planned}\napply: {applied}");
    // relink to a missing file: the plan must predict media_offline like the apply reports it.
    let op = json!([{"op": "relink_media", "asset": "m1", "path": "Z:/nope/missing2.wav"}]);
    let planned = call(&b, "edit", json!({"plan": true, "ops": op}));
    let applied = call(&b, "edit", json!({"ops": op}));
    assert_eq!(planned["new_findings"], applied["new_findings"]);
    // A tool's label is the plan's label too.
    let planned = call(&b, "delete_words", json!({"clip": "c1", "from": 2, "to": 3, "plan": true}));
    let applied = call(&b, "delete_words", json!({"clip": "c1", "from": 2, "to": 3}));
    assert_eq!(planned["label"], applied["label"]);
    let planned = call(&b, "edit", json!({"plan": true, "label": "Mine", "ops": [{"op": "add_marker", "time": 2.0}]}));
    assert_eq!(planned["label"], "Mine");
}

#[test]
fn plan_does_not_report_a_future_revision() {
    let (s, b) = backend();
    offline_project(&b);
    let current = call(&b, "history", json!({}))["revision"].as_u64().unwrap();
    let planned = call(&b, "edit", json!({"plan": true, "ops": [{"op": "add_marker", "time": 1.0}]}));
    assert_eq!(planned["revision"].as_u64().unwrap(), current);
    {
        let (_, d) = s.hub.active().unwrap();
        let mut g = suite_hub::lock(&d);
        let op = aiframes_core::Op::from_json(&json!({"op": "add_marker", "time": 2.0, "name": "user"})).unwrap();
        g.doc.apply("user", "User marker", vec![op]).unwrap();
    }
    let c = call(&b, "changes_since", json!({"revision": planned["revision"]}));
    assert_eq!(c["entries"].as_array().unwrap().len(), 1, "the person's edit is visible: {c}");
}

#[test]
fn open_project_show_false_never_switches_the_active_project() {
    let dir = scratch("show");
    let (s, b) = backend();
    let a = dir.join("a.aifr");
    let ida = call(&b, "new_project", json!({"name": "A", "path": path_str(&a)}))["project"].as_u64().unwrap();
    let idb = call(&b, "new_project", json!({"name": "B"}))["project"].as_u64().unwrap();
    call(&b, "open_project", json!({"path": path_str(&a), "show": false}));
    assert_eq!(s.hub.active().unwrap().0, idb, "already open + show:false");
    call(&b, "close_project", json!({"project": ida}));
    call(&b, "open_project", json!({"path": path_str(&a), "show": false}));
    assert_eq!(s.hub.active().unwrap().0, idb, "newly opened + show:false");
    let r = call(&b, "open_project", json!({"path": path_str(&a)}));
    assert_eq!(s.hub.active().unwrap().0, r["project"].as_u64().unwrap(), "show defaults to true");
}

// ------------------------------------------------------------------------------- bug 2 / 8: bounds

#[test]
fn contact_sheet_huge_count_is_refused_not_a_crash() {
    if std::env::var_os("AIFRAMES_ABORT_CHILD").is_some() {
        let (_, b) = backend();
        call(&b, "new_project", json!({"width": 64, "height": 64}));
        call(&b, "edit", json!({"ops": [{"op": "add_clip", "clip": {"id": "s1", "track": "v1", "start": 0, "duration": 2, "source": {"type": "solid", "color": "#ff0000"}}}]}));
        for args in [json!({"count": 1e13}), json!({"every": 1e-12}), json!({"times": vec![0.5; 100]}), json!({"cols": 1e19, "width": 1e19, "count": 2})] {
            println!("CHILD-RESULT {:?}", try_call(&b, "contact_sheet", args).map(|_| "ok"));
        }
        println!("CHILD-RETURNED");
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "contact_sheet_huge_count_is_refused_not_a_crash", "--nocapture", "--test-threads=1"])
        .env("AIFRAMES_ABORT_CHILD", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("CHILD-RETURNED"), "the process died: {:?}\n{stdout}", out.status);
    assert_eq!(stdout.matches("CHILD-RESULT Err").count(), 3, "count/every/times beyond 60 are errors; cols/width clamp: {stdout}");
}

#[test]
fn integer_arguments_are_range_checked() {
    let (_, b) = backend();
    let e = fails(&b, "new_project", json!({"width": 4294967312u64, "height": 1080}));
    assert!(e.contains("16 to 16384"), "{e}");
    assert!(try_call(&b, "new_project", json!({"width": 640.5})).is_err());
    offline_project(&b);
    let r = call(&b, "search_transcript", json!({"query": "w3", "context": 1.8446744073709552e19}));
    assert_eq!(r["total"], 1);
    let r = call(&b, "history", json!({"limit": 1e300}));
    assert!(r["entries"].is_array());
    assert!(try_call(&b, "undo", json!({"steps": 2.5})).is_err());
}

#[test]
fn delete_words_ranges_accept_whole_floats_and_reject_fractions() {
    let (_, b) = backend();
    offline_project(&b);
    let r = call(&b, "delete_words", json!({"clip": "c1", "ranges": [[2, 3.0]], "plan": true}));
    assert_eq!(r["deleted_text"], json!(["w2 w3"]), "{r}");
    let e = fails(&b, "delete_words", json!({"clip": "c1", "ranges": [[2, 3.5]], "plan": true}));
    assert!(e.contains("whole-number"), "{e}");
    let e = fails(&b, "delete_words", json!({"clip": "c1", "ranges": [[4, 2]], "plan": true}));
    assert!(e.contains("to ≥ from"), "{e}");
    let r = call(&b, "delete_words", json!({"clip": "c1", "ranges": [[5], [1, 1]], "plan": true}));
    assert_eq!(r["deleted_text"], json!(["w1", "w5"]), "{r}");
}

// ------------------------------------------------------------------------------- bug 4: file safety

#[test]
fn save_as_needs_overwrite_and_never_replaces_another_open_project() {
    let dir = scratch("saveas");
    let (_, b) = backend();
    let a = dir.join("a.aifr");
    call(&b, "new_project", json!({"name": "Project A", "path": path_str(&a)}));
    call(&b, "edit", json!({"project": 1, "ops": [{"op": "add_marker", "time": 0, "name": "A's work"}]}));
    call(&b, "save_project", json!({"project": 1}));
    call(&b, "new_project", json!({"name": "Project B"}));
    let e = fails(&b, "save_project", json!({"project": "Project B", "path": path_str(&a)}));
    assert!(e.contains("open project"), "{e}");
    let e = fails(&b, "save_project", json!({"project": "Project B", "path": path_str(&a), "overwrite": true}));
    assert!(e.contains("open project"), "even with overwrite: {e}");
    assert!(std::fs::read_to_string(&a).unwrap().contains("Project A"));
    // An existing file that is not open needs overwrite:true.
    let other = dir.join("other.aifr");
    std::fs::write(&other, "{}").unwrap();
    assert!(fails(&b, "save_project", json!({"project": "Project B", "path": path_str(&other)})).contains("overwrite"));
    call(&b, "save_project", json!({"project": "Project B", "path": path_str(&other), "overwrite": true}));
    assert!(std::fs::read_to_string(&other).unwrap().contains("Project B"));
    // Saving to its own path again is a plain save.
    call(&b, "save_project", json!({"project": "Project B", "path": path_str(&other)}));
    // new_project with overwrite:true still never replaces an open project's file.
    assert!(fails(&b, "new_project", json!({"path": path_str(&a), "overwrite": true})).contains("open project"));
}

#[test]
fn export_never_targets_project_files_or_media_and_needs_overwrite() {
    let dir = scratch("exportsafe");
    let (s, b) = backend();
    let pf = dir.join("p.aifr");
    call(&b, "new_project", json!({"name": "P", "path": path_str(&pf), "width": 320, "height": 180}));
    call(&b, "import_media", json!({"paths": [path_str(&talk())], "place": "append"}));
    call(&b, "edit", json!({"ops": [{"op": "add_clip", "clip": {"id": "s1", "track": "v1", "start": 0, "duration": 0.5, "source": {"type": "solid", "color": "#ff0000"}}}]}));
    call(&b, "save_project", json!({}));
    let protected = [pf.clone(), dir.join("p.aifr.history.jsonl"), dir.join("p.aifr.autosave"), talk(), dir.join("other.aifr")];
    for target in &protected {
        let e = fails(&b, "export", json!({"path": path_str(target), "codec": "wav", "overwrite": true}));
        assert!(e.contains("refusing"), "{}: {e}", target.display());
    }
    // The app's export action refuses them too.
    let e = actions::start_export(&s, 1, aiframes_core::ExportSettings::default(), pf.clone()).unwrap_err();
    assert!(e.contains("refusing"), "{e}");
    assert!(serde_json::from_str::<J>(&std::fs::read_to_string(&pf).unwrap()).is_ok(), "the project file is intact");
    // An unrelated existing file needs overwrite:true.
    let out = dir.join("existing.wav");
    std::fs::write(&out, b"not yet").unwrap();
    assert!(fails(&b, "export", json!({"path": path_str(&out), "codec": "wav"})).contains("overwrite"));
    let r = call(&b, "export", json!({"path": path_str(&out), "codec": "wav", "overwrite": true, "wait_seconds": 120}));
    let done = r["status"] == "done";
    if done {
        // Re-exporting a file this session exported is allowed without overwrite.
        let again = call(&b, "export", json!({"path": path_str(&out), "codec": "wav", "wait_seconds": 120}));
        assert_eq!(again["status"], "done", "{again}");
    } else {
        assert!(render_is_stub(r["job_state"]["error"].as_str().unwrap_or("")), "{r}");
    }
}

#[test]
fn export_size_presets_set_the_output_size() {
    let dir = scratch("exportsize");
    let (_, b) = backend();
    call(&b, "new_project", json!({"path": path_str(&dir.join("p.aifr")), "preset": "youtube"}));
    call(&b, "edit", json!({"ops": [{"op": "add_clip", "clip": {"id": "s1", "track": "v1", "start": 0, "duration": 0.5, "source": {"type": "solid", "color": "#2060ff"}}}]}));
    let r = call(&b, "export", json!({"preset": "shorts", "path": path_str(&dir.join("short.mp4")), "quality": "draft", "wait_seconds": 120}));
    assert_eq!(r["output_size"], json!([1080, 1920]), "{r}");
    assert!(r["note"].as_str().unwrap_or("").contains("cover"), "another aspect is explained: {r}");
    if r["status"] == "done" {
        assert_eq!((r["job_state"]["result"]["width"].as_u64(), r["job_state"]["result"]["height"].as_u64()), (Some(1080), Some(1920)), "{r}");
    }
    let r = call(&b, "export", json!({"preset": "4k", "fit": "cover", "path": path_str(&dir.join("uhd.mp4")), "wait_seconds": 0}));
    assert_eq!(r["output_size"], json!([3840, 2160]));
    let r = call(&b, "export", json!({"width": 1280, "path": path_str(&dir.join("w.mp4"))}));
    assert_eq!(r["output_size"], json!([1280, 720]), "one side keeps the aspect");
    assert!(try_call(&b, "export", json!({"fit": "zoom", "path": path_str(&dir.join("z.mp4"))})).is_err());
}

#[test]
fn image_outputs_are_relative_to_the_project_and_never_overwrite_media() {
    let dir = scratch("imgout");
    let (_, b) = backend();
    call(&b, "new_project", json!({"path": path_str(&dir.join("p.aifr")), "width": 160, "height": 90}));
    let logo = dir.join("logo.png");
    ffmpeg(&["-f", "lavfi", "-i", "color=c=red:s=64x64:d=1", "-frames:v", "1", "-update", "1", &path_str(&logo)]);
    call(&b, "import_media", json!({"paths": [path_str(&logo)], "place": "append"}));
    let e = fails(&b, "contact_sheet", json!({"count": 2, "save_path": "logo.png"}));
    assert!(e.contains("refusing"), "{e}");
    assert!(fails(&b, "contact_sheet", json!({"count": 2, "save_path": "sheet.jpg"})).contains("PNG"));
    let r = call(&b, "contact_sheet", json!({"count": 2, "save_path": "out/sheet", "image": false}));
    assert!(dir.join("out").join("sheet.png").exists(), "{r}");
    let r = call(&b, "snapshot", json!({"at": [0.5], "save_dir": "frames", "image": false}));
    assert!(dir.join("frames").read_dir().unwrap().count() == 1, "{r}");
    // readOnlyHint is false for tools that can write files.
    let defs = suite_mcp::Backend::tools(&*b, false);
    for name in ["snapshot", "contact_sheet", "filmstrip"] {
        let t = defs.iter().find(|t| t["name"] == name).unwrap();
        assert_eq!(t["annotations"]["readOnlyHint"], false, "{name}");
    }
}

#[test]
fn snapshots_report_render_warnings() {
    let (_, b) = backend();
    call(&b, "new_project", json!({"width": 160, "height": 90}));
    call(
        &b,
        "edit",
        json!({"ops": [
            {"op": "add_media", "id": "mv", "path": "Z:/gone/clip.mp4", "kind": "video", "info": {"duration": 10.0, "has_video": true, "width": 160, "height": 90, "fps": 30}},
            {"op": "add_clip", "clip": {"id": "c1", "track": "v1", "asset": "mv", "start": 0, "duration": 2}},
        ]}),
    );
    let r = call(&b, "snapshot", json!({"at": 1.0}));
    assert!(r["warnings"].as_array().is_some_and(|w| !w.is_empty()), "single frame: {r}");
    let r = call(&b, "contact_sheet", json!({"count": 2}));
    assert!(r["warnings"][0].as_str().unwrap().contains("offline"), "{r}");
}

// ------------------------------------------------------------------------------- bug 1 / 3 / 5: silences

#[test]
fn remove_silences_skips_pictures_dedupes_and_keeps_links() {
    let dir = scratch("silences");
    let (_, b) = backend();
    call(&b, "new_project", json!({"path": path_str(&dir.join("p.aifr"))}));
    let talk_v = dir.join("talk.mp4");
    ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=s=320x180:r=30:d=6", "-f", "lavfi", "-i", "aevalsrc='0.5*sin(2*PI*220*t)*between(mod(t,3),0.2,1.7)':s=48000:d=6", "-c:v", "libx264",
        "-preset", "veryfast", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest", &path_str(&talk_v),
    ]);
    let broll = dir.join("broll.mp4");
    ffmpeg(&["-f", "lavfi", "-i", "smptebars=s=320x180:r=30:d=4", "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p", &path_str(&broll)]);
    let r = call(&b, "import_media", json!({"paths": [path_str(&talk_v), path_str(&broll)], "place": "append"}));
    let (talk_c, broll_c) = (r["clips"][0].as_str().unwrap().to_string(), r["clips"][1].as_str().unwrap().to_string());
    // No-audio b-roll is skipped with a note; listing a clip twice (clip + its track) cuts it once.
    let plan = call(&b, "remove_silences", json!({"track": "v1", "clip": talk_c, "plan": true}));
    let cuts = plan["cuts"].as_array().unwrap();
    assert!(!cuts.is_empty(), "{plan}");
    assert!(cuts.iter().all(|c| c["clip"] == talk_c.as_str()), "b-roll must not be cut: {plan}");
    assert!(plan["notes"].to_string().contains(&broll_c) && plan["notes"].to_string().contains("no sound"), "{plan}");
    // Detached sound: cutting the audio cuts the picture through its leader (same length after).
    call(&b, "edit", json!({"ops": [{"op": "detach_audio", "clip": talk_c, "track": "a1"}]}));
    let audio = clips_on(&b, 1, "a1")[0].0.clone();
    let r = call(&b, "remove_silences", json!({"clip": audio, "min_length": 0.5}));
    assert!(r["notes"].to_string().contains("in sync"), "{r}");
    let total = |t: &str| clips_on(&b, 1, t).iter().filter(|(id, _, _)| *id != broll_c).map(|(_, a, z)| z - a).sum::<f64>();
    assert!((total("v1") - total("a1")).abs() < 1e-6, "picture {} vs sound {}", total("v1"), total("a1"));
    assert!(total("a1") < 5.5, "it was cut");
}

#[test]
fn silence_cut_ops_follow_core_link_rules() {
    // An L-cut sound (longer than its picture) is NOT linked: both are cut on their own.
    let mut proj: aiframes_core::Project = serde_json::from_value(json!({
        "format": 1,
        "tracks": [{"id": "v1", "kind": "video", "role": "main"}, {"id": "a1", "kind": "audio", "role": "voice"}],
        "media": [{"id": "m1", "path": "cam.mp4", "kind": "video", "info": {"duration": 20.0, "has_video": true, "has_audio": true, "width": 640, "height": 360, "fps": 30}}],
        "clips": [
            {"id": "c1", "track": "v1", "start": 0.0, "duration": 10.0, "source": {"type": "media", "asset": "m1", "in_point": 0.0}},
            {"id": "c2", "track": "a1", "start": {"with": "c1"}, "duration": 11.0, "source": {"type": "media", "asset": "m1", "in_point": 0.0}}
        ]
    }))
    .unwrap();
    let r = aiframes_core::resolve(&proj);
    let mut sil = HashMap::new();
    sil.insert("m1".to_string(), vec![[3.0, 5.0]]);
    let o = actions::SilenceOptions { noise_db: -35.0, min_len: 0.6, padding: 0.12 };
    let (ops, cuts, _) = actions::silence_cut_ops(&proj, &r, &["c1".to_string(), "c2".to_string(), "c1".to_string()], &sil, &o).unwrap();
    assert_eq!(cuts.len(), 2, "each cut once: {cuts:?}");
    aiframes_core::apply_batch(&mut proj, &ops).unwrap();
    let total = |t: &str| -> f64 { proj.clips.iter().filter(|c| c.track == t).map(|c| c.duration).sum() };
    assert!(total("a1") < 10.0, "the L-cut sound was cut too: {}", total("a1"));
    // A linked detached sound listed alone is cut through its picture.
    let mut proj: aiframes_core::Project = serde_json::from_value(json!({
        "format": 1,
        "tracks": [{"id": "v1", "kind": "video", "role": "main"}, {"id": "a1", "kind": "audio", "role": "voice"}],
        "media": [{"id": "m1", "path": "cam.mp4", "kind": "video", "info": {"duration": 20.0, "has_video": true, "has_audio": true, "width": 640, "height": 360, "fps": 30}}],
        "clips": [
            {"id": "c1", "track": "v1", "start": 0.0, "duration": 10.0, "muted": true, "source": {"type": "media", "asset": "m1", "in_point": 0.0}},
            {"id": "c2", "track": "a1", "start": {"with": "c1"}, "duration": 10.0, "source": {"type": "media", "asset": "m1", "in_point": 0.0}}
        ]
    }))
    .unwrap();
    assert_eq!(actions::link_leader(&proj, "c2"), "c1");
    let r = aiframes_core::resolve(&proj);
    let (ops, cuts, notes) = actions::silence_cut_ops(&proj, &r, &["c2".to_string()], &sil, &o).unwrap();
    assert_eq!(cuts[0].clip, "c1", "{notes:?}");
    aiframes_core::apply_batch(&mut proj, &ops).unwrap();
    let total = |t: &str| -> f64 { proj.clips.iter().filter(|c| c.track == t).map(|c| c.duration).sum() };
    assert!((total("v1") - total("a1")).abs() < 1e-6 && total("v1") < 10.0, "picture {} sound {}", total("v1"), total("a1"));
}

#[test]
fn analysis_applies_to_the_project_named_at_call_time() {
    let dir = scratch("race");
    let (s, b) = backend();
    let a = call(&b, "new_project", json!({"name": "A", "path": path_str(&dir.join("a.aifr"))}))["project"].as_u64().unwrap();
    call(&b, "import_media", json!({"project": a, "paths": [path_str(&talk())], "place": "append"}));
    let p2 = call(&b, "new_project", json!({"name": "B", "path": path_str(&dir.join("b.aifr"))}))["project"].as_u64().unwrap();
    call(&b, "import_media", json!({"project": p2, "paths": [path_str(&talk())], "place": "append"}));
    let (a_before, b_before) = (clips_on(&b, a, "a1"), clips_on(&b, p2, "a1"));
    s.hub.set_active(a);
    let da = s.hub.get(a).unwrap();
    let guard = suite_hub::lock(&da); // the window is busy with A
    let b2 = b.clone();
    let h = std::thread::spawn(move || try_call(&b2, "remove_silences", json!({"clip": "c1"})));
    std::thread::sleep(Duration::from_millis(400));
    s.hub.set_active(p2); // the person switches tabs while the AI's call waits
    drop(guard);
    let res = h.join().unwrap().unwrap();
    assert_eq!(res["project"].as_u64(), Some(a), "{res}");
    assert_ne!(clips_on(&b, a, "a1"), a_before, "A was cut");
    assert_eq!(clips_on(&b, p2, "a1"), b_before, "B untouched");
}

#[test]
fn long_analyses_run_as_jobs() {
    let dir = scratch("jobs");
    let (_, b) = backend();
    call(&b, "new_project", json!({"path": path_str(&dir.join("p.aifr"))}));
    let r = call(&b, "import_media", json!({"paths": [path_str(&talk())], "place": "append"}));
    let clip = r["clips"][0].as_str().unwrap().to_string();
    let r = call(&b, "remove_silences", json!({"clip": clip, "background": true}));
    let job = r["job"].as_u64().expect("a job id");
    assert_eq!(r["status"], "running", "{r}");
    let j = call(&b, "jobs", json!({"job": job, "wait_seconds": 60}));
    assert_eq!(j["status"], "done", "{j}");
    assert!(j["result"]["removed_seconds"].as_f64().unwrap() > 1.0, "{j}");
    assert_eq!(j["result"]["project"].as_u64(), Some(1));
    // background + wait_seconds returns the result directly, with the job id.
    let r = call(&b, "detect_silence", json!({"clip": clips_on(&b, 1, "a1")[0].0, "background": true, "wait_seconds": 60}));
    assert!(r["job"].is_u64() && r["silences"].is_array(), "{r}");
}

// ------------------------------------------------------------------------------- bug 9: media in several places

#[test]
fn media_played_twice_maps_to_every_place() {
    let (_, b) = backend();
    call(&b, "new_project", json!({}));
    call(
        &b,
        "edit",
        json!({"ops": [
            {"op": "add_media", "id": "m1", "path": path_str(&talk()), "kind": "audio", "info": {"duration": 10.0, "has_audio": true}},
            {"op": "add_clip", "clip": {"id": "first", "track": "a1", "asset": "m1", "start": 0, "duration": 5}},
            {"op": "add_clip", "clip": {"id": "again", "track": "a2", "asset": "m1", "start": 20, "duration": 5}},
            {"op": "set_transcript", "asset": "m1", "transcript": talk_transcript()},
        ]}),
    );
    let t = call(&b, "get_transcript", json!({"asset": "m1", "format": "words", "to": 2}));
    assert_eq!(t["words"][0]["also"][0]["clip"], "again", "{t}");
    assert!((t["words"][0]["also"][0]["t"].as_f64().unwrap() - 20.25).abs() < 1e-6);
    let m = call(&b, "search_transcript", json!({"query": "alpha bravo"}));
    assert_eq!(m["matches"][0]["places"].as_array().map(|v| v.len()), Some(2), "{m}");
    let e = fails(&b, "delete_words", json!({"asset": "m1", "from": 0, "to": 1}));
    assert!(e.contains("more than one place") && e.contains("again"), "{e}");
    let r = call(&b, "delete_words", json!({"clip": "again", "from": 0, "to": 1}));
    assert!(r["changed"].to_string().contains("again") || r["removed"].to_string().contains("again") || r["created"].is_array(), "{r}");
    assert_eq!(clips_on(&b, 1, "a1"), vec![("first".to_string(), 0.0, 5.0)], "the first place is untouched");
}

// ------------------------------------------------------------------------------- bug 7: CLI paths

/// `path` relative to the current directory (what a shell user types).
fn rel_to_cwd(path: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap();
    let (a, c): (Vec<_>, Vec<_>) = (path.components().collect(), cwd.components().collect());
    let common = a.iter().zip(&c).take_while(|(x, y)| x.as_os_str().to_string_lossy().eq_ignore_ascii_case(&y.as_os_str().to_string_lossy())).count();
    let mut r = PathBuf::new();
    for _ in common..c.len() {
        r.push("..");
    }
    for x in &a[common..] {
        r.push(x.as_os_str());
    }
    r
}

fn cli(args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = aiframes_mcp::cli_with(services(), &args, &mut out, &mut err);
    (code, String::from_utf8_lossy(&out).into_owned(), String::from_utf8_lossy(&err).into_owned())
}

#[test]
fn cli_paths_are_relative_to_the_current_directory() {
    let dir = scratch("clipaths");
    std::fs::create_dir_all(dir.join("proj")).unwrap();
    let proj = path_str(&dir.join("proj").join("p.aifr"));
    assert_eq!(cli(&["new", &proj, "--width", "64", "--height", "64"]).0, 0);
    let media = dir.join("clip.wav");
    ffmpeg(&["-f", "lavfi", "-i", "sine=f=440:r=48000:d=1", "-c:a", "pcm_s16le", &path_str(&media)]);
    let (code, out, err) = cli(&["import", &proj, &rel_to_cwd(&media).to_string_lossy()]);
    assert_eq!(code, 0, "{out}{err}");
    let ops = dir.join("ops.json");
    std::fs::write(&ops, r##"[{"op":"add_clip","clip":{"id":"s1","track":"v1","start":0,"duration":0.5,"source":{"type":"solid","color":"#ff0000"}}}]"##).unwrap();
    assert_eq!(cli(&["apply", &proj, &path_str(&ops)]).0, 0);
    let out_rel = rel_to_cwd(&dir.join("render.mp4"));
    let (code, _, err) = cli(&["render", &proj, "-o", &out_rel.to_string_lossy(), "--preset", "draft"]);
    let line = err.lines().find(|l| l.starts_with("Rendering")).unwrap_or("").replace('\\', "/");
    assert!(line.to_lowercase().ends_with(&path_str(&dir.join("render.mp4")).to_lowercase()), "{line:?} vs {:?}", path_str(&dir.join("render.mp4")));
    if code == 0 {
        assert_eq!(cli(&["render", &proj, "-o", &out_rel.to_string_lossy(), "--preset", "draft"]).0, 1, "an existing file needs --force");
        assert_eq!(cli(&["render", &proj, "-o", &out_rel.to_string_lossy(), "--preset", "draft", "--force"]).0, 0);
    }
    let sheet_rel = rel_to_cwd(&dir.join("sheet.png"));
    assert_eq!(cli(&["contact-sheet", &proj, "--count", "2", "-o", &sheet_rel.to_string_lossy()]).0, 0);
    assert!(dir.join("sheet.png").exists() && !dir.join("proj").join("sheet.png").exists());
}

// ------------------------------------------------------------------------------- APIs

#[test]
fn clip_params_and_param_paths_are_exposed() {
    let (_, b) = backend();
    call(&b, "new_project", json!({}));
    call(&b, "edit", json!({"ops": [{"op": "add_clip", "clip": {"id": "t", "track": "v2", "text": "Hi", "duration": 2}}, {"op": "add_effect", "clip": "t", "effect": "blur"}]}));
    let c = call(&b, "get_clip", json!({"clip": "t"}));
    let paths = c["param_paths"].to_string();
    assert!(paths.contains("transform.scale") && paths.contains("effects.0.radius"), "{paths}");
    let ops = call(&b, "list_ops", json!({}));
    assert!(ops["clip_params"].as_array().unwrap().iter().any(|p| p["name"] == "opacity"));
    assert!(ops["ducking_params"].as_array().unwrap().iter().any(|p| p["name"] == "amount_db"));
    assert!(ops["ops"].to_string().contains("move_clips"));
}

#[test]
fn analyze_audio_uses_complete_audio_and_per_track_mixes() {
    let dir = scratch("loud");
    let (_, b) = backend();
    call(&b, "new_project", json!({"path": path_str(&dir.join("p.aifr"))}));
    call(&b, "import_media", json!({"paths": [path_str(&talk())], "place": "append"}));
    call(&b, "import_media", json!({"paths": [path_str(&music())], "place": "none"}));
    let m = call(&b, "list_media", json!({}));
    let music_id = m["media"].as_array().unwrap().iter().find(|x| x["file"] == "music.wav").unwrap()["id"].as_str().unwrap().to_string();
    call(&b, "edit", json!({"ops": [{"op": "add_clip", "clip": {"track": "a2", "asset": music_id, "start": 0, "volume": 0.5}}]}));
    let r = call(&b, "analyze_audio", json!({}));
    let lufs = r["loudness"]["integrated_lufs"].as_f64().unwrap();
    assert!(lufs > -40.0, "the mix has sound right away (complete audio): {r}");
    let tracks = r["tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), 2, "{r}");
    assert!(tracks.iter().all(|t| t["integrated_lufs"].as_f64().unwrap() > -60.0), "{r}");
    assert!(r["voice_over_music_lu"].is_number(), "{r}");
    let one = call(&b, "analyze_audio", json!({"track": "a2"}));
    assert_eq!(one["what"], "track a2");
}
