//! Text (box, upright glyphs, reveals, karaoke highlight, pill/stroke/shadow, wrapping, alignment,
//! weights, fallback) and shapes (draw-on).

mod common;
use aiframes_core::Project;
use aiframes_render::Image;
use common::*;
use serde_json::{json, Value};
use std::path::Path;

fn text_project(text: Value) -> Project {
    let mut p = project(640, 360, 30.0);
    let mut src = json!({"type": "text", "text": "HELLO", "size": 80, "color": "#FFFFFF"});
    for (k, v) in text.as_object().unwrap() {
        src[k] = v.clone();
    }
    p.clips = vec![clip(json!({"id": "t1", "track": "v2", "start": 0, "duration": 4, "source": src}))];
    p
}

fn lit(img: &Image) -> usize {
    img.rgba.as_chunks::<4>().0.iter().filter(|q| q[0] > 128 || q[1] > 128 || q[2] > 128).count()
}

#[test]
fn text_is_inside_its_box_and_upright() {
    let r = renderer();
    let p = text_project(json!({}));
    let res = resolved(&p);
    let img = render(&r, &p, 1.0, 1.0);
    let boxes = r.layers_at(Path::new("."), &p, &res, 1.0);
    assert_eq!(boxes.len(), 1);
    let b = boxes[0].bounds;
    let ink = bbox(&img, |q| q[0] > 128).expect("text drawn");
    assert!(ink[0] as f32 >= b[0] - 2.0 && ink[1] as f32 >= b[1] - 2.0, "{ink:?} vs {b:?}");
    assert!(ink[2] as f32 <= b[0] + b[2] + 2.0 && ink[3] as f32 <= b[1] + b[3] + 2.0, "{ink:?} vs {b:?}");
    // Centred in the frame.
    let cx = (ink[0] + ink[2]) as f32 / 2.0;
    assert!((cx - 320.0).abs() < 6.0, "{cx}");
    let (w, h) = r.text_size(Path::new("."), match &p.clips[0].source {
        aiframes_core::Source::Text(t) => t,
        _ => unreachable!(),
    });
    assert!((w as f32 - b[2]).abs() < 0.01 && (h as f32 - b[3]).abs() < 0.01);
    // "T": the bar is at the top.
    let p = text_project(json!({"text": "T", "size": 200}));
    let img = render(&r, &p, 1.0, 1.0);
    let ink = bbox(&img, |q| q[0] > 128).unwrap();
    let row_ink = |y: u32| (0..640).filter(|&x| px(&img, x, y)[0] > 128).count();
    let top = row_ink(ink[1] + 5);
    let bottom = row_ink(ink[3] - 5);
    assert!(top > bottom * 3, "top {top} bottom {bottom}");
}

#[test]
fn reveals_change_over_time() {
    let r = renderer();
    let full = lit(&render(&r, &text_project(json!({"text": "one two three four"})), 2.0, 1.0));
    for kind in ["typewriter", "fade_words", "pop_words", "slide_up", "fade"] {
        let p = text_project(json!({"text": "one two three four", "reveal": {"kind": kind, "duration": 1.0}}));
        let early = render(&r, &p, 0.2, 1.0);
        let mid = render(&r, &p, 0.5, 1.0);
        let done = render(&r, &p, 1.5, 1.0);
        assert!(lit(&done) == full, "{kind}: finished reveal looks like the plain text");
        assert!(diff(&early, &done) > 0.5, "{kind}: early differs");
        assert!(diff(&mid, &done) > 0.1, "{kind}: middle differs");
        assert!(lit(&early) <= lit(&mid) + 50, "{kind}: reveals progressively");
        if kind != "pop_words" {
            assert_eq!(lit(&render(&r, &p, 0.0, 1.0)), 0, "{kind}: nothing visible at the start");
        }
    }
    // Typewriter shows strictly more as time passes.
    let p = text_project(json!({"text": "typewriter", "reveal": {"kind": "typewriter", "duration": 1.0}}));
    let counts: Vec<usize> = [0.15, 0.45, 0.75, 1.0].iter().map(|&t| lit(&render(&r, &p, t, 1.0))).collect();
    assert!(counts.windows(2).all(|w| w[0] < w[1]), "{counts:?}");
}

