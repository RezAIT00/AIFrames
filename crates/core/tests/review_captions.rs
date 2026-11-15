//! Regression tests ported from the independent review (round 3): captions stay in sync with the voice.
mod common;
use aiframes_core::*;
use common::*;
use serde_json::json;

/// For every caption: each highlight window's start must be when its word is heard on the timeline.
fn check_captions(p: &Project, tag: &str) -> Vec<String> {
    let r = resolve(p);
    let words = &p.transcripts["m1"].words;
    let vo: Vec<&Clip> = p.clips.iter().filter(|c| c.track == "a1").collect();
    // timeline time of each word start (first clip that plays it)
    let heard = |i: usize| -> Option<f64> {
        vo.iter().find_map(|c| {
            let l = local_time_of_source(c, words[i].start)?;
            Some(r.span(&c.id)?.start + l)
        })
    };
    let mut bad = Vec::new();
    for c in p.clips.iter().filter(|c| c.tags.iter().any(|t| t == "caption")) {
        let Source::Text(t) = &c.source else { continue };
        let Some(h) = &t.highlight else { continue };
        let Some(s) = r.span(&c.id) else {
            bad.push(format!("{tag}: {} unresolved", c.id));
            continue;
        };
        for (k, word) in t.text.split_whitespace().enumerate() {
            let idx: usize = word.trim_start_matches('w').trim_end_matches('.').parse().unwrap();
            let Some(win) = h.words.get(k) else {
                bad.push(format!("{tag}: {} has no window for {word}", c.id));
                continue;
            };
            match heard(idx) {
                Some(tt) if (s.start + win[0] - tt).abs() < 1e-3 => {}
                other => bad.push(format!("{tag}: {} word {word}: highlighted at {:.3}, heard at {:?}", c.id, s.start + win[0], other)),
            }
        }
    }
    bad
}

#[test]
fn captions_follow_words_through_in_point_speed_and_delete_words() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"vo","track":"a1","asset":"m1","start":2.0,"in_point":3.3,"duration":12.0,"speed":1.5}}));
    ap(&mut p, json!({"op":"generate_captions","clips":["vo"],"track":"v2","max_words":3}));
    let mut bad = check_captions(&p, "generate");
    let o = ap(&mut p, json!({"op":"delete_words","clip":"vo","from":8,"to":9}));
    println!("{}", o.summary);
    bad.extend(check_captions(&p, "delete 8-9"));
    let o = ap(&mut p, json!({"op":"delete_words","clip":"vo","from":13,"to":13}));
    println!("{}", o.summary);
    bad.extend(check_captions(&p, "delete 13"));
    for b in &bad {
        println!("{b}");
    }
    assert!(bad.is_empty());
}

#[test]
fn captions_after_cut_source_keep_highlight_in_sync() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"vo","track":"a1","asset":"m1","start":0.0,"duration":12.0}}));
    ap(&mut p, json!({"op":"generate_captions","clips":["vo"],"track":"v2","max_words":4,"pause":5.0}));
    // cut a silence between words 5 (6.0-6.5) and 6 (7.0-7.5): inside one caption line
    let o = ap(&mut p, json!({"op":"cut_source","clip":"vo","from":6.55,"to":6.95}));
    println!("{}", o.summary);
    let bad = check_captions(&p, "cut_source");
    for b in &bad {
        println!("{b}");
    }
    assert!(bad.is_empty());
}

#[test]
fn reverse_trim_keeps_content_locked() {
    // reverse clip: trimming the start must not change what is shown at a given timeline time
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":6.0,"in_point":10.0,"reverse":true,
        "speed":{"keys":[{"t":0,"v":1},{"t":6,"v":2,"ease":"linear"}]}}}));
    let before = clip(&p, "c1").clone();
    ap(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"start","delta":1.5,"ripple":false}));
    let after = clip(&p, "c1").clone();
    let s0 = source_time(&before, 3.0).unwrap();
    let s1 = source_time(&after, 3.0 - 1.5).unwrap();
    assert!((s0 - s1).abs() < 1e-6, "reverse trim start: {s0} vs {s1}");
    ap(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"end","delta":-1.0,"ripple":false}));
    let after2 = clip(&p, "c1").clone();
    let s2 = source_time(&after2, 1.5).unwrap();
    assert!((s0 - s2).abs() < 1e-6, "reverse trim end: {s0} vs {s2}");
    // split reverse
    ap(&mut p, json!({"op":"split_clip","clip":"c1","at":3.0,"new_id":"r"}));
    let l = clip(&p, "c1").clone();
    let r = clip(&p, "r").clone();
    let sl = source_time(&l, 1.0).unwrap();
    let sr = source_time(&r, 0.5).unwrap();
    assert!((sl - source_time(&after2, 1.0).unwrap()).abs() < 1e-6 && (sr - source_time(&after2, 2.0).unwrap()).abs() < 1e-6, "split reverse {sl} {sr}");
}

