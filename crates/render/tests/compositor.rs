//! Compositor: solid/position/scale/rotation/crop/fit/opacity/fades/blend modes, media frame
//! accuracy, preview decode size, cache behaviour, layers_at, adjustment layers, track flags.

mod common;
use aiframes_core::{resolve, BlendMode};
use aiframes_render::{contact_sheet, encode_png, FrameOptions, Renderer};
use common::*;
use serde_json::json;
use std::path::Path;

#[test]
fn solid_fills_and_background_shows() {
    let r = renderer();
    let mut p = project(64, 36, 30.0);
    p.settings.background = aiframes_core::Color::rgba8(0, 0, 255, 255);
    p.clips.push(solid("c1", "v1", 0.0, 2.0, "#FF0000"));
    let img = render(&r, &p, 0.5, 1.0);
    assert_eq!((img.width, img.height), (64, 36));
    assert!(img.rgba.as_chunks::<4>().0.iter().all(|px| *px == [255, 0, 0, 255]));
    // After the clip: background.
    let img = render(&r, &p, 2.5, 1.0);
    assert_eq!(px(&img, 10, 10), [0, 0, 255, 255]);
}

#[test]
fn position_scale_rotation() {
    let r = renderer();
    let mut p = project(200, 100, 30.0);
    let mut c = solid("c1", "v1", 0.0, 2.0, "#FFFFFF");
    c.transform.scale = 0.5.into();
    p.clips.push(c.clone());
    let white = |q: [u8; 4]| q[0] > 128;
    let img = render(&r, &p, 0.0, 1.0);
    assert_eq!(bbox(&img, white), Some([50, 25, 150, 75]));
    c.transform.x = 20.0.into();
    c.transform.y = (-10.0).into();
    p.clips[0] = c.clone();
    let img = render(&r, &p, 0.0, 1.0);
    assert_eq!(bbox(&img, white), Some([70, 15, 170, 65]));
    c.transform.x = 0.0.into();
    c.transform.y = 0.0.into();
    c.transform.rotation = 90.0.into();
    p.clips[0] = c.clone();
    let img = render(&r, &p, 0.0, 1.0);
    assert_eq!(bbox(&img, white), Some([75, 0, 125, 100]));
    // Anchor at the top-left corner: scaling keeps that corner where it was.
    c.transform.rotation = 0.0.into();
    c.transform.anchor_x = 0.0;
    c.transform.anchor_y = 0.0;
    c.transform.scale = 1.0.into();
    let mut small = c.clone();
    small.transform.crop.right = 0.5.into();
    small.transform.crop.bottom = 0.5.into();
    small.transform.scale = 0.5.into();
    p.clips[0] = small;
    let img = render(&r, &p, 0.0, 1.0);
    // Box 100x50 centred at (100,50) → top-left (50,25); half scale about it → 50x25.
    assert_eq!(bbox(&img, white), Some([50, 25, 100, 50]));
}

#[test]
fn crop_and_fit_modes() {
    let r = renderer();
    let png = halves_png();
    let mut p = project(200, 100, 30.0);
    add_media(&mut p, "m1", &png);
    let media = |fit: &str| clip(json!({"id": "c1", "track": "v1", "start": 0, "duration": 2, "source": {"type": "media", "asset": "m1"}, "transform": {"fit": fit}}));
    let red = |q: [u8; 4]| q[0] > 200 && q[2] < 60;
    let blue = |q: [u8; 4]| q[2] > 200 && q[0] < 60;
    let lit = |q: [u8; 4]| q[0] > 60 || q[2] > 60;
    p.clips = vec![media("contain")];
    let img = render(&r, &p, 0.0, 1.0);
    let b = bbox(&img, lit).unwrap();
    assert!((b[0] as i32 - 33).abs() <= 1 && (b[2] as i32 - 167).abs() <= 1 && b[1] == 0 && b[3] == 100, "{b:?}");
    assert!(red(px(&img, 60, 50)) && blue(px(&img, 140, 50)));
    p.clips = vec![media("cover")];
    let img = render(&r, &p, 0.0, 1.0);
    assert!(red(px(&img, 1, 50)) && blue(px(&img, 198, 50)) && red(px(&img, 1, 1)));
    p.clips = vec![media("stretch")];
    let img = render(&r, &p, 0.0, 1.0);
    assert!(red(px(&img, 1, 1)) && blue(px(&img, 198, 98)) && red(px(&img, 94, 50)) && blue(px(&img, 106, 50)));
    p.clips = vec![media("none")];
    let img = render(&r, &p, 0.0, 1.0);
    assert_eq!(bbox(&img, lit), Some([68, 26, 132, 74]));
    // Crop the right half away: only red remains, re-fitted (32x48 → 66.7x100).
    let mut c = media("contain");
    c.transform.crop.right = 0.5.into();
    p.clips = vec![c];
    let img = render(&r, &p, 0.0, 1.0);
    assert!(bbox(&img, blue).is_none());
    let b = bbox(&img, red).unwrap();
    assert!((b[0] as i32 - 67).abs() <= 1 && (b[2] as i32 - 133).abs() <= 1, "{b:?}");
}