#[test]
fn spoken_reveal_follows_the_word_windows() {
    // Each word appears when its window starts and stays (the line types itself in with the voice).
    let r = renderer();
    let hl = json!({"color": "#FFFFFF", "words": [[0.5, 1.0], [1.5, 2.0], [2.5, 3.0]]});
    let p = text_project(json!({"text": "one two three", "size": 60, "highlight": hl, "reveal": {"kind": "spoken"}}));
    let plain = text_project(json!({"text": "one two three", "size": 60}));
    assert_eq!(lit(&render(&r, &p, 0.2, 1.0)), 0, "nothing before the first word");
    let counts: Vec<usize> = [0.8, 1.8, 2.8].iter().map(|&t| lit(&render(&r, &p, t, 1.0))).collect();
    assert!(counts[0] > 0 && counts.windows(2).all(|w| w[0] < w[1]), "{counts:?}");
    assert_eq!(lit(&render(&r, &p, 3.5, 1.0)), lit(&render(&r, &plain, 3.5, 1.0)), "all words stay once spoken");
    // A background pill grows with the words instead of waiting empty for them.
    let pill = |t: f64| {
        let p = text_project(json!({"text": "one two three", "size": 60, "color": "#000000", "background": {"color": "#FFFFFF", "padding": 10, "radius": 8},
                                    "highlight": {"color": "#000000", "words": [[0.5, 1.0], [1.5, 2.0], [2.5, 3.0]]}, "reveal": {"kind": "spoken"}}));
        bbox(&render(&r, &p, t, 1.0), |q| q[0] > 200 && q[1] > 200 && q[2] > 200).map(|b| b[2] - b[0])
    };
    assert_eq!(pill(0.2), None, "no pill before the first word");
    let (w1, w3) = (pill(0.8).unwrap(), pill(2.8).unwrap());
    assert!(w1 * 2 < w3, "pill around the first word only ({w1}) vs the whole line ({w3})");
    // No word windows: the text simply shows.
    let bare = text_project(json!({"text": "one two three", "size": 60, "reveal": {"kind": "spoken"}}));
    assert_eq!(lit(&render(&r, &bare, 0.2, 1.0)), lit(&render(&r, &plain, 0.2, 1.0)));
}

#[test]
fn highlight_colours_the_active_word() {
    let r = renderer();
    let red = |q: [u8; 4]| q[0] > 180 && q[1] < 90 && q[2] < 90;
    let p = text_project(json!({"text": "one two three", "size": 60, "highlight": {"color": "#FF0000", "words": [[0, 1], [1, 2], [2, 3]]}}));
    let white = |q: [u8; 4]| q[0] > 180 && q[1] > 180;
    let all = bbox(&render(&r, &p, 3.5, 1.0), white).unwrap();
    assert!(bbox(&render(&r, &p, 3.5, 1.0), red).is_none(), "no word active after the last window");
    let w0 = bbox(&render(&r, &p, 0.5, 1.0), red).unwrap();
    let w1 = bbox(&render(&r, &p, 1.5, 1.0), red).unwrap();
    let w2 = bbox(&render(&r, &p, 2.5, 1.0), red).unwrap();
    assert!(w0[0] <= all[0] + 2 && w0[2] < w1[0], "{w0:?} {w1:?}");
    assert!(w1[2] < w2[0] && w2[2] + 2 >= all[2], "{w1:?} {w2:?} {all:?}");
    let c1 = (w1[0] + w1[2]) / 2;
    assert!((c1 as i32 - 320).abs() < 30, "middle word near the centre: {c1}");
    // Optional box behind the active word.
    let p = text_project(json!({"text": "one two three", "size": 60, "highlight": {"color": "#FFFFFF", "background": "#0000FF", "words": [[0, 1], [1, 2], [2, 3]]}}));
    let blue = |q: [u8; 4]| q[2] > 200 && q[0] < 60;
    let b1 = bbox(&render(&r, &p, 1.5, 1.0), blue).unwrap();
    assert!(b1[0] < w1[0] && b1[2] > w1[2], "box surrounds the word: {b1:?} vs {w1:?}");
}

