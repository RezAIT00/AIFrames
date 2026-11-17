//! Every effect of `aiframes_core::effects::EFFECTS`, applied in canvas space to a premultiplied
//! layer (after the clip's transform; positions are fractions of the frame, sizes in project pixels
//! scaled to the output).

use crate::blur;
use crate::canvas::{blend_px, transform_layer, Layer, Px};
use crate::geom::{Affine, IRect};
use crate::lut::Lut;
use crate::util::{hash64, noise_1d, noise_i, smoothstep};
use aiframes_core::effects::{effect_color, effect_param, effect_string};
use aiframes_core::{BlendMode, Effect};
use rayon::prelude::*;
use std::sync::Arc;

/// What effects need to know about the frame being rendered.
pub(crate) struct FxCtx<'a> {
    pub t_local: f64,
    /// Timeline frame index (seeds the grain).
    pub frame: i64,
    /// The project frame on the canvas `[x, y, w, h]` (effect positions are fractions of it).
    pub rect: [f64; 4],
    /// Output pixels per project pixel.
    pub px_scale: f64,
    pub draft: bool,
    /// Loads a LUT by its `path` string (cached by the renderer).
    pub lut: &'a (dyn Fn(&str) -> Result<Arc<Lut>, String> + Sync),
    /// Reports a non-fatal problem; returns Err when rendering is strict.
    pub problem: &'a (dyn Fn(String) -> Result<(), String> + Sync),
}

const R709: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// Upper bound for any pixel-sized effect parameter in project pixels (larger values change nothing
/// visible and would only overflow or stall).
pub(crate) const MAX_PX: f64 = 100_000.0;

#[inline]
fn luma(c: &[f32; 3]) -> f32 {
    c[0] * R709[0] + c[1] * R709[1] + c[2] * R709[2]
}

/// Runs `f` on the straight colour of every non-transparent pixel.
fn map_straight(layer: &mut Layer, f: impl Fn(&mut [f32; 3]) + Sync) {
    layer.px.par_chunks_mut(4096).for_each(|chunk| {
        for p in chunk {
            let a = p[3];
            if a <= 1e-6 {
                continue;
            }
            let inv = 1.0 / a;
            let mut c = [p[0] * inv, p[1] * inv, p[2] * inv];
            f(&mut c);
            p[0] = c[0].max(0.0) * a;
            p[1] = c[1].max(0.0) * a;
            p[2] = c[2].max(0.0) * a;
        }
    });
}

/// Runs `f(x, y, px)` over every pixel of the layer (canvas coordinates).
fn map_xy(layer: &mut Layer, f: impl Fn(i32, i32, &mut Px) + Sync) {
    let r = layer.rect;
    let w = layer.stride();
    if w == 0 {
        return;
    }
    layer.px.par_chunks_mut(w).enumerate().for_each(|(row, line)| {
        let y = r.y + row as i32;
        for (i, p) in line.iter_mut().enumerate() {
            f(r.x + i as i32, y, p);
        }
    });
}

/// Canvas-space offset `(dx, dy)` in project px and rotation (degrees) of a `shake` effect.
pub(crate) fn shake_offset(e: &Effect, t_local: f64) -> (f64, f64, f64) {
    let amount = effect_param(e, "amount", t_local).clamp(0.0, MAX_PX);
    let freq = effect_param(e, "frequency", t_local).clamp(0.01, 240.0);
    let rot = effect_param(e, "rotation", t_local).clamp(0.0, 360.0);
    let seed = hash64(effect_param(e, "seed", t_local).round() as i64 as u64);
    let x = t_local * freq;
    (
        noise_1d(seed ^ 0x51, x) * amount,
        noise_1d(seed ^ 0xA7, x + 17.31) * amount,
        noise_1d(seed ^ 0x3C, x + 41.7) * rot,
    )
}

/// How far (canvas pixels) an effect stack can spread a layer beyond its content; `i32::MAX` when
/// an effect paints the whole frame (letterbox, zoom blur).
pub(crate) fn reach(effects: &[Effect], t_local: f64, px_scale: f64) -> i32 {
    let mut r = 0.0f64;
    for e in effects.iter().filter(|e| e.enabled) {
        let p = |n: &str| effect_param(e, n, t_local).clamp(0.0, MAX_PX);
        r += match e.kind.as_str() {
            "blur" => 3.0 * p("radius") * px_scale + 2.0,
            "drop_shadow" => (p("distance") + 1.5 * p("blur")) * px_scale + 3.0,
            "glow" => 1.5 * p("radius") * px_scale + 2.0,
            "rgb_split" => p("amount") * px_scale / 2.0 + 2.0,
            "shake" => p("amount") * px_scale + 2.0,
            "directional_blur" => p("length") * px_scale / 2.0 + 2.0,
            "edge_glow" => 1.5 * p("radius") * px_scale + 4.0,
            "letterbox" | "zoom_blur" | "glitch" | "lens_distortion" => return i32::MAX,
            _ => 0.0,
        };
    }
    r.ceil().min(i32::MAX as f64 / 2.0) as i32
}

