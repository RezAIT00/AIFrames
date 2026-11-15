//! `base_revision`: an edit planned against an older revision is refused when what the person
//! changed in the meantime affects it, and applied (with a note) when it does not.

mod common;

use aiframes_mcp::{AiframesBackend, Services};
use common::*;
use serde_json::{json, Value as J};
use std::sync::Arc;

/// Three chained 4 s clips of an offline audio asset on a1: c1 0–4, c2 4–8, c3 8–12.
/// Returns the revision an AI would have read.
fn project() -> (Arc<Services>, Arc<AiframesBackend>, u64) {
    let s = services();
    let b = AiframesBackend::new(s.clone());
    call(&b, "new_project", json!({"name": "P"}));
    let r = call(
        &b,
        "edit",
        json!({"ops": [
            {"op": "add_media", "id": "m1", "path": "Z:/definitely/missing/voice.wav", "kind": "audio", "info": {"duration": 60.0, "has_audio": true}},
            {"op": "add_clip", "clip": {"id": "c1", "track": "a1", "asset": "m1", "start": 0, "duration": 4}},
            {"op": "append_clip", "track": "a1", "clip": {"id": "c2", "asset": "m1", "in_point": 10, "duration": 4}},
            {"op": "append_clip", "track": "a1", "clip": {"id": "c3", "asset": "m1", "in_point": 20, "duration": 4}},
        ]}),
    );
    let rev = r["revision"].as_u64().unwrap();
    (s, b, rev)
}

/// An edit by the person in the window.
fn user_edit(s: &Services, op: J) {
    let (_, d) = s.hub.active().unwrap();
    let mut g = suite_hub::lock(&d);
    let op = aiframes_core::Op::from_json(&op).unwrap();
    g.doc.apply("user", "", vec![op]).unwrap();
}

fn state(b: &AiframesBackend) -> (u64, J) {
    let t = call(b, "get_timeline", json!({}));
    (call(b, "history", json!({}))["revision"].as_u64().unwrap(), t["tracks"].clone())
}

const SPLIT_C3: fn() -> J = || json!([{"op": "split_clip", "clip": "c3", "at": 10.0}]);

#[test]
fn without_base_revision_nothing_is_checked() {
    let (s, b, _) = project();
    user_edit(&s, json!({"op": "trim_clip", "clip": "c1", "edge": "end", "delta": -1.0}));
    let r = call(&b, "edit", json!({"ops": SPLIT_C3()}));
    assert!(r.get("applied_over").is_none(), "{r}");
    assert_eq!(r["created"].as_array().unwrap().len(), 1, "applied as before: {r}");
}

#[test]
fn a_matching_base_revision_applies_normally() {
    let (_, b, rev) = project();
    let r = call(&b, "edit", json!({"ops": SPLIT_C3(), "base_revision": rev}));
    assert_eq!(r["revision"].as_u64().unwrap(), rev + 1);
    assert!(r.get("applied_over").is_none(), "{r}");
    // The receipt's revision is the next base.
    let r2 = call(&b, "edit", json!({"ops": [{"op": "add_marker", "time": 1.0}], "base_revision": rev + 1}));
    assert!(r2.get("applied_over").is_none(), "{r2}");
    // A revision the project never had is a mistake, not a stale edit.
    let e = fails(&b, "edit", json!({"ops": [{"op": "add_marker", "time": 2.0}], "base_revision": rev + 50}));
    assert!(e.contains("ahead of the project"), "{e}");
}