#[test]
fn pill_stroke_and_shadow() {
    let r = renderer();
    let p = text_project(json!({"background": {"color": "#00FF00", "padding": 20, "radius": 12}}));
    let img = render(&r, &p, 1.0, 1.0);
    let res = resolved(&p);
    let b = r.layers_at(Path::new("."), &p, &res, 1.0)[0].bounds;
    let green = |q: [u8; 4]| q[1] > 200 && q[0] < 60;
    let g = bbox(&img, green).unwrap();
    assert!((g[0] as f32 - b[0]).abs() <= 1.5 && (g[2] as f32 - (b[0] + b[2])).abs() <= 1.5, "pill fills the box: {g:?} {b:?}");
    let p = text_project(json!({"stroke": {"color": "#FF0000", "width": 5}}));
    let img = render(&r, &p, 1.0, 1.0);
    assert!(img.rgba.as_chunks::<4>().0.iter().filter(|q| q[0] > 200 && q[1] < 60).count() > 500);
    // Shadow below-right on a white background.
    let mut p = text_project(json!({"color": "#FF0000", "shadow": {"color": "#000000", "blur": 6, "x": 10, "y": 10}}));
    p.clips.push(solid("bg", "v1", 0.0, 4.0, "#FFFFFF"));
    let img = render(&r, &p, 1.0, 1.0);
    let ink = bbox(&img, |q| q[0] > 200 && q[1] < 80).unwrap();
    let dark = bbox(&img, |q| q[0] < 100 && q[1] < 100).unwrap();
    assert!(dark[2] > ink[2] + 4 && dark[3] > ink[3] + 4, "shadow offset: ink {ink:?} shadow {dark:?}");
}

#[test]
fn wrapping_alignment_spacing_weight() {
    let r = renderer();
    let res_box = |p: &Project| {
        let res = resolved(p);
        r.layers_at(Path::new("."), p, &res, 1.0)[0].bounds
    };
    let long = "the quick brown fox jumps over the lazy dog";
    let one = res_box(&text_project(json!({"text": long, "size": 40})));
    let wrapped = res_box(&text_project(json!({"text": long, "size": 40, "max_width": 300})));
    assert!(wrapped[2] <= 300.5 && wrapped[3] >= 2.0 * 40.0, "{wrapped:?}");
    assert!(one[2] > 500.0 && one[3] < 60.0, "{one:?}");
    let spaced = res_box(&text_project(json!({"text": "HELLO", "letter_spacing": 20})));
    let plain = res_box(&text_project(json!({})));
    assert!(spaced[2] > plain[2] + 70.0, "{spaced:?} {plain:?}");
    // Alignment of the short line inside the box.
    let first_line_ink = |align: &str| {
        let p = text_project(json!({"text": "I\nWWWWWWW", "align": align}));
        let img = render(&r, &p, 1.0, 1.0);
        let b = res_box(&p);
        let y = (b[1] + 30.0) as u32;
        let xs: Vec<u32> = (0..640).filter(|&x| px(&img, x, y)[0] > 128).collect();
        (xs[0] as f32 - b[0], b[0] + b[2] - *xs.last().unwrap() as f32)
    };
    let (l_left, l_right) = first_line_ink("left");
    let (r_left, r_right) = first_line_ink("right");
    let (c_left, c_right) = first_line_ink("center");
    assert!(l_left < 20.0 && l_right > 100.0, "left {l_left} {l_right}");
    assert!(r_right < 20.0 && r_left > 100.0, "right {r_left} {r_right}");
    assert!((c_left - c_right).abs() < 12.0, "center {c_left} {c_right}");
    let light = lit(&render(&r, &text_project(json!({"weight": 300})), 1.0, 1.0));
    let heavy = lit(&render(&r, &text_project(json!({"weight": 900})), 1.0, 1.0));
    assert!(heavy as f64 > light as f64 * 1.2, "{light} {heavy}");
    let upright = render(&r, &text_project(json!({})), 1.0, 1.0);
    let italic = render(&r, &text_project(json!({"italic": true})), 1.0, 1.0);
    assert!(diff(&upright, &italic) > 1.0);
}

