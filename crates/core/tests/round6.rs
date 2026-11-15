//! Round 6: edit-culture tools — motion blur, frame blending, 3-D cards, track mattes, and the new
//! effects and transitions (model, registries, ops, validation, schema, describe).
mod common;
use aiframes_core::*;
use common::*;
use serde_json::{json, Value};

fn codes(p: &Project) -> Vec<String> {
    validate(p, &resolve(p)).into_iter().map(|f| f.code.to_string()).collect()
}

/// u1 footage on v1 (0–6 s), t1 big text on v2 (0–6 s), a1 voice.
fn matte_project() -> Project {
    let mut p = base();
    ap(&mut p, json!({"op": "add_track", "id": "v3", "kind": "video"}));
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "u1", "track": "v1", "asset": "mv", "start": 0.0, "duration": 6.0}}));
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "t1", "track": "v2", "text": "WIN", "start": 0.0, "duration": 6.0}}));
    p
}

#[test]
fn new_fields_round_trip_and_defaults_are_omitted() {
    let mut p = matte_project();
    let o = ap(
        &mut p,
        json!({"op": "set_clip", "clip": "u1", "patch": {
            "motion_blur": {"shutter": 270},
            "frame_blend": true,
            "matte": {"clip": "t1", "mode": "luma"},
            "transform": {"rotation_x": 20, "rotation_y": {"keys": [{"t": 0, "v": -90}, {"t": 0.5, "v": 0, "ease": "out"}]}, "perspective": 1200}
        }}),
    );
    assert!(o.summary.contains("motion_blur"), "{}", o.summary);
    let c = clip(&p, "u1");
    assert_eq!(c.motion_blur, Some(MotionBlur { shutter: 270.0, samples: 12 }), "samples defaults to 12");
    assert!(c.frame_blend);
    assert_eq!(c.matte, Some(Matte { clip: "t1".into(), mode: MatteMode::Luma }));
    assert_eq!(c.transform.rotation_x, Param::Value(20.0));
    assert!(c.transform.rotation_y.is_animated());
    assert_eq!(c.transform.perspective, 1200.0);
    // Canonical JSON round trip.
    let text = document::canonical_json(&p);
    let q = document::parse_project(&text).unwrap();
    assert_eq!(q, p);
    let v: Value = serde_json::from_str(&text).unwrap();
    let u = v["clips"].as_array().unwrap().iter().find(|c| c["id"] == "u1").unwrap();
    assert_eq!(u["motion_blur"], json!({"shutter": 270.0, "samples": 12}));
    assert_eq!(u["matte"], json!({"clip": "t1", "mode": "luma"}));
    assert_eq!(u["transform"]["perspective"], json!(1200.0));
    // Defaults are omitted: a plain clip has none of the new keys, alpha mode is implied.
    let t = v["clips"].as_array().unwrap().iter().find(|c| c["id"] == "t1").unwrap();
    for k in ["motion_blur", "frame_blend", "matte"] {
        assert!(t.get(k).is_none(), "{k} written for a plain clip: {t}");
    }
    assert!(t["transform"].get("perspective").is_none());
    // A patch merges into the matte (JSON merge patch); the default mode is left out of the file.
    ap(&mut p, json!({"op": "set_clip", "clip": "u1", "patch": {"matte": {"mode": "alpha"}}}));
    let v: Value = serde_json::from_str(&document::canonical_json(&p)).unwrap();
    let u = v["clips"].as_array().unwrap().iter().find(|c| c["id"] == "u1").unwrap();
    assert_eq!(u["matte"], json!({"clip": "t1"}));
}

