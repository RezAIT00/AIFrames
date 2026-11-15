//! Round 6 edit-culture toolkit: motion blur, frame blending, 3-D cards, track mattes, the new
//! effects and the new transitions.

mod common;
use aiframes_core::{Project, Transition};
use aiframes_render::{FrameOptions, Image};
use common::*;
use serde_json::{json, Value};
use std::path::Path;

fn base(w: u32, h: u32, fps: f64, clips: Value) -> Project {
    let mut p = project(w, h, fps);
    p.clips = serde_json::from_value(clips).unwrap();
    p
}

fn frame(p: &Project, t: f64) -> Image {
    render(&renderer(), p, t, 1.0)
}

/// Columns / rows where a channel is partly on (between `lo` and `hi`), along one line.
fn partial_run(img: &Image, horizontal: bool, at: u32, ch: usize, lo: u8, hi: u8) -> u32 {
    let n = if horizontal { img.width } else { img.height };
    (0..n)
        .filter(|&i| {
            let q = if horizontal { px(img, i, at) } else { px(img, at, i) };
            q[ch] > lo && q[ch] < hi
        })
        .count() as u32
}

// ---------------------------------------------------------------------------------------------
// Motion blur, frame blending
// ---------------------------------------------------------------------------------------------

/// A white box crossing the frame fast: motion blur smears it along the motion only; a still clip
/// with motion blur is exactly the unblurred picture.
#[test]
fn motion_blur_smears_along_the_motion() {
    let mk = |mb: bool, moving: bool| {
        let x = if moving { json!({"keys": [{"t": 0, "v": -150}, {"t": 1, "v": 150}]}) } else { json!(0) };
        let mut c = json!({"id": "c", "track": "v2", "start": 0, "duration": 1, "source": {"type": "solid", "color": "#FFFFFF"},
            "transform": {"scale": 0.25, "x": x}});
        if mb {
            c["motion_blur"] = json!({"shutter": 360, "samples": 32});
        }
        base(320, 180, 30.0, json!([c]))
    };
    let sharp = frame(&mk(false, true), 0.5);
    let blurred = frame(&mk(true, true), 0.5);
    // 300 px/s at 30 fps, 360 degree shutter: a 10 px smear on each side edge, none vertically.
    let h_sharp = partial_run(&sharp, true, 90, 0, 10, 245);
    let h_blur = partial_run(&blurred, true, 90, 0, 10, 245);
    let v_blur = partial_run(&blurred, false, 160, 0, 10, 245);
    assert!(h_sharp <= 2 && h_blur >= 14, "horizontal edges: sharp {h_sharp} px, blurred {h_blur} px");
    assert!(v_blur <= 2, "vertical edges stay sharp: {v_blur}");
    // The smear is centred on the frame's position (shutter centred on the frame).
    let ink = |img: &Image| bbox(img, |q| q[0] > 128).unwrap();
    let (a, b) = (ink(&sharp), ink(&blurred));
    assert!((a[0] as i32 - b[0] as i32).abs() <= 1 && (a[2] as i32 - b[2] as i32).abs() <= 1, "{a:?} {b:?}");
    // Still: identical.
    assert_eq!(frame(&mk(true, false), 0.5), frame(&mk(false, false), 0.5));
    // Rotation smears tangentially: the box corner far from the pivot blurs, the centre does not.
    let spin = base(320, 180, 30.0, json!([{"id": "c", "track": "v2", "start": 0, "duration": 1, "source": {"type": "solid", "color": "#FFFFFF"},
        "transform": {"scale": 0.4, "rotation": {"keys": [{"t": 0, "v": 0}, {"t": 1, "v": 180}]}}, "motion_blur": {"shutter": 180, "samples": 24}}]));
    let img = frame(&spin, 0.5);
    assert_eq!(px(&img, 160, 90), [255, 255, 255, 255]);
    assert!(img.rgba.as_chunks::<4>().0.iter().filter(|q| q[0] > 20 && q[0] < 235).count() > 300, "rotational smear at the corners");
}