#[test]
fn missing_font_falls_back_with_a_warning() {
    let r = renderer();
    assert!(r.has_font(Path::new("."), "Segoe UI"));
    assert!(!r.has_font(Path::new("."), "No Such Font 12345"));
    let p = text_project(json!({"font": "No Such Font 12345"}));
    let res = resolved(&p);
    let (img, warn) = r.frame_checked(Path::new("."), &p, &res, 1.0, &aiframes_render::FrameOptions::default()).unwrap();
    assert!(lit(&img) > 500);
    assert!(warn.iter().any(|w| w.contains("No Such Font")), "{warn:?}");
}

#[test]
fn shapes_draw_on() {
    let r = renderer();
    let shape = |s: Value| {
        let mut p = project(400, 400, 30.0);
        p.clips = vec![clip(json!({"id": "s1", "track": "v1", "start": 0, "duration": 4, "source": s}))];
        p
    };
    let rect = |draw: f64| shape(json!({"type": "shape", "kind": "rect", "width": 300, "height": 200, "stroke": {"color": "#FFFFFF", "width": 8}, "draw": draw}));
    let full = lit(&render(&r, &rect(1.0), 0.0, 1.0));
    let half = lit(&render(&r, &rect(0.5), 0.0, 1.0));
    assert!(full > 3000 && (half as f64 / full as f64 - 0.5).abs() < 0.08, "{full} {half}");
    assert_eq!(lit(&render(&r, &rect(0.0), 0.0, 1.0)), 0);
    // Animated draw with keys.
    let p = shape(json!({"type": "shape", "kind": "ellipse", "width": 300, "height": 300, "stroke": {"color": "#FFFFFF", "width": 8},
        "draw": {"keys": [{"t": 0, "v": 0}, {"t": 2, "v": 1}]}}));
    let a = lit(&render(&r, &p, 0.5, 1.0));
    let b = lit(&render(&r, &p, 1.5, 1.0));
    assert!(a > 0 && b > a * 2, "{a} {b}");
    // Fill-only sweep.
    let fill = |draw: f64| shape(json!({"type": "shape", "kind": "rect", "width": 200, "height": 200, "fill": "#FFFFFF", "draw": draw}));
    let f1 = lit(&render(&r, &fill(1.0), 0.0, 1.0));
    let f4 = lit(&render(&r, &fill(0.25), 0.0, 1.0));
    assert!((f1 as f64 - 40000.0).abs() < 500.0 && (f4 as f64 / f1 as f64 - 0.25).abs() < 0.05, "{f1} {f4}");
    // Arrow: the head sits at the end point (+x end).
    let arrow = shape(json!({"type": "shape", "kind": "arrow", "width": 300, "height": 0, "stroke": {"color": "#FFFFFF", "width": 6}}));
    let img = render(&r, &arrow, 0.0, 1.0);
    let thick = |x: u32| (0..400).filter(|&y| px(&img, x, y)[0] > 128).count();
    assert!(thick(330) > thick(150) * 2, "head {} shaft {}", thick(330), thick(150));
    let line = shape(json!({"type": "shape", "kind": "line", "width": 300, "height": 300, "draw": 0.5}));
    let img = render(&r, &line, 0.0, 1.0);
    let b = bbox(&img, |q| q[0] > 128).unwrap();
    assert!(b[2] <= 205 && b[0] >= 45, "half a diagonal from the top-left: {b:?}");
}

