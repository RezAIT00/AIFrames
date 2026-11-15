//! Regression tests ported from the independent review of aiframes-core (round 3): each test was a
//! runnable repro of a confirmed bug.
mod common;
use aiframes_core::*;
use common::*;
use serde_json::json;

/// c9 at [10,12] on v1; c1 (4 s) on v2 anchored `before` c9 => [6,10].
fn before_setup() -> Project {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c9","track":"v1","text":"anchor","start":10.0,"duration":2.0}}));
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v2","text":"title","start":{"before":"c9"},"duration":4.0}}));
    assert_eq!(span(&p, "c1"), Some((6.0, 10.0)));
    p
}

#[test]
fn split_of_before_anchored_clip_moves_left_half() {
    let mut p = before_setup();
    ap(&mut p, json!({"op":"split_clip","clip":"c1","at":8.0,"new_id":"c1r"}));
    let (l, r) = (span(&p, "c1").unwrap(), span(&p, "c1r").unwrap());
    println!("left {l:?} right {r:?}");
    assert!(close(l, (6.0, 8.0)) && close(r, (8.0, 10.0)), "expected [6,8]+[8,10], got {l:?} + {r:?}");
}

#[test]
fn trim_start_of_before_anchored_clip_shifts_twice() {
    let mut p = before_setup();
    ap(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"start","delta":1.0}));
    let s = span(&p, "c1").unwrap();
    assert!(close(s, (7.0, 10.0)), "trim start +1 of [6,10]: expected [7,10], got {s:?}");
}

#[test]
fn trim_end_of_before_anchored_clip_moves_its_start() {
    let mut p = before_setup();
    ap(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"end","delta":-1.0}));
    let s = span(&p, "c1").unwrap();
    assert!(close(s, (6.0, 9.0)), "trim end -1 of [6,10]: expected [6,9], got {s:?}");
}

#[test]
fn freeze_first_frame_of_before_anchored_clip_fails() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c9","track":"v1","text":"anchor","start":10.0,"duration":2.0}}));
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v2","asset":"mv","start":{"before":"c9"},"duration":4.0}}));
    let r = try_ap(&mut p, json!({"op":"freeze_frame","clip":"c1","at":6.0,"duration":1.5,"new_id":"fz"}));
    match r {
        Ok(_) => {
            let (f, c) = (span(&p, "fz").unwrap(), span(&p, "c1").unwrap());
            assert!(close(f, (6.0, 7.5)) && close(c, (7.5, 11.5)), "freeze {f:?} clip {c:?}");
        }
        Err(e) => panic!("freeze_frame at the first frame of a before-anchored clip refused: {e}"),
    }
}

#[test]
fn roll_edit_right_before_anchored_double_shift() {
    // left c1 [0,4] absolute; right c2 [4,8] anchored before c3 (c3 at 8 on v2)
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c3","track":"v2","text":"x","start":8.0,"duration":2.0}}));
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","text":"a","start":0.0,"duration":4.0}}));
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c2","track":"v1","text":"b","start":{"before":"c3"},"duration":4.0}}));
    assert_eq!(span(&p, "c2"), Some((4.0, 8.0)));
    let r = try_ap(&mut p, json!({"op":"roll_edit","left":"c1","right":"c2","delta":1.0}));
    let l = span(&p, "c1");
    let rr = span(&p, "c2");
    assert!(r.is_ok(), "roll edit refused: {:?}", r.err().map(|e| e.message));
    assert!(close(l.unwrap(), (0.0, 5.0)) && close(rr.unwrap(), (5.0, 8.0)), "left {l:?} right {rr:?}");
}