/// Frame blending: at 4x speed each output frame averages the four source frames it covers.
#[test]
fn frame_blend_averages_the_covered_source_frames() {
    let mut p = project(320, 180, 30.0);
    add_media(&mut p, "m1", &bars(30, 6));
    let mk = |blend: bool| {
        let mut q = p.clone();
        q.clips = vec![clip(json!({"id": "c", "track": "v1", "start": 0, "duration": 1, "speed": 4.0, "frame_blend": blend,
            "source": {"type": "media", "asset": "m1"}}))];
        q
    };
    let plain = frame(&mk(false), 0.5);
    let blended = frame(&mk(true), 0.5);
    // Without blending the bars are crisp (only black/white); blending 4 frames yields greys.
    let greys = |img: &Image| (0..9).filter(|k| { let v = px(img, ((*k as f64 + 0.5) * 320.0 / 9.0) as u32, 90)[0]; v > 40 && v < 215 }).count();
    assert_eq!(greys(&plain), 0);
    assert!(greys(&blended) >= 2, "blended bars show intermediate values");
    // Frames 60..63 are averaged: bit 2 (value 4) is on in 60..63 → stays fully white; bit 0
    // alternates → about half.
    let bit = |k: u32| px(&blended, ((k as f64 + 0.5) * 320.0 / 9.0) as u32, 90)[0];
    assert!(bit(2) > 200, "{}", bit(2));
    assert!((bit(0) as i32 - 128).abs() < 40, "{}", bit(0));
    // At 1x it changes nothing.
    let mut one = mk(true);
    one.clips[0].speed = 1.0.into();
    let mut one_plain = one.clone();
    one_plain.clips[0].frame_blend = false;
    assert_eq!(frame(&one, 0.5), frame(&one_plain, 0.5));
}

// ---------------------------------------------------------------------------------------------
// 3-D cards
// ---------------------------------------------------------------------------------------------

#[test]
fn three_d_card_is_a_perspective_trapezoid_matching_layers_at() {
    let p = base(400, 300, 30.0, json!([{"id": "c", "track": "v2", "start": 0, "duration": 1, "source": {"type": "solid", "color": "#FFFFFF"},
        "transform": {"scale": 0.5, "rotation_y": 50, "perspective": 600}}]));
    let r = renderer();
    let res = resolved(&p);
    let img = render(&r, &p, 0.0, 1.0);
    let boxes = r.layers_at(Path::new("."), &p, &res, 0.0);
    let b = &boxes[0];
    let ink = bbox(&img, |q| q[0] > 128).unwrap();
    let want = [b.bounds[0], b.bounds[1], b.bounds[0] + b.bounds[2], b.bounds[1] + b.bounds[3]];
    for k in 0..4 {
        assert!((ink[k] as f32 - want[k]).abs() <= 1.5, "drawn {ink:?} vs layers_at {want:?}");
    }
    // + turn: the right edge recedes (shorter), the left edge comes closer (taller).
    let col = |x: u32| (0..300).filter(|&y| px(&img, x, y)[0] > 128).count() as i32;
    let (left, right) = (col(ink[0] + 2), col(ink[2] - 3));
    assert!(left > right + 10, "left {left} right {right}");
    // Corners are reported in visual order and the left side is taller.
    let c = b.corners;
    assert!(c[0][0] < c[1][0] && (c[3][1] - c[0][1]) > (c[2][1] - c[1][1]));
    // The pivot is the projected anchor (frame centre).
    assert!((b.pivot[0] - 200.0).abs() < 0.01 && (b.pivot[1] - 150.0).abs() < 0.01);
}

/// Back faces are visible, mirrored: a card turned 180 degrees shows its picture flipped.
#[test]
fn three_d_back_face_is_mirrored() {
    let mut p = project(200, 100, 30.0);
    add_media(&mut p, "m1", &halves_png());
    p.clips = vec![clip(json!({"id": "c", "track": "v1", "start": 0, "duration": 1, "source": {"type": "media", "asset": "m1"},
        "transform": {"fit": "stretch", "rotation_y": 180, "perspective": 0}}))];
    let img = frame(&p, 0.0);
    assert!(px(&img, 20, 50)[2] > 200 && px(&img, 180, 50)[0] > 200, "{:?} {:?}", px(&img, 20, 50), px(&img, 180, 50));
}

