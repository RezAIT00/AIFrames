//! Round-3 regression tests for the fixes that the reviewer's repros (review_*.rs) do not cover.
mod common;
use aiframes_core::*;
use common::*;
use serde_json::json;

/// Every caption word must be highlighted exactly when it is heard on the timeline.
fn captions_in_sync(p: &Project) -> Vec<String> {
    let r = resolve(p);
    let words = &p.transcripts["m1"].words;
    let vo: Vec<&Clip> = p.clips.iter().filter(|c| c.track == "a1").collect();
    let heard = |i: usize| vo.iter().find_map(|c| Some(r.span(&c.id)?.start + local_time_of_source(c, words[i].start)?));
    let mut bad = Vec::new();
    for c in p.clips.iter().filter(|c| c.tags.iter().any(|t| t == "caption")) {
        let Source::Text(t) = &c.source else { continue };
        let Some(h) = &t.highlight else { continue };
        let Some(s) = r.span(&c.id) else {
            bad.push(format!("{} unresolved", c.id));
            continue;
        };
        for (k, w) in t.text.split_whitespace().enumerate() {
            let idx: usize = w.trim_start_matches('w').trim_end_matches('.').parse().unwrap();
            match (h.words.get(k), heard(idx)) {
                (Some(win), Some(tt)) if (s.start + win[0] - tt).abs() < 1e-3 => {}
                other => bad.push(format!("{} word {w}: {other:?}", c.id)),
            }
        }
    }
    bad
}

fn captioned() -> Project {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"vo","track":"a1","asset":"m1","start":0.0,"duration":20.0}}));
    ap(&mut p, json!({"op":"generate_captions","clips":["vo"],"track":"v2","max_words":3,"pause":5.0}));
    assert!(captions_in_sync(&p).is_empty());
    p
}

// ---------------------------------------------------------------------------------------- E

#[test]
fn split_keeps_every_eased_curve() {
    for ease in ["linear", "hold", "in", "out", "in_out", "back_out", "elastic"] {
        let mut p = base();
        ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","text":"a","start":0.0,"duration":4.0,
            "transform":{"scale":{"keys":[{"t":0.0,"v":1.0,"ease":ease},{"t":2.0,"v":2.0},{"t":4.0,"v":1.5}]}}}}));
        let orig = clip(&p, "c1").clone();
        ap(&mut p, json!({"op":"split_clip","clip":"c1","at":0.7,"new_id":"r"}));
        let (l, r) = (clip(&p, "c1").clone(), clip(&p, "r").clone());
        // Exact for every ease but elastic, which is rebuilt from Hermite pieces.
        let tol = if ease == "elastic" { 5e-4 } else { 1e-9 };
        for i in 0..=80 {
            let t = i as f64 * 0.05;
            let want = sample(&orig.transform.scale, t);
            let got = if t <= 0.7 { sample(&l.transform.scale, t) } else { sample(&r.transform.scale, t - 0.7) };
            assert!((want - got).abs() <= tol, "{ease}: at {t} {want} became {got}");
        }
    }
}

// ---------------------------------------------------------------------------------------- M