#[test]
fn split_inside_fade_in_changes_the_envelope() {
    use aiframes_core::eval::{audio_gain, visual_opacity};
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":4.0,"fade_in":2.0,"fade_out":1.5,"opacity":0.8}}));
    let orig = clip(&p, "c1").clone();
    for at in [1.0, 3.0] {
        let mut q = p.clone();
        ap(&mut q, json!({"op":"split_clip","clip":"c1","at":at,"new_id":"r"}));
        let (l, r) = (clip(&q, "c1").clone(), clip(&q, "r").clone());
        for i in 0..=40 {
            let t = i as f64 * 0.1;
            let (vo, ao) = (visual_opacity(&orig, t), audio_gain(&orig, t));
            let (vn, an) = if t < at { (visual_opacity(&l, t), audio_gain(&l, t)) } else { (visual_opacity(&r, t - at), audio_gain(&r, t - at)) };
            assert!((vo - vn).abs() < 1e-9 && (ao - an).abs() < 1e-9, "split at {at}: gain at {t} changed: picture {vo} -> {vn}, sound {ao} -> {an}");
        }
    }
}

#[test]
fn split_of_in_out_speed_ramp_changes_source_time() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":4.0,
        "speed":{"keys":[{"t":0.0,"v":1.0,"ease":"in_out"},{"t":4.0,"v":3.0}]}}}));
    let orig = clip(&p, "c1").clone();
    ap(&mut p, json!({"op":"split_clip","clip":"c1","at":1.0,"new_id":"r"}));
    let r = clip(&p, "r").clone();
    let want = source_time(&orig, 3.0).unwrap();
    let got = source_time(&r, 2.0).unwrap();
    let want_end = source_time(&orig, 4.0).unwrap();
    let got_end = source_time(&r, 3.0).unwrap();
    println!("timeline 3.0: source {want} before split, {got} after; clip end {want_end} vs {got_end}");
    assert!((want - got).abs() < 1e-3 && (want_end - got_end).abs() < 1e-3, "source time drifted by {} s (end {} s)", got - want, got_end - want_end);
}

#[test]
fn save_as_then_undo_breaks_relative_media_paths() {
    let root = std::env::temp_dir().join(format!("aifr-review-saveas-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut p = base();
    p.media[0].path = root.join("media").join("g.mp4").to_string_lossy().replace('\\', "/");
    let mut d = Document::new(p);
    d.save_as(root.join("p.aifr")).unwrap();
    assert_eq!(d.project.media[0].path, "media/g.mp4");
    let want = d.media_path(&d.project.media[0]);
    d.apply("ai", "rename", vec![op(json!({"op":"set_project","name":"x"}))]).unwrap();
    d.save_as(root.join("sub").join("deeper").join("q.aifr")).unwrap();
    assert_eq!(d.media_path(&d.project.media[0]), want, "save_as keeps the file");
    d.undo().unwrap();
    let got = d.media_path(&d.project.media[0]);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(got, want, "after undo the media path points elsewhere ({})", d.project.media[0].path);
}

#[test]
fn deleted_highest_id_is_reused() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"track":"v1","text":"a","start":0.0,"duration":1.0}}));
    let o = ap(&mut p, json!({"op":"add_clip","clip":{"track":"v1","text":"b","start":1.0,"duration":1.0}}));
    let first = o.created[0].clone();
    ap(&mut p, json!({"op":"delete_clip","clip":first}));
    let o2 = ap(&mut p, json!({"op":"add_clip","clip":{"track":"v1","text":"c","start":5.0,"duration":1.0}}));
    assert_ne!(o2.created[0], first, "id {first} of a deleted clip was handed out again");
}