// ---------------------------------------------------------------------------------------------
// Track mattes
// ---------------------------------------------------------------------------------------------

fn matte_project(mode: &str) -> Project {
    base(640, 360, 30.0, json!([
        {"id": "bg", "track": "v1", "start": 0, "duration": 2, "source": {"type": "solid", "color": "#0000FF"}},
        {"id": "fill", "track": "v2", "start": 0, "duration": 2, "source": {"type": "solid", "color": "#FF0000"}, "matte": {"clip": "txt", "mode": mode}},
        {"id": "txt", "track": "v3", "start": 0, "duration": 2, "source": {"type": "text", "text": "HI", "size": 200, "weight": 900}},
    ]))
}

#[test]
fn alpha_matte_confines_the_clip_to_the_text() {
    let img = frame(&matte_project("alpha"), 0.5);
    let red = bbox(&img, |q| q[0] > 128 && q[2] < 100).expect("red inside the text");
    // The text clip alone (white) for reference: red sits exactly where the glyphs are.
    let mut text_only = matte_project("alpha");
    text_only.clips.retain(|c| c.id != "fill");
    let glyphs = bbox(&frame(&text_only, 0.5), |q| q[0] > 128 && q[1] > 128).unwrap();
    for k in 0..4 {
        assert!((red[k] as i32 - glyphs[k] as i32).abs() <= 1, "red {red:?} vs glyphs {glyphs:?}");
    }
    // The matte source itself is not drawn: no white pixels anywhere.
    assert!(img.rgba.as_chunks::<4>().0.iter().all(|q| !(q[0] > 200 && q[1] > 200)), "matte source drawn");
    // Crisp: across a glyph edge the transition from blue to red takes at most 2 pixels.
    let row = (red[1] + red[3]) / 2;
    let mixed = (red[0].saturating_sub(4)..red[0] + 6).filter(|&x| { let q = px(&img, x, row); q[0] > 30 && q[2] > 30 }).count();
    assert!(mixed <= 2, "soft edge: {mixed} px");
    // Inverted: red everywhere but the text.
    let inv = frame(&matte_project("alpha_inverted"), 0.5);
    assert!(px(&inv, 5, 5)[0] > 200);
    let c = ((red[0] + red[2]) / 2, (red[1] + red[3]) / 2);
    assert!(px(&inv, red[0] + 20, c.1)[2] > 200 || px(&inv, c.0, c.1)[2] > 200);
}

#[test]
fn luma_matte_uses_brightness() {
    let mk = |mode: &str| {
        base(200, 100, 30.0, json!([
            {"id": "fill", "track": "v2", "start": 0, "duration": 2, "source": {"type": "solid", "color": "#FF0000"}, "matte": {"clip": "m", "mode": mode}},
            {"id": "m", "track": "v3", "start": 0, "duration": 2, "source": {"type": "solid", "color": "#808080"}},
        ]))
    };
    let luma = frame(&mk("luma"), 0.5);
    let r = px(&luma, 100, 50)[0] as i32;
    // Rec.709 luma of #808080 = 0.502 → about half of the red over black.
    assert!((r - 128).abs() <= 4, "{r}");
    let inv = frame(&mk("luma_inverted"), 0.5);
    assert!((px(&inv, 100, 50)[0] as i32 - 127).abs() <= 4);
    // A matte that is not on screen at that time hides an alpha-matted clip entirely.
    let mut gone = matte_project("alpha");
    gone.clips[2].start = aiframes_core::Time::At(5.0);
    let img = frame(&gone, 0.5);
    assert!(bbox(&img, |q| q[0] > 128 && q[2] < 100).is_none());
}

// ---------------------------------------------------------------------------------------------
// Effects
// ---------------------------------------------------------------------------------------------