#[test]
fn nested_json_typos_are_errors_that_name_the_key() {
    let e = Op::from_json(&json!({"op":"move_clip","clip":"c2","start":{"after":"c1","ofset":3}})).unwrap_err();
    assert!(e.message.contains("unknown anchor key `ofset`") && e.message.contains("Did you mean offset?"), "{}", e.message);
    let e = Op::from_json(&json!({"op":"move_clip","clip":"c2","start":{"after":"c1","with":"c9"}})).unwrap_err();
    assert!(e.message.contains("exactly one target"), "{}", e.message);
    let e = Op::from_json(&json!({"op":"move_clip","clip":"c2","start":"soon"})).unwrap_err();
    assert!(e.message.contains("a time is seconds"), "{}", e.message);
    let e = Op::from_json(&json!({"op":"add_effect","clip":"c1","effect":{"kind":"blur","parms":{}}})).unwrap_err();
    assert!(e.message.contains("unknown field `parms`"), "{}", e.message);
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","text":"a","start":0.0,"duration":2.0}}));
    let e = try_ap(&mut p, json!({"op":"set_clip","clip":"c1","patch":{"transform":{"sclae":2.0}}})).unwrap_err();
    assert!(e.message.contains("unknown field `sclae`") && e.message.contains("`scale`"), "{}", e.message);
    let e = try_ap(&mut p, json!({"op":"set_clip","clip":"c1","patch":{"opacity":{"kyes":[]}}})).unwrap_err();
    assert!(e.message.contains("missing `keys`"), "{}", e.message);
    let e = try_ap(&mut p, json!({"op":"add_clip","clip":{"track":"v2","text":"x","source_text":1}})).unwrap_err();
    assert!(e.message.contains("unknown clip field"), "{}", e.message);
    let e = aiframes_core::document::parse_project(r#"{"format":1,"settings":{"width":8,"height":8,"fps":30,"frame_rate":30}}"#).unwrap_err();
    assert!(e.message.contains("unknown field `frame_rate`"), "{}", e.message);
}

#[test]
fn files_written_by_earlier_rounds_still_load() {
    // Every kind of nested object the other crates' fixtures use, in the shape written before round 3.
    let text = json!({
        "format": 1, "name": "fixture",
        "settings": {"width": 1920, "height": 1080, "fps": 30.0, "sample_rate": 48000, "background": "#000000", "title_safe": 0.8, "action_safe": 0.9},
        "media": [{"id": "mv", "path": "a.mp4", "kind": "video", "info": {"duration": 10.0, "width": 1920, "height": 1080, "fps": 30.0, "has_video": true, "has_audio": true, "codec": "h264"}}],
        "tracks": [{"id": "v1", "kind": "video", "role": "main"}, {"id": "a1", "kind": "audio", "volume": 0.5}],
        "clips": [
            {"id": "c1", "track": "v1", "start": 0.0, "duration": 4.0, "source": {"type": "media", "asset": "mv", "in_point": 1.0},
             "speed": {"keys": [{"t": 0.0, "v": 1.0, "ease": "in_out"}, {"t": 4.0, "v": 2.0}]}, "transform": {"x": 10.0, "scale": 1.2, "crop": {"left": 0.1}},
             "effects": [{"kind": "drop_shadow", "params": {"distance": 8.0}, "colors": {"color": "#000000"}}], "fade_in": 0.5},
            {"id": "t1", "track": "v1", "start": {"after": "c1", "offset": 0.0}, "duration": 2.0,
             "source": {"type": "text", "text": "Hi there", "font": "Segoe UI", "size": 64.0, "weight": 800, "color": "#FFFFFF", "line_height": 1.15, "max_width": 900.0,
                        "background": {"color": "#000000B3", "padding": 20.0, "radius": 12.0}, "stroke": {"color": "#000000", "width": 2.0},
                        "shadow": {"color": "#000000", "blur": 4.0, "x": 2.0, "y": 2.0}, "reveal": {"kind": "slide_up", "duration": 0.4},
                        "highlight": {"color": "#FFD60A", "words": [[0.0, 0.5], [0.5, 1.0]]}}},
            {"id": "s1", "track": "a1", "start": {"word": {"clip": "c1", "index": 0}}, "duration": 1.0, "source": {"type": "media", "asset": "mv"}}
        ],
        "transitions": [{"id": "tr1", "from": "c1", "to": "t1", "kind": "crossfade", "duration": 0.5}],
        "markers": [{"id": "mk1", "time": {"with": "t1"}, "name": "x", "kind": "chapter"}],
        "ducking": [{"id": "d1", "target": "a1", "trigger": "v1"}],
        "transcripts": {"mv": {"language": "en", "words": [{"text": "Hi", "start": 1.0, "end": 1.4, "conf": 0.9}]}},
        "export": {"codec": "hevc", "quality": "high", "hardware": true, "audio_bitrate_kbps": 256, "loudness_lufs": -14.0, "true_peak_db": -1.5}
    })
    .to_string();
    let p = aiframes_core::document::parse_project(&text).unwrap_or_else(|e| panic!("{e}"));
    let again = aiframes_core::document::parse_project(&aiframes_core::document::canonical_json(&p)).unwrap();
    assert_eq!(again, p);
}

// ---------------------------------------------------------------------------------------- H

#[test]
fn non_finite_numbers_never_reach_a_file() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","text":"a","start":0.0,"duration":2.0}}));
    let e = try_ap(&mut p, json!({"op":"add_clip","clip":{"track":"v2","start":0.0,"duration":1.0,"source":{"type":"text","text":"x","size":1e300}}})).unwrap_err();
    assert!(e.message.contains("text style values are out of range"), "{}", e.message);
    let e = try_ap(&mut p, json!({"op":"add_clip","clip":{"track":"v2","start":0.0,"duration":1.0,"source":{"type":"text","text":"x","background":{"color":"#000000","radius":1e300}}}})).unwrap_err();
    assert!(e.message.contains("out of range"), "{}", e.message);
    let e = try_ap(&mut p, json!({"op":"generate_captions","clips":["c1"],"track":"v2","style":{"y":1e300}})).unwrap_err();
    assert!(e.message.contains("out of range"), "{}", e.message);
    // A project that somehow holds NaN refuses to save instead of writing `null`.
    p.clips[0].duration = f64::NAN;
    let e = aiframes_core::document::try_canonical_json(&p).unwrap_err();
    assert!(e.message.contains("duration of clip c1"), "{}", e.message);
    let dir = std::env::temp_dir().join(format!("aifr-r3-nan-{}", std::process::id()));
    let mut d = Document::new(p);
    assert!(d.save_as(dir.join("p.aifr")).is_err());
    assert!(!dir.join("p.aifr").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------- I

fn with_transition() -> Project {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":4.0}}));
    ap(&mut p, json!({"op":"append_clip","track":"v1","clip":{"id":"c2","asset":"mv","in_point":10.0,"duration":4.0}}));
    ap(&mut p, json!({"op":"add_transition","id":"x1","from":"c1","to":"c2","kind":"crossfade","duration":0.5}));
    p
}

#[test]
fn transitions_follow_or_leave_their_cut() {
    let none_broken = |p: &Project| validate(p, &resolve(p)).iter().all(|f| f.code != "transition_not_adjacent");
    for v in [
        json!({"op":"move_clip","clip":"c2","start":9.0}),
        json!({"op":"nudge","clips":["c2"],"delta":0.5}),
        json!({"op":"lift_range","from":3.5,"to":4.5,"tracks":["v1"]}),
    ] {
        let mut p = with_transition();
        let o = ap(&mut p, v.clone());
        assert!(p.transitions.is_empty(), "{v}: transition kept");
        assert!(o.warnings.iter().any(|w| w.contains("crossfade transition between c1 and c2") && !w.contains("x1")), "{v}: {:?}", o.warnings);
        assert!(none_broken(&p));
    }
    // Splitting the outgoing clip keeps the transition on the cut (re-pointed to the right half).
    let mut p = with_transition();
    ap(&mut p, json!({"op":"split_clip","clip":"c1","at":2.0,"new_id":"c1r"}));
    assert_eq!((p.transitions[0].from.as_str(), p.transitions[0].to.as_str()), ("c1r", "c2"));
    // A freeze inside the outgoing clip keeps it too.
    let mut p = with_transition();
    ap(&mut p, json!({"op":"freeze_frame","clip":"c1","at":2.0,"duration":1.0}));
    assert_eq!(p.transitions.len(), 1);
    assert!(none_broken(&p));
}

// ---------------------------------------------------------------------------------------- D

fn detached() -> Project {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"v","track":"v1","asset":"mv","start":0.0,"duration":10.0}}));
    ap(&mut p, json!({"op":"detach_audio","clip":"v","track":"a1","new_id":"a"}));
    p
}

