//! Every transition of `aiframes_core::effects::TRANSITIONS`. A transition is centred on the cut;
//! `p` runs 0..1 over its window. At `p = 0` the result is exactly the outgoing picture and at
//! `p = 1` exactly the incoming one.

use crate::blur;
use crate::canvas::{accumulate, accumulate_res, blend_px, map2, map2_xy, mix_layers, AccItem, AccSource, Layer};
use crate::geom::{facing, front_facing, m3_apply, m3_mul, plane_homography, rot_x, rot_y, Affine, IRect, Proj, V3};
use crate::util::smoothstep;
use aiframes_core::effects::{transition_color, transition_param};
use rayon::prelude::*;
use aiframes_core::eval::ease;
use aiframes_core::{BlendMode, Ease, Transition};

fn inout(p: f64) -> f64 {
    ease(&Ease::InOut, p)
}

/// Derivative of the in-out cubic (0 at both ends, 3 at the middle).
fn inout_speed(p: f64) -> f64 {
    let p = p.clamp(0.0, 1.0);
    if p < 0.5 {
        12.0 * p * p
    } else {
        let u = -2.0 * p + 2.0;
        3.0 * u * u
    }
}

fn whip_dir(tr: &Transition, w: f64, h: f64) -> ((f64, f64), f64) {
    let a = transition_param(tr, "angle").to_radians();
    let m = (-a.cos(), -a.sin());
    let travel = (w * a.cos()).abs() + (h * a.sin()).abs();
    (m, travel)
}

/// Extra motion of the outgoing (A) and incoming (B) pictures in project pixels (frame `w`×`h`).
pub(crate) fn geometry(tr: &Transition, p: f64, w: f64, h: f64) -> (Affine, Affine) {
    let e = inout(p);
    let id = Affine::IDENTITY;
    let t = Affine::translate;
    let (cx, cy) = (w / 2.0, h / 2.0);
    match tr.kind.as_str() {
        "slide_left" => (id, t((1.0 - e) * w, 0.0)),
        "slide_right" => (id, t(-(1.0 - e) * w, 0.0)),
        "slide_up" => (id, t(0.0, (1.0 - e) * h)),
        "slide_down" => (id, t(0.0, -(1.0 - e) * h)),
        "push_left" => (t(-e * w, 0.0), t((1.0 - e) * w, 0.0)),
        "push_right" => (t(e * w, 0.0), t(-(1.0 - e) * w, 0.0)),
        "push_up" => (t(0.0, -e * h), t(0.0, (1.0 - e) * h)),
        "push_down" => (t(0.0, e * h), t(0.0, -(1.0 - e) * h)),
        "zoom_in" => {
            let m = transition_param(tr, "amount").max(1.0);
            let qa = p / 0.5;
            let sa = 1.0 + (m - 1.0) * qa * qa;
            let qb = ((p - 0.5) / 0.5).clamp(-1.0, 1.0);
            let sb = 1.0 + (m - 1.0) * (1.0 - qb).powi(2);
            (Affine::scale_about(sa, cx, cy), Affine::scale_about(sb, cx, cy))
        }
        "zoom_out" => {
            let m = transition_param(tr, "amount").max(1.0);
            let sa = 1.0 + (1.0 / m - 1.0) * ease(&Ease::Out, p);
            let sb = m + (1.0 - m) * ease(&Ease::Out, p);
            (Affine::scale_about(sa, cx, cy), Affine::scale_about(sb, cx, cy))
        }
        "whip" => {
            let ((mx, my), l) = whip_dir(tr, w, h);
            (t(mx * l * e, my * l * e), t(mx * l * (e - 1.0), my * l * (e - 1.0)))
        }
        _ => (id, id),
    }
}

/// Whether the transition moves the pictures (layers must be resampled through `geometry`).
pub(crate) fn is_geometric(kind: &str) -> bool {
    matches!(
        kind,
        "slide_left" | "slide_right" | "slide_up" | "slide_down" | "push_left" | "push_right" | "push_up" | "push_down" | "zoom_in" | "zoom_out" | "whip"
    )
}

pub(crate) struct TCtx {
    /// Output pixels per project pixel.
    pub sx: f64,
    pub sy: f64,
    pub draft: bool,
    /// The project frame on the canvas `[x, y, w, h]`.
    pub frame: [f64; 4],
    /// Project frame size in project pixels.
    pub pw: f64,
    pub ph: f64,
    /// Timeline frame index (re-rolls glitches every frame).
    pub frame_index: i64,
    /// One frame as a fraction of the transition window (motion-blur shutters).
    pub frame_p: f64,
    /// Per-transition seed (from its id).
    pub seed: u64,
}

/// B over A.
fn over(a: &Layer, b: &Layer) -> Layer {
    map2(a, b, |pa, pb| {
        let mut d = pa;
        blend_px(BlendMode::Normal, &mut d, pb);
        d
    })
}

