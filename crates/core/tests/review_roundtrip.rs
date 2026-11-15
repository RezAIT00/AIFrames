//! Regression tests ported from the independent review (round 3): save/open is lossless.
mod common;
use aiframes_core::document::{canonical_json, parse_project};
use common::*;
use serde_json::json;

fn first_diff(a: &serde_json::Value, b: &serde_json::Value, path: &str) -> Option<String> {
    match (a, b) {
        (serde_json::Value::Object(x), serde_json::Value::Object(y)) => {
            for k in x.keys().chain(y.keys()) {
                if let Some(d) = first_diff(x.get(k).unwrap_or(&serde_json::Value::Null), y.get(k).unwrap_or(&serde_json::Value::Null), &format!("{path}.{k}")) {
                    return Some(d);
                }
            }
            None
        }
        (serde_json::Value::Array(x), serde_json::Value::Array(y)) if x.len() == y.len() => {
            x.iter().zip(y).enumerate().find_map(|(i, (p, q))| first_diff(p, q, &format!("{path}[{i}]")))
        }
        _ if a == b => None,
        _ => Some(format!("{path}: {a} vs {b}")),
    }
}

#[test]
fn project_roundtrip_after_add_marker() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":5.0}}));
    ap(&mut p, json!({"op":"add_marker","id":"mk1","time":{"after":"c1","offset":-0.5}}));
    let text = canonical_json(&p);
    let q = parse_project(&text).unwrap();
    if q != p {
        // find what differs (structurally, via Debug)
        let a = format!("{p:#?}");
        let b = format!("{q:#?}");
        let d = a.lines().zip(b.lines()).enumerate().find(|(_, (x, y))| x != y);
        panic!("round trip differs at debug line {:?}", d);
    }
    let _ = first_diff;
}

#[test]
fn project_roundtrip_stepwise() {
    let steps = vec![
        json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":5.0}}),
        json!({"op":"append_clip","track":"v1","clip":{"id":"c2","asset":"mv","in_point":10.0,"duration":6.0}}),
        json!({"op":"append_clip","track":"v1","clip":{"id":"c3","asset":"mv","in_point":30.0,"duration":4.0,"speed":2.0}}),
        json!({"op":"add_clip","clip":{"id":"vo","track":"a1","asset":"m1","start":1.0,"duration":12.0,"in_point":0.5}}),
        json!({"op":"add_clip","clip":{"id":"t1","track":"v2","text":"title","start":{"with":"c2","offset":0.5},"duration":2.0,"fade_in":0.5,"fade_out":0.5,
            "opacity":{"keys":[{"t":0,"v":0},{"t":1,"v":1,"ease":"out"}]}}}),
        json!({"op":"add_clip","clip":{"id":"t2","track":"v2","text":"word","start":{"word":{"clip":"vo","index":8}},"duration":1.0}}),
        json!({"op":"add_marker","id":"mk1","time":{"after":"c1","offset":-0.5},"name":"m"}),
        json!({"op":"add_transition","from":"c1","to":"c2","kind":"crossfade","duration":0.5}),
    ];
    let mut p = base();
    for s in steps {
        ap(&mut p, s.clone());
        let text = canonical_json(&p);
        let q = parse_project(&text).unwrap();
        if q != p {
            let a = format!("{p:#?}");
            let b = format!("{q:#?}");
            let d = a.lines().zip(b.lines()).enumerate().find(|(_, (x, y))| x != y);
            panic!("after {s}: round trip differs at debug line {:?}", d);
        }
    }
}

#[test]
fn reopened_project_resaves_differently() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":5.0}}));
    ap(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"end","delta":-1.36}));
    let d = p.clips[0].duration;
    let text1 = canonical_json(&p);
    let q = parse_project(&text1).unwrap();
    let text2 = canonical_json(&q);
    println!("duration in memory {d:?}, after reopen {:?}", q.clips[0].duration);
    assert_eq!(q, p, "project changed by save+open");
    assert_eq!(text1, text2, "re-saving an unchanged reopened project rewrites numbers");
}