fn in_sync(p: &Project) {
    assert_eq!(linked_partners(p, "v"), vec!["a".to_string()], "link lost");
    let (v, a) = (clip(p, "v"), clip(p, "a"));
    for t in [0.0, 1.3, 4.9] {
        assert!((source_time(v, t).unwrap() - source_time(a, t).unwrap()).abs() < 1e-9);
    }
}

#[test]
fn edits_on_the_detached_audio_move_the_picture_too() {
    for v in [
        json!({"op":"slip_clip","clip":"a","delta":2.0}),
        json!({"op":"set_speed","clip":"a","speed":2.0}),
        json!({"op":"add_keyframe","clip":"a","path":"speed","t":1.0,"v":1.5}),
        json!({"op":"remove_keyframe","clip":"a","path":"speed","t":1.0}),
        json!({"op":"trim_clip","clip":"a","edge":"start","delta":1.0}),
        json!({"op":"trim_clip","clip":"a","edge":"end","delta":-2.0}),
        json!({"op":"cut_source","clip":"a","from":3.0,"to":4.0}),
    ] {
        let mut p = detached();
        if v["op"] == "remove_keyframe" {
            ap(&mut p, json!({"op":"set_param","clip":"a","path":"speed","keys":[{"t":0,"v":1},{"t":1,"v":2},{"t":4,"v":1}]}));
        }
        ap(&mut p, v.clone());
        in_sync(&p);
        let r = resolve(&p);
        let ends = |t: &str| r.on_track(&p, t).iter().map(|(_, s)| s.end).fold(0.0, f64::max);
        assert!((ends("v1") - ends("a1")).abs() < 1e-9, "{v}");
    }
}