#[test]
fn a_stale_edit_with_absolute_seconds_is_refused_and_changes_nothing() {
    let (s, b, rev) = project();
    // The person shortens c1 by a second: c3 now plays 7–11, so "10.0" is another moment of it.
    user_edit(&s, json!({"op": "trim_clip", "clip": "c1", "edge": "end", "delta": -1.0}));
    let before = state(&b);
    let e = fails(&b, "edit", json!({"ops": SPLIT_C3(), "base_revision": rev}));
    assert!(e.contains("stale edit") && e.contains("Nothing was changed"), "{e}");
    assert!(e.contains(&format!("base_revision {rev}")) && e.contains(&format!("now at revision {}", rev + 1)), "{e}");
    assert!(e.contains(&format!("r{} by user", rev + 1)) && e.contains("changed c1"), "what changed, by whom: {e}");
    assert!(e.contains("op #0 (split_clip): at 10 is absolute timeline seconds"), "which op and why: {e}");
    assert!(e.contains(&format!("changes_since {{\"revision\": {rev}}}")) && e.contains(&format!("base_revision: {}", rev + 1)), "the fix: {e}");
    assert_eq!(state(&b), before, "nothing changed");

    // A dry run and a suggestion are checked the same way.
    let e = fails(&b, "edit", json!({"ops": SPLIT_C3(), "base_revision": rev, "plan": true}));
    assert!(e.contains("stale edit"), "{e}");
    let e = fails(&b, "suggest", json!({"label": "Split c3", "ops": SPLIT_C3(), "base_revision": rev, "show": false}));
    assert!(e.contains("stale edit") && e.contains("op #0 (add_suggestion)"), "{e}");
    assert_eq!(state(&b), before, "still nothing changed");

    // Re-read and resend against the current revision: applies.
    let r = call(&b, "edit", json!({"ops": [{"op": "split_clip", "clip": "c3", "at": 9.0}], "base_revision": rev + 1}));
    assert!(r.get("applied_over").is_none(), "{r}");
}

#[test]
fn a_stale_edit_on_a_clip_the_person_changed_is_refused() {
    let (s, b, rev) = project();
    user_edit(&s, json!({"op": "set_param", "clip": "c2", "path": "volume", "value": 0.5}));
    let before = state(&b);
    let ops = json!([
        {"op": "set_param", "clip": "c3", "path": "volume", "value": 0.8},
        {"op": "set_param", "clip": "c2", "path": "volume", "value": 0.9},
    ]);
    let e = fails(&b, "edit", json!({"ops": ops, "base_revision": rev}));
    assert!(e.contains("stale edit"), "{e}");
    assert!(e.contains(&format!("op #1 (set_param): it references c2, which r{} by user changed", rev + 1)), "{e}");
    assert!(!e.contains("op #0"), "only the affected op is named: {e}");
    assert_eq!(state(&b), before, "the whole batch is refused");
    // An anchor on the changed clip counts as a reference too.
    let e = fails(&b, "edit", json!({"ops": [{"op": "add_marker", "time": {"with": "c2"}}], "base_revision": rev}));
    assert!(e.contains("it references c2"), "{e}");
}

#[test]
fn a_stale_edit_the_changes_do_not_affect_applies_with_a_note() {
    let (s, b, rev) = project();
    // An unrelated change and a retime: id- and anchor-based ops follow both.
    user_edit(&s, json!({"op": "set_param", "clip": "c2", "path": "volume", "value": 0.5}));
    user_edit(&s, json!({"op": "trim_clip", "clip": "c1", "edge": "end", "delta": -1.0}));
    let ops = json!([
        {"op": "set_param", "clip": "c3", "path": "volume", "value": 0.8},
        {"op": "add_keyframe", "clip": "c3", "path": "volume", "t": 1.0, "v": 0.2},
        {"op": "add_marker", "time": {"with": "c3"}, "name": "here"},
    ]);
    let planned = call(&b, "edit", json!({"ops": ops, "base_revision": rev, "plan": true}));
    assert_eq!(planned["applied_over"]["newer_changes"], 2, "{planned}");
    let r = call(&b, "edit", json!({"ops": ops, "base_revision": rev}));
    assert_eq!(r["revision"].as_u64().unwrap(), rev + 3);
    let over = &r["applied_over"];
    assert_eq!(over["base_revision"].as_u64().unwrap(), rev);
    assert_eq!(over["newer_changes"], 2);
    assert_eq!(over["by_author"]["user"], 2);
    assert!(over["note"].as_str().unwrap().contains(&format!("applied on top of 2 newer change(s) since revision {rev} (by user)")), "{r}");
    let marker = call(&b, "get_timeline", json!({}))["markers"][0]["time"].as_f64().unwrap();
    assert!((marker - 7.0).abs() < 1e-6, "the anchored marker landed on c3 where it is now: {marker}");
}