#[test]
fn opacity_fades_and_blend_modes() {
    let r = renderer();
    let mut p = project(32, 18, 30.0);
    let mut c = solid("c1", "v1", 0.0, 4.0, "#FFFFFF");
    c.opacity = 0.5.into();
    p.clips = vec![c];
    assert!(near(px(&render(&r, &p, 0.0, 1.0), 5, 5), [128, 128, 128, 255], 1));
    let mut c = solid("c1", "v1", 0.0, 4.0, "#FFFFFF");
    c.fade_in = 1.0;
    c.fade_out = 1.0;
    p.clips = vec![c];
    assert!(near(px(&render(&r, &p, 0.0, 1.0), 5, 5), [0, 0, 0, 255], 0));
    assert!(near(px(&render(&r, &p, 0.5, 1.0), 5, 5), [128, 128, 128, 255], 1));
    assert!(near(px(&render(&r, &p, 2.0, 1.0), 5, 5), [255, 255, 255, 255], 0));
    assert!(near(px(&render(&r, &p, 3.5, 1.0), 5, 5), [128, 128, 128, 255], 1));
    // Animated opacity keys (clip-local).
    let mut c = solid("c1", "v1", 1.0, 4.0, "#FFFFFF");
    c.opacity = serde_json::from_value(json!({"keys": [{"t": 0, "v": 0}, {"t": 2, "v": 1}]})).unwrap();
    p.clips = vec![c];
    assert!(near(px(&render(&r, &p, 2.0, 1.0), 5, 5), [128, 128, 128, 255], 1));

    let cases = [
        (BlendMode::Normal, 128),
        (BlendMode::Multiply, 64),
        (BlendMode::Screen, 191),
        (BlendMode::Add, 255),
        (BlendMode::Overlay, 128),
        (BlendMode::Darken, 128),
        (BlendMode::Lighten, 128),
        (BlendMode::Difference, 0),
    ];
    for (mode, want) in cases {
        let mut top = solid("c2", "v2", 0.0, 1.0, "#808080");
        top.blend = mode;
        p.clips = vec![solid("c1", "v1", 0.0, 1.0, "#808080"), top];
        let got = px(&render(&r, &p, 0.0, 1.0), 3, 3);
        assert!((got[0] as i32 - want).abs() <= 2, "{mode:?}: {got:?}");
    }
    // Lighten/darken pick per channel.
    let mut top = solid("c2", "v2", 0.0, 1.0, "#20E020");
    top.blend = BlendMode::Lighten;
    p.clips = vec![solid("c1", "v1", 0.0, 1.0, "#808080"), top];
    let got = px(&render(&r, &p, 0.0, 1.0), 3, 3);
    assert!(near(got, [128, 224, 128, 255], 2), "{got:?}");
}

#[test]
fn media_frames_follow_in_point_speed_and_reverse() {
    let r = renderer();
    let v = bars(30, 6);
    let mut p = project(320, 180, 30.0);
    add_media(&mut p, "m1", &v);
    let base = json!({"id": "c1", "track": "v1", "start": 1.0, "duration": 2.0, "source": {"type": "media", "asset": "m1", "in_point": 2.0}});
    p.clips = vec![clip(base.clone())];
    let img = render(&r, &p, 1.5, 1.0);
    assert_eq!(code(&img, 0, 0, 320, 180), 75);
    let mut c = clip(base.clone());
    c.speed = 2.0.into();
    p.clips = vec![c];
    assert_eq!(code(&render(&r, &p, 1.5, 1.0), 0, 0, 320, 180), 90);
    let mut c = clip(base.clone());
    c.reverse = true;
    p.clips = vec![c];
    // Reverse plays source frames 119, 118, ... 60 (each output frame shows the source frame under
    // its middle): at local 0.5 s (output frame 15) that is frame 104.
    assert_eq!(code(&render(&r, &p, 1.5, 1.0), 0, 0, 320, 180), 104);
    assert_eq!(code(&render(&r, &p, 1.0, 1.0), 0, 0, 320, 180), 119);
    let mut c = clip(base);
    c.speed = 0.0.into();
    p.clips = vec![c];
    assert_eq!(code(&render(&r, &p, 2.9, 1.0), 0, 0, 320, 180), 60);
    // The preview is readable too.
    p.clips[0].speed = 1.0.into();
    let img = render(&r, &p, 1.5, 0.5);
    assert_eq!((img.width, img.height), (160, 90));
    assert_eq!(code(&img, 0, 0, 160, 90), 75);
}