fn fx_on_box(effects: Value, extra: Value) -> Project {
    let mut c = json!({"id": "c", "track": "v2", "start": 0, "duration": 2, "source": {"type": "solid", "color": "#FFFFFF"},
        "transform": {"scale": 0.3}, "effects": effects});
    for (k, v) in extra.as_object().unwrap() {
        c[k] = v.clone();
    }
    base(320, 180, 30.0, json!([c]))
}

#[test]
fn directional_blur_follows_its_angle() {
    let h = frame(&fx_on_box(json!([{"kind": "directional_blur", "params": {"angle": 0, "length": 40}}]), json!({})), 0.5);
    let v = frame(&fx_on_box(json!([{"kind": "directional_blur", "params": {"angle": 90, "length": 40}}]), json!({})), 0.5);
    assert!(partial_run(&h, true, 90, 0, 10, 245) > 20 && partial_run(&h, false, 160, 0, 10, 245) <= 3);
    assert!(partial_run(&v, false, 160, 0, 10, 245) > 20 && partial_run(&v, true, 90, 0, 10, 245) <= 3);
}

#[test]
fn glitch_is_deterministic_per_frame_and_rerolls() {
    let mut p = project(320, 180, 30.0);
    add_media(&mut p, "m1", &halves_png());
    p.clips = vec![clip(json!({"id": "c", "track": "v1", "start": 0, "duration": 2, "source": {"type": "media", "asset": "m1"},
        "transform": {"fit": "stretch"}, "effects": [{"kind": "glitch", "params": {"amount": 0.8, "slices": 20, "rgb": 12}}]}))];
    let plain = {
        let mut q = p.clone();
        q.clips[0].effects.clear();
        frame(&q, 0.5)
    };
    let a = frame(&p, 0.5);
    assert_eq!(a, frame(&p, 0.5));
    assert!(diff(&a, &plain) > 2.0);
    assert!(diff(&a, &frame(&p, 0.5 + 1.0 / 30.0)) > 1.0, "re-rolls on the next frame");
    // Rows move sideways as whole bands: the red|blue boundary jumps in some rows.
    let boundary = |y: u32| (0..320).find(|&x| px(&a, x, y)[2] > 128).unwrap_or(320);
    let xs: std::collections::BTreeSet<u32> = (0..180).step_by(3).map(boundary).collect();
    assert!(xs.len() > 3, "{xs:?}");
}

#[test]
fn echo_trails_behind_a_moving_clip() {
    let moving = json!({"transform": {"scale": 0.2, "x": {"keys": [{"t": 0, "v": -120}, {"t": 2, "v": 120}]}}});
    let plain = frame(&fx_on_box(json!([]), moving.clone()), 1.0);
    let echo = frame(&fx_on_box(json!([{"kind": "echo", "params": {"count": 3, "interval": 0.1, "decay": 0.5}}]), moving), 1.0);
    let lit = |img: &Image| bbox(img, |q| q[0] > 20).unwrap();
    let (a, b) = (lit(&plain), lit(&echo));
    // The box moves right at 120 px/s: three echoes reach 36 px further left, nothing ahead.
    assert!(b[0] + 30 < a[0] && b[2] == a[2], "plain {a:?} echo {b:?}");
    // Echo k is at decay^k, each under the newer ones (the 64 px box: echoes 12, 24 and 36 px
    // behind). Left of the box: echoes 1-3 over each other, then 2-3, then 3 alone.
    let over = |ks: &[i32]| 255.0 * (1.0 - ks.iter().map(|&k| 1.0 - 0.5f64.powi(k)).product::<f64>());
    for (x, ks) in [(a[0] - 6, &[1, 2, 3][..]), (a[0] - 18, &[2, 3][..]), (a[0] - 30, &[3][..])] {
        let q = px(&echo, x, 90)[0] as f64;
        assert!((q - over(ks)).abs() < 12.0, "x {x}: {q} vs {}", over(ks));
    }
}

