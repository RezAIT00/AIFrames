//! A caption leaves with its voice: it never runs past the end of the clip its words are heard in.
mod common;
use common::*;
use serde_json::json;

#[test]
fn a_caption_ends_with_its_clip_on_a_cut_to_someone_else() {
    let mut p = base();
    // m1's words sit at i + 1.0 .. i + 1.5. Clip A plays source 0–5.2 (its last word, 5.0–5.5, is cut off
    // by the clip's end); clip B plays from source 20.
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"a","track":"a1","asset":"m1","start":0.0,"duration":5.2}}));
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"b","track":"a1","asset":"m1","in_point":20.0,"start":{"after":"a"},"duration":4.0}}));
    ap(&mut p, json!({"op":"add_track","id":"v9","kind":"video","role":"captions"}));
    ap(&mut p, json!({"op":"generate_captions","clips":["a","b"],"track":"v9","max_words":1}));
    let a_end = span(&p, "a").unwrap().1;
    let caps: Vec<(f64, f64)> = p.clips.iter().filter(|c| c.track == "v9").map(|c| span(&p, &c.id).unwrap()).collect();
    assert!(!caps.is_empty());
    for (s, e) in &caps {
        // A caption that starts inside clip a ends by the time a does.
        if *s < a_end - 1e-6 {
            assert!(*e <= a_end + 1e-6, "caption {s}–{e} outlasts clip a (ends {a_end})");
        }
    }
}
