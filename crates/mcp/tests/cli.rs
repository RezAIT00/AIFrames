//! The headless CLI through `cli_with` in temp folders.

mod common;

use common::*;
use serde_json::Value as J;
use std::path::Path;

struct Run {
    code: i32,
    out: String,
    err: String,
}

fn run(args: &[&str]) -> Run {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = aiframes_mcp::cli_with(services(), &args, &mut out, &mut err);
    Run { code, out: String::from_utf8_lossy(&out).into_owned(), err: String::from_utf8_lossy(&err).into_owned() }
}

fn ok(args: &[&str]) -> Run {
    let r = run(args);
    assert_eq!(r.code, 0, "aiframes {args:?} exited {}\nstdout: {}\nstderr: {}", r.code, r.out, r.err);
    r
}

fn s(p: &Path) -> String {
    path_str(p)
}

#[test]
fn cli_commands() {
    let dir = scratch("cli");
    let proj = s(&dir.join("cli.aifr"));
    let r = ok(&["new", &proj, "--preset", "shorts", "--name", "CLI test"]);
    assert!(r.out.contains("1080x1920"), "{}", r.out);
    assert_eq!(run(&["new", &proj]).code, 1, "refuses to overwrite without --force");

    let r = ok(&["import", &proj, &s(&video()), &s(&talk()), "--append"]);
    assert!(r.out.contains("+ m1") && r.out.contains("Appended clips"), "{}", r.out);

    let r = ok(&["describe", &proj]);
    assert!(r.out.starts_with("Project \"CLI test\""), "{}", r.out);
    assert!(r.out.contains("Findings:"));
    let r = ok(&["timeline", &proj, "--json"]);
    let tl: J = serde_json::from_str(&r.out).unwrap();
    assert_eq!(tl["settings"]["height"], 1920);

    let ops = dir.join("ops.json");
    std::fs::write(&ops, r#"[{"op":"add_clip","clip":{"id":"t1","track":"v2","text":"Hi","start":{"with":"c1","offset":0.5},"duration":1.5}},{"op":"add_marker","time":0,"name":"Start","kind":"chapter"}]"#).unwrap();
    let r = ok(&["apply", &proj, &s(&ops), "--plan"]);
    assert!(r.out.starts_with("Plan (nothing changed)"), "{}", r.out);
    let r = ok(&["apply", &proj, &s(&ops), "--author", "tester"]);
    assert!(r.out.starts_with("Applied r2"), "{}", r.out);
    let bad = dir.join("bad.json");
    std::fs::write(&bad, r#"{"ops":[{"op":"slpit_clip","clip":"c1","at":1}]}"#).unwrap();
    let r = run(&["apply", &proj, &s(&bad)]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("split_clip"), "{}", r.err);

    let r = ok(&["history", &proj]);
    assert!(r.out.contains("tester") && r.out.contains("cli"), "{}", r.out);
    let r = run(&["validate", &proj]);
    assert!(r.out.contains("error(s)"), "{}", r.out);
    let r = run(&["validate", &proj, "--json"]);
    let v: J = serde_json::from_str(&r.out).unwrap();
    assert_eq!(r.code, if v["errors"].as_u64().unwrap() > 0 { 1 } else { 0 });
    let r = run(&["chapters", &proj]);
    assert_eq!(r.code, 1, "only one chapter: {}", r.out);
    assert!(r.out.starts_with("0:00 Start"), "{}", r.out);

    let frames = dir.join("frames");
    let r = ok(&["snapshot", &proj, "--at", "0.5,1", "-o", &s(&frames), "--scale", "0.25"]);
    assert_eq!(r.out.lines().count(), 2, "{}", r.out);
    assert_eq!(std::fs::read_dir(&frames).unwrap().count(), 2);
    let sheet = dir.join("sheet.png");
    ok(&["contact-sheet", &proj, "--count", "4", "-o", &s(&sheet)]);
    assert!(sheet.exists());

    let out = dir.join("render.mp4");
    let r = run(&["render", &proj, "-o", &s(&out), "--preset", "draft", "--range", "0,1"]);
    if r.code == 0 {
        assert!(out.exists(), "{}", r.out);
        println!("render ran against the real renderer: {}", r.out);
    } else {
        assert!(render_is_stub(&r.err), "render failed: {}", r.err);
        println!("render crate is still a stub: {}", r.err.trim());
    }

    assert!(ok(&["ops"]).out.contains("split_clip"));
    assert!(ok(&["help"]).out.contains("aiframes demo"));
    assert_eq!(run(&["bogus"]).code, 2);
    assert_eq!(run(&["describe"]).code, 2, "missing project is a usage error");
    assert_eq!(run(&["describe", &s(&dir.join("missing.aifr"))]).code, 1);
}

#[test]
fn demo_project_validates_and_exercises_features() {
    let dir = scratch("demo");
    let r = ok(&["demo", &s(&dir), "--json"]);
    let d: J = serde_json::from_str(&r.out).unwrap();
    assert_eq!(d["errors"].as_array().unwrap().len(), 0, "{d}");
    assert!(d["clips"].as_u64().unwrap() >= 12, "{d}");
    assert!((d["duration"].as_f64().unwrap() - 15.0).abs() < 1e-6, "{d}");
    let proj = d["project"].as_str().unwrap().to_string();
    let text = std::fs::read_to_string(&proj).unwrap();
    for needle in ["\"after\"", "\"with\"", "\"word\"", "pop_words", "\"keys\"", "crossfade", "dip_black", "vignette", "\"highlight\"", "\"ducking\"", "\"chapter\"", "arrow"] {
        assert!(text.contains(needle), "demo project lacks {needle}");
    }
    let r = ok(&["describe", &proj]);
    assert!(r.out.contains("AIFrames demo"));
    let r = run(&["chapters", &proj, "--json"]);
    let c: J = serde_json::from_str(&r.out).unwrap();
    assert_eq!(c["chapters"].as_array().unwrap().len(), 3);
    // Running it again reuses the media and rewrites the project.
    ok(&["demo", &s(&dir)]);
}