#[test]
fn new_json_is_strict() {
    let clip = |extra: Value| {
        let mut c = json!({"id": "c1", "track": "v1", "start": 0, "duration": 1, "source": {"type": "solid", "color": "#000000"}});
        for (k, v) in extra.as_object().unwrap() {
            c[k] = v.clone();
        }
        serde_json::from_value::<Clip>(c).map(|_| ()).map_err(|e| e.to_string())
    };
    assert!(clip(json!({"motion_blur": {"shuter": 180}})).unwrap_err().contains("shuter"));
    assert!(clip(json!({"matte": {"clip": "t1", "mod": "luma"}})).unwrap_err().contains("mod"));
    let e = clip(json!({"matte": {"clip": "t1", "mode": "lumaa"}})).unwrap_err();
    assert!(e.contains("lumaa") && e.contains("luma_inverted"), "{e}");
    assert!(clip(json!({"matte": {"mode": "luma"}})).unwrap_err().contains("clip"));
    assert!(clip(json!({"transform": {"rotationx": 3}})).unwrap_err().contains("rotationx"));
    assert!(clip(json!({"frame_blend": "yes"})).is_err());
    assert!(clip(json!({"motion_blur": {}, "frame_blend": false, "transform": {"rotation_x": 1, "rotation_y": 2, "perspective": 0}})).is_ok());
    let tr = |v: Value| serde_json::from_value::<Transition>(v).map_err(|e| e.to_string());
    assert!(tr(json!({"id": "t", "from": "a", "to": "b", "kind": "film_burn", "duration": 0.8, "colors": {"color": "#FF0000"}})).is_ok());
    assert!(tr(json!({"id": "t", "from": "a", "to": "b", "kind": "film_burn", "duration": 0.8, "colours": {}})).unwrap_err().contains("colours"));
    // Unknown patch keys are refused by set_clip too.
    let mut p = matte_project();
    let e = try_ap(&mut p, json!({"op": "set_clip", "clip": "u1", "patch": {"motionblur": {}}})).unwrap_err().to_string();
    assert!(e.contains("motionblur") && e.contains("motion_blur"), "{e}");
}

#[test]
fn three_d_rotations_are_params_everywhere() {
    let mut p = matte_project();
    for path in ["transform.rotation_x", "transform.rotation_y"] {
        assert!(param_paths(clip(&p, "u1")).iter().any(|x| x == path), "{path}");
        let spec = clip_param_spec(path).unwrap();
        assert_eq!((spec.default, spec.unit), (0.0, "deg"));
    }
    assert!(SCALAR_PARAM_PATHS.contains(&"transform.perspective"));
    assert_eq!(clip_param_spec("transform.perspective").unwrap().default, 2000.0);
    ap(&mut p, json!({"op": "set_param", "clip": "u1", "path": "transform.rotation_y", "keys": [{"t": 0, "v": -90, "ease": "out"}, {"t": 2, "v": 0}, {"t": 4, "v": 45, "ease": "in_out"}]}));
    ap(&mut p, json!({"op": "add_keyframe", "clip": "u1", "path": "transform.rotation_x", "t": 1.0, "v": 15.0}));
    ap(&mut p, json!({"op": "set_param", "clip": "u1", "path": "transform.perspective", "value": 800.0}));
    assert!(try_ap(&mut p, json!({"op": "set_param", "clip": "u1", "path": "transform.perspective", "keys": [{"t": 0, "v": 1}]})).is_err(), "not animatable");
    assert!(try_ap(&mut p, json!({"op": "set_param", "clip": "u1", "path": "transform.perspective", "value": -5.0})).is_err());
    // Split keeps the curves exactly; the right half's keys are shifted.
    let orig = clip(&p, "u1").clone();
    ap(&mut p, json!({"op": "split_clip", "clip": "u1", "at": 3.0, "new_id": "u2"}));
    let (l, r) = (clip(&p, "u1").clone(), clip(&p, "u2").clone());
    for i in 0..=60 {
        let t = i as f64 * 0.1;
        let want = sample(&orig.transform.rotation_y, t);
        let got = if t <= 3.0 { sample(&l.transform.rotation_y, t) } else { sample(&r.transform.rotation_y, t - 3.0) };
        assert!((want - got).abs() < 1e-9, "at {t}: {want} vs {got}");
    }
    assert_eq!(r.transform.perspective, 800.0);
    // set_speed rescales their keys like every other param.
    ap(&mut p, json!({"op": "set_speed", "clip": "u2", "speed": 2.0}));
    assert!(clip(&p, "u2").transform.rotation_y.is_animated());
    let d = describe(&p, &resolve(&p));
    assert!(d.contains("3d x") && d.contains("persp 800"), "{d}");
}