/// Applies an effect stack in order. `skip_leading_shakes`: shakes at the start of the stack were
/// already folded into the clip transform.
pub(crate) fn apply_effects(layer: &mut Layer, effects: &[Effect], cx: &FxCtx, skip_leading_shakes: bool) -> Result<(), String> {
    let mut leading = skip_leading_shakes;
    for e in effects {
        if !e.enabled {
            continue;
        }
        if leading && e.kind == "shake" {
            continue;
        }
        leading = false;
        apply_one(layer, e, cx)?;
    }
    Ok(())
}

fn apply_one(layer: &mut Layer, e: &Effect, cx: &FxCtx) -> Result<(), String> {
    let t = cx.t_local;
    let p = |n: &str| effect_param(e, n, t);
    let s = cx.px_scale;
    let passes = if cx.draft { 2 } else { 3 };
    // Frame-relative geometry: (fx0, fy0) is the frame's top-left on the canvas, (cw, ch) its size.
    let [fx0, fy0, cw, ch] = cx.rect;
    // Sizes in canvas pixels, clamped to a few canvas sizes (bigger changes nothing visible).
    let max_px = 4.0 * layer.cw.max(layer.ch).max(1) as f64;
    let size = |n: &str| (p(n) * s).clamp(0.0, max_px);
    let frame_px = IRect::from_bounds([fx0, fy0, fx0 + cw, fy0 + ch]).intersect(&IRect::of_size(layer.cw, layer.ch));
    match e.kind.as_str() {
        "color" => color(layer, e, t),
        "blur" => {
            let sigma = size("radius") as f32;
            if sigma > 0.3 {
                let g = (3.0 * sigma).ceil() as i32 + 1;
                let r = layer.rect.expand(g);
                layer.expand_to(r);
                blur::gaussian(layer, sigma, passes);
            }
        }
        "sharpen" => {
            let amount = p("amount").clamp(0.0, 100.0) as f32;
            // At least 0.6 canvas px: the box-blur approximation of a narrower Gaussian is the
            // identity, so a radius under 0.6 px (or any radius in a reduced-size preview) would show
            // no sharpening at all.
            let sigma = size("radius").max(0.6) as f32;
            if amount > 0.0 {
                let mut soft = layer.clone();
                blur::gaussian(&mut soft, sigma, passes);
                let src = &soft;
                let r = layer.rect;
                let w = layer.stride();
                // Unsharp mask on straight colour against the alpha-weighted blur, so transparent
                // surroundings do not count as dark detail (no bright rim at the clip's edge).
                layer.px.par_chunks_mut(w.max(1)).enumerate().for_each(|(row, line)| {
                    let y = r.y + row as i32;
                    for (i, o) in line.iter_mut().enumerate() {
                        let a = o[3];
                        let b = src.get(r.x + i as i32, y);
                        if a <= 1e-6 || b[3] <= 1e-6 {
                            continue;
                        }
                        for k in 0..3 {
                            let c = o[k] / a;
                            let bc = b[k] / b[3];
                            o[k] = (c + amount * (c - bc)).clamp(0.0, 4.0) * a;
                        }
                    }
                });
            }
        }
        "vignette" => {
            let amount = p("amount").clamp(0.0, 1.0) as f32;
            let size = p("size").clamp(0.0, 1.0) as f32;
            let soft = p("softness").clamp(0.0, 1.0) as f32;
            let col = effect_color(e, "color");
            let vc = [col.r, col.g, col.b];
            let half_diag = ((cw / 2.0).powi(2) + (ch / 2.0).powi(2)).sqrt().max(1.0);
            let (hx, hy) = (fx0 + cw / 2.0, fy0 + ch / 2.0);
            let outer = size + soft.max(0.002);
            map_xy(layer, |x, y, px| {
                let dx = (x as f64 + 0.5 - hx) / half_diag;
                let dy = (y as f64 + 0.5 - hy) / half_diag;
                let d = (dx * dx + dy * dy).sqrt() as f32;
                let m = smoothstep(size, outer, d) * amount;
                if m > 0.0 {
                    let a = px[3];
                    for k in 0..3 {
                        px[k] += (vc[k] * a - px[k]) * m;
                    }
                }
            });
        }
        "chroma_key" => chroma_key(layer, e, t),
        "mask_rect" | "mask_ellipse" => {
            let ellipse = e.kind == "mask_ellipse";
            let mx = fx0 + p("x") * cw;
            let my = fy0 + p("y") * ch;
            let hw = (p("width").max(0.0) * cw / 2.0) as f32;
            let hh = (p("height").max(0.0) * ch / 2.0) as f32;
            let feather = (p("feather").max(0.0) * s) as f32;
            let radius = if ellipse { 0.0 } else { ((p("radius").max(0.0) * s) as f32).min(hw.min(hh)) };
            let invert = p("invert") >= 0.5;
            map_xy(layer, |x, y, px| {
                let qx = (x as f64 + 0.5 - mx) as f32;
                let qy = (y as f64 + 0.5 - my) as f32;
                let sd = if ellipse { ellipse_sd(qx, qy, hw, hh) } else { rrect_sd(qx, qy, hw, hh, radius) };
                let mut cov = if feather > 0.5 { (0.5 - sd / feather).clamp(0.0, 1.0) } else { (0.5 - sd).clamp(0.0, 1.0) };
                if feather > 0.5 {
                    cov = cov * cov * (3.0 - 2.0 * cov);
                }
                if invert {
                    cov = 1.0 - cov;
                }
                if cov < 1.0 {
                    for v in px.iter_mut() {
                        *v *= cov;
                    }
                }
            });
        }
        "drop_shadow" => {
            let dist = size("distance");
            let ang = p("angle").to_radians();
            let sigma = (size("blur") / 2.0) as f32;
            let opacity = p("opacity").clamp(0.0, 1.0) as f32;
            let c = effect_color(e, "color");
            let (dx, dy) = ((ang.cos() * dist).round() as i32, (ang.sin() * dist).round() as i32);
            let ca = c.a * opacity;
            blur::shadow_under(layer, dx, dy, sigma, [c.r * ca, c.g * ca, c.b * ca, ca], passes);
        }
        "glow" => {
            let sigma = (size("radius") / 2.0) as f32;
            let intensity = p("intensity").clamp(0.0, 1000.0) as f32;
            let thr = p("threshold").clamp(0.0, 1.0) as f32;
            let c = effect_color(e, "color");
            if intensity > 0.0 {
                let grow = (3.0 * sigma).ceil() as i32 + 1;
                let r = layer.rect.expand(grow);
                layer.expand_to(r);
                let mut bright = layer.clone();
                let tint = [c.r, c.g, c.b];
                bright.px.par_chunks_mut(4096).for_each(|chunk| {
                    for p in chunk {
                        let a = p[3];
                        if a <= 1e-6 {
                            *p = [0.0; 4];
                            continue;
                        }
                        let st = [p[0] / a, p[1] / a, p[2] / a];
                        let y = luma(&st);
                        let w = if thr >= 1.0 { 0.0 } else { ((y - thr) / (1.0 - thr)).clamp(0.0, 1.0) };
                        let w = w * w * (3.0 - 2.0 * w);
                        *p = [p[0] * w * tint[0], p[1] * w * tint[1], p[2] * w * tint[2], a * w];
                    }
                });
                blur::gaussian(&mut bright, sigma.max(0.31), passes);
                let src = &bright;
                let r = layer.rect;
                let w = layer.stride();
                layer.px.par_chunks_mut(w.max(1)).enumerate().for_each(|(row, line)| {
                    let y = r.y + row as i32;
                    for (i, o) in line.iter_mut().enumerate() {
                        let g = src.get(r.x + i as i32, y);
                        let add = [g[0] * intensity, g[1] * intensity, g[2] * intensity];
                        o[0] += add[0];
                        o[1] += add[1];
                        o[2] += add[2];
                        let ga = add[0].max(add[1]).max(add[2]).min(1.0);
                        o[3] = o[3] + ga * (1.0 - o[3]);
                    }
                });
            }
        }
        "pixelate" => {
            let b = size("size").min(layer.cw.max(layer.ch) as f64).round().max(1.0) as i32;
            pixelate(layer, b, (fx0.round() as i32, fy0.round() as i32));
        }
        "grain" => {
            let amount = p("amount").clamp(0.0, 1.0) as f32;
            if amount > 0.0 {
                let size = (p("size").max(0.5) * s).max(0.35);
                let chroma = p("chroma") >= 0.5;
                let seed = hash64((p("seed").round() as i64 as u64).wrapping_mul(0x9E37_79B9) ^ hash64(cx.frame as u64));
                let k = amount * 0.5;
                let (gx0, gy0) = (fx0.round() as i32, fy0.round() as i32);
                map_xy(layer, |x, y, px| {
                    let (x, y) = (x - gx0, y - gy0);
                    let a = px[3];
                    if a <= 1e-6 {
                        return;
                    }
                    let n = |sd: u64| -> f32 {
                        if size <= 1.0 {
                            noise_i(sd, x as i64, y as i64)
                        } else {
                            let fx = x as f64 / size;
                            let fy = y as f64 / size;
                            let (ix, iy) = (fx.floor(), fy.floor());
                            let (tx, ty) = ((fx - ix) as f32, (fy - iy) as f32);
                            let (ix, iy) = (ix as i64, iy as i64);
                            let a0 = noise_i(sd, ix, iy) + (noise_i(sd, ix + 1, iy) - noise_i(sd, ix, iy)) * tx;
                            let b0 = noise_i(sd, ix, iy + 1) + (noise_i(sd, ix + 1, iy + 1) - noise_i(sd, ix, iy + 1)) * tx;
                            (a0 + (b0 - a0) * ty) * 1.4
                        }
                    };
                    if chroma {
                        let (nr, ng, nb) = (n(seed), n(seed ^ 0xABCD), n(seed ^ 0x1234_5678));
                        px[0] = (px[0] + nr * k * a).max(0.0);
                        px[1] = (px[1] + ng * k * a).max(0.0);
                        px[2] = (px[2] + nb * k * a).max(0.0);
                    } else {
                        let v = n(seed) * k * a;
                        px[0] = (px[0] + v).max(0.0);
                        px[1] = (px[1] + v).max(0.0);
                        px[2] = (px[2] + v).max(0.0);
                    }
                });
            }
        }
        "lut" => {
            let path = effect_string(e, "path");
            let intensity = p("intensity").clamp(0.0, 1.0) as f32;
            if path.is_empty() {
                (cx.problem)("lut effect has no `path` string (set strings.path to a .cube file)".to_string())?;
            } else if intensity > 0.0 {
                match (cx.lut)(path) {
                    Ok(lut) => {
                        map_straight(layer, |c| {
                            let o = lut.apply(*c);
                            for k in 0..3 {
                                c[k] += (o[k] - c[k]) * intensity;
                            }
                        });
                    }
                    Err(msg) => (cx.problem)(msg)?,
                }
            }
        }
        "black_white" => {
            let amount = p("amount").clamp(0.0, 1.0) as f32;
            let w = [p("red") as f32, p("green") as f32, p("blue") as f32];
            map_straight(layer, |c| {
                let g = c[0] * w[0] + c[1] * w[1] + c[2] * w[2];
                for v in c.iter_mut() {
                    *v += (g - *v) * amount;
                }
            });
        }
        "invert" => {
            let amount = p("amount").clamp(0.0, 1.0) as f32;
            map_straight(layer, |c| {
                for v in c.iter_mut() {
                    let inv = 1.0 - v.clamp(0.0, 1.0);
                    *v += (inv - *v) * amount;
                }
            });
        }
        "posterize" => {
            let levels = p("levels").round().clamp(2.0, 256.0) as f32;
            let n = levels - 1.0;
            map_straight(layer, |c| {
                for v in c.iter_mut() {
                    *v = (v.clamp(0.0, 1.0) * n).round() / n;
                }
            });
        }
        "letterbox" => {
            let aspect = p("aspect").clamp(0.05, 20.0);
            let c = effect_color(e, "color");
            let col = crate::canvas::premul(c);
            let frame_aspect = cw / ch.max(1.0);
            layer.expand_to(frame_px);
            if aspect > frame_aspect {
                let bar = (ch - cw / aspect) / 2.0;
                map_xy(layer, |_, y, px| {
                    let yc = y as f64 + 0.5 - fy0;
                    let cov = (bar - yc + 0.5).clamp(0.0, 1.0).max((yc - (ch - bar) + 0.5).clamp(0.0, 1.0)) as f32;
                    if cov > 0.0 {
                        let s = [col[0] * cov, col[1] * cov, col[2] * cov, col[3] * cov];
                        blend_px(BlendMode::Normal, px, s);
                    }
                });
            } else if aspect < frame_aspect {
                let bar = (cw - ch * aspect) / 2.0;
                map_xy(layer, |x, _, px| {
                    let xc = x as f64 + 0.5 - fx0;
                    let cov = (bar - xc + 0.5).clamp(0.0, 1.0).max((xc - (cw - bar) + 0.5).clamp(0.0, 1.0)) as f32;
                    if cov > 0.0 {
                        let s = [col[0] * cov, col[1] * cov, col[2] * cov, col[3] * cov];
                        blend_px(BlendMode::Normal, px, s);
                    }
                });
            }
        }
        "zoom_blur" => {
            let amount = p("amount").clamp(0.0, 1.0);
            if amount > 0.001 {
                layer.expand_to(frame_px);
                let (taps, passes) = if cx.draft { (5, 2) } else { (6, 3) };
                *layer = blur::zoom(layer, fx0 + p("x") * cw, fy0 + p("y") * ch, amount, taps, passes);
            }
        }
        "rgb_split" => {
            let amount = size("amount");
            if amount > 0.01 {
                let a = p("angle").to_radians();
                let (hx, hy) = (a.cos() * amount / 2.0, a.sin() * amount / 2.0);
                let grow = (amount / 2.0).ceil() as i32 + 1;
                let r = layer.rect.expand(grow);
                layer.expand_to(r);
                let src = layer.clone();
                let clamp = src.touches_edges();
                map_xy(layer, |x, y, px| {
                    let (fx, fy) = (x as f64 + 0.5, y as f64 + 0.5);
                    let red = src.sample(fx - hx, fy - hy, clamp);
                    let blue = src.sample(fx + hx, fy + hy, clamp);
                    let g = *px;
                    *px = [red[0], g[1], blue[2], red[3].max(g[3]).max(blue[3])];
                });
            }
        }
        // Time effects: applied by the clip pipeline (compose), not to pixels.
        "echo" | "stutter" => {}
        "directional_blur" => {
            let len = size("length");
            if len > 0.5 {
                let a = p("angle").to_radians();
                let (taps, passes) = if cx.draft { (5, 2) } else { (6, 3) };
                *layer = blur::directional_multi(layer, a.cos() * len, a.sin() * len, taps, passes);
            }
        }
        "glitch" => {
            let amount = p("amount").clamp(0.0, 1.0);
            if amount > 0.0 {
                let slices = p("slices").round().clamp(1.0, 100.0) as usize;
                let seed = hash64((p("seed").round() as i64 as u64).wrapping_mul(0x9E37_79B9) ^ hash64(cx.frame as u64 ^ 0xC0FFEE));
                *layer = glitch_layer(layer, None, 0.0, amount, slices, size("rgb"), seed, cx.rect);
            }
        }
        "edge_glow" => {
            let intensity = p("intensity").clamp(0.0, 100.0) as f32;
            if intensity > 0.0 {
                let c = effect_color(e, "color");
                edge_glow(layer, p("threshold").clamp(0.0, 1.0) as f32, (size("radius") / 2.0).max(0.5) as f32, intensity, [c.r, c.g, c.b], passes);
            }
        }
        "lens_distortion" => lens_distortion(layer, p("amount"), cx.rect),
        "exposure_flash" => {
            let a = p("amount").clamp(0.0, 100.0) as f32;
            if a > 0.0 {
                layer.px.par_chunks_mut(4096).for_each(|chunk| {
                    for q in chunk {
                        let k = a * q[3];
                        q[0] += k;
                        q[1] += k;
                        q[2] += k;
                    }
                });
            }
        }
        "shake" => {
            let (dx, dy, rot) = shake_offset(e, t);
            let (hx, hy) = (fx0 + cw / 2.0, fy0 + ch / 2.0);
            let m = Affine::translate(-hx, -hy)
                .then(&Affine::rotate_deg(rot))
                .then(&Affine::translate(hx + (dx * s).clamp(-max_px, max_px), hy + (dy * s).clamp(-max_px, max_px)));
            *layer = transform_layer(layer, &m);
        }
        other => {
            (cx.problem)(format!("unknown effect `{other}` (see list_effects for the available kinds)"))?;
        }
    }
    Ok(())
}