#[test]
fn color_parse_panics_on_multibyte_short_hex() {
    let r = std::panic::catch_unwind(|| Color::parse("#é1"));
    assert!(r.is_ok(), "Color::parse panicked");
    let r = std::panic::catch_unwind(|| Op::from_json(&json!({"op":"set_settings","background":"#é1"})));
    assert!(r.is_ok(), "Op::from_json panicked on a bad colour");
    let r = std::panic::catch_unwind(|| aiframes_core::document::parse_project(r##"{"format":1,"settings":{"width":8,"height":8,"fps":30,"background":"#é1"}}"##));
    assert!(r.is_ok(), "parse_project panicked on a bad colour");
}

#[test]
fn typo_in_anchor_offset_is_silently_ignored() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","text":"a","start":0.0,"duration":2.0}}));
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c2","track":"v2","text":"b","start":0.0,"duration":1.0}}));
    let r = Op::from_json(&json!({"op":"move_clip","clip":"c2","start":{"after":"c1","ofset":3.0}}));
    assert!(r.is_err(), "unknown key `ofset` accepted: {:?}", r.map(|o| serde_json::to_string(&o).unwrap()));
}

#[test]
fn anchor_with_two_targets_is_accepted() {
    let r = Op::from_json(&json!({"op":"move_clip","clip":"c2","start":{"after":"c1","with":"c9"}}));
    assert!(r.is_err(), "anchor with both after and with accepted as {:?}", r.map(|o| serde_json::to_string(&o).unwrap()));
}

#[test]
fn typo_in_nested_clip_fields_is_silently_ignored() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","text":"a","start":0.0,"duration":2.0}}));
    let before = clip(&p, "c1").clone();
    let r = try_ap(&mut p, json!({"op":"set_clip","clip":"c1","patch":{"transform":{"sclae":2.0}}}));
    let o = r.as_ref().map(|o| o.summary.clone());
    assert!(r.is_err() || clip(&p, "c1") != &before, "set_clip with transform.sclae: {o:?}, clip unchanged");
}

#[test]
fn insert_clip_at_a_transition_cut_leaves_dangling_transition() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":4.0}}));
    ap(&mut p, json!({"op":"append_clip","track":"v1","clip":{"id":"c2","asset":"mv","in_point":10.0,"duration":4.0}}));
    ap(&mut p, json!({"op":"add_transition","from":"c1","to":"c2","kind":"crossfade","duration":0.5}));
    let o = ap(&mut p, json!({"op":"insert_clip","track":"v1","at":4.0,"clip":{"id":"ins","text":"x","duration":2.0}}));
    let f = validate(&p, &resolve(&p));
    let bad: Vec<_> = f.iter().filter(|f| f.code == "transition_not_adjacent").map(|f| f.message.clone()).collect();
    assert!(bad.is_empty(), "insert_clip succeeded ({}; warnings {:?}) but left: {bad:?}", o.summary, o.warnings);
}

#[test]
fn set_transition_accepts_duration_longer_than_clips() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":1.0}}));
    ap(&mut p, json!({"op":"append_clip","track":"v1","clip":{"id":"c2","asset":"mv","in_point":10.0,"duration":1.0}}));
    assert!(try_ap(&mut p, json!({"op":"add_transition","from":"c1","to":"c2","kind":"crossfade","duration":5.0})).is_err());
    ap(&mut p, json!({"op":"add_transition","id":"t1","from":"c1","to":"c2","kind":"crossfade","duration":0.5}));
    let r = try_ap(&mut p, json!({"op":"set_transition","transition":"t1","duration":5.0}));
    assert!(r.is_err(), "set_transition accepted a 5 s transition over 1 s clips (add_transition refuses it)");
}

#[test]
fn generate_captions_with_duplicate_clip_doubles_words() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"vo","track":"a1","asset":"m1","start":0.0,"duration":6.0}}));
    ap(&mut p, json!({"op":"generate_captions","clips":["vo","vo"],"track":"v2"}));
    let texts: Vec<String> = p.clips.iter().filter_map(|c| match &c.source { Source::Text(t) => Some(t.text.clone()), _ => None }).collect();
    println!("{texts:?}");
    assert!(texts.iter().all(|t| !t.contains("w0 w0")), "captions repeat words: {texts:?}");
}

#[test]
fn delete_words_on_detached_audio_desyncs_video() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"v","track":"v1","asset":"mv","start":0.0,"duration":10.0}}));
    ap(&mut p, json!({"op":"detach_audio","clip":"v","track":"a1","new_id":"a"}));
    let o = ap(&mut p, json!({"op":"delete_words","clip":"a","from":2,"to":3}));
    let r = resolve(&p);
    let vend = r.on_track(&p, "v1").iter().map(|(_, s)| s.end).fold(0.0, f64::max);
    let aend = r.on_track(&p, "a1").iter().map(|(_, s)| s.end).fold(0.0, f64::max);
    println!("{}", o.summary);
    assert!((vend - aend).abs() < 1e-6, "after delete_words on the detached audio: video ends {vend}, audio ends {aend}");
}