/// Combines the (already moved) outgoing `a` and incoming `b` pictures.
pub(crate) fn combine(tr: &Transition, p: f64, mut a: Layer, mut b: Layer, cx: &TCtx) -> Layer {
    // Frame-relative geometry on the canvas.
    let [fx0, fy0, cw, ch] = cx.frame;
    let pf = p as f32;
    match tr.kind.as_str() {
        "dip_black" | "dip_white" => {
            let v = if tr.kind == "dip_white" { 1.0 } else { 0.0 };
            let hold = transition_param(tr, "hold").clamp(0.0, 0.98) as f32;
            let u = ((1.0 - hold) / 2.0).max(1e-4);
            map2(&a, &b, |pa, pb| {
                let cov = pa[3].max(pb[3]);
                let d = [v * cov, v * cov, v * cov, cov];
                let (src, k) = if pf < u {
                    (pa, pf / u)
                } else if pf > 1.0 - u {
                    (pb, (1.0 - pf) / u)
                } else {
                    (pa, 1.0)
                };
                let mut o = [0f32; 4];
                for i in 0..4 {
                    o[i] = src[i] + (d[i] - src[i]) * k;
                }
                o
            })
        }
        "wipe_left" | "wipe_right" | "wipe_up" | "wipe_down" => {
            let s = transition_param(tr, "softness").clamp(0.0, 1.0) as f32;
            let horizontal = matches!(tr.kind.as_str(), "wipe_left" | "wipe_right");
            let forward = matches!(tr.kind.as_str(), "wipe_left" | "wipe_up"); // edge moves towards 0
            let e = if forward { 1.0 + s / 2.0 - pf * (1.0 + s) } else { -s / 2.0 + pf * (1.0 + s) };
            map2_xy(&a, &b, |x, y, pa, pb| {
                let f = if horizontal { (x as f64 + 0.5 - fx0) / cw } else { (y as f64 + 0.5 - fy0) / ch } as f32;
                let m = smoothstep(e - s / 2.0, e + s / 2.0, f);
                let k = if forward { m } else { 1.0 - m };
                let mut o = [0f32; 4];
                for i in 0..4 {
                    o[i] = pa[i] + (pb[i] - pa[i]) * k;
                }
                o
            })
        }
        "iris" => {
            let ix = fx0 + transition_param(tr, "x").clamp(0.0, 1.0) * cw;
            let iy = fy0 + transition_param(tr, "y").clamp(0.0, 1.0) * ch;
            let soft = (transition_param(tr, "softness").clamp(0.0, 1.0) * cw.max(ch)) as f32;
            let corners = [(fx0, fy0), (fx0 + cw, fy0), (fx0, fy0 + ch), (fx0 + cw, fy0 + ch)];
            let far = corners.iter().map(|(x, y)| ((x - ix).powi(2) + (y - iy).powi(2)).sqrt()).fold(0.0, f64::max) as f32;
            let r = -soft / 2.0 + pf * (far + soft);
            map2_xy(&a, &b, |x, y, pa, pb| {
                let d = (((x as f64 + 0.5 - ix).powi(2) + (y as f64 + 0.5 - iy).powi(2)).sqrt()) as f32;
                let k = if soft > 0.5 { 1.0 - smoothstep(r - soft / 2.0, r + soft / 2.0, d) } else { (r - d + 0.5).clamp(0.0, 1.0) };
                let mut o = [0f32; 4];
                for i in 0..4 {
                    o[i] = pa[i] + (pb[i] - pa[i]) * k;
                }
                o
            })
        }
        "blur_dissolve" => {
            let peak = transition_param(tr, "radius").max(0.0) * (cx.sx + cx.sy) / 2.0;
            let lim = 4.0 * a.cw.max(a.ch).max(1) as f64;
            let sigma = (peak * (1.0 - (2.0 * p - 1.0).abs())).clamp(0.0, lim) as f32;
            let passes = if cx.draft { 2 } else { 3 };
            if sigma > 0.3 {
                let g = (3.0 * sigma).ceil() as i32 + 1;
                for l in [&mut a, &mut b] {
                    let r = l.rect.expand(g);
                    l.expand_to(r);
                    blur::gaussian(l, sigma, passes);
                }
            }
            mix_layers(&a, &b, pf)
        }
        "slide_left" | "slide_right" | "slide_up" | "slide_down" | "push_left" | "push_right" | "push_up" | "push_down" => over(&a, &b),
        "zoom_in" => mix_layers(&a, &b, smoothstep(0.4, 0.6, pf)),
        "zoom_out" => mix_layers(&a, &b, smoothstep(0.15, 0.85, pf)),
        "whip" => {
            let both = over(&a, &b);
            let ((mx, my), travel) = whip_dir(tr, cx.pw, cx.ph);
            let len = transition_param(tr, "blur").clamp(0.0, 1.0) * travel * 0.25 * inout_speed(p) / 1.5;
            let (dx, dy) = (mx * len * cx.sx, my * len * cx.sy);
            if (dx * dx + dy * dy).sqrt() < 1.0 {
                return both;
            }
            let (taps, passes) = if cx.draft { (5, 2) } else { (6, 3) };
            blur::directional_multi(&both, dx, dy, taps, passes)
        }
        "glitch" => {
            let env = (1.0 - (2.0 * p - 1.0).abs()).clamp(0.0, 1.0).powf(0.7);
            let amount = transition_param(tr, "amount").clamp(0.0, 1.0) * env;
            if amount <= 0.0 {
                return if p < 0.5 { a } else { b };
            }
            let (base, other) = if p < 0.5 { (&a, &b) } else { (&b, &a) };
            let slices = transition_param(tr, "slices").round().clamp(1.0, 100.0) as usize;
            let seed = crate::util::hash64(cx.seed ^ (cx.frame_index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let rgb = 60.0 * amount * (cx.sx + cx.sy) / 2.0;
            crate::effects::glitch_layer(base, Some(other), env * 0.45, amount, slices, rgb, seed, cx.frame)
        }
        "film_burn" => film_burn(tr, p, &a, &b, cx),
        "luma_wipe" => luma_wipe(tr, p, &a, &b),
        _ => mix_layers(&a, &b, pf),
    }
}

// ---------------------------------------------------------------------------------------------
// Motion transitions: the two pictures placed by homographies ("faces"), averaged over a shutter
// around each frame (true motion blur), with mirrored edges where a full-frame picture turns.
// ---------------------------------------------------------------------------------------------

/// One clip's picture at one instant of a motion transition.
#[derive(Clone, Copy)]
struct Face {
    /// The incoming clip (else the outgoing one).
    b: bool,
    /// Layer canvas coordinates → output canvas.
    proj: Proj,
    shade: f32,
    /// Mirror the picture's edges (full-frame pictures that turn or shake).
    mirror: bool,
}

pub(crate) fn is_motion(kind: &str) -> bool {
    matches!(kind, "spin" | "zoom_through" | "shake" | "flash_zoom" | "cube" | "flip" | "slide_blur" | "stretch")
}

fn ease_in(x: f64) -> f64 {
    ease(&Ease::In, x)
}
fn ease_out(x: f64) -> f64 {
    ease(&Ease::Out, x)
}

/// Unit direction of the `direction` param: 0 = left, 1 = right, 2 = up, 3 = down.
fn dir4(tr: &Transition) -> (f64, f64) {
    match transition_param(tr, "direction").round() as i64 {
        1 => (1.0, 0.0),
        2 => (0.0, -1.0),
        3 => (0.0, 1.0),
        _ => (-1.0, 0.0),
    }
}

/// Affine about the frame centre, as a face projection.
fn about_centre(cx: &TCtx, m: Affine) -> Proj {
    let [fx, fy, fw, fh] = cx.frame;
    let (c0, c1) = (fx + fw / 2.0, fy + fh / 2.0);
    Proj::from_affine(&Affine::translate(-c0, -c1).then(&m).then(&Affine::translate(c0, c1)))
}

/// A 3-D card through the frame: canvas coordinates relative to the frame centre are the plane
/// coordinates; `origin`, `eu`, `ev` in canvas pixels relative to the centre (z away).
fn card(cx: &TCtx, origin: V3, eu: V3, ev: V3, f: f64) -> Proj {
    let [fx, fy, fw, fh] = cx.frame;
    let (c0, c1) = (fx + fw / 2.0, fy + fh / 2.0);
    Proj::from_affine(&Affine::translate(-c0, -c1)).then(&plane_homography(origin, eu, ev, f, c0, c1))
}

/// The faces of a motion transition at progress `p` (output canvas coordinates).
fn faces(tr: &Transition, p: f64, cx: &TCtx) -> Vec<Face> {
    let p = p.clamp(0.0, 1.0);
    let [_, _, fw, fh] = cx.frame;
    let id = |b: bool, proj: Proj, mirror: bool| Face { b, proj, shade: 1.0, mirror };
    let half_a = p < 0.5;
    let qa = (p / 0.5).clamp(0.0, 1.0);
    let qb = ((p - 0.5) / 0.5).clamp(0.0, 1.0);
    match tr.kind.as_str() {
        "spin" => {
            let dir = if transition_param(tr, "direction") < 0.0 { -1.0 } else { 1.0 };
            let total = transition_param(tr, "turns").clamp(0.05, 20.0) * 360.0 * dir;
            let theta = total * inout(p);
            // Punch in while spinning (peaks at the cut) so the blur reads as a whip.
            let s = 1.0 + 0.35 * (std::f64::consts::PI * p).sin().powi(2);
            let rot = if half_a { theta } else { theta - total };
            vec![id(!half_a, about_centre(cx, Affine::rotate_deg(rot).then(&Affine::scale(s, s))), true)]
        }
        "zoom_through" | "flash_zoom" => {
            let amount = transition_param(tr, "amount").clamp(1.0, 50.0);
            let s = if half_a { amount.powf(ease_in(qa)) } else { amount.powf(1.0 - ease_out(qb)) };
            vec![id(!half_a, about_centre(cx, Affine::scale(s, s)), false)]
        }
        "shake" => {
            let amount = transition_param(tr, "amount").clamp(0.0, 5000.0);
            let a = transition_param(tr, "angle").to_radians();
            let env = (1.0 - (2.0 * p - 1.0).abs()).powf(1.5);
            let n = crate::util::noise_1d(cx.seed, p * 11.0);
            let m = crate::util::noise_1d(cx.seed ^ 0x9E37, p * 11.0 + 7.3);
            let along = amount * env * (n * 1.6).clamp(-1.0, 1.0);
            let across = amount * env * 0.2 * m;
            let (dx, dy) = (a.cos() * along - a.sin() * across, a.sin() * along + a.cos() * across);
            let (dx, dy) = (dx * cx.sx, dy * cx.sy);
            vec![id(!half_a, Proj::from_affine(&Affine::translate(dx, dy)), true)]
        }
        "slide_blur" => {
            let (ux, uy) = dir4(tr);
            let e = inout(p);
            let (lx, ly) = (ux * fw, uy * fh);
            vec![
                id(false, Proj::from_affine(&Affine::translate(lx * e, ly * e)), false),
                id(true, Proj::from_affine(&Affine::translate(lx * (e - 1.0), ly * (e - 1.0))), false),
            ]
        }
        "stretch" => {
            let (ux, uy) = dir4(tr);
            let [fx, fy, _, _] = cx.frame;
            // A stretches towards the direction from its trailing edge; B snaps back towards the
            // edge it travels to with an elastic rebound. The rebound stretches again rather than
            // squashing: a squashed picture would leave the frame uncovered behind it.
            let (k, b, anchor_sign) = if half_a { (1.0 + 3.0 * ease_in(qa), false, -1.0) } else { (1.0 + 3.0 * (1.0 - ease(&Ease::BackOut, qb)).abs(), true, 1.0) };
            let (c0, c1) = (fx + fw / 2.0, fy + fh / 2.0);
            let (ax, ay) = (c0 + anchor_sign * ux * fw / 2.0, c1 + anchor_sign * uy * fh / 2.0);
            let (sx, sy) = if ux != 0.0 { (k, 1.0) } else { (1.0, k) };
            let m = Affine::translate(-ax, -ay).then(&Affine::scale(sx, sy)).then(&Affine::translate(ax, ay));
            // Mirrored edges: the anchored edge stays at the frame edge, where plain sampling would
            // blend in the transparent outside (a dark seam, smeared into a band by the blur).
            vec![id(b, Proj::from_affine(&m), true)]
        }
        "cube" => {
            let dir = transition_param(tr, "direction").round() as i64;
            let vertical = dir >= 2;
            let sign = if dir == 1 || dir == 3 { -1.0 } else { 1.0 };
            let e = inout(p);
            let side = if vertical { fh } else { fw };
            let f = 2.0 * fw.max(fh);
            // Pull the cube back while it turns so its nearer edge stays inside the frame.
            let depth = 1.1 * side * (std::f64::consts::PI * e).sin();
            let centre = [0.0, 0.0, side / 2.0 + depth];
            let phi = -90.0 * e * sign;
            let rot = |deg: f64| if vertical { rot_x(-deg) } else { rot_y(deg) };
            let mut out = Vec::new();
            for (b, extra) in [(false, 0.0), (true, 90.0 * sign)] {
                let r = m3_mul(&rot(phi), &rot(extra));
                let o = m3_apply(&r, [0.0, 0.0, -side / 2.0]);
                let origin = [centre[0] + o[0], centre[1] + o[1], centre[2] + o[2]];
                let (eu, ev) = (m3_apply(&r, [1.0, 0.0, 0.0]), m3_apply(&r, [0.0, 1.0, 0.0]));
                if !front_facing(origin, eu, ev, f) {
                    continue;
                }
                let shade = (0.35 + 0.65 * facing(eu, ev)) as f32;
                out.push(Face { b, proj: card(cx, origin, eu, ev, f), shade, mirror: false });
            }
            out
        }
        "flip" => {
            let around_y = transition_param(tr, "axis") >= 0.5;
            let e = inout(p);
            let theta = 180.0 * e;
            let f = 2.0 * fw.max(fh);
            let depth = 0.35 * fw.max(fh) * (std::f64::consts::PI * e).sin();
            let rot = |deg: f64| if around_y { rot_y(deg) } else { rot_x(deg) };
            let mut out = Vec::new();
            for (b, ang) in [(false, theta), (true, theta - 180.0)] {
                let r = rot(ang);
                let (eu, ev) = (m3_apply(&r, [1.0, 0.0, 0.0]), m3_apply(&r, [0.0, 1.0, 0.0]));
                let origin = [0.0, 0.0, depth];
                if !front_facing(origin, eu, ev, f) {
                    continue;
                }
                let shade = (0.55 + 0.45 * facing(eu, ev)) as f32;
                out.push(Face { b, proj: card(cx, origin, eu, ev, f), shade, mirror: false });
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Shutter of a motion transition, in frames.
fn shutter_frames(tr: &Transition) -> f64 {
    match tr.kind.as_str() {
        "spin" | "zoom_through" => 1.6 * transition_param(tr, "blur").clamp(0.0, 1.0),
        "shake" => 1.6 * transition_param(tr, "blur").clamp(0.0, 1.0),
        "slide_blur" => 1.5,
        "stretch" => 1.2,
        "flash_zoom" => 0.8,
        "cube" | "flip" => 0.6,
        _ => 0.0,
    }
}

/// Largest movement (output pixels) of the frame corners and centre between two face sets.
fn faces_moved(a: &[Face], b: &[Face], cx: &TCtx) -> f64 {
    let [fx, fy, fw, fh] = cx.frame;
    let pts = [(fx, fy), (fx + fw, fy), (fx + fw, fy + fh), (fx, fy + fh), (fx + fw / 2.0, fy + fh / 2.0), (fx + fw / 2.0, fy)];
    let mut d: f64 = 0.0;
    for fa in a {
        // A face that is not in the other set (a cut, a face turning away) does not count.
        let Some(fb) = b.iter().find(|f| f.b == fa.b) else { continue };
        for &(x, y) in &pts {
            match (fa.proj.apply(x, y), fb.proj.apply(x, y)) {
                (Some(p), Some(q)) => d = d.max((p.0 - q.0).hypot(p.1 - q.1)),
                _ => return f64::INFINITY,
            }
        }
    }
    d
}

/// Box blur along x or y over `len` canvas pixels (fractional lengths exact), edges extended.
fn box_blur_axis(layer: &mut Layer, horizontal: bool, len: f64) {
    if !(len > 0.5) || layer.is_empty() {
        return;
    }
    let r = layer.rect;
    let (w, h) = (r.w as usize, r.h as usize);
    let half = len / 2.0;
    let run = |line: &mut [[f32; 4]]| {
        let n = line.len();
        // Prefix sums (f64) with edge extension through clamped indexing.
        let mut pre = vec![[0f64; 4]; n + 1];
        for i in 0..n {
            for k in 0..4 {
                pre[i + 1][k] = pre[i][k] + line[i][k] as f64;
            }
        }
        let total = pre[n];
        let first = line[0];
        let last = line[n - 1];
        // Integral of the extended signal from -inf-anchored origin at position x (pixel units).
        let integ = |x: f64, k: usize| -> f64 {
            if x <= 0.0 {
                x * first[k] as f64
            } else if x >= n as f64 {
                total[k] + (x - n as f64) * last[k] as f64
            } else {
                let i = x.floor() as usize;
                pre[i][k] + (x - i as f64) * line[i][k] as f64
            }
        };
        let src: Vec<[f64; 4]> = (0..n)
            .map(|i| {
                let c = i as f64 + 0.5;
                let mut o = [0f64; 4];
                for (k, v) in o.iter_mut().enumerate() {
                    *v = (integ(c + half, k) - integ(c - half, k)) / len;
                }
                o
            })
            .collect();
        for (o, s) in line.iter_mut().zip(src) {
            *o = [s[0] as f32, s[1] as f32, s[2] as f32, s[3] as f32];
        }
    };
    if horizontal {
        layer.px.par_chunks_mut(w).for_each(run);
    } else {
        let mut cols: Vec<Vec<[f32; 4]>> = (0..w).map(|x| (0..h).map(|y| layer.px[y * w + x]).collect()).collect();
        cols.par_iter_mut().for_each(|c| run(c));
        for (x, c) in cols.iter().enumerate() {
            for (y, v) in c.iter().enumerate() {
                layer.px[y * w + x] = *v;
            }
        }
    }
}

/// Renders a motion transition (`None` for other kinds).
pub(crate) fn motion(tr: &Transition, p: f64, a: &Layer, b: &Layer, cx: &TCtx) -> Option<Layer> {
    if !is_motion(&tr.kind) {
        return None;
    }
    let canvas = IRect::of_size(a.cw, a.ch);
    let [fx, fy, fw, fh] = cx.frame;
    let frame_px = IRect::from_bounds([fx, fy, fx + fw, fy + fh]).intersect(&canvas);
    let mirror_rect = [fx.max(0.0), fy.max(0.0), (fx + fw).min(a.cw as f64), (fy + fh).min(a.ch as f64)];
    let covers = |l: &Layer| l.rect.intersect(&frame_px) == frame_px;
    let (mirror_a, mirror_b) = (covers(a), covers(b));
    let item = |f: &Face, w: f32| -> Option<AccItem<'_>> {
        let (layer, can_mirror) = if f.b { (b, mirror_b) } else { (a, mirror_a) };
        let inv = f.proj.invert()?;
        let mirror = (f.mirror && can_mirror).then_some(mirror_rect);
        Some(AccItem { src: AccSource::Layer { layer, mirror }, inv, weight: w, shade: f.shade })
    };
    let sh = shutter_frames(tr) * cx.frame_p;
    let edge = p <= 0.0 || p >= 1.0 || sh <= 0.0;
    let (p0, p1) = if edge { (p, p) } else { ((p - sh / 2.0).max(0.0), (p + sh / 2.0).min(1.0)) };
    let analytic = matches!(tr.kind.as_str(), "slide_blur" | "stretch");
    let mut out = if analytic && !edge {
        // Translational smear: the shutter integral of a uniformly moving picture is a box blur
        // along the motion (exact, O(1) per pixel whatever the length). A shutter that straddles
        // the cut of `stretch` is split so both pictures contribute their share.
        let segs: Vec<(f64, f64)> = if tr.kind == "stretch" && p0 < 0.5 && p1 > 0.5 { vec![(p0, 0.5 - 1e-9), (0.5, p1)] } else { vec![(p0, p1)] };
        let (ux, _) = dir4(tr);
        let mut parts: Vec<(Layer, f32)> = Vec::new();
        for &(s0, s1) in &segs {
            let fm = faces(tr, (s0 + s1) / 2.0, cx);
            let items: Vec<AccItem> = fm.iter().filter_map(|f| item(f, 1.0)).collect();
            let mut l = accumulate(a.cw, a.ch, &items, canvas);
            let travel = faces_moved(&faces(tr, s0, cx), &faces(tr, s1, cx), cx);
            let travel = if travel.is_finite() { travel } else { 0.0 };
            let len = if tr.kind == "stretch" { travel * 0.6 } else { travel };
            if len > 0.5 {
                let grow = (len / 2.0).ceil() as i32 + 1;
                let r = if ux != 0.0 { IRect::new(l.rect.x - grow, l.rect.y, l.rect.w + 2 * grow, l.rect.h) } else { IRect::new(l.rect.x, l.rect.y - grow, l.rect.w, l.rect.h + 2 * grow) };
                l.expand_to(r.intersect(&canvas));
                box_blur_axis(&mut l, ux != 0.0, len);
            }
            parts.push((l, ((s1 - s0) / (p1 - p0)) as f32));
        }
        if parts.len() == 1 {
            parts.pop().map(|(l, _)| l).unwrap_or_else(|| Layer::empty(a.cw, a.ch))
        } else {
            let items: Vec<AccItem> = parts.iter().map(|(l, w)| AccItem { src: AccSource::Layer { layer: l, mirror: None }, inv: Proj::IDENTITY, weight: *w, shade: 1.0 }).collect();
            accumulate(a.cw, a.ch, &items, canvas)
        }
    } else {
        // Movement along the shutter path (a cut inside it compares each clip with itself).
        let path: Vec<Vec<Face>> = (0..=6).map(|k| faces(tr, p0 + (p1 - p0) * k as f64 / 6.0, cx)).collect();
        let moved: f64 = path.windows(2).map(|w| faces_moved(&w[0], &w[1], cx)).filter(|m| m.is_finite()).sum();
        let (div, n) = if edge || moved < 0.5 { (1, 1) } else { crate::canvas::smear_plan(moved, 96, cx.draft) };
        let mut items: Vec<AccItem> = Vec::new();
        for j in 0..n {
            let pj = if n == 1 { p } else { p0 + (p1 - p0) * (j as f64 + 0.5) / n as f64 };
            for f in faces(tr, pj, cx) {
                if let Some(it) = item(&f, 1.0 / n as f32) {
                    items.push(it);
                }
            }
        }
        accumulate_res(a.cw, a.ch, items, canvas, div)
    };
    if tr.kind == "flash_zoom" {
        // White flash peaking at the cut.
        let f = (1.0 - (2.0 * p - 1.0).abs()).clamp(0.0, 1.0).powi(3) as f32;
        if f > 0.0 {
            out.expand_to(frame_px);
            out.px.par_chunks_mut(4096).for_each(|chunk| {
                for q in chunk {
                    for k in 0..4 {
                        q[k] += (1.0 - q[k]) * f;
                    }
                }
            });
        }
    }
    Some(out)
}

/// Where each clip's picture sits during a motion transition, in project pixels: `(outgoing,
/// incoming)` projections when visible; `None` for other kinds.
pub(crate) fn face_projs(tr: &Transition, p: f64, pw: f64, ph: f64) -> Option<(Option<Proj>, Option<Proj>)> {
    if !is_motion(&tr.kind) {
        return None;
    }
    let cx = TCtx { sx: 1.0, sy: 1.0, draft: false, frame: [0.0, 0.0, pw, ph], pw, ph, frame_index: 0, frame_p: 0.0, seed: 0 };
    let fs = faces(tr, p, &cx);
    let get = |b: bool| fs.iter().find(|f| f.b == b).map(|f| f.proj);
    Some((get(false), get(true)))
}

// ---------------------------------------------------------------------------------------------
// Film burn and luma wipe
// ---------------------------------------------------------------------------------------------

/// Smooth 3-D value noise in 0..1.
fn vnoise3(seed: u64, x: f64, y: f64, z: f64) -> f64 {
    let (ix, iy, iz) = (x.floor(), y.floor(), z.floor());
    let (fx, fy, fz) = (x - ix, y - iy, z - iz);
    // Quintic fade: no visible grid creases.
    let s = |t: f64| t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
    let (sx, sy, sz) = (s(fx), s(fy), s(fz));
    let (ix, iy, iz) = (ix as i64, iy as i64, iz as i64);
    let v = |dx: i64, dy: i64, dz: i64| (crate::util::noise_i(seed ^ ((iz + dz) as u64).wrapping_mul(0x2545_F491_4F6C_DD1D), ix + dx, iy + dy) as f64 + 1.0) / 2.0;
    let l = |a: f64, b: f64, t: f64| a + (b - a) * t;
    let x00 = l(v(0, 0, 0), v(1, 0, 0), sx);
    let x10 = l(v(0, 1, 0), v(1, 1, 0), sx);
    let x01 = l(v(0, 0, 1), v(1, 0, 1), sx);
    let x11 = l(v(0, 1, 1), v(1, 1, 1), sx);
    l(l(x00, x10, sy), l(x01, x11, sy), sz)
}

fn fbm3(seed: u64, x: f64, y: f64, z: f64) -> f64 {
    let mut a = 0.5;
    let (mut x, mut y, mut z) = (x, y, z);
    let (mut sum, mut norm) = (0.0, 0.0);
    // Each octave is rotated and shifted so the lattices of the octaves never line up (value noise
    // otherwise looks blocky).
    let (c, s) = (0.7f64.cos(), 0.7f64.sin());
    for o in 0..4 {
        sum += a * vnoise3(seed.wrapping_add(o * 7919), x, y, z);
        norm += a;
        a *= 0.5;
        let (rx, ry) = (x * c - y * s, x * s + y * c);
        (x, y, z) = (rx * 2.03 + 17.3, ry * 2.03 - 9.1, z * 1.6 + 3.7);
    }
    sum / norm
}

/// Film burn: an organic light leak in `color` that sweeps over the cut, blooming to white at its
/// peak (computed on a coarse grid; it is soft by nature).
fn film_burn(tr: &Transition, p: f64, a: &Layer, b: &Layer, cx: &TCtx) -> Layer {
    let env = (std::f64::consts::PI * p).sin().max(0.0);
    let base = if p < 0.5 { a } else { b };
    let mut out = base.clone();
    if env <= 1e-6 {
        return out;
    }
    let seed = crate::util::hash64(transition_param(tr, "seed").round() as i64 as u64 ^ 0xB0B);
    let c = transition_color(tr, "color");
    let tint = [c.r as f64, c.g as f64, c.b as f64];
    let [fx, fy, fw, fh] = cx.frame;
    let frame_px = IRect::from_bounds([fx, fy, fx + fw, fy + fh]).intersect(&IRect::of_size(a.cw, a.ch));
    out.expand_to(frame_px);
    // Entry direction and the coarse field.
    let ang = crate::effects::rand01(seed, 1) * std::f64::consts::TAU;
    let (dx, dy) = (ang.cos(), ang.sin());
    let step = (fw.max(fh) / 160.0).max(2.0);
    let gw = (fw / step).ceil() as usize + 2;
    let gh = (fh / step).ceil() as usize + 2;
    // Threshold sweeps from above every field value (nothing burns at the window edges) to a level
    // where the field spans the whole ramp at the peak: hot white cores, orange fringes, picture
    // still glowing through the darkest parts.
    let thr = 1.6 - 1.05 * env;
    let aspect = fw / fh.max(1.0);
    let field: Vec<f64> = (0..gw * gh)
        .into_par_iter()
        .map(|i| {
            let (gx, gy) = ((i % gw) as f64 * step / fw, (i / gw) as f64 * step / fh);
            let (nx, ny, nz) = (gx * 3.0 * aspect.max(1.0), gy * 3.0 / aspect.min(1.0), p * 1.7);
            // Domain warp: smoky, flowing tongues instead of round blobs.
            let qx = fbm3(seed ^ 0x51, nx + 5.2, ny + 1.3, nz);
            let qy = fbm3(seed ^ 0x93, nx + 1.7, ny + 9.2, nz);
            let n = fbm3(seed, nx + 1.6 * (qx - 0.5), ny + 1.6 * (qy - 0.5), nz);
            let sweep = 0.5 + 0.5 * ((gx - 0.5) * dx + (gy - 0.5) * dy) * 2.0 * (1.0 - 2.0 * (p - 0.5).abs()).max(0.2);
            let v = 0.62 * n + 0.38 * sweep.clamp(0.0, 1.0);
            smoothstep(thr as f32 - 0.45, thr as f32 + 0.15, v as f32) as f64
        })
        .collect();
    let at = |x: f64, y: f64| -> f64 {
        let (u, v) = ((x - fx) / step, (y - fy) / step);
        let (i, j) = (u.floor().clamp(0.0, (gw - 2) as f64), v.floor().clamp(0.0, (gh - 2) as f64));
        let (tx, ty) = ((u - i).clamp(0.0, 1.0), (v - j).clamp(0.0, 1.0));
        let (i, j) = (i as usize, j as usize);
        let g = |a: usize, b: usize| field[b * gw + a];
        let top = g(i, j) + (g(i + 1, j) - g(i, j)) * tx;
        let bot = g(i, j + 1) + (g(i + 1, j + 1) - g(i, j + 1)) * tx;
        top + (bot - top) * ty
    };
    let r = out.rect;
    let w = out.stride();
    out.px.par_chunks_mut(w.max(1)).enumerate().for_each(|(row, line)| {
        let y = (r.y + row as i32) as f64 + 0.5;
        for (i, o) in line.iter_mut().enumerate() {
            let x = (r.x + i as i32) as f64 + 0.5;
            let burn = at(x, y);
            if burn <= 0.0 {
                continue;
            }
            // Ramp: saturated tint at the fringe, warm white only in the hottest core.
            let hot = [1.0, 0.96, 0.86];
            let light: [f64; 3] = if burn < 0.7 {
                let k = (burn / 0.7).powf(0.8) * 1.25;
                [tint[0] * k, tint[1] * k, tint[2] * k]
            } else {
                let k = ((burn - 0.7) / 0.3).powf(1.5);
                [tint[0] * 1.25 + (hot[0] - tint[0] * 1.25) * k, tint[1] * 1.25 + (hot[1] - tint[1] * 1.25) * k, tint[2] * 1.25 + (hot[2] - tint[2] * 1.25) * k]
            };
            // The film burns: the picture darkens away under the light that replaces it.
            let b = burn.min(1.0) as f32;
            for k in 0..3 {
                o[k] = o[k] * (1.0 - 0.85 * b) + light[k].min(1.2) as f32 * b;
            }
            o[3] = o[3] + b * (1.0 - o[3]);
        }
    });
    out
}

/// Luma wipe: the incoming clip appears where its own luminance passes a moving threshold (bright
/// first; dark first with `invert`).
fn luma_wipe(tr: &Transition, p: f64, a: &Layer, b: &Layer) -> Layer {
    let s = transition_param(tr, "softness").clamp(0.001, 1.0) as f32;
    let invert = transition_param(tr, "invert") >= 0.5;
    let t = p as f32 * (1.0 + s);
    map2(a, b, |pa, pb| {
        let lum = if pb[3] > 1e-6 { (0.2126 * pb[0] + 0.7152 * pb[1] + 0.0722 * pb[2]) / pb[3] } else { 0.0 };
        let v = if invert { 1.0 - lum.clamp(0.0, 1.0) } else { lum.clamp(0.0, 1.0) };
        let m = smoothstep(1.0 - t, 1.0 - t + s, v);
        let mut o = [0f32; 4];
        for k in 0..4 {
            o[k] = pa[k] + (pb[k] - pa[k]) * m;
        }
        o
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiframes_core::effects::TRANSITIONS;

    fn tr(kind: &str) -> Transition {
        Transition { id: "t".into(), from: "a".into(), to: "b".into(), kind: kind.into(), duration: 1.0, params: Default::default(), colors: Default::default() }
    }

    #[test]
    fn edges_are_exact_and_middle_differs() {
        let (w, h) = (32u32, 18u32);
        let cx = TCtx { sx: 1.0, sy: 1.0, draft: false, frame: [0.0, 0.0, w as f64, h as f64], pw: w as f64, ph: h as f64, frame_index: 7, frame_p: 1.0 / 12.0, seed: 3 };
        for spec in TRANSITIONS {
            let t = tr(spec.kind);
            let run = |p: f64| {
                let a = Layer::full(w, h, [1.0, 0.0, 0.0, 1.0]);
                let mut b = Layer::full(w, h, [0.0, 0.0, 1.0, 1.0]);
                if spec.kind == "luma_wipe" {
                    // A luma wipe needs luminance detail in the incoming picture.
                    for (i, q) in b.px.iter_mut().enumerate() {
                        let v = (i % w as usize) as f32 / w as f32 * 0.6;
                        *q = [v, v, 1.0, 1.0];
                    }
                }
                if let Some(o) = motion(&t, p, &a, &b, &cx) {
                    return o;
                }
                let (ga, gb) = geometry(&t, p, w as f64, h as f64);
                let a = if is_geometric(spec.kind) { crate::canvas::transform_layer(&a, &ga) } else { a };
                let b = if is_geometric(spec.kind) { crate::canvas::transform_layer(&b, &gb) } else { b };
                combine(&t, p, a, b, &cx)
            };
            let l0 = run(0.0);
            let l1 = run(1.0);
            let lm = run(0.5);
            let a_ref = |_x: i32, _y: i32| [1.0f32, 0.0, 0.0, 1.0];
            let b_ref = |x: i32, _y: i32| {
                if spec.kind == "luma_wipe" {
                    let v = x as f32 / w as f32 * 0.6;
                    [v, v, 1.0, 1.0]
                } else {
                    [0.0, 0.0, 1.0, 1.0]
                }
            };
            let close = |p: [f32; 4], q: [f32; 4]| (0..4).all(|k| (p[k] - q[k]).abs() < 0.02);
            for (x, y) in [(1, 1), (16, 9), (30, 16)] {
                assert!(close(l0.get(x, y), a_ref(x, y)), "{} p=0 at {x},{y}: {:?}", spec.kind, l0.get(x, y));
                assert!(close(l1.get(x, y), b_ref(x, y)), "{} p=1 at {x},{y}: {:?}", spec.kind, l1.get(x, y));
            }
            let mut differs = false;
            let mut differs_b = false;
            for y in 0..h as i32 {
                for x in 0..w as i32 {
                    let q = lm.get(x, y);
                    differs |= !close(q, a_ref(x, y));
                    differs_b |= !close(q, b_ref(x, y));
                }
            }
            assert!(differs && differs_b, "{} midpoint equals an end", spec.kind);
        }
    }
}