#[test]
fn inverse_speed_property() {
    // local_time_of_source(source_time(t)) must map back to a time showing the same source
    let mut seed: u64 = 12345;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64
    };
    let eases = ["linear", "in", "out", "in_out", "hold"];
    let mut fails = Vec::new();
    for case in 0..400 {
        let n = 2 + (rnd() * 3.0) as usize;
        let dur = 2.0 + rnd() * 6.0;
        let mut keys = Vec::new();
        for k in 0..n {
            let t = dur * k as f64 / (n - 1) as f64;
            let v = if rnd() < 0.2 { 0.0 } else { 0.2 + rnd() * 3.0 };
            keys.push(json!({"t": t, "v": v, "ease": eases[(rnd() * 5.0) as usize % 5]}));
        }
        let reverse = rnd() < 0.3;
        let c: Clip = serde_json::from_value(json!({"id":"c","track":"v1","start":0.0,"duration":dur,"source":{"type":"media","asset":"mv","in_point":5.0},"speed":{"keys":keys},"reverse":reverse})).unwrap();
        for _ in 0..10 {
            let t = rnd() * dur;
            let s = source_time(&c, t).unwrap();
            match local_time_of_source(&c, s) {
                Some(t2) => {
                    let s2 = source_time(&c, t2).unwrap();
                    if (s2 - s).abs() > 1e-5 || t2 > t + 1e-6 && !reverse {
                        fails.push(format!("case {case}: t={t:.4} s={s:.6} -> t2={t2:.4} s2={s2:.6} rev={reverse}"));
                    }
                }
                None => fails.push(format!("case {case}: t={t:.4} s={s:.6} not found (rev={reverse}) keys={}", serde_json::to_string(&c.speed).unwrap())),
            }
        }
    }
    for f in fails.iter().take(10) {
        println!("{f}");
    }
    assert!(fails.is_empty(), "{} failures", fails.len());
}

#[test]
fn cut_source_of_a_silence_after_captions_fails_with_overlap() {
    let mut p = base();
    ap(&mut p, json!({"op":"set_transcript","asset":"m1","transcript":{"words":[
        {"text":"a","start":1.0,"end":1.4},{"text":"b","start":1.5,"end":1.9},
        {"text":"c","start":2.9,"end":3.3},{"text":"d","start":3.4,"end":3.8}]}}));
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"vo","track":"a1","asset":"m1","start":0.0,"duration":6.0}}));
    ap(&mut p, json!({"op":"generate_captions","clips":["vo"],"track":"v2"}));
    let r = resolve(&p);
    for c in p.clips.iter().filter(|c| c.track == "v2") {
        println!("{} {:?}", c.id, r.span(&c.id));
    }
    // what remove_silences does with its defaults (min_len 0.6, padding 0.12) for the 1.9..2.9 silence
    let res = try_ap(&mut p, json!({"op":"cut_source","clip":"vo","from":2.02,"to":2.78}));
    assert!(res.is_ok(), "{}", res.err().unwrap());
}

#[test]
fn set_speed_of_captioned_voice_is_refused() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"vo","track":"a1","asset":"m1","start":0.0,"duration":12.0}}));
    ap(&mut p, json!({"op":"generate_captions","clips":["vo"],"track":"v2","max_words":3,"pause":5.0}));
    let res = try_ap(&mut p, json!({"op":"set_speed","clip":"vo","speed":1.25}));
    assert!(res.is_ok(), "{}", res.err().unwrap());
    let bad = check_captions(&p, "set_speed");
    assert!(bad.is_empty(), "{bad:?}");
}
