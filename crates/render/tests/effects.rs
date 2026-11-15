//! Every registry effect changes pixels in the expected direction.

mod common;
use aiframes_core::effects::EFFECTS;
use aiframes_core::{Effect, Project};
use aiframes_render::Image;
use common::*;
use serde_json::{json, Value};

/// halves.png (red | blue) stretched full frame with `effects`, over a white or black background.
fn with_fx(effects: Value) -> Project {
    let mut p = project(160, 90, 30.0);
    add_media(&mut p, "m1", &halves_png());
    p.clips = vec![clip(json!({"id": "c1", "track": "v1", "start": 0, "duration": 4, "source": {"type": "media", "asset": "m1"},
        "transform": {"fit": "stretch"}, "effects": effects}))];
    p
}

/// testsrc2 (colours, gradients, text) as a still, for the "every effect" sweep.
fn with_fx_src(effects: Value) -> Project {
    let png = fixture("testsrc.png", |out| ffmpeg(&["-f", "lavfi", "-i", "testsrc2=s=160x90", "-frames:v", "1", out]));
    let mut p = project(160, 90, 30.0);
    add_media(&mut p, "m1", &png);
    p.clips = vec![clip(json!({"id": "c1", "track": "v1", "start": 0, "duration": 4, "source": {"type": "media", "asset": "m1"},
        "transform": {"fit": "stretch"}, "effects": effects}))];
    p
}

fn fx(kind: &str, params: Value) -> Value {
    json!([{"kind": kind, "params": params}])
}

fn edge_energy(img: &Image) -> f64 {
    let mut s = 0.0;
    for y in 0..img.height {
        for x in 1..img.width {
            let a = px(img, x - 1, y);
            let b = px(img, x, y);
            s += (0..3).map(|k| (a[k] as f64 - b[k] as f64).powi(2)).sum::<f64>();
        }
    }
    s
}

#[test]
fn colour_correction_directions() {
    let r = renderer();
    let base = render(&r, &with_fx(json!([])), 1.0, 1.0);
    let m = mean_rgb(&base);
    let run = |params: Value| render(&r, &with_fx(fx("color", params)), 1.0, 1.0);
    assert!(mean_rgb(&run(json!({"exposure": 1.0})))[0] >= m[0] - 0.5 && mean_rgb(&run(json!({"exposure": -1.0})))[0] < m[0] - 20.0);
    assert!(mean_rgb(&run(json!({"brightness": -0.3})))[0] < m[0] - 30.0);
    let grey = run(json!({"saturation": 0.0}));
    let q = px(&grey, 20, 45);
    assert!((q[0] as i32 - q[1] as i32).abs() <= 2 && (q[1] as i32 - q[2] as i32).abs() <= 2, "{q:?}");
    let warm = run(json!({"temperature": 1.0}));
    let w = px(&warm, 140, 45); // blue half
    assert!(w[2] < 240, "warmer pulls blue down: {w:?}");
    let cool = run(json!({"temperature": -1.0}));
    assert!(px(&cool, 20, 45)[0] < 240, "cooler pulls red down");
    let flat = run(json!({"contrast": -1.0}));
    assert!(near(px(&flat, 20, 45), [128, 128, 128, 255], 2));
    let hue = run(json!({"hue": 120.0}));
    let h = px(&hue, 20, 45);
    assert!(h[1] > h[0] && h[1] > h[2], "red rotated 120° is green: {h:?}");
    let g = run(json!({"gamma": 2.0, "brightness": -0.5}));
    assert!(px(&g, 20, 45)[0] > px(&run(json!({"brightness": -0.5})), 20, 45)[0] + 30, "gamma > 1 brightens mid-tones");
    let sh = run(json!({"shadows": 1.0, "saturation": 0.0}));
    assert!(px(&sh, 20, 45)[0] > px(&run(json!({"saturation": 0.0})), 20, 45)[0]);
    let vib = run(json!({"contrast": -0.5, "vibrance": -1.0}));
    assert!(px(&vib, 20, 45)[1] > px(&run(json!({"contrast": -0.5})), 20, 45)[1] + 5, "negative vibrance desaturates muted colours");
    let sat = run(json!({"vibrance": -1.0}));
    assert!(near(px(&sat, 20, 45), px(&base, 20, 45), 1), "vibrance leaves saturated colours alone");
}