#[test]
fn preview_decodes_at_output_size_and_cache_skips_ffmpeg() {
    let pool = fresh_pool("preview");
    let r = Renderer::new(pool.clone());
    let v = testsrc_1080("src1080", 2);
    let mut p = project(1920, 1080, 30.0);
    add_media(&mut p, "m1", &v);
    p.clips = vec![clip(json!({"id": "c1", "track": "v1", "start": 0, "duration": 2, "source": {"type": "media", "asset": "m1"}}))];
    let res = resolved(&p);
    let o = FrameOptions { scale: 0.5, ..FrameOptions::default() };
    let img = r.frame(Path::new("."), &p, &res, 0.5, &o).unwrap();
    assert_eq!((img.width, img.height), (960, 540));
    let s = pool.stats();
    assert!(s.cached_frames >= 1);
    let per = s.cache_bytes as f64 / s.cached_frames as f64;
    assert!((per - 960.0 * 540.0 * 4.0).abs() < 4096.0, "cached frame size {per} bytes: decoded at the wrong size");
    // Same frame again: served from the cache, no decoding.
    let before = pool.stats();
    let again = r.frame(Path::new("."), &p, &res, 0.5, &o).unwrap();
    let after = pool.stats();
    assert_eq!(img, again);
    assert_eq!(before.frames_decoded, after.frames_decoded);
    assert_eq!(before.seeks, after.seeks);
    assert!(after.cache_hits > before.cache_hits);
}

#[test]
fn layers_at_matches_drawn_pixels() {
    let r = renderer();
    let mut p = project(400, 200, 30.0);
    let mut c = solid("c1", "v1", 0.0, 2.0, "#FFFFFF");
    c.transform.scale = 0.4.into();
    c.transform.rotation = 30.0.into();
    c.transform.x = 50.0.into();
    c.transform.y = (-20.0).into();
    p.clips = vec![c, solid("c0", "v2", 5.0, 1.0, "#FF0000")];
    let res = resolved(&p);
    let boxes = r.layers_at(Path::new("."), &p, &res, 1.0);
    assert_eq!(boxes.len(), 1);
    let b = &boxes[0];
    assert_eq!(b.clip, "c1");
    assert!((b.pivot[0] - 250.0).abs() < 1e-3 && (b.pivot[1] - 80.0).abs() < 1e-3, "{:?}", b.pivot);
    let img = render(&r, &p, 1.0, 1.0);
    let drawn = bbox(&img, |q| q[0] > 128).unwrap();
    let want = [b.bounds[0], b.bounds[1], b.bounds[0] + b.bounds[2], b.bounds[1] + b.bounds[3]];
    for k in 0..4 {
        assert!((drawn[k] as f32 - want[k]).abs() <= 1.5, "drawn {drawn:?} vs layers_at {want:?}");
    }
    // Corners: top-left is rotated 30° about the centre.
    let (w, h) = (160.0f32, 80.0f32);
    let (s, co) = 30f32.to_radians().sin_cos();
    let tl = [250.0 + (-w / 2.0) * co - (-h / 2.0) * s, 80.0 + (-w / 2.0) * s + (-h / 2.0) * co];
    assert!((b.corners[0][0] - tl[0]).abs() < 0.01 && (b.corners[0][1] - tl[1]).abs() < 0.01);
}