#[test]
fn lower_third_pill_hugs_the_text_while_sliding_up() {
    // The demo's lower third (mcp demo.rs): left-aligned text, padded pill, slide_up reveal, fades.
    let r = renderer();
    let mut p = project(1920, 1080, 30.0);
    p.clips = vec![clip(json!({"id": "lower3", "track": "v3", "start": 4.0, "duration": 2.5, "fade_in": 0.3, "fade_out": 0.3,
        "transform": {"x": -420, "y": 340},
        "source": {"type": "text", "text": "testsrc2 · 1280×720", "size": 44, "weight": 600, "align": "left",
            "background": {"color": "#000000B0", "padding": 20, "radius": 12}, "reveal": {"kind": "slide_up", "duration": 0.5}}}))];
    p.clips.push(solid("bg", "v1", 0.0, 10.0, "#FFFFFF"));
    let res = resolved(&p);
    let b = r.layers_at(Path::new("."), &p, &res, 5.0).into_iter().find(|l| l.clip == "lower3").unwrap().bounds;
    assert!(b[2] > 300.0 && b[2] < 520.0 && b[3] > 80.0 && b[3] < 100.0, "content box {b:?}");
    for t in [4.1, 4.2, 4.35, 5.0, 6.2, 6.4] {
        let img = render(&r, &p, t, 1.0);
        if std::env::var("AIFRAMES_DUMP").is_ok() {
            std::fs::write(root().join(format!("lower3-{t}.png")), aiframes_render::encode_png(&img)).unwrap();
        }
        // Everything drawn (darker than the white background) stays inside the content box.
        if let Some(ink) = bbox(&img, |q| q[0] < 250) {
            assert!(ink[0] as f32 >= b[0] - 2.0 && ink[2] as f32 <= b[0] + b[2] + 2.0, "t={t}: {ink:?} vs {b:?}");
            assert!(ink[1] as f32 >= b[1] - 2.0 && ink[3] as f32 <= b[1] + b[3] + 2.0, "t={t}: {ink:?} vs {b:?}");
        }
    }
}

/// A stroke follows the glyph's outer silhouette: fonts built from overlapping contours
/// (Bahnschrift, a variable font) show no stroke inside the filled letters.
#[test]
fn stroke_follows_the_outer_silhouette_of_overlapping_contours() {
    // Opaque white, then half-transparent white (which used to show the contours' strokes).
    for color in ["#FFFFFF", "#FFFFFF80"] {
        stroke_inside_check(color);
    }
}

fn stroke_inside_check(color: &str) {
    let mk = |stroke: bool| {
        let mut p = project(900, 400, 30.0);
        let mut src = json!({"type": "text", "text": "BRA", "font": "Bahnschrift", "weight": 700, "size": 200, "color": color});
        if stroke {
            src["stroke"] = json!({"color": "#FF0000", "width": 10});
        }
        p.clips = vec![clip(json!({"id": "t1", "track": "v2", "start": 0, "duration": 4, "source": src}))];
        render(&renderer(), &p, 1.0, 1.0)
    };
    let (fill, stroked) = (mk(false), mk(true));
    // Inside the letters: fully covered by the (grey) fill over the black background.
    let full = fill.rgba.as_chunks::<4>().0.iter().map(|q| q[0]).max().unwrap();
    let white = |img: &Image, x: u32, y: u32| {
        let q = px(img, x, y);
        q[0] as u16 + 1 >= full as u16 && q[0] == q[1] && q[1] == q[2]
    };
    let (w, h) = (fill.width, fill.height);
    let mut interior = 0;
    let mut bad = Vec::new();
    for y in 2..h - 2 {
        for x in 2..w - 2 {
            // Inside the fill, 2 px away from its edges.
            if (0..25).all(|k| white(&fill, x + k % 5 - 2, y + k / 5 - 2)) {
                interior += 1;
                // Same pixel as without the stroke (no red inside the letters).
                let (q, r) = (px(&stroked, x, y), px(&fill, x, y));
                if (q[0] as i32 - r[0] as i32).abs() > 2 || (q[1] as i32 - r[1] as i32).abs() > 2 {
                    bad.push((x, y, q));
                }
            }
        }
    }
    assert!(interior > 5000, "{color}: {interior}");
    assert!(bad.is_empty(), "{color}: {} stroke pixels inside the letters, e.g. {:?}", bad.len(), &bad[..bad.len().min(5)]);
    // The stroke is there, outside the letters.
    assert!(stroked.rgba.as_chunks::<4>().0.iter().filter(|q| q[0] > 200 && q[1] < 60).count() > 2000);
}
