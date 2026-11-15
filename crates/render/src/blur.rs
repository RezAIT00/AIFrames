//! Fast blurs: separable box passes approximating a Gaussian (O(1) per pixel at any radius),
//! directional and radial (zoom) blurs.

use crate::canvas::{filled_par, Layer, Px};
use rayon::prelude::*;

const ZERO: Px = [0.0; 4];

/// Box radii whose `n` successive passes approximate a Gaussian of standard deviation `sigma`.
pub(crate) fn box_radii(sigma: f32, n: usize) -> Vec<usize> {
    let n = n.max(1);
    let s2 = (sigma as f64).powi(2);
    let w_ideal = (12.0 * s2 / n as f64 + 1.0).sqrt();
    let mut wl = w_ideal.floor() as i64;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wl = wl.max(1);
    let wu = wl + 2;
    let nf = n as f64;
    let wlf = wl as f64;
    let m_ideal = (12.0 * s2 - nf * wlf * wlf - 4.0 * nf * wlf - 3.0 * nf) / (-4.0 * wlf - 4.0);
    let m = m_ideal.round().clamp(0.0, nf) as usize;
    (0..n).map(|i| (if i < m { wl } else { wu } as usize - 1) / 2).collect()
}

/// Running-sum box blur of every row of `px` (rows of `w` pixels). `ext_lo`/`ext_hi`: extend the
/// edge pixels beyond the row ends (else transparent).
fn blur_rows(px: &mut [Px], w: usize, r: usize, ext_lo: bool, ext_hi: bool) {
    if r == 0 || w == 0 {
        return;
    }
    px.par_chunks_mut(w).for_each_init(
        || Vec::with_capacity(w),
        |tmp: &mut Vec<Px>, row| {
            tmp.clear();
            tmp.extend_from_slice(row);
            let n = w as isize;
            let ri = r as isize;
            let first = tmp[0];
            let last = tmp[w - 1];
            let get = |i: isize| -> Px {
                if i < 0 {
                    if ext_lo {
                        first
                    } else {
                        ZERO
                    }
                } else if i >= n {
                    if ext_hi {
                        last
                    } else {
                        ZERO
                    }
                } else {
                    tmp[i as usize]
                }
            };
            let mut acc = [0f32; 4];
            for i in -ri..=ri {
                let p = get(i);
                for k in 0..4 {
                    acc[k] += p[k];
                }
            }
            let inv = 1.0 / (2 * r + 1) as f32;
            for x in 0..n {
                let o = &mut row[x as usize];
                for k in 0..4 {
                    o[k] = (acc[k] * inv).max(0.0);
                }
                let add = get(x + ri + 1);
                let sub = get(x - ri);
                for k in 0..4 {
                    acc[k] += add[k] - sub[k];
                }
            }
        },
    );
}

/// `src` is `h` rows of `w`; returns `w` rows of `h`.
fn transpose(src: &[Px], w: usize, h: usize) -> Vec<Px> {
    let mut out = filled_par(w * h, ZERO);
    const B: usize = 8;
    out.par_chunks_mut(h * B).enumerate().for_each(|(blk, chunk)| {
        let c0 = blk * B;
        let cols = chunk.len() / h;
        for y in 0..h {
            let srow = &src[y * w + c0..y * w + c0 + cols];
            for (j, p) in srow.iter().enumerate() {
                chunk[j * h + y] = *p;
            }
        }
    });
    out
}

/// Gaussian blur of the layer in place (σ in canvas pixels). The caller expands the layer first if
/// the blur should spread beyond it; canvas edges the layer touches extend their pixels.
pub(crate) fn gaussian(layer: &mut Layer, sigma: f32, passes: usize) {
    if !(sigma > 0.3) || layer.is_empty() {
        return;
    }
    // Beyond a few canvas sizes a blur changes nothing visible; cap it so huge values stay fast.
    let sigma = sigma.min(4.0 * layer.cw.max(layer.ch).max(1) as f32);
    let (w, h) = (layer.rect.w as usize, layer.rect.h as usize);
    let (l, t, r, b) = layer.touches_edges();
    let radii = box_radii(sigma, passes);
    for &rad in &radii {
        blur_rows(&mut layer.px, w, rad, l, r);
    }
    let mut tp = transpose(&layer.px, w, h);
    for &rad in &radii {
        blur_rows(&mut tp, h, rad, t, b);
    }
    let back = transpose(&tp, h, w);
    crate::canvas::recycle(tp);
    let old = std::mem::replace(&mut layer.px, back);
    crate::canvas::recycle(old);
}