// ---------------------------------------------------------------------------------------- B

#[test]
fn captions_stay_in_sync_through_every_voice_edit() {
    for v in [
        json!({"op":"split_clip","clip":"vo","at":7.25}),
        json!({"op":"slip_clip","clip":"vo","delta":0.5}),
        json!({"op":"trim_clip","clip":"vo","edge":"start","delta":2.2}),
        json!({"op":"trim_clip","clip":"vo","edge":"end","delta":-5.0}),
        json!({"op":"set_param","clip":"vo","path":"speed","keys":[{"t":0,"v":1},{"t":10,"v":1.6,"ease":"in_out"}]}),
        json!({"op":"set_speed","clip":"vo","speed":0.8}),
        json!({"op":"cut_source","clip":"vo","from":4.6,"to":5.9}),
        json!({"op":"delete_words","clip":"vo","from":5,"to":5}),
    ] {
        let mut p = captioned();
        let o = ap(&mut p, v.clone());
        let bad = captions_in_sync(&p);
        assert!(bad.is_empty(), "{v} ({}): {bad:?}", o.summary);
        assert!(validate(&p, &resolve(&p)).iter().all(|f| f.code != "overlap_same_track"), "{v}");
    }
}

// ---------------------------------------------------------------------------------------- C

#[test]
fn before_anchored_clips_keep_their_start_in_slide_and_speed() {
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"k","track":"v2","text":"x","start":12.0,"duration":1.0}}));
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":4.0}}));
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c3","track":"v1","asset":"mv","start":{"before":"k","offset":-0.0},"duration":4.0,"in_point":20.0}}));
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c2","track":"v1","asset":"mv","start":{"before":"c3"},"duration":4.0,"in_point":10.0}}));
    assert_eq!((span(&p, "c2"), span(&p, "c3")), (Some((4.0, 8.0)), Some((8.0, 12.0))));
    let mut q = p.clone();
    ap(&mut q, json!({"op":"slide_clip","clip":"c2","delta":1.0}));
    assert_eq!((span(&q, "c1"), span(&q, "c2"), span(&q, "c3")), (Some((0.0, 5.0)), Some((5.0, 9.0)), Some((9.0, 12.0))));
    let mut q = p.clone();
    ap(&mut q, json!({"op":"set_speed","clip":"c3","speed":2.0}));
    assert_eq!(span(&q, "c3"), Some((8.0, 10.0)), "a speed change keeps the start");
}

// ---------------------------------------------------------------------------------------- O