/// Signed distance to a rounded box of half size (hw, hh) centred at the origin.
#[inline]
fn rrect_sd(x: f32, y: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = x.abs() - (hw - r);
    let qy = y.abs() - (hh - r);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

/// Approximate signed distance to an ellipse with radii (rx, ry).
#[inline]
fn ellipse_sd(x: f32, y: f32, rx: f32, ry: f32) -> f32 {
    if rx <= 0.0 || ry <= 0.0 {
        return f32::INFINITY;
    }
    let k0 = ((x / rx).powi(2) + (y / ry).powi(2)).sqrt();
    let k1 = ((x / (rx * rx)).powi(2) + (y / (ry * ry)).powi(2)).sqrt();
    if k1 <= 1e-12 {
        return -rx.min(ry);
    }
    k0 * (k0 - 1.0) / k1
}

fn color(layer: &mut Layer, e: &Effect, t: f64) {
    let p = |n: &str| effect_param(e, n, t) as f32;
    let exposure = p("exposure");
    let contrast = p("contrast").clamp(-1.0, 4.0);
    let saturation = p("saturation").max(0.0);
    let vibrance = p("vibrance");
    let temperature = p("temperature");
    let tint = p("tint");
    let brightness = p("brightness");
    let gamma = p("gamma").max(0.01);
    let highlights = p("highlights");
    let shadows = p("shadows");
    let hue = p("hue");
    // Exposure in linear light: with a 2.2 power curve, ×2^e in linear is ×2^(e/2.2) encoded.
    let gain = 2f32.powf(exposure / 2.2);
    let wb = [1.0 + 0.2 * temperature + 0.1 * tint, 1.0 - 0.2 * tint, 1.0 - 0.2 * temperature + 0.1 * tint];
    let inv_gamma = 1.0 / gamma;
    let hue_m = if hue.abs() > 1e-3 { Some(hue_matrix(hue)) } else { None };
    let tone = highlights.abs() > 1e-4 || shadows.abs() > 1e-4;
    let sat_on = (saturation - 1.0).abs() > 1e-4 || vibrance.abs() > 1e-4;
    map_straight(layer, |c| {
        for k in 0..3 {
            c[k] = c[k] * gain * wb[k];
            c[k] = (c[k] - 0.5) * (1.0 + contrast) + 0.5 + brightness;
            if (gamma - 1.0).abs() > 1e-4 {
                c[k] = c[k].max(0.0).powf(inv_gamma);
            }
        }
        if tone {
            let y = luma(c).max(0.0);
            let wh = smoothstep(0.45, 1.0, y);
            let ws = 1.0 - smoothstep(0.0, 0.55, y);
            let delta = 0.5 * (highlights * wh + shadows * ws);
            let ny = (y + delta).max(0.0);
            if y > 0.02 {
                let k = ny / y;
                for v in c.iter_mut() {
                    *v *= k;
                }
            } else {
                for v in c.iter_mut() {
                    *v += delta;
                }
            }
        }
        if let Some(m) = &hue_m {
            let (r, g, b) = (c[0], c[1], c[2]);
            c[0] = m[0] * r + m[1] * g + m[2] * b;
            c[1] = m[3] * r + m[4] * g + m[5] * b;
            c[2] = m[6] * r + m[7] * g + m[8] * b;
        }
        if sat_on {
            let y = luma(c);
            let mx = c[0].max(c[1]).max(c[2]);
            let mn = c[0].min(c[1]).min(c[2]);
            let sat = if mx > 1e-6 { ((mx - mn) / mx).clamp(0.0, 1.0) } else { 0.0 };
            let s = (saturation * (1.0 + vibrance * (1.0 - sat))).max(0.0);
            for v in c.iter_mut() {
                *v = y + (*v - y) * s;
            }
        }
    });
}

/// RGB rotation about the grey axis by `deg`.
fn hue_matrix(deg: f32) -> [f32; 9] {
    let (s, c) = deg.to_radians().sin_cos();
    let k = 1.0 / 3.0;
    let sq = (1.0f32 / 3.0).sqrt();
    let t = k * (1.0 - c);
    [c + t, t - sq * s, t + sq * s, t + sq * s, c + t, t - sq * s, t - sq * s, t + sq * s, c + t]
}

fn chroma_key(layer: &mut Layer, e: &Effect, t: f64) {
    let tol = effect_param(e, "tolerance", t).clamp(0.0, 1.0) as f32;
    let soft = effect_param(e, "softness", t).clamp(0.0, 1.0) as f32;
    let spill = effect_param(e, "spill", t).clamp(0.0, 1.0) as f32;
    let key = effect_color(e, "key");
    // BT.601 YCbCr like common keyers (OBS): Cb/Cr in -0.5..0.5.
    let ycc = |r: f32, g: f32, b: f32| {
        let y = 0.299 * r + 0.587 * g + 0.114 * b;
        (y, -0.168_736 * r - 0.331_264 * g + 0.5 * b, 0.5 * r - 0.418_688 * g - 0.081_312 * b)
    };
    let (_, kcb, kcr) = ycc(key.r, key.g, key.b);
    let klen = (kcb * kcb + kcr * kcr).sqrt();
    let kdir = if klen > 1e-4 { Some((kcb / klen, kcr / klen)) } else { None };
    layer.px.par_chunks_mut(4096).for_each(|chunk| {
        for p in chunk {
            let a = p[3];
            if a <= 1e-6 {
                continue;
            }
            let (r, g, b) = (p[0] / a, p[1] / a, p[2] / a);
            let (y, mut cb, mut cr) = ycc(r, g, b);
            let d = ((cb - kcb).powi(2) + (cr - kcr).powi(2)).sqrt();
            let keep = if soft > 1e-4 { smoothstep(tol, tol + soft, d) } else if d > tol { 1.0 } else { 0.0 };
            if let Some((kx, ky)) = kdir {
                let proj = cb * kx + cr * ky;
                if proj > 0.0 && spill > 0.0 {
                    cb -= kx * proj * spill;
                    cr -= ky * proj * spill;
                }
            }
            let nr = y + 1.402 * cr;
            let nb = y + 1.772 * cb;
            let ng = (y - 0.299 * nr - 0.114 * nb) / 0.587;
            let na = a * keep;
            *p = [nr.max(0.0) * na, ng.max(0.0) * na, nb.max(0.0) * na, na];
        }
    });
}

fn pixelate(layer: &mut Layer, b: i32, origin: (i32, i32)) {
    if b <= 1 || layer.is_empty() {
        return;
    }
    // Work in frame-relative coordinates so blocks align with the frame's corner.
    let (ox, oy) = origin;
    let r = layer.rect.offset(-ox, -oy);
    // Blocks aligned to the canvas origin.
    let bx0 = r.x.div_euclid(b);
    let by0 = r.y.div_euclid(b);
    let bx1 = (r.right() - 1).div_euclid(b);
    let by1 = (r.bottom() - 1).div_euclid(b);
    let nbx = (bx1 - bx0 + 1) as usize;
    let nby = (by1 - by0 + 1) as usize;
    let src = &*layer;
    // The canvas in the same frame-relative coordinates: edge blocks are averaged over their
    // on-canvas pixels only (pixels off the canvas do not exist; pixels off the layer are transparent).
    let canvas = IRect::of_size(layer.cw, layer.ch).offset(-ox, -oy);
    let avgs: Vec<Px> = (0..nby)
        .into_par_iter()
        .flat_map_iter(|j| {
            let by = by0 + j as i32;
            (0..nbx).map(move |i| {
                let bx = bx0 + i as i32;
                let x0 = bx.saturating_mul(b).max(r.x);
                let x1 = (bx + 1).saturating_mul(b).min(r.right());
                let y0 = by.saturating_mul(b).max(r.y);
                let y1 = (by + 1).saturating_mul(b).min(r.bottom());
                let mut acc = [0f64; 4];
                for y in y0..y1 {
                    for x in x0..x1 {
                        let p = src.get(x + ox, y + oy);
                        for k in 0..4 {
                            acc[k] += p[k] as f64;
                        }
                    }
                }
                let bx0c = (bx as i64 * b as i64).max(canvas.x as i64);
                let bx1c = ((bx as i64 + 1) * b as i64).min(canvas.right() as i64);
                let by0c = (by as i64 * b as i64).max(canvas.y as i64);
                let by1c = ((by as i64 + 1) * b as i64).min(canvas.bottom() as i64);
                let n = ((bx1c - bx0c).max(1) * (by1c - by0c).max(1)) as f64;
                [(acc[0] / n) as f32, (acc[1] / n) as f32, (acc[2] / n) as f32, (acc[3] / n) as f32]
            })
        })
        .collect();
    map_xy(layer, |x, y, px| {
        let i = ((x - ox).div_euclid(b) - bx0) as usize;
        let j = ((y - oy).div_euclid(b) - by0) as usize;
        *px = avgs[j * nbx + i];
    });
}

// ---------------------------------------------------------------------------------------------
// Edit-culture effects
// ---------------------------------------------------------------------------------------------

/// Uniform random 0..1 from a seed and an index.
#[inline]
pub(crate) fn rand01(seed: u64, i: u64) -> f64 {
    (hash64(seed ^ hash64(i.wrapping_add(0x5851_F42D_4C95_7F2D))) >> 11) as f64 / (1u64 << 53) as f64
}

/// Digital glitch over the frame rect `[x, y, w, h]`: horizontal bands of `src` jump sideways and
/// split their colour channels. `amount` 0..1 scales how many bands move and how far; `rgb` is the
/// channel split in canvas pixels; with `other`, bands may show it instead (`swap` = probability).
/// Deterministic for a `seed` (callers mix in the frame index to re-roll each frame).
#[allow(clippy::too_many_arguments)]
pub(crate) fn glitch_layer(src: &Layer, other: Option<&Layer>, swap: f64, amount: f64, slices: usize, rgb: f64, seed: u64, rect: [f64; 4]) -> Layer {
    let [fx, fy, fw, fh] = rect;
    let canvas = IRect::of_size(src.cw, src.ch);
    let frame = IRect::from_bounds([fx, fy, fx + fw, fy + fh]).intersect(&canvas);
    let out_rect = frame.union(&src.rect).intersect(&canvas);
    let mut out = Layer::new(src.cw, src.ch, out_rect);
    if out_rect.is_empty() {
        return out;
    }
    let amount = amount.clamp(0.0, 1.0);
    let n = slices.clamp(1, 400);
    // Band edges: random cuts, sorted.
    let mut cuts: Vec<f64> = (0..n.saturating_sub(1)).map(|k| fy + rand01(seed, 1000 + k as u64) * fh).collect();
    cuts.sort_by(f64::total_cmp);
    struct Band {
        dx: i32,
        split: i32,
        other: bool,
    }
    let bands: Vec<Band> = (0..n)
        .map(|k| {
            let r = |j: u64| rand01(seed, (k as u64) * 16 + j);
            let moves = r(0) < amount * 0.9;
            let big = r(1) < 0.25;
            let reach = if big { 0.35 } else { 0.08 };
            let dx = if moves { ((r(2) * 2.0 - 1.0) * amount * reach * fw).round() as i32 } else { 0 };
            let split = if moves || r(3) < amount * 0.3 { ((r(4) * 0.5 + 0.5) * rgb * amount * if r(5) < 0.5 { -1.0 } else { 1.0 }).round() as i32 } else { 0 };
            Band { dx, split, other: other.is_some() && r(6) < swap }
        })
        .collect();
    let band_of = |y: f64| cuts.partition_point(|c| *c <= y);
    let r = out.rect;
    let w = out.stride();
    out.px.par_chunks_mut(w.max(1)).enumerate().for_each(|(row, line)| {
        let y = r.y + row as i32;
        let b = &bands[band_of(y as f64 + 0.5).min(n - 1)];
        let s = if b.other { other.unwrap_or(src) } else { src };
        for (i, o) in line.iter_mut().enumerate() {
            let x = r.x + i as i32 - b.dx;
            if b.split == 0 {
                *o = s.get(x, y);
            } else {
                let red = s.get(x - b.split, y);
                let mid = s.get(x, y);
                let blue = s.get(x + b.split, y);
                let a = red[3].max(mid[3]).max(blue[3]);
                *o = [red[0], mid[1], blue[2], a];
            }
        }
    });
    out
}

/// Sobel edges of the (alpha-weighted) luma → blurred → added as coloured light.
fn edge_glow(layer: &mut Layer, threshold: f32, sigma: f32, intensity: f32, color: [f32; 3], passes: usize) {
    let grow = (3.0 * sigma).ceil() as i32 + 2;
    let r = layer.rect.expand(grow);
    layer.expand_to(r);
    let r = layer.rect;
    let (w, h) = (r.w as usize, r.h as usize);
    if w < 3 || h < 3 {
        return;
    }
    let luma: Vec<f32> = layer.px.par_iter().map(|p| R709[0] * p[0] + R709[1] * p[1] + R709[2] * p[2]).collect();
    let at = |x: usize, y: usize| luma[y * w + x];
    let mut edges = vec![0f32; w * h];
    edges.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        if y == 0 || y + 1 >= h {
            return;
        }
        for x in 1..w - 1 {
            let gx = -at(x - 1, y - 1) - 2.0 * at(x - 1, y) - at(x - 1, y + 1) + at(x + 1, y - 1) + 2.0 * at(x + 1, y) + at(x + 1, y + 1);
            let gy = -at(x - 1, y - 1) - 2.0 * at(x, y - 1) - at(x + 1, y - 1) + at(x - 1, y + 1) + 2.0 * at(x, y + 1) + at(x + 1, y + 1);
            let m = (gx * gx + gy * gy).sqrt() / 4.0;
            row[x] = smoothstep(threshold, threshold + 0.15, m);
        }
    });
    let sharp = edges.clone();
    blur::gaussian_plane(&mut edges, w, h, sigma.max(0.31), passes);
    let (sharp, edges) = (&sharp, &edges);
    layer.px.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            // The glow plus a thin bright core on the edge itself.
            let g = (edges[y * w + x] * 2.5 + sharp[y * w + x] * 0.6) * intensity;
            if g <= 0.0 {
                continue;
            }
            o[0] += color[0] * g;
            o[1] += color[1] * g;
            o[2] += color[2] * g;
            o[3] = o[3] + g.min(1.0) * (1.0 - o[3]);
        }
    });
}