/// Running-sum box blur of rows of a single-channel plane (transparent beyond the ends).
fn blur_plane_rows(px: &mut [f32], w: usize, r: usize) {
    if r == 0 || w == 0 {
        return;
    }
    px.par_chunks_mut(w).for_each_init(
        || Vec::with_capacity(w),
        |tmp: &mut Vec<f32>, row| {
            tmp.clear();
            tmp.extend_from_slice(row);
            let n = w as isize;
            let ri = r as isize;
            let get = |i: isize| if i < 0 || i >= n { 0.0 } else { tmp[i as usize] };
            let mut acc = 0f32;
            for i in -ri..=ri {
                acc += get(i);
            }
            let inv = 1.0 / (2 * r + 1) as f32;
            for x in 0..n {
                row[x as usize] = (acc * inv).max(0.0);
                acc += get(x + ri + 1) - get(x - ri);
            }
        },
    );
}

fn transpose_plane(src: &[f32], w: usize, h: usize) -> Vec<f32> {
    let mut out = vec![0f32; w * h];
    const B: usize = 16;
    out.par_chunks_mut(h * B).enumerate().for_each(|(blk, chunk)| {
        let c0 = blk * B;
        let cols = chunk.len() / h;
        for y in 0..h {
            let srow = &src[y * w + c0..y * w + c0 + cols];
            for (j, p) in srow.iter().enumerate() {
                chunk[j * h + y] = *p;
            }
        }
    });
    out
}

/// Gaussian blur of a single-channel `w`×`h` plane (zero outside), used for shadows: a quarter of
/// the work of blurring a whole layer.
pub(crate) fn gaussian_plane(plane: &mut Vec<f32>, w: usize, h: usize, sigma: f32, passes: usize) {
    if !(sigma > 0.3) || w == 0 || h == 0 {
        return;
    }
    let sigma = sigma.min(4.0 * w.max(h) as f32);
    let radii = box_radii(sigma, passes);
    for &rad in &radii {
        blur_plane_rows(plane, w, rad);
    }
    let mut tp = transpose_plane(plane, w, h);
    for &rad in &radii {
        blur_plane_rows(&mut tp, h, rad);
    }
    *plane = transpose_plane(&tp, h, w);
}

/// A soft shadow under `layer`: its alpha shifted by `(dx, dy)` canvas pixels, blurred by `sigma`,
/// tinted with premultiplied `color`; the layer is composited over it (the layer grows to fit).
pub(crate) fn shadow_under(layer: &mut Layer, dx: i32, dy: i32, sigma: f32, color: Px, passes: usize) {
    if layer.is_empty() || color[3] <= 0.0 {
        return;
    }
    let lim = 4 * layer.cw.max(layer.ch).max(1) as i32;
    let sigma = if sigma.is_finite() { sigma.clamp(0.0, lim as f32) } else { 0.0 };
    let (dx, dy) = (dx.clamp(-lim, lim), dy.clamp(-lim, lim));
    let grow = if sigma > 0.3 { (3.0 * sigma).ceil() as i32 + 1 } else { 1 };
    let srect = layer.rect.offset(dx, dy).expand(grow);
    layer.expand_to(srect);
    let r = layer.rect;
    let (w, h) = (r.w as usize, r.h as usize);
    // Soft shadows are computed at half resolution (4x less work, visually identical).
    let f: usize = if sigma >= 6.0 { 2 } else { 1 };
    let (qw, qh) = (w.div_ceil(f), h.div_ceil(f));
    let src = &layer.px;
    let alpha_at = |x: i64, y: i64| -> f32 {
        // Layer-relative coordinates of the shadow's source pixel.
        let (sx, sy) = (x - dx as i64, y - dy as i64);
        if sx < 0 || sy < 0 || sx >= w as i64 || sy >= h as i64 {
            0.0
        } else {
            src[sy as usize * w + sx as usize][3]
        }
    };
    let mut plane = vec![0f32; qw * qh];
    plane.par_chunks_mut(qw.max(1)).enumerate().for_each(|(j, row)| {
        for (i, o) in row.iter_mut().enumerate() {
            let mut acc = 0.0;
            for v in 0..f {
                for u in 0..f {
                    acc += alpha_at((i * f + u) as i64, (j * f + v) as i64);
                }
            }
            *o = acc / (f * f) as f32;
        }
    });
    gaussian_plane(&mut plane, qw, qh, sigma / f as f32, passes);
    let plane = &plane;
    let sample = |x: usize, y: usize| -> f32 {
        if f == 1 {
            return plane[y * qw + x];
        }
        let fx = ((x as f32 + 0.5) / f as f32 - 0.5).max(0.0);
        let fy = ((y as f32 + 0.5) / f as f32 - 0.5).max(0.0);
        let (x0, y0) = (fx as usize, fy as usize);
        let (x1, y1) = ((x0 + 1).min(qw - 1), (y0 + 1).min(qh - 1));
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let a = plane[y0 * qw + x0] + (plane[y0 * qw + x1] - plane[y0 * qw + x0]) * tx;
        let b = plane[y1 * qw + x0] + (plane[y1 * qw + x1] - plane[y1 * qw + x0]) * tx;
        a + (b - a) * ty
    };
    layer.px.par_chunks_mut(w.max(1)).enumerate().for_each(|(row, line)| {
        for (i, o) in line.iter_mut().enumerate() {
            let k = 1.0 - o[3];
            if k <= 0.0 {
                continue;
            }
            let a = sample(i, row);
            if a <= 0.0 {
                continue;
            }
            for c in 0..4 {
                o[c] += color[c] * a * k;
            }
        }
    });
}