#[test]
fn matte_ops_keep_references_consistent() {
    let mut p = matte_project();
    ap(&mut p, json!({"op": "set_clip", "clip": "u1", "patch": {"matte": {"clip": "t1", "mode": "alpha"}}}));
    assert_eq!(matte_sources(&p).into_iter().collect::<Vec<_>>(), vec!["t1".to_string()]);
    // Invalid mattes are refused.
    let bad = |p: &mut Project, v: Value| try_ap(p, json!({"op": "set_clip", "clip": "u1", "patch": {"matte": v}})).unwrap_err().to_string();
    assert!(bad(&mut p, json!({"clip": "zz"})).contains("does not exist"));
    assert!(bad(&mut p, json!({"clip": "u1"})).contains("its own matte"));
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "s1", "track": "a1", "asset": "m1", "start": 0.0, "duration": 3.0}}));
    assert!(bad(&mut p, json!({"clip": "s1"})).contains("audio track"));
    let e = try_ap(&mut p, json!({"op": "set_clip", "clip": "t1", "patch": {"matte": {"clip": "u1"}}})).unwrap_err().to_string();
    assert!(e.contains("each other") || e.contains("loop"), "{e}");
    // Duplicating the user copies its matte; describe and the timeline JSON show both sides.
    ap(&mut p, json!({"op": "duplicate_clip", "clip": "u1", "new_id": "u9", "track": "v3", "start": 0.0}));
    assert_eq!(clip(&p, "u9").matte.as_ref().map(|m| m.clip.as_str()), Some("t1"));
    let r = resolve(&p);
    let d = describe(&p, &r);
    assert!(d.contains("matte t1 alpha") && d.contains("matte for u1, u9 (not drawn itself)"), "{d}");
    let j = timeline_json(&p, &r);
    let find = |id: &str| j["tracks"].as_array().unwrap().iter().flat_map(|t| t["clips"].as_array().unwrap().iter()).find(|c| c["id"] == id).unwrap().clone();
    assert_eq!(find("u1")["matte"], json!({"clip": "t1"}));
    assert_eq!(find("t1")["matte_for"], json!(["u1", "u9"]));
    ap(&mut p, json!({"op": "delete_clip", "clip": "u9"}));
    // Splitting the user: both halves keep the matte.
    ap(&mut p, json!({"op": "split_clip", "clip": "u1", "at": 2.0, "new_id": "u1b"}));
    assert_eq!(clip(&p, "u1b").matte.as_ref().map(|m| m.clip.as_str()), Some("t1"));
    // Splitting the matte source: each user follows the piece it overlaps.
    ap(&mut p, json!({"op": "split_clip", "clip": "t1", "at": 3.0, "new_id": "t1b"}));
    assert_eq!(clip(&p, "u1").matte.as_ref().map(|m| m.clip.as_str()), Some("t1"), "u1 (0–2) overlaps t1 (0–3)");
    assert_eq!(clip(&p, "u1b").matte.as_ref().map(|m| m.clip.as_str()), Some("t1b"), "u1b (2–6) overlaps t1b (3–6) most");
    assert!(!codes(&p).iter().any(|c| c.starts_with("matte")), "{:?}", codes(&p));
    // Deleting a matte source removes the reference, with a warning that names the user.
    let o = ap(&mut p, json!({"op": "delete_clip", "clip": "t1b"}));
    assert!(clip(&p, "u1b").matte.is_none());
    assert!(o.warnings.iter().any(|w| w.contains("u1b") && w.contains("without a matte") && !w.contains("t1b")), "{:?}", o.warnings);
    assert_eq!(clip(&p, "u1").matte.as_ref().map(|m| m.clip.as_str()), Some("t1"));
    // A matte source that only plays at another time: warning with a working fix.
    ap(&mut p, json!({"op": "move_clip", "clip": "t1", "start": 20.0}));
    let f = validate(&p, &resolve(&p));
    let w = f.iter().find(|f| f.code == "matte_not_overlapping").expect("finding");
    assert!(w.message.contains("invisible"), "{}", w.message);
    apply_batch(&mut p, &w.fix_ops).unwrap();
    assert!(!codes(&p).iter().any(|c| c == "matte_not_overlapping"));
}