#[test]
fn stutter_holds_then_jumps() {
    let moving = json!({"transform": {"scale": 0.2, "x": {"keys": [{"t": 0, "v": -150}, {"t": 2, "v": 150}]}}});
    let p = fx_on_box(json!([{"kind": "stutter", "params": {"rate": 5, "hold": 1.0}}]), moving);
    // Steps of 0.2 s fully held: frames 0.2 .. 0.39 are identical, 0.4 jumps.
    let f = |t: f64| frame(&p, t);
    assert_eq!(f(0.2), f(0.3));
    assert_eq!(f(0.2), f(0.2 + 5.0 / 30.0));
    assert!(diff(&f(0.2), &f(0.4)) > 0.5);
    // Half hold: frozen for the first half of each step, then moving.
    let q = fx_on_box(json!([{"kind": "stutter", "params": {"rate": 5, "hold": 0.5}}]), json!({"transform": {"scale": 0.2, "x": {"keys": [{"t": 0, "v": -150}, {"t": 2, "v": 150}]}}}));
    assert_eq!(frame(&q, 0.4), frame(&q, 0.4 + 2.0 / 30.0));
    assert!(diff(&frame(&q, 0.4 + 4.0 / 30.0), &frame(&q, 0.4 + 5.0 / 30.0)) > 0.1);
}

#[test]
fn edge_glow_lens_and_flash() {
    let plain = frame(&fx_on_box(json!([]), json!({})), 0.5);
    // Edge glow: cyan light just outside the box edge, nothing far away.
    let g = frame(&fx_on_box(json!([{"kind": "edge_glow", "params": {"radius": 10, "intensity": 2}}]), json!({})), 0.5);
    let ink = bbox(&plain, |q| q[0] > 128).unwrap();
    let out = px(&g, ink[0] - 4, 90);
    assert!(out[1] > 40 && out[2] > 40 && out[0] < out[2], "{out:?}");
    assert_eq!(px(&g, 5, 5), px(&plain, 5, 5));
    // Lens distortion: the centre stays, the box (centred) bulges bigger with barrel.
    let l = frame(&fx_on_box(json!([{"kind": "lens_distortion", "params": {"amount": 0.5}}]), json!({})), 0.5);
    let lb = bbox(&l, |q| q[0] > 128).unwrap();
    assert!(lb[2] - lb[0] > ink[2] - ink[0] + 6, "{lb:?} vs {ink:?}");
    assert_eq!(px(&l, 160, 90), [255, 255, 255, 255]);
    // Exposure flash brightens a grey clip, keyable.
    let grey = |amount: f64| {
        base(64, 36, 30.0, json!([{"id": "c", "track": "v1", "start": 0, "duration": 1, "source": {"type": "solid", "color": "#404040"},
            "effects": [{"kind": "exposure_flash", "params": {"amount": amount}}]}]))
    };
    assert!(px(&frame(&grey(0.5), 0.2), 10, 10)[0] > 64 + 100);
    assert_eq!(px(&frame(&grey(0.0), 0.2), 10, 10), [64, 64, 64, 255]);
}

// ---------------------------------------------------------------------------------------------
// Transitions
// ---------------------------------------------------------------------------------------------

fn pair(kind: &str, params: Value) -> Project {
    let mut p = project(160, 90, 30.0);
    add_media(&mut p, "red", &flat_video("red4", "red", 4));
    add_media(&mut p, "bars", &bars(30, 6));
    p.clips = vec![
        clip(json!({"id": "A", "track": "v1", "start": 0, "duration": 2, "source": {"type": "media", "asset": "red"}})),
        clip(json!({"id": "B", "track": "v1", "start": {"after": "A"}, "duration": 2, "source": {"type": "media", "asset": "bars", "in_point": 1.0}})),
    ];
    let mut t: Transition = serde_json::from_value(json!({"id": "tr1", "from": "A", "to": "B", "kind": kind, "duration": 1.0})).unwrap();
    t.params = serde_json::from_value(params).unwrap();
    p.transitions = vec![t];
    p
}