/// Motion blur along the vector `(dx, dy)` (its full length, centred on each pixel): `passes`
/// passes of `taps` bilinear samples with growing spacing (taps^passes effective samples).
pub(crate) fn directional_multi(layer: &Layer, dx: f64, dy: f64, taps: usize, passes: u32) -> Layer {
    let taps = taps.max(2);
    let total = (taps as f64).powi(passes as i32) - 1.0;
    let mut cur = layer.clone();
    let reach = ((dx * dx + dy * dy).sqrt() / 2.0).ceil() as i32 + 1;
    cur.expand_to(cur.rect.expand(reach));
    let clamp = layer.touches_edges();
    let inv = 1.0 / taps as f32;
    for j in 0..passes {
        let step = (taps as f64).powi(j as i32) / total;
        let src = cur;
        let mut out = Layer::new(src.cw, src.ch, src.rect);
        let r = out.rect;
        let w = out.stride();
        out.px.par_chunks_mut(w.max(1)).enumerate().for_each(|(row, line)| {
            let y = (r.y + row as i32) as f64 + 0.5;
            for (i, o) in line.iter_mut().enumerate() {
                let x = (r.x + i as i32) as f64 + 0.5;
                let mut acc = [0f32; 4];
                for k in 0..taps {
                    let f = (k as f64 - (taps - 1) as f64 / 2.0) * step;
                    let p = src.sample(x + dx * f, y + dy * f, clamp);
                    for c in 0..4 {
                        acc[c] += p[c];
                    }
                }
                *o = [acc[0] * inv, acc[1] * inv, acc[2] * inv, acc[3] * inv];
            }
        });
        cur = out;
    }
    cur
}

/// Radial blur towards `(cx, cy)` (canvas px): each pixel averages the picture along the segment to
/// `amount` of the way to the centre. Three passes of `taps` samples (taps³ effective samples).
pub(crate) fn zoom(layer: &Layer, cx: f64, cy: f64, amount: f64, taps: usize, passes: u32) -> Layer {
    let taps = taps.max(2);
    let mut cur = layer.clone();
    let total = (taps as f64).powi(passes as i32) - 1.0;
    let clamp = layer.touches_edges();
    for j in 0..passes {
        let step = amount * (taps as f64).powi(j as i32) / total; // fraction per tap in this pass
        let src = cur;
        let mut out = Layer::new(src.cw, src.ch, src.rect);
        let r = out.rect;
        let w = out.stride();
        let inv = 1.0 / taps as f32;
        out.px.par_chunks_mut(w.max(1)).enumerate().for_each(|(row, line)| {
            let y = (r.y + row as i32) as f64 + 0.5;
            for (i, o) in line.iter_mut().enumerate() {
                let x = (r.x + i as i32) as f64 + 0.5;
                let mut acc = [0f32; 4];
                for k in 0..taps {
                    let f = 1.0 - step * k as f64;
                    let p = src.sample(cx + (x - cx) * f, cy + (y - cy) * f, clamp);
                    for c in 0..4 {
                        acc[c] += p[c];
                    }
                }
                *o = [acc[0] * inv, acc[1] * inv, acc[2] * inv, acc[3] * inv];
            }
        });
        cur = out;
    }
    cur
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::IRect;

    #[test]
    fn radii_match_sigma() {
        for sigma in [1.0f32, 3.0, 10.0, 40.0] {
            let r = box_radii(sigma, 3);
            // Variance of n boxes of width w: sum (w²-1)/12.
            let var: f64 = r.iter().map(|&r| (((2 * r + 1) as f64).powi(2) - 1.0) / 12.0).sum();
            assert!((var.sqrt() - sigma as f64).abs() < 0.6, "{sigma} -> {r:?} ({})", var.sqrt());
        }
    }

    #[test]
    fn blur_spreads_and_conserves() {
        let mut l = Layer::new(64, 64, IRect::new(0, 0, 64, 64));
        l.px[32 * 64 + 32] = [1.0, 1.0, 1.0, 1.0];
        gaussian(&mut l, 4.0, 3);
        let sum: f32 = l.px.iter().map(|p| p[3]).sum();
        assert!((sum - 1.0).abs() < 0.02, "{sum}");
        assert!(l.get(32, 32)[3] < 0.05);
        assert!(l.get(36, 32)[3] > 0.0);
        // Flat field stays flat at extended edges.
        let mut f = Layer::full(32, 16, [0.5, 0.5, 0.5, 1.0]);
        gaussian(&mut f, 6.0, 3);
        assert!((f.get(0, 0)[0] - 0.5).abs() < 1e-3 && (f.get(31, 15)[3] - 1.0).abs() < 1e-3);
    }
}