#[test]
fn hand_written_projects_get_matte_and_look_findings() {
    let mut p = matte_project();
    let set = |p: &mut Project, id: &str, f: &dyn Fn(&mut Clip)| f(p.clips.iter_mut().find(|c| c.id == id).unwrap());
    set(&mut p, "u1", &|c| c.matte = Some(Matte { clip: "nope".into(), mode: MatteMode::Alpha }));
    set(&mut p, "t1", &|c| c.motion_blur = Some(MotionBlur { shutter: 900.0, samples: 1 }));
    let f = validate(&p, &resolve(&p));
    for code in ["matte_unknown", "motion_blur_out_of_range"] {
        let x = f.iter().find(|f| f.code == code).unwrap_or_else(|| panic!("{code}: {f:#?}"));
        let mut q = p.clone();
        apply_batch(&mut q, &x.fix_ops).unwrap();
        assert!(!codes(&q).iter().any(|c| c == code), "{code} fixed");
    }
    set(&mut p, "u1", &|c| c.matte = Some(Matte { clip: "t1".into(), mode: MatteMode::Alpha }));
    set(&mut p, "t1", &|c| c.matte = Some(Matte { clip: "u1".into(), mode: MatteMode::Luma }));
    assert!(codes(&p).iter().any(|c| c == "matte_cycle"));
    set(&mut p, "t1", &|c| c.matte = Some(Matte { clip: "t1".into(), mode: MatteMode::Luma }));
    assert!(codes(&p).iter().filter(|c| *c == "matte_cycle").count() >= 1);
    // Every new code is documented.
    for code in ["matte_unknown", "matte_not_visual", "matte_cycle", "matte_not_overlapping", "motion_blur_out_of_range", "unknown_transition_color"] {
        assert!(validate::CODES.iter().any(|(c, _, _)| *c == code), "{code}");
    }
}