#[test]
fn adjustment_layers_affect_what_is_below() {
    let r = renderer();
    let mut p = project(100, 50, 30.0);
    let adj = clip(json!({"id": "adj", "track": "v2", "start": 0, "duration": 1, "source": {"type": "adjustment"},
        "effects": [{"kind": "black_white"}]}));
    p.clips = vec![solid("c1", "v1", 0.0, 2.0, "#FF0000"), adj, solid("c3", "v3", 0.0, 2.0, "#00000000")];
    let img = render(&r, &p, 0.5, 1.0);
    let q = px(&img, 50, 25);
    assert!(q[0] == q[1] && q[1] == q[2] && q[0] > 50 && q[0] < 110, "{q:?}");
    // Outside its time range the picture is untouched.
    assert_eq!(px(&render(&r, &p, 1.5, 1.0), 50, 25), [255, 0, 0, 255]);
    // A mask limits where it applies; opacity mixes.
    let adj = clip(json!({"id": "adj", "track": "v2", "start": 0, "duration": 1, "opacity": 0.5, "source": {"type": "adjustment"},
        "effects": [{"kind": "invert"}, {"kind": "mask_rect", "params": {"x": 0.25, "width": 0.5, "height": 1.0}}]}));
    p.clips[1] = adj;
    let img = render(&r, &p, 0.5, 1.0);
    assert!(near(px(&img, 20, 25), [128, 128, 128, 255], 2), "{:?}", px(&img, 20, 25));
    assert_eq!(px(&img, 80, 25), [255, 0, 0, 255]);
}

#[test]
fn hidden_solo_disabled_tracks() {
    let r = renderer();
    let mut p = project(20, 10, 30.0);
    p.clips = vec![solid("c1", "v1", 0.0, 1.0, "#FF0000"), solid("c2", "v2", 0.0, 1.0, "#00FF00")];
    assert_eq!(px(&render(&r, &p, 0.0, 1.0), 1, 1), [0, 255, 0, 255]);
    p.tracks[1].hidden = true;
    assert_eq!(px(&render(&r, &p, 0.0, 1.0), 1, 1), [255, 0, 0, 255]);
    p.tracks[1].hidden = false;
    p.tracks[0].solo = true;
    assert_eq!(px(&render(&r, &p, 0.0, 1.0), 1, 1), [255, 0, 0, 255]);
    p.tracks[0].solo = false;
    p.clips[1].enabled = false;
    assert_eq!(px(&render(&r, &p, 0.0, 1.0), 1, 1), [255, 0, 0, 255]);
}

#[test]
fn offline_media_is_a_placeholder_with_a_warning() {
    let r = renderer();
    let mut p = project(64, 36, 30.0);
    p.media.push(serde_json::from_value(json!({"id": "m9", "path": "C:/definitely/not/here.mp4", "kind": "video", "info": {"duration": 5, "width": 64, "height": 36, "has_video": true}})).unwrap());
    p.clips = vec![clip(json!({"id": "c1", "track": "v1", "start": 0, "duration": 2, "source": {"type": "media", "asset": "m9"}}))];
    let res = resolve(&p);
    let (img, warn) = r.frame_checked(Path::new("."), &p, &res, 0.5, &FrameOptions::default()).unwrap();
    assert!(warn.iter().any(|w| w.contains("offline")), "{warn:?}");
    assert!(img.rgba.as_chunks::<4>().0.iter().any(|q| q[0] > 100 && q[2] > 60), "placeholder is drawn");
    assert!(r.frame_strict(Path::new("."), &p, &res, 0.5, &FrameOptions::default()).is_err());
}

#[test]
fn guides_contact_sheet_and_png() {
    let r = renderer();
    let mut p = project(160, 90, 30.0);
    p.clips = vec![solid("c1", "v1", 0.0, 3.0, "#202020")];
    let res = resolved(&p);
    let plain = r.frame(Path::new("."), &p, &res, 0.0, &FrameOptions::default()).unwrap();
    let guided = r.frame(Path::new("."), &p, &res, 0.0, &FrameOptions { guides: true, ..FrameOptions::default() }).unwrap();
    // Title safe 0.8: left edge of the box at x = 16.
    assert!(px(&guided, 16, 45)[0] > px(&plain, 16, 45)[0] + 50);
    assert_eq!(px(&guided, 5, 5), px(&plain, 5, 5));
    let sheet = contact_sheet(&r, Path::new("."), &p, &res, &[0.0, 1.0, 2.5], 2, 80).unwrap();
    assert_eq!(sheet.width, 2 * 80 + 3 * 4);
    assert!(sheet.height > 2 * 45);
    // Labels are light text in the strip under each thumbnail.
    let strip = (4..80).flat_map(|x| (49..60).map(move |y| (x, y))).filter(|&(x, y)| px(&sheet, x, y)[0] > 150).count();
    assert!(strip > 20, "label pixels: {strip}");
    let png = encode_png(&sheet);
    let dec = png::Decoder::new(std::io::Cursor::new(png));
    let reader = dec.read_info().unwrap();
    assert_eq!(reader.info().width, sheet.width);
}
