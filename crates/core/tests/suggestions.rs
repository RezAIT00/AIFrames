//! Suggestions: edits proposed for review, applied as one step or dismissed.
mod common;
use aiframes_core::document::{canonical_json, parse_project};
use aiframes_core::{apply_batch, suggestion_ops};
use common::*;
use serde_json::json;

fn with_clip() -> aiframes_core::Project {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":5.0}}));
    p
}

#[test]
fn a_suggestion_waits_then_applies_as_one_step() {
    let mut p = with_clip();
    let o = ap(&mut p, json!({"op":"add_suggestion","label":"Trim the opening","reason":"The first second is dead air","at":0.5,"ops":[{"op":"trim_clip","clip":"c1","edge":"start","delta":1.0}]}));
    let id = o.created[0].clone();
    assert!(id.starts_with("sg"), "{id}");
    // Nothing changed yet.
    assert_eq!(span(&p, "c1"), Some((0.0, 5.0)));
    assert_eq!(p.suggestions.len(), 1);
    assert_eq!(p.suggestions[0].reason, "The first second is dead air");
    // It survives save and load.
    let q = parse_project(&canonical_json(&p)).unwrap();
    assert_eq!(q.suggestions, p.suggestions);
    // Applying runs its ops and removes it.
    let ops = suggestion_ops(&p, &id).unwrap();
    apply_batch(&mut p, &ops).unwrap();
    assert_eq!(span(&p, "c1").map(|s| s.1 - s.0), Some(4.0), "{:?}", span(&p, "c1"));
    assert!(p.suggestions.is_empty());
    // Gone now.
    assert!(suggestion_ops(&p, &id).unwrap_err().to_string().contains("none are pending"));
}

#[test]
fn dismissing_leaves_the_timeline_alone_and_bad_suggestions_are_refused() {
    let mut p = with_clip();
    ap(&mut p, json!({"op":"add_suggestion","id":"sg7","label":"Mark the hook","ops":[{"op":"add_marker","time":1.0,"name":"Hook"}]}));
    let before = p.clips.clone();
    ap(&mut p, json!({"op":"remove_suggestion","suggestion":"sg7"}));
    assert!(p.suggestions.is_empty() && p.markers.is_empty());
    assert_eq!(p.clips, before);

    let e = try_ap(&mut p, json!({"op":"add_suggestion","label":"Move a ghost","ops":[{"op":"move_clip","clip":"nope","start":1.0}]})).unwrap_err().to_string();
    assert!(e.contains("do not apply") && e.contains("nope"), "{e}");
    let e = try_ap(&mut p, json!({"op":"add_suggestion","label":"   ","ops":[{"op":"add_marker","time":1.0}]})).unwrap_err().to_string();
    assert!(e.contains("label"), "{e}");
    let e = try_ap(&mut p, json!({"op":"add_suggestion","label":"Nest","ops":[{"op":"add_suggestion","label":"x","ops":[{"op":"add_marker","time":1.0}]}]})).unwrap_err().to_string();
    assert!(e.contains("do not nest"), "{e}");
    let e = try_ap(&mut p, json!({"op":"remove_suggestion","suggestion":"sg99"})).unwrap_err().to_string();
    assert!(e.contains("sg99"), "{e}");
    assert!(p.suggestions.is_empty());
}

#[test]
fn a_suggestion_overtaken_by_later_edits_fails_to_apply_and_stays() {
    let mut p = with_clip();
    let o = ap(&mut p, json!({"op":"add_suggestion","label":"Trim c1","ops":[{"op":"trim_clip","clip":"c1","edge":"end","delta":-1.0}]}));
    let id = o.created[0].clone();
    ap(&mut p, json!({"op":"delete_clip","clip":"c1","ripple":false}));
    let ops = suggestion_ops(&p, &id).unwrap();
    assert!(apply_batch(&mut p, &ops).is_err());
    assert_eq!(p.suggestions.len(), 1, "a failed apply leaves it for the person to dismiss");
}