/// Barrel (+) / pincushion (−) distortion about the frame centre; corners stay in place.
fn lens_distortion(layer: &mut Layer, k: f64, rect: [f64; 4]) {
    if k.abs() < 1e-4 {
        return;
    }
    let k = k.clamp(-0.9, 2.0);
    let [fx, fy, fw, fh] = rect;
    let frame = IRect::from_bounds([fx, fy, fx + fw, fy + fh]);
    layer.expand_to(frame);
    let src = layer.clone();
    let clamp = src.touches_edges();
    let (cx, cy) = (fx + fw / 2.0, fy + fh / 2.0);
    let half_diag2 = (fw / 2.0).powi(2) + (fh / 2.0).powi(2);
    let r = layer.rect;
    let w = layer.stride();
    layer.px.par_chunks_mut(w.max(1)).enumerate().for_each(|(row, line)| {
        let y = (r.y + row as i32) as f64 + 0.5;
        for (i, o) in line.iter_mut().enumerate() {
            let x = (r.x + i as i32) as f64 + 0.5;
            let (dx, dy) = (x - cx, y - cy);
            let rr = (dx * dx + dy * dy) / half_diag2;
            let f = (1.0 + k * rr) / (1.0 + k);
            *o = src.sample(cx + dx * f, cy + dy * f, clamp);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiframes_core::effects::effect_spec;
    use aiframes_core::Param;

    fn ctx_run(layer: &mut Layer, e: &Effect) {
        let lut = |_: &str| -> Result<Arc<Lut>, String> { Err("no".into()) };
        let problem = |m: String| -> Result<(), String> { Err(m) };
        let rect = [0.0, 0.0, layer.cw as f64, layer.ch as f64];
        let cx = FxCtx { t_local: 0.0, frame: 3, rect, px_scale: 1.0, draft: false, lut: &lut, problem: &problem };
        apply_effects(layer, std::slice::from_ref(e), &cx, false).unwrap();
    }

    fn fx(kind: &str, params: &[(&str, f64)]) -> Effect {
        let mut e = effect_spec(kind).unwrap().instantiate();
        for (k, v) in params {
            e.params.insert(k.to_string(), Param::Value(*v));
        }
        e
    }

    #[test]
    fn colour_directions() {
        let base = [0.4, 0.4, 0.4, 1.0];
        let run = |e: Effect| {
            let mut l = Layer::full(4, 4, base);
            ctx_run(&mut l, &e);
            l.get(1, 1)
        };
        assert!(run(fx("color", &[("exposure", 1.0)]))[0] > 0.5);
        assert!(run(fx("color", &[("brightness", -0.2)]))[0] < 0.25);
        assert!(run(fx("color", &[("temperature", 1.0)]))[0] > run(fx("color", &[("temperature", 1.0)]))[2]);
        assert!(run(fx("invert", &[]))[0] > 0.55);
        let flat = run(fx("color", &[("contrast", -1.0)]));
        assert!((flat[0] - 0.5).abs() < 1e-5);
    }

    #[test]
    fn masks_cut_alpha() {
        let mut l = Layer::full(100, 100, [1.0, 1.0, 1.0, 1.0]);
        ctx_run(&mut l, &fx("mask_ellipse", &[("width", 0.5), ("height", 0.5)]));
        assert!(l.get(50, 50)[3] > 0.99);
        assert!(l.get(5, 5)[3] < 0.01);
        let mut l = Layer::full(100, 100, [1.0, 1.0, 1.0, 1.0]);
        ctx_run(&mut l, &fx("mask_rect", &[("width", 0.5), ("height", 0.5), ("invert", 1.0)]));
        assert!(l.get(50, 50)[3] < 0.01);
        assert!(l.get(5, 5)[3] > 0.99);
    }

    #[test]
    fn key_removes_green_keeps_red() {
        let mut l = Layer::new(2, 1, IRect::new(0, 0, 2, 1));
        l.px[0] = [0.0, 1.0, 0.0, 1.0];
        l.px[1] = [1.0, 0.0, 0.0, 1.0];
        ctx_run(&mut l, &fx("chroma_key", &[]));
        assert!(l.px[0][3] < 0.01);
        assert!(l.px[1][3] > 0.99);
    }

    #[test]
    fn unknown_effect_is_reported() {
        let mut l = Layer::full(2, 2, [0.0; 4]);
        let e = Effect { kind: "nope".into(), enabled: true, params: Default::default(), colors: Default::default(), strings: Default::default() };
        let lut = |_: &str| -> Result<Arc<Lut>, String> { Err("no".into()) };
        let problem = |m: String| -> Result<(), String> { Err(m) };
        let cx = FxCtx { t_local: 0.0, frame: 0, rect: [0.0, 0.0, 2.0, 2.0], px_scale: 1.0, draft: false, lut: &lut, problem: &problem };
        assert!(apply_effects(&mut l, &[e], &cx, false).unwrap_err().contains("unknown effect"));
    }
}