#[test]
fn trim_of_detached_audio_breaks_link_silently() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"v","track":"v1","asset":"mv","start":0.0,"duration":10.0}}));
    ap(&mut p, json!({"op":"detach_audio","clip":"v","track":"a1","new_id":"a"}));
    ap(&mut p, json!({"op":"split_clip","clip":"a","at":5.0}));
    let v = clip(&p, "v").duration;
    let a = clip(&p, "a").duration;
    assert!((v - a).abs() < 1e-9, "split of the audio side split only the audio: video {v} s, audio {a} s (link lost: {:?})", linked_partners(&p, "v"));
}

#[test]
fn detach_audio_result_is_flagged_track_kind_mismatch() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"v","track":"v1","asset":"mv","start":0.0,"duration":10.0}}));
    ap(&mut p, json!({"op":"detach_audio","clip":"v","track":"a1","new_id":"a"}));
    let f: Vec<Finding> = validate(&p, &resolve(&p)).into_iter().filter(|f| f.code == "track_kind_mismatch").collect();
    for x in &f {
        println!("{:?} {} fix_ops={}", x.severity, x.message, serde_json::to_string(&x.fix_ops).unwrap());
    }
    assert!(f.is_empty(), "detach_audio output reported as an error by validate");
}

#[test]
fn trim_with_ripple_false_leaves_transition_not_adjacent() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":4.0}}));
    ap(&mut p, json!({"op":"append_clip","track":"v1","clip":{"id":"c2","asset":"mv","in_point":10.0,"duration":4.0}}));
    ap(&mut p, json!({"op":"add_transition","from":"c1","to":"c2","kind":"crossfade","duration":0.5}));
    let o = ap(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"end","delta":-1.0,"ripple":false}));
    let bad: Vec<String> = validate(&p, &resolve(&p)).into_iter().filter(|f| f.code == "transition_not_adjacent").map(|f| format!("{:?} {}", f.severity, f.message)).collect();
    assert!(bad.is_empty(), "trim accepted (warnings {:?}) but: {bad:?}", o.warnings);
}

#[test]
fn speed_ramp_on_detached_video_desyncs_audio() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"v","track":"v1","asset":"mv","start":0.0,"duration":10.0}}));
    ap(&mut p, json!({"op":"detach_audio","clip":"v","track":"a1","new_id":"a"}));
    ap(&mut p, json!({"op":"set_param","clip":"v","path":"speed","keys":[{"t":0,"v":1},{"t":5,"v":2}]}));
    let (v, a) = (clip(&p, "v").clone(), clip(&p, "a").clone());
    let (sv, sa) = (source_time(&v, 8.0).unwrap(), source_time(&a, 8.0).unwrap());
    assert!((sv - sa).abs() < 1e-6, "at 8 s the picture shows source {sv} but the detached sound plays {sa}; linked now {:?}", linked_partners(&p, "v"));
}

#[test]
fn scrub_ids_rewrites_ordinary_words_matching_deleted_ids() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":4.0}}));
    ap(&mut p, json!({"op":"append_clip","track":"v1","clip":{"id":"a","asset":"mv","in_point":10.0,"duration":4.0}}));
    ap(&mut p, json!({"op":"add_transition","from":"c1","to":"a","kind":"crossfade","duration":0.5}));
    let o = ap(&mut p, json!({"op":"delete_clip","clip":"a"}));
    println!("{:?}", o.warnings);
    assert!(o.warnings.iter().all(|w| !w.contains("a removed clip deleted clip")), "{:?}", o.warnings);
}

#[test]
fn caption_style_overflow_writes_unreadable_project() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"vo","track":"a1","asset":"m1","start":0.0,"duration":4.0}}));
    let r = try_ap(&mut p, json!({"op":"generate_captions","clips":["vo"],"track":"v2","style":{"size":1.5e308,"y":0}}));
    if let Err(e) = &r {
        println!("refused: {e}");
        return; // refused: fine
    }
    let text = aiframes_core::document::canonical_json(&p);
    let back = aiframes_core::document::parse_project(&text);
    assert!(back.is_ok(), "accepted op, but the saved project cannot be opened again: {}", back.err().unwrap());
}