#[test]
fn every_effect_does_something() {
    let r = renderer();
    let dir = scratch("lut");
    // An inverting 3D LUT.
    let mut cube = String::from("LUT_3D_SIZE 2\n");
    for b in 0..2 {
        for g in 0..2 {
            for rr in 0..2 {
                cube += &format!("{} {} {}\n", 1 - rr, 1 - g, 1 - b);
            }
        }
    }
    let lut_path = dir.join("invert.cube");
    std::fs::write(&lut_path, cube).unwrap();
    let base = render(&r, &with_fx_src(json!([])), 1.0, 1.0);
    for spec in EFFECTS {
        let mut e: Effect = spec.instantiate();
        if spec.kind == "lut" {
            e.strings.insert("path".into(), lut_path.to_string_lossy().into_owned());
        }
        if spec.kind == "chroma_key" {
            e.colors.insert("key".into(), aiframes_core::Color::rgba8(255, 0, 0, 255));
        }
        if spec.kind == "color" {
            e.params.insert("exposure".into(), (-1.0).into());
        }
        if spec.kind == "zoom_blur" {
            e.params.insert("x".into(), 0.3.into());
        }
        if spec.kind == "echo" || spec.kind == "stutter" {
            continue; // time effects: invisible on a still picture (tested in tests/edit_culture.rs)
        }
        if spec.kind == "drop_shadow" || spec.kind == "glow" {
            continue; // need transparency around the content / soft detail: tested separately
        }
        let p = with_fx_src(serde_json::to_value(vec![e]).unwrap());
        let img = render(&r, &p, 1.0, 1.0);
        assert!(diff(&img, &base) > 0.2, "{} changed nothing ({})", spec.kind, diff(&img, &base));
    }
}