#[test]
fn glitch_and_film_burn_are_deterministic() {
    for (kind, params) in [("glitch", json!({})), ("film_burn", json!({"seed": 4}))] {
        let p = pair(kind, params);
        let a = frame(&p, 1.9);
        assert_eq!(a, frame(&p, 1.9), "{kind}");
        assert!(diff(&a, &frame(&p, 1.9 + 1.0 / 30.0)) > 0.1, "{kind} animates");
    }
    // Another seed burns another pattern; the burn is warm (orange) light.
    let a = frame(&pair("film_burn", json!({"seed": 1})), 1.85);
    let b = frame(&pair("film_burn", json!({"seed": 2})), 1.85);
    assert!(diff(&a, &b) > 1.0);
    let warm = a.rgba.as_chunks::<4>().0.iter().filter(|q| q[0] > 200 && q[1] > 80 && q[2] < q[1]).count();
    assert!(warm > 200, "warm pixels {warm}");
}

#[test]
fn luma_wipe_reveals_bright_areas_first() {
    // Incoming: a still picture, white left half, black right half; outgoing: flat red.
    let halves = fixture("white_black.png", |out| {
        ffmpeg(&["-f", "lavfi", "-i", "color=c=black:s=160x90,format=rgb24,geq=r='if(lt(X,80),255,0)':g='if(lt(X,80),255,0)':b='if(lt(X,80),255,0)'", "-frames:v", "1", out]);
    });
    let mk = |params: Value| {
        let mut p = pair("luma_wipe", params);
        add_media(&mut p, "wb", &halves);
        p.clips[1].source = serde_json::from_value(json!({"type": "media", "asset": "wb"})).unwrap();
        p
    };
    let is_red = |q: [u8; 4]| q[0] > 200 && q[1] < 60 && q[2] < 60;
    // Early in the wipe (p = 0.2) only the bright half has arrived; the end is the full picture.
    let img = frame(&mk(json!({"softness": 0.05})), 1.7);
    assert!(near(px(&img, 40, 45), [255, 255, 255, 255], 3), "{:?}", px(&img, 40, 45));
    assert!(is_red(px(&img, 120, 45)), "{:?}", px(&img, 120, 45));
    let late = frame(&mk(json!({"softness": 0.05})), 2.5);
    assert!(near(px(&late, 120, 45), [0, 0, 0, 255], 3), "{:?}", px(&late, 120, 45));
    // Inverted: dark first.
    let inv = frame(&mk(json!({"softness": 0.05, "invert": 1})), 1.7);
    assert!(near(px(&inv, 120, 45), [0, 0, 0, 255], 3), "{:?}", px(&inv, 120, 45));
    assert!(is_red(px(&inv, 40, 45)), "{:?}", px(&inv, 40, 45));
}

#[test]
fn motion_transitions_report_their_faces_in_layers_at() {
    let r = renderer();
    // Cube half way: both faces, each a projected quad inside the frame.
    let p = pair("cube", json!({}));
    let res = resolved(&p);
    let boxes = r.layers_at(Path::new("."), &p, &res, 2.0);
    assert_eq!(boxes.len(), 2, "{boxes:?}");
    for b in &boxes {
        assert!(b.bounds[2] < 160.0 && b.bounds[3] <= 92.0, "{:?}", b.bounds);
    }
    // Spin: one face (the outgoing clip before the cut), centred, rotated.
    let p = pair("spin", json!({}));
    let res = resolved(&p);
    let boxes = r.layers_at(Path::new("."), &p, &res, 1.8);
    assert_eq!(boxes.len(), 1);
    assert_eq!(boxes[0].clip, "A");
    let c = boxes[0].corners;
    assert!((c[0][1] - c[1][1]).abs() > 1.0, "rotated: {c:?}");
}