#[test]
fn open_reports_unreadable_history_and_keeps_numbering() {
    let dir = std::env::temp_dir().join(format!("aifr-r3-hist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("p.aifr");
    let mut d = Document::new(base());
    d.save_as(&path).unwrap();
    d.apply("ai", "one", vec![op(json!({"op":"set_project","name":"a"}))]).unwrap();
    d.apply("ai", "two", vec![op(json!({"op":"set_project","name":"b"}))]).unwrap();
    d.save().unwrap();
    let hp = aiframes_core::document::history_path(&path);
    let mut text = std::fs::read_to_string(&hp).unwrap();
    text.push_str("{this is not json\n");
    std::fs::write(&hp, text).unwrap();
    let mut d2 = Document::open(&path).unwrap();
    assert_eq!(d2.load_warnings().len(), 1, "{:?}", d2.load_warnings());
    assert!(d2.load_warnings()[0].contains("1 history line(s)"));
    assert_eq!(d2.revision(), 3, "the unreadable line counts as a revision");
    let r = d2.apply("ai", "three", vec![op(json!({"op":"set_project","name":"c"}))]).unwrap();
    assert_eq!(r.revision, 4);
    d2.save().unwrap();
    let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().contains(".tmp-")).collect();
    assert!(leftovers.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn validate_offers_only_fixes_that_work() {
    let check = |p: &Project, code: &str| {
        let f = validate(p, &resolve(p)).into_iter().find(|f| f.code == code).unwrap_or_else(|| panic!("no {code}"));
        assert!(!f.fix_ops.is_empty(), "{code} has no fix");
        let mut q = p.clone();
        apply_batch(&mut q, &f.fix_ops).unwrap_or_else(|e| panic!("{code}: {e}"));
        assert!(!validate(&q, &resolve(&q)).iter().any(|g| g.code == code && g.ids == f.ids), "{code} not fixed");
    };
    // An early first chapter moves to 0:00 instead of getting a chapter 0–10 s before it.
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":60.0}}));
    for (i, t) in [3.0, 30.0, 50.0].iter().enumerate() {
        ap(&mut p, json!({"op":"add_marker","id":format!("ch{i}"),"time":t,"kind":"chapter"}));
    }
    check(&p, "chapter_markers");
    // A speed problem on a text clip is fixed with set_param (set_speed is for media).
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"t","track":"v1","text":"a","start":0.0,"duration":2.0}}));
    p.clips[0].speed = Param::Value(-1.0);
    check(&p, "speed_out_of_range");
    // A tiny transition fix never rounds to 0.
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":0.002}}));
    ap(&mut p, json!({"op":"append_clip","track":"v1","clip":{"id":"c2","asset":"mv","in_point":10.0,"duration":2.0}}));
    p.transitions.push(Transition { id: "x".into(), from: "c1".into(), to: "c2".into(), kind: "crossfade".into(), duration: 0.4, params: Default::default(), colors: Default::default() });
    let f = validate(&p, &resolve(&p)).into_iter().find(|f| f.code == "transition_too_long").unwrap();
    if let Some(Op::SetTransition { duration: Some(d), .. }) = f.fix_ops.first() {
        assert!(*d > 0.0);
    }
    // A detached-audio project validates clean of track errors, and every keyframe problem of a clip is
    // one finding fixed at once.
    let mut p = base();
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"v","track":"v1","asset":"mv","start":0.0,"duration":4.0}}));
    ap(&mut p, json!({"op":"detach_audio","clip":"v","track":"a1"}));
    p.clips[0].opacity = Param::Animated { keys: vec![Key { t: -1.0, v: 0.0, ease: Ease::Linear }, Key { t: 9.0, v: 1.0, ease: Ease::Linear }] };
    p.clips[0].volume = Param::Animated { keys: vec![Key { t: 0.0, v: 0.0, ease: Ease::Linear }, Key { t: 7.0, v: 1.0, ease: Ease::Linear }] };
    let f = validate(&p, &resolve(&p));
    assert_eq!(f.iter().filter(|f| f.code == "keyframe_outside_clip").count(), 1);
    assert!(f.iter().all(|f| f.code != "track_kind_mismatch"));
    check(&p, "keyframe_outside_clip");
}