#[test]
fn new_effects_and_transitions_are_registered() {
    let fx: &[(&str, &[&str])] = &[
        ("directional_blur", &["angle", "length"]),
        ("glitch", &["amount", "slices", "seed", "rgb"]),
        ("echo", &["count", "interval", "decay"]),
        ("stutter", &["rate", "hold"]),
        ("edge_glow", &["threshold", "radius", "intensity"]),
        ("lens_distortion", &["amount"]),
        ("exposure_flash", &["amount"]),
    ];
    for (kind, params) in fx {
        let s = effect_spec(kind).unwrap_or_else(|| panic!("{kind}"));
        assert_eq!(s.params.iter().map(|p| p.name).collect::<Vec<_>>(), params.to_vec(), "{kind}");
        for p in s.params {
            assert!(!p.help.is_empty() && !p.label.is_empty(), "{kind}.{}", p.name);
            assert!(p.min <= p.default && p.default <= p.max, "{kind}.{}", p.name);
        }
        assert!(!s.help.is_empty());
    }
    assert_eq!(effect_spec("edge_glow").unwrap().color_default("color").unwrap().to_hex(), "#00E5FF");
    let tr: &[(&str, &[&str])] = &[
        ("spin", &["turns", "direction", "blur"]),
        ("zoom_through", &["amount", "blur"]),
        ("shake", &["amount", "angle", "blur"]),
        ("glitch", &["amount", "slices"]),
        ("flash_zoom", &["amount"]),
        ("film_burn", &["seed"]),
        ("cube", &["direction"]),
        ("flip", &["axis"]),
        ("slide_blur", &["direction"]),
        ("stretch", &["direction"]),
        ("luma_wipe", &["softness", "invert"]),
    ];
    for (kind, params) in tr {
        let s = transition_spec(kind).unwrap_or_else(|| panic!("{kind}"));
        assert_eq!(s.params.iter().map(|p| p.name).collect::<Vec<_>>(), params.to_vec(), "{kind}");
        assert!(s.default_duration > 0.0 && !s.help.is_empty());
        for p in s.params {
            assert!(p.min <= p.default && p.default <= p.max, "{kind}.{}", p.name);
        }
    }
    // Effects are added and set like any other, and keyed for beats.
    let mut p = matte_project();
    ap(&mut p, json!({"op": "add_effect", "clip": "u1", "effect": "exposure_flash"}));
    ap(&mut p, json!({"op": "set_param", "clip": "u1", "path": "effects.0.amount", "keys": [{"t": 0, "v": 0}, {"t": 0.1, "v": 1}, {"t": 0.3, "v": 0}]}));
    ap(&mut p, json!({"op": "add_effect", "clip": "u1", "effect": {"kind": "glitch", "params": {"amount": 0.8, "slices": 20}}}));
    assert!(try_ap(&mut p, json!({"op": "add_effect", "clip": "u1", "effect": {"kind": "echo", "params": {"count": 40}}})).is_err(), "out of range");
    // Transitions with colours.
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "a", "track": "v3", "asset": "mv", "start": 0.0, "duration": 3.0}}));
    ap(&mut p, json!({"op": "append_clip", "track": "v3", "clip": {"id": "b", "asset": "mv", "duration": 3.0}}));
    let o = ap(&mut p, json!({"op": "add_transition", "from": "a", "to": "b", "kind": "film_burn"}));
    let tid = o.created[0].clone();
    let t = p.transitions.iter().find(|t| t.id == tid).unwrap();
    assert_eq!((t.duration, transition_color(t, "color").to_hex()), (0.8, "#FF8A2B".to_string()));
    ap(&mut p, json!({"op": "set_transition_colors", "transition": tid, "colors": {"color": "#FF0040"}}));
    assert_eq!(transition_color(p.transitions.iter().find(|t| t.id == tid).unwrap(), "color").to_hex(), "#FF0040");
    assert!(try_ap(&mut p, json!({"op": "set_transition_colors", "transition": tid, "colors": {"tint": "#FF0040"}})).unwrap_err().to_string().contains("no colour"));
    let text = document::canonical_json(&p);
    assert_eq!(document::parse_project(&text).unwrap(), p);
    // Changing the kind drops colours the new kind does not have.
    ap(&mut p, json!({"op": "set_transition", "transition": tid, "kind": "cube", "params": {"direction": 2}}));
    assert!(p.transitions.iter().find(|t| t.id == tid).unwrap().colors.is_empty());
    assert!(try_ap(&mut p, json!({"op": "set_transition", "transition": tid, "params": {"direction": 7}})).is_err());
    assert!(try_ap(&mut p, json!({"op": "set_transition_colors", "transition": tid, "colors": {}})).is_err());
}

#[test]
fn schema_docs_cover_round6() {
    let d = schema_docs();
    for f in ["motion_blur", "frame_blend", "matte"] {
        assert!(d["types"]["clip"]["fields"][f].is_object(), "{f}");
    }
    for f in ["rotation_x", "rotation_y", "perspective"] {
        assert!(d["types"]["transform"]["fields"][f].is_object(), "{f}");
    }
    assert_eq!(d["types"]["motion_blur"]["fields"]["shutter"]["default"], json!(180.0));
    assert_eq!(d["types"]["motion_blur"]["fields"]["samples"]["default"], json!(12));
    assert_eq!(d["types"]["transform"]["fields"]["perspective"]["default"], json!(2000.0));
    assert_eq!(d["enums"]["matte_mode"], json!(["alpha", "alpha_inverted", "luma", "luma_inverted"]));
    assert!(d["types"]["transition"]["fields"]["colors"].is_object());
}