#[test]
fn specific_effect_directions() {
    let r = renderer();
    let base = render(&r, &with_fx(json!([])), 1.0, 1.0);
    let run = |e: Value| render(&r, &with_fx(e), 1.0, 1.0);
    // Blur softens the red|blue edge, sharpen strengthens edges.
    assert!(edge_energy(&run(fx("blur", json!({"radius": 6})))) < edge_energy(&base) * 0.9);
    let soft = run(fx("blur", json!({"radius": 3})));
    let sharp = run(json!([{"kind": "blur", "params": {"radius": 3}}, {"kind": "sharpen", "params": {"amount": 2.0, "radius": 2}}]));
    assert!(edge_energy(&sharp) > edge_energy(&soft) * 1.2, "sharpen restores edges");
    // Vignette darkens the corners, not the centre.
    let v = run(fx("vignette", json!({"amount": 1.0})));
    assert!(px(&v, 1, 1)[0] < px(&base, 1, 1)[0] / 2 && near(px(&v, 70, 45), px(&base, 70, 45), 3));
    // Chroma key red: the red half becomes the (black) background; spill keeps blue.
    let mut k = aiframes_core::effects::effect_spec("chroma_key").unwrap().instantiate();
    k.colors.insert("key".into(), aiframes_core::Color::rgba8(255, 0, 0, 255));
    let ck = run(serde_json::to_value(vec![k]).unwrap());
    assert!(px(&ck, 20, 45)[0] < 10 && px(&ck, 140, 45)[2] > 200);
    // Masks.
    let m = run(fx("mask_rect", json!({"x": 0.5, "y": 0.5, "width": 0.5, "height": 0.5})));
    assert_eq!(px(&m, 2, 2), [0, 0, 0, 255]);
    assert_eq!(px(&m, 60, 45), px(&base, 60, 45));
    let mf = run(fx("mask_rect", json!({"width": 0.5, "height": 0.5, "feather": 20})));
    let edge = px(&mf, 40, 45)[0];
    assert!(edge > 40 && edge < 220, "feathered edge is partial: {edge}");
    let me = run(fx("mask_ellipse", json!({"width": 1.0, "height": 1.0})));
    assert_eq!(px(&me, 1, 1), [0, 0, 0, 255]);
    assert_eq!(px(&me, 40, 45), px(&base, 40, 45));
    // Pixelate: blocks are flat.
    let px8 = run(fx("pixelate", json!({"size": 8})));
    assert_eq!(px(&px8, 72, 8), px(&px8, 79, 15));
    // Grain is deterministic per frame and changes between frames.
    let g = with_fx(fx("grain", json!({"amount": 0.5})));
    let (g1, g1b, g2) = (render(&r, &g, 1.0, 1.0), render(&r, &g, 1.0, 1.0), render(&r, &g, 1.0 + 1.0 / 30.0, 1.0));
    assert_eq!(g1, g1b);
    assert!(diff(&g1, &g2) > 1.0);
    // Black & white, invert, posterize.
    let bw = run(fx("black_white", json!({})));
    let q = px(&bw, 20, 45);
    assert!(q[0] == q[1] && q[1] == q[2]);
    let inv = run(fx("invert", json!({})));
    assert!(near(px(&inv, 20, 45), [0, 255, 255, 255], 3));
    let po = render(&r, &with_fx_src(fx("posterize", json!({"levels": 2}))), 1.0, 1.0);
    assert!(po.rgba.as_chunks::<4>().0.iter().all(|q| q[..3].iter().all(|v| *v < 3 || *v > 252)));
    // Letterbox: 2.39 on 16:9 → bars of (90 - 160/2.39)/2 ≈ 11.5 px.
    let lb = run(fx("letterbox", json!({})));
    assert_eq!(px(&lb, 80, 5), [0, 0, 0, 255]);
    assert_eq!(px(&lb, 20, 45), px(&base, 20, 45));
    // Zoom blur keeps the centre, streaks the edge.
    let zb = run(fx("zoom_blur", json!({"amount": 0.5, "x": 0.25})));
    assert!(diff(&zb, &base) > 1.0);
    // RGB split: at the red|blue edge red moves right and blue left.
    let rs = run(fx("rgb_split", json!({"amount": 10})));
    assert!(px(&rs, 83, 45)[0] > 100, "red pushed over the edge");
    // Shake is deterministic.
    let sk = with_fx(fx("shake", json!({"amount": 20})));
    assert_eq!(render(&r, &sk, 0.7, 1.0), render(&r, &sk, 0.7, 1.0));
    assert!(diff(&render(&r, &sk, 0.7, 1.0), &render(&r, &sk, 1.4, 1.0)) > 0.5);
    // Animated parameter: blur radius keys (clip-local).
    let anim = with_fx(json!([{"kind": "blur", "params": {"radius": {"keys": [{"t": 0, "v": 0}, {"t": 2, "v": 10}]}}}]));
    assert!(diff(&render(&r, &anim, 0.0, 1.0), &base) < 0.01);
    assert!(edge_energy(&render(&r, &anim, 2.0, 1.0)) < edge_energy(&render(&r, &anim, 1.0, 1.0)));
}

#[test]
fn shadow_and_glow_spread_beyond_the_content() {
    let r = renderer();
    let mut p = project(200, 200, 30.0);
    let mut c = solid("c1", "v2", 0.0, 2.0, "#FFFFFF");
    c.transform.scale = 0.3.into();
    let mut bg = solid("bg", "v1", 0.0, 2.0, "#808080");
    bg.enabled = true;
    c.effects = serde_json::from_value(json!([{"kind": "drop_shadow", "params": {"distance": 20, "angle": 45, "blur": 4, "opacity": 1}}])).unwrap();
    p.clips = vec![bg, c.clone()];
    let img = render(&r, &p, 0.5, 1.0);
    // Box spans 70..130; shadow offset (14, 14) shows below-right of it, not above-left.
    assert!(px(&img, 138, 138)[0] < 60, "{:?}", px(&img, 138, 138));
    assert!(px(&img, 62, 62)[0] > 120);
    assert_eq!(px(&img, 100, 100), [255, 255, 255, 255]);
    // Glow brightens just outside a white box on black.
    c.effects = serde_json::from_value(json!([{"kind": "glow", "params": {"radius": 30, "threshold": 0.5}}])).unwrap();
    p.clips = vec![c];
    let img = render(&r, &p, 0.5, 1.0);
    assert!(px(&img, 136, 100)[0] > 20, "{:?}", px(&img, 136, 100));
}