#[test]
fn every_new_transition_is_exact_outside_its_window_on_real_frames() {
    let kinds = ["spin", "zoom_through", "shake", "glitch", "flash_zoom", "film_burn", "cube", "flip", "slide_blur", "stretch", "luma_wipe"];
    let plain = {
        let mut p = pair("crossfade", json!({}));
        p.transitions.clear();
        p
    };
    // B is the frame-code bars (a different picture every frame): compare at the same times.
    let reference: Vec<Image> = [1.4, 1.5, 2.0, 2.5, 2.6].iter().map(|&t| frame(&plain, t)).collect();
    for kind in kinds {
        let p = pair(kind, json!({}));
        assert_eq!(frame(&p, 1.4), reference[0], "{kind}: before the window");
        assert!(diff(&frame(&p, 1.5), &reference[1]) < 0.05, "{kind}: first frame of the window (p = 0)");
        assert!(diff(&frame(&p, 2.5), &reference[3]) < 0.05, "{kind}: last frame of the window (p = 1)");
        assert_eq!(frame(&p, 2.6), reference[4], "{kind}: after the window");
        let mid = frame(&p, 2.0);
        assert!(diff(&mid, &reference[0]) > 2.0 && diff(&mid, &reference[2]) > 2.0, "{kind}: middle");
    }
    let _ = FrameOptions::default();
}

/// Solid red → solid blue through a 1 s transition centred at 2 s (window 1.5 .. 2.5).
fn solid_pair(w: u32, h: u32, kind: &str, params: Value) -> Project {
    let mut p = project(w, h, 30.0);
    p.clips = vec![solid("A", "v1", 0.0, 2.0, "#FF0000"), solid("B", "v1", 2.0, 2.0, "#0000FF")];
    let mut t: Transition = serde_json::from_value(json!({"id": "tr1", "from": "A", "to": "B", "kind": kind, "duration": 1.0})).unwrap();
    t.params = serde_json::from_value(params).unwrap();
    p.transitions = vec![t];
    p
}

#[test]
fn cube_faces_meet_at_their_shared_edge_in_every_direction() {
    for (w, h) in [(90, 160), (160, 90)] {
        for dir in 0..4 {
            let p = solid_pair(w, h, "cube", json!({"direction": dir}));
            for q in [0.25, 0.35, 0.5, 0.65, 0.75] {
                let img = frame(&p, 1.5 + q);
                // Along the line across the seam (the centre column for up/down, the centre row for
                // left/right): one unbroken run of cube, never background between the faces.
                let line: Vec<[u8; 4]> = if dir >= 2 { (0..h).map(|y| px(&img, w / 2, y)).collect() } else { (0..w).map(|x| px(&img, x, h / 2)).collect() };
                let class = |c: [u8; 4]| if c[0] < 24 && c[1] < 24 && c[2] < 24 { 'k' } else if c[0] > c[2] { 'r' } else { 'b' };
                let s: String = line.iter().map(|&c| class(c)).collect();
                let t = s.trim_matches('k');
                assert!(!t.is_empty() && !t.contains('k'), "{w}x{h} direction {dir} p {q}: {s}");
                // Both faces are in view from a third of the turn to two thirds (at p = 0.25 the
                // incoming face is still edge-on to the camera).
                if (0.35..=0.65).contains(&q) {
                    assert!(t.contains('r') && t.contains('b'), "{w}x{h} direction {dir} p {q}: {s}");
                }
            }
            // The faces reported by layers_at share their edge at the cut.
            let res = resolved(&p);
            let boxes = renderer().layers_at(Path::new("."), &p, &res, 2.0);
            assert_eq!(boxes.len(), 2, "{boxes:?}");
            let shared = boxes[0].corners.iter().filter(|c| boxes[1].corners.iter().any(|d| (c[0] - d[0]).hypot(c[1] - d[1]) < 0.01)).count();
            assert_eq!(shared, 2, "direction {dir}: {boxes:?}");
        }
    }
}

#[test]
fn stretch_covers_the_frame_at_every_instant() {
    for dir in 0..4 {
        let p = solid_pair(90, 160, "stretch", json!({"direction": dir}));
        for k in 0..=30 {
            let t = 1.5 + k as f64 / 30.0;
            let img = frame(&p, t);
            // Red and blue blend at the cut (motion blur), so coverage is red + blue.
            let empty = img.rgba.as_chunks::<4>().0.iter().filter(|q| (q[0] as u32 + q[2] as u32) < 245).count();
            assert_eq!(empty, 0, "direction {dir} at {t:.3} s: {empty} pixels not covered");
        }
    }
}
