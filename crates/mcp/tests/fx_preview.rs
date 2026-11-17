//! (Added) `preview_effects`: effects and transitions previewed on scratch copies, never applied.

mod common;

use aiframes_mcp::AiframesBackend;
use common::*;
use serde_json::{json, Value as J};
use suite_mcp::IMAGE_KEY;

fn kinds(v: &J, what: &str) -> Vec<String> {
    v["tiles"].as_array().unwrap().iter().filter(|t| t["what"] == what).map(|t| t["kind"].as_str().unwrap().to_string()).collect()
}

#[test]
fn previews_every_effect_and_transition_without_editing() {
    let dir = scratch("fxpreview");
    let s = services();
    let b = AiframesBackend::new(s.clone());

    // Without a project: the sample picture, every effect and transition, plus the original.
    let all = call(&b, "preview_effects", json!({"width": 96, "image": false}));
    assert_eq!(kinds(&all, "effect").len(), aiframes_core::EFFECTS.len(), "{all}");
    assert_eq!(kinds(&all, "transition").len(), aiframes_core::TRANSITIONS.len());
    assert!(all["on"].as_str().unwrap().contains("sample"), "{all}");
    assert!(all.get("failed").is_none(), "every effect and transition renders at its defaults: {}", all["failed"]);
    assert!(all.get(IMAGE_KEY).is_none(), "image: false");

    // A project with two different shots: c1 (testsrc2) | c2 (smptebars), a cut at 2 s.
    let path = dir.join("fx.aifr");
    call(&b, "new_project", json!({"preset": "720p", "path": path_str(&path)}));
    call(&b, "import_media", json!({"paths": [path_str(&cut_video())], "place": "append"}));
    let tl = call(&b, "get_timeline", json!({}));
    let c1 = tl["tracks"].as_array().unwrap().iter().find(|t| t["id"] == "v1").unwrap()["clips"][0]["id"].as_str().unwrap().to_string();
    call(&b, "edit", json!({"ops": [{"op": "split_clip", "clip": c1, "at": 2.0, "new_id": "c2"}]}));
    let before = call(&b, "get_timeline", json!({}));
    let rev = before["revision"].clone();

    let out = dir.join("sheet.png");
    let r = call(&b, "preview_effects", json!({"clip": c1, "at": 1.0, "effects": ["invert", "blur"], "transitions": "wipe_left", "width": 160, "cols": 2, "save_path": path_str(&out)}));
    assert_eq!(kinds(&r, "effect"), ["invert", "blur"]);
    assert_eq!(kinds(&r, "transition"), ["wipe_left"]);
    assert!(r["on"].as_str().unwrap().contains(&c1), "{r}");
    assert!(r.get(IMAGE_KEY).is_some() && out.exists(), "{r}");
    // The transition is shown on the clip's own cut (not the sample), 35 % of the way through.
    let tr = r["tiles"].as_array().unwrap().iter().find(|t| t["what"] == "transition").unwrap();
    assert!(tr.get("on").is_none(), "{tr}");
    let at = tr["at"].as_f64().unwrap();
    assert!(at > 1.7 && at < 2.0, "35 % into a 0.5 s transition centred on 2.0: {at}");
    // Nothing changed.
    let after = call(&b, "get_timeline", json!({}));
    assert_eq!(after["revision"], rev);
    assert_eq!(after["tracks"], before["tracks"]);
    let hist = call(&b, "history", json!({}));
    assert!(!hist.to_string().contains("Invert"), "{hist}");

    // Loud failures for bad arguments.
    assert!(fails(&b, "preview_effects", json!({"effects": ["blurr"]})).contains("blur"), "did-you-mean");
    assert!(fails(&b, "preview_effects", json!({"clip": "nope"})).contains("unknown clip"));
    assert!(fails(&b, "preview_effects", json!({"effects": false, "transitions": false, "original": false})).contains("nothing to preview"));
}
