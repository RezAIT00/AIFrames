//! Pieces of a split clip keep a readable id: `vo` → `vo-2` → `vo-3`, generic ids stay `c<n>`.
mod common;
use common::*;
use serde_json::json;

#[test]
fn split_pieces_keep_the_clip_name() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"vo","track":"a1","asset":"m1","start":0.0,"duration":20.0}}));
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c7","track":"v1","asset":"mv","start":0.0,"duration":20.0}}));

    let o = ap(&mut p, json!({"op":"split_clip","clip":"vo","at":5.0}));
    assert_eq!(o.created, vec!["vo-2".to_string()], "{o:?}");
    let o = ap(&mut p, json!({"op":"split_clip","clip":"vo-2","at":10.0}));
    assert_eq!(o.created, vec!["vo-3".to_string()], "{o:?}");
    // A deleted piece's number is not handed out again.
    ap(&mut p, json!({"op":"delete_clip","clip":"vo-3","ripple":false}));
    let o = ap(&mut p, json!({"op":"split_clip","clip":"vo","at":2.0}));
    assert_eq!(o.created, vec!["vo-4".to_string()], "{o:?}");
    // Generic ids keep the c<n> scheme.
    let o = ap(&mut p, json!({"op":"split_clip","clip":"c7","at":5.0}));
    assert!(o.created.len() == 1 && o.created[0].starts_with('c') && !o.created[0].contains('-'), "{o:?}");
    // Text-based cuts split the same way.
    let o = ap(&mut p, json!({"op":"delete_words","clip":"vo","from":0,"to":0}));
    assert!(o.created.iter().all(|c| c.starts_with("vo-")), "{o:?}");
}
