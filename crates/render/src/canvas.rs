//! Premultiplied-alpha f32 layers, blend modes, raster sampling with affine transforms.
//!
//! Colour values are display-referred sRGB (like most NLEs composite by default), premultiplied,
//! nominally 0..1 (additive modes and glows may exceed 1 until the final conversion clamps).

use crate::geom::{Affine, IRect, Proj, W_EPS};
use crate::util::lock;
use aiframes_core::BlendMode;
use rayon::prelude::*;
use std::sync::{Arc, Mutex};

pub(crate) type Px = [f32; 4];

// ---------------------------------------------------------------------------------------------
// Buffer pool (fresh multi-megabyte allocations page-fault on every first touch; reuse them)
// ---------------------------------------------------------------------------------------------

static POOL: Mutex<Vec<Vec<Px>>> = Mutex::new(Vec::new());
const POOL_MAX_BUFFERS: usize = 32;
/// Idle pooled pixels kept at most: 24 M pixels = 384 MB (an export's few frames in flight at
/// 1080p; exports trim it afterwards). Larger buffers are freed when returned.
const POOL_MAX_PIXELS: usize = 24_000_000;
/// The current cap (an export raises it to fit its own frames in flight, then restores it).
static POOL_LIMIT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(POOL_MAX_PIXELS);

/// Sets the pool cap to `pixels` (at least the default); `None` restores the default.
pub(crate) fn set_pool_limit(pixels: Option<usize>) {
    let v = pixels.map(|p| p.max(POOL_MAX_PIXELS)).unwrap_or(POOL_MAX_PIXELS);
    POOL_LIMIT.store(v, std::sync::atomic::Ordering::Relaxed);
}
/// What the pool keeps after an export (enough for preview frames).
pub(crate) const POOL_IDLE_PIXELS: usize = 4_000_000;

fn take_pooled(n: usize) -> Option<Vec<Px>> {
    if n < 4096 {
        return None;
    }
    let mut pool = lock(&POOL);
    // The smallest buffer that fits (layers kept long term, i.e. cached titles, are compacted
    // with `Layer::compact` so they never hold on to a whole canvas).
    let best = pool.iter().enumerate().filter(|(_, v)| v.capacity() >= n).min_by_key(|(_, v)| v.capacity()).map(|(i, _)| i);
    best.map(|i| pool.swap_remove(i))
}

fn take_buffer(n: usize) -> Vec<Px> {
    match take_pooled(n) {
        Some(mut v) => {
            v.clear();
            v
        }
        None => Vec::with_capacity(n),
    }
}

/// `n` pixels with unspecified (stale but initialised) contents: the caller writes every pixel.
fn take_dirty(n: usize) -> Vec<Px> {
    let mut v = take_pooled(n).unwrap_or_default();
    if v.len() >= n {
        v.truncate(n);
    } else if n - v.len() >= 1 << 16 {
        let extra = n - v.len();
        v.par_extend(rayon::iter::repeat_n([0.0; 4], extra));
    } else {
        v.resize(n, [0.0; 4]);
    }
    v
}

fn give_buffer(v: Vec<Px>) {
    if v.capacity() < 4096 {
        return;
    }
    let mut pool = lock(&POOL);
    let total: usize = pool.iter().map(|b| b.capacity()).sum();
    if pool.len() < POOL_MAX_BUFFERS && total + v.capacity() <= POOL_LIMIT.load(std::sync::atomic::Ordering::Relaxed) {
        pool.push(v);
    }
}

/// Returns a buffer to the pool.
pub(crate) fn recycle(v: Vec<Px>) {
    give_buffer(v);
}

/// Frees pooled buffers until at most `max_pixels` remain pooled.
pub(crate) fn trim_pool(max_pixels: usize) {
    let mut pool = lock(&POOL);
    pool.sort_by_key(|b| b.capacity());
    while pool.iter().map(|b| b.capacity()).sum::<usize>() > max_pixels {
        pool.pop();
    }
}

/// Frees every pooled buffer.
pub(crate) fn clear_pool() {
    lock(&POOL).clear();
}

/// A (pooled) buffer of `n` pixels all set to `value`; large buffers are written in parallel.
pub(crate) fn filled_par(n: usize, value: Px) -> Vec<Px> {
    let mut v = take_buffer(n);
    if n < 1 << 16 {
        v.resize(n, value);
    } else {
        v.par_extend(rayon::iter::repeat_n(value, n));
    }
    v
}

// ---------------------------------------------------------------------------------------------
// Layer
// ---------------------------------------------------------------------------------------------

/// Premultiplied RGBA covering `rect` of a `cw`×`ch` canvas; transparent outside `rect`.
pub(crate) struct Layer {
    pub cw: u32,
    pub ch: u32,
    pub rect: IRect,
    pub px: Vec<Px>,
}

impl Drop for Layer {
    fn drop(&mut self) {
        give_buffer(std::mem::take(&mut self.px));
    }
}

impl Clone for Layer {
    fn clone(&self) -> Self {
        let mut px = take_buffer(self.px.len());
        px.extend_from_slice(&self.px);
        Layer { cw: self.cw, ch: self.ch, rect: self.rect, px }
    }
}

impl Layer {
    pub fn canvas(&self) -> IRect {
        IRect::of_size(self.cw, self.ch)
    }

    /// A transparent layer over `rect ∩ canvas`.
    pub fn new(cw: u32, ch: u32, rect: IRect) -> Layer {
        let rect = rect.intersect(&IRect::of_size(cw, ch));
        let px = filled_par(rect.area(), [0.0; 4]);
        Layer { cw, ch, rect, px }
    }

    /// A full-canvas layer whose pixels are unspecified until the caller writes all of them
    /// (`draw_raster` with a fill colour does).
    pub fn dirty(cw: u32, ch: u32) -> Layer {
        let rect = IRect::of_size(cw, ch);
        Layer { cw, ch, rect, px: take_dirty(rect.area()) }
    }

    /// Moves the pixels into a buffer of their own size when the current one is much larger (a
    /// pooled canvas reused for a small layer); the large buffer goes back to the pool. Call before
    /// keeping a layer for long.
    pub fn compact(&mut self) {
        let n = self.px.len();
        if self.px.capacity() > n * 5 / 4 + 4096 {
            let mut v = Vec::with_capacity(n);
            v.extend_from_slice(&self.px);
            give_buffer(std::mem::replace(&mut self.px, v));
        }
    }

    /// The whole canvas as a premultiplied RGBA8 picture.
    pub fn to_premul_picture(&self) -> Picture {
        let (w, h) = (self.cw as usize, self.ch as usize);
        let mut out = vec![0u8; w * h * 4];
        let r = self.rect;
        out.par_chunks_mut((w * 4).max(4)).enumerate().for_each(|(y, row)| {
            let y = y as i32;
            if y < r.y || y >= r.bottom() {
                return;
            }
            let src = &self.px[(y - r.y) as usize * r.w as usize..(y - r.y + 1) as usize * r.w as usize];
            let x0 = r.x.max(0) as usize;
            for (i, p) in src.iter().enumerate() {
                let a = p[3].clamp(0.0, 1.0);
                let o = &mut row[(x0 + i) * 4..(x0 + i) * 4 + 4];
                for k in 0..3 {
                    o[k] = (p[k].clamp(0.0, a) * 255.0 + 0.5) as u8;
                }
                o[3] = (a * 255.0 + 0.5) as u8;
            }
        });
        Picture { w: self.cw, h: self.ch, data: Arc::new(out), premul: true }
    }

    /// Sets every pixel to `value`.
    pub fn fill(&mut self, value: Px) {
        self.px.par_chunks_mut(1 << 14).for_each(|c| c.fill(value));
    }

    /// A full-canvas layer filled with `value`.
    pub fn full(cw: u32, ch: u32, value: Px) -> Layer {
        let rect = IRect::of_size(cw, ch);
        Layer { cw, ch, rect, px: filled_par(rect.area(), value) }
    }

    pub fn empty(cw: u32, ch: u32) -> Layer {
        Layer { cw, ch, rect: IRect::default(), px: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.rect.is_empty()
    }

    #[inline]
    pub fn stride(&self) -> usize {
        self.rect.w.max(0) as usize
    }

    /// The pixel at canvas `(x, y)`, transparent outside the layer.
    #[inline]
    pub fn get(&self, x: i32, y: i32) -> Px {
        let r = &self.rect;
        if x < r.x || y < r.y || x >= r.right() || y >= r.bottom() {
            return [0.0; 4];
        }
        self.px[(y - r.y) as usize * r.w as usize + (x - r.x) as usize]
    }

    /// Grows the layer (with transparency) to cover `rect ∩ canvas`.
    pub fn expand_to(&mut self, rect: IRect) {
        let want = self.rect.union(&rect).intersect(&self.canvas());
        if want == self.rect {
            return;
        }
        let mut out = Layer::new(self.cw, self.ch, want);
        if !self.rect.is_empty() {
            let (ow, sw) = (out.stride(), self.stride());
            let dx = (self.rect.x - want.x) as usize;
            let dy = (self.rect.y - want.y) as usize;
            let src = &self.px;
            out.px.par_chunks_mut(ow).enumerate().for_each(|(row, line)| {
                if row < dy || row >= dy + self.rect.h as usize {
                    return;
                }
                let sr = row - dy;
                line[dx..dx + sw].copy_from_slice(&src[sr * sw..(sr + 1) * sw]);
            });
        }
        std::mem::swap(self, &mut out);
    }

    /// Whether the layer's rect touches each canvas edge: (left, top, right, bottom).
    pub fn touches_edges(&self) -> (bool, bool, bool, bool) {
        let r = &self.rect;
        (r.x <= 0, r.y <= 0, r.right() >= self.cw as i32, r.bottom() >= self.ch as i32)
    }

    /// Multiplies every pixel by `k`.
    pub fn scale_alpha(&mut self, k: f32) {
        if (k - 1.0).abs() < 1e-6 {
            return;
        }
        self.px.par_chunks_mut(4096).for_each(|c| {
            for p in c {
                p[0] *= k;
                p[1] *= k;
                p[2] *= k;
                p[3] *= k;
            }
        });
    }

    /// Bilinear sample at canvas coordinates (pixel centres at +0.5). Outside the layer the sample is
    /// transparent, except along canvas edges the layer touches, where edge pixels extend.
    #[inline]
    pub fn sample(&self, x: f64, y: f64, clamp: (bool, bool, bool, bool)) -> Px {
        let r = &self.rect;
        if r.is_empty() {
            return [0.0; 4];
        }
        let mut fx = x - 0.5;
        let mut fy = y - 0.5;
        if clamp.0 {
            fx = fx.max(r.x as f64);
        }
        if clamp.1 {
            fy = fy.max(r.y as f64);
        }
        if clamp.2 {
            fx = fx.min((r.right() - 1) as f64);
        }
        if clamp.3 {
            fy = fy.min((r.bottom() - 1) as f64);
        }
        let ix = fx.floor();
        let iy = fy.floor();
        let tx = (fx - ix) as f32;
        let ty = (fy - iy) as f32;
        let (ix, iy) = (ix as i32, iy as i32);
        let p00 = self.get(ix, iy);
        let p10 = self.get(ix + 1, iy);
        let p01 = self.get(ix, iy + 1);
        let p11 = self.get(ix + 1, iy + 1);
        let mut o = [0.0; 4];
        for k in 0..4 {
            let a = p00[k] + (p10[k] - p00[k]) * tx;
            let b = p01[k] + (p11[k] - p01[k]) * tx;
            o[k] = a + (b - a) * ty;
        }
        o
    }

    /// Straight RGBA8 of the whole canvas (pixels outside the layer are transparent black).
    pub fn to_rgba8(&self) -> Vec<u8> {
        let (w, h) = (self.cw as usize, self.ch as usize);
        let mut out = vec![0u8; w * h * 4];
        let r = self.rect;
        out.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
            let y = y as i32;
            if y < r.y || y >= r.bottom() {
                return;
            }
            let src = &self.px[(y - r.y) as usize * r.w as usize..(y - r.y + 1) as usize * r.w as usize];
            let x0 = r.x.max(0) as usize;
            for (i, p) in src.iter().enumerate() {
                let x = x0 + i;
                row[x * 4..x * 4 + 4].copy_from_slice(&unpremul8(*p));
            }
        });
        out
    }
}

/// Premultiplied f32 → straight RGBA8 (clamped, rounded).
#[inline]
pub(crate) fn unpremul8(p: Px) -> [u8; 4] {
    let a = p[3].clamp(0.0, 1.0);
    if a <= 1.0 / 512.0 {
        return [0, 0, 0, 0];
    }
    let inv = 1.0 / a;
    let c = |v: f32| ((v * inv).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    [c(p[0]), c(p[1]), c(p[2]), (a * 255.0 + 0.5) as u8]
}

/// Straight colour → premultiplied pixel.
#[inline]
pub(crate) fn premul(c: aiframes_core::Color) -> Px {
    let a = c.a.clamp(0.0, 1.0);
    [c.r.clamp(0.0, 1.0) * a, c.g.clamp(0.0, 1.0) * a, c.b.clamp(0.0, 1.0) * a, a]
}

// ---------------------------------------------------------------------------------------------
// Blending
// ---------------------------------------------------------------------------------------------

#[inline]
fn blend_channel(mode: BlendMode, cs: f32, cd: f32) -> f32 {
    match mode {
        BlendMode::Normal => cs,
        BlendMode::Add => cs + cd,
        BlendMode::Multiply => cs * cd,
        BlendMode::Screen => cs + cd - cs * cd,
        BlendMode::Overlay => {
            if cd <= 0.5 {
                2.0 * cs * cd
            } else {
                let s2 = 2.0 * cd - 1.0;
                cs + s2 - cs * s2
            }
        }
        BlendMode::Darken => cs.min(cd),
        BlendMode::Lighten => cs.max(cd),
        BlendMode::Difference => (cs - cd).abs(),
    }
}

/// Composites premultiplied `s` onto `d` with a blend mode (W3C compositing, source-over).
#[inline]
pub(crate) fn blend_px(mode: BlendMode, d: &mut Px, s: Px) {
    let sa = s[3];
    if sa <= 0.0 && mode != BlendMode::Add {
        return;
    }
    match mode {
        BlendMode::Normal => {
            let k = 1.0 - sa;
            d[0] = s[0] + d[0] * k;
            d[1] = s[1] + d[1] * k;
            d[2] = s[2] + d[2] * k;
            d[3] = sa + d[3] * k;
        }
        BlendMode::Add => {
            d[0] += s[0];
            d[1] += s[1];
            d[2] += s[2];
            d[3] = sa + d[3] - sa * d[3];
        }
        _ => {
            let da = d[3];
            if da <= 0.0 {
                *d = s;
                return;
            }
            let isa = 1.0 / sa.max(1e-6);
            let ida = 1.0 / da;
            for k in 0..3 {
                let cs = (s[k] * isa).clamp(0.0, 1.0);
                let cd = (d[k] * ida).clamp(0.0, 1.0);
                d[k] = s[k] * (1.0 - da) + d[k] * (1.0 - sa) + sa * da * blend_channel(mode, cs, cd);
            }
            d[3] = sa + da - sa * da;
        }
    }
}

/// Blends `src` (× `opacity`) onto `dst` over the overlap of their rects.
pub(crate) fn blend_layer(dst: &mut Layer, src: &Layer, opacity: f32, mode: BlendMode) {
    let ov = dst.rect.intersect(&src.rect);
    if ov.is_empty() || opacity <= 0.0 {
        return;
    }
    let (dw, sw) = (dst.stride(), src.stride());
    let (dr, sr) = (dst.rect, src.rect);
    let rows = ov.y - dr.y..ov.bottom() - dr.y;
    dst.px.par_chunks_mut(dw).enumerate().for_each(|(row, line)| {
        let row = row as i32;
        if !rows.contains(&row) {
            return;
        }
        let y = dr.y + row;
        let s_off = (y - sr.y) as usize * sw + (ov.x - sr.x) as usize;
        let d_off = (ov.x - dr.x) as usize;
        let s = &src.px[s_off..s_off + ov.w as usize];
        let d = &mut line[d_off..d_off + ov.w as usize];
        if opacity >= 1.0 && mode == BlendMode::Normal {
            for (dp, sp) in d.iter_mut().zip(s) {
                if sp[3] >= 1.0 {
                    *dp = *sp;
                } else if sp[3] > 0.0 {
                    blend_px(BlendMode::Normal, dp, *sp);
                }
            }
        } else {
            for (dp, sp) in d.iter_mut().zip(s) {
                blend_px(mode, dp, [sp[0] * opacity, sp[1] * opacity, sp[2] * opacity, sp[3] * opacity]);
            }
        }
    });
}

/// `b` over `a` (normal blend), grown to cover both.
pub(crate) fn over_union(mut a: Layer, b: &Layer) -> Layer {
    a.expand_to(b.rect);
    blend_layer(&mut a, b, 1.0, BlendMode::Normal);
    a
}

/// `a·(1−k) + b·k` per pixel over the union of rects (missing pixels are transparent).
pub(crate) fn mix_layers(a: &Layer, b: &Layer, k: f32) -> Layer {
    map2(a, b, |pa, pb| {
        let mut o = [0.0; 4];
        for i in 0..4 {
            o[i] = pa[i] + (pb[i] - pa[i]) * k;
        }
        o
    })
}

/// A new layer over the union of `a` and `b` with `f(pa, pb)` per pixel.
pub(crate) fn map2(a: &Layer, b: &Layer, f: impl Fn(Px, Px) -> Px + Sync) -> Layer {
    map2_xy(a, b, |_, _, pa, pb| f(pa, pb))
}

/// Like `map2` with canvas coordinates.
pub(crate) fn map2_xy(a: &Layer, b: &Layer, f: impl Fn(i32, i32, Px, Px) -> Px + Sync) -> Layer {
    let rect = a.rect.union(&b.rect);
    let mut out = Layer::new(a.cw, a.ch, rect);
    let w = out.stride();
    let r = out.rect;
    out.px.par_chunks_mut(w.max(1)).enumerate().for_each(|(row, line)| {
        let y = r.y + row as i32;
        for (i, o) in line.iter_mut().enumerate() {
            let x = r.x + i as i32;
            *o = f(x, y, a.get(x, y), b.get(x, y));
        }
    });
    out
}

/// Resamples a layer through the canvas-space affine `m` (bilinear; whole-pixel translations are
/// exact copies).
pub(crate) fn transform_layer(src: &Layer, m: &Affine) -> Layer {
    let canvas = src.canvas();
    if src.is_empty() {
        return Layer::empty(src.cw, src.ch);
    }
    if let Some((dx, dy)) = m.integer_translation() {
        let moved = src.rect.offset(dx as i32, dy as i32);
        let vis = moved.intersect(&canvas);
        let mut out = Layer::new(src.cw, src.ch, vis);
        if vis.is_empty() {
            return out;
        }
        let ow = out.stride();
        let sw = src.stride();
        out.px.par_chunks_mut(ow).enumerate().for_each(|(row, line)| {
            let y = vis.y + row as i32 - dy as i32; // source canvas y
            let sx0 = (vis.x - dx as i32 - src.rect.x) as usize;
            let sy = (y - src.rect.y) as usize;
            line.copy_from_slice(&src.px[sy * sw + sx0..sy * sw + sx0 + ow]);
        });
        return out;
    }
    let Some(inv) = m.invert() else { return Layer::empty(src.cw, src.ch) };
    let r = src.rect;
    let b = m.bounds_of(r.x as f64, r.y as f64, r.right() as f64, r.bottom() as f64);
    let rect = IRect::from_bounds(b).expand(1).intersect(&canvas);
    let mut out = Layer::new(src.cw, src.ch, rect);
    let w = out.stride();
    out.px.par_chunks_mut(w.max(1)).enumerate().for_each(|(row, line)| {
        let oy = (rect.y + row as i32) as f64 + 0.5;
        for (i, o) in line.iter_mut().enumerate() {
            let ox = (rect.x + i as i32) as f64 + 0.5;
            let (sx, sy) = inv.apply(ox, oy);
            *o = src.sample(sx, sy, (false, false, false, false));
        }
    });
    out
}

// ---------------------------------------------------------------------------------------------
// Raster pictures
// ---------------------------------------------------------------------------------------------

/// A source picture: RGBA8, straight (media frames) or premultiplied.
#[derive(Clone)]
pub(crate) struct Picture {
    pub w: u32,
    pub h: u32,
    pub data: Arc<Vec<u8>>,
    pub premul: bool,
}

impl Picture {
    pub fn solid(c: aiframes_core::Color) -> Picture {
        Picture { w: 1, h: 1, data: Arc::new(c.to_rgba8().to_vec()), premul: false }
    }

    /// A 2× box-filtered copy (premultiplied).
    pub fn half(&self) -> Picture {
        let (w, h) = ((self.w / 2).max(1), (self.h / 2).max(1));
        let sw = self.w as usize;
        let src = &self.data;
        let premul = self.premul;
        let mut out = vec![0u8; w as usize * h as usize * 4];
        out.par_chunks_mut(w as usize * 4).enumerate().for_each(|(y, row)| {
            for x in 0..w as usize {
                let mut acc = [0f32; 4];
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let sx = (2 * x + dx).min(self.w as usize - 1);
                    let sy = (2 * y + dy).min(self.h as usize - 1);
                    let p = fetch(src, sw, sx, sy, premul);
                    for k in 0..4 {
                        acc[k] += p[k] * 0.25;
                    }
                }
                for k in 0..4 {
                    row[x * 4 + k] = (acc[k].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                }
            }
        });
        Picture { w, h, data: Arc::new(out), premul: true }
    }
}

/// Premultiplied f32 of pixel (x, y).
#[inline]
fn fetch(data: &[u8], w: usize, x: usize, y: usize, premul: bool) -> Px {
    let i = (y * w + x) * 4;
    let p = &data[i..i + 4];
    const K: f32 = 1.0 / 255.0;
    if premul {
        [p[0] as f32 * K, p[1] as f32 * K, p[2] as f32 * K, p[3] as f32 * K]
    } else {
        let a = p[3] as f32 * K;
        let ka = a * K;
        [p[0] as f32 * ka, p[1] as f32 * ka, p[2] as f32 * ka, a]
    }
}

/// How a picture lands on the canvas.
pub(crate) struct RasterDraw<'a> {
    pub pic: &'a Picture,
    /// Canvas (output pixel) coordinates → picture pixel coordinates (projective for 3-D cards).
    pub inv: Proj,
    /// The visible part of the picture `[x0, y0, x1, y1]` in picture pixels (crop).
    pub src: [f64; 4],
    pub opacity: f32,
    pub nearest: bool,
}

impl RasterDraw<'_> {
    /// Canvas bounds of the visible part.
    pub fn bounds(&self) -> IRect {
        match self.inv.to_affine() {
            Some(a) => match a.invert() {
                Some(fwd) => IRect::from_bounds(fwd.bounds_of(self.src[0], self.src[1], self.src[2], self.src[3])).expand(1),
                None => IRect::default(),
            },
            None => proj_bounds(&self.inv, &self.src, IRect::new(-(1 << 24), -(1 << 24), 1 << 25, 1 << 25)),
        }
    }
}

/// `ox` range (output pixel centre coordinate) where `lo < k·ox + m < hi`.
#[inline]
fn span_1d(k: f64, m: f64, lo: f64, hi: f64) -> (f64, f64) {
    if k.abs() < 1e-12 {
        if m > lo && m < hi {
            (f64::NEG_INFINITY, f64::INFINITY)
        } else {
            (f64::INFINITY, f64::NEG_INFINITY)
        }
    } else {
        let a = (lo - m) / k;
        let b = (hi - m) / k;
        (a.min(b), a.max(b))
    }
}

/// Draws (blends) a picture onto `dst`.
pub(crate) fn draw_raster(dst: &mut Layer, d: &RasterDraw, mode: BlendMode) {
    draw_raster_impl(dst, d, mode, None);
}

/// Draws a picture over the constant colour `bg` (normal blend), writing every pixel of `dst`:
/// the first layer of a frame needs no separate background fill.
pub(crate) fn draw_raster_over(dst: &mut Layer, d: &RasterDraw, bg: Px) {
    if !draw_raster_impl(dst, d, BlendMode::Normal, Some(bg)) {
        dst.fill(bg);
    }
}

/// `v as f32 / 255` the way `fetch` computes it.
static U8_F32: [f32; 256] = {
    let mut t = [0f32; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = i as f32 * (1.0 / 255.0);
        i += 1;
    }
    t
};

/// `draw_raster_over` onto a fresh `cw`×`ch` canvas followed by `Layer::to_rgba8`, for a draw
/// without rotation or skew: the same sampling, edge coverage and blending, written straight to
/// RGBA8 (no f32 canvas), with each column's and row's sample position worked out once. The
/// result is the same bytes. `None` for rotated, skewed or projective draws.
///
/// This is the whole frame of the most common case (one plain video clip), so it is what playback
/// spends most of its compositing time on.
pub(crate) fn raster_over_rgba8(d: &RasterDraw, bg: Px, cw: u32, ch: u32) -> Option<Vec<u8>> {
    let inv = d.inv.to_affine()?;
    if inv.b != 0.0 || inv.c != 0.0 {
        return None;
    }
    let (w, h) = (cw as usize, ch as usize);
    let bg8 = unpremul8(bg);
    let mut out = vec![0u8; w * h * 4];
    let fill_all = |out: &mut Vec<u8>| {
        out.par_chunks_mut((w * 4).max(4)).for_each(|row| {
            for px in row.as_chunks_mut::<4>().0 {
                *px = bg8;
            }
        })
    };
    let pic = d.pic;
    let dst_rect = IRect::of_size(cw, ch);
    if pic.w == 0 || pic.h == 0 || d.opacity <= 0.0 || dst_rect.is_empty() {
        fill_all(&mut out);
        return Some(out);
    }
    let mut src = d.src;
    src[0] = src[0].max(0.0);
    src[1] = src[1].max(0.0);
    src[2] = src[2].min(pic.w as f64);
    src[3] = src[3].min(pic.h as f64);
    if src[2] <= src[0] || src[3] <= src[1] {
        fill_all(&mut out);
        return Some(out);
    }
    let region = d.bounds().intersect(&dst_rect);
    if region.is_empty() {
        fill_all(&mut out);
        return Some(out);
    }
    let data: &[u8] = &pic.data;
    let premul = pic.premul;
    let opacity = d.opacity;
    let pw = pic.w as usize;
    // Rows of about 64k pixels per task: the work per row is small, and every extra task wakes
    // (and spins) another pool thread.
    let min_rows = (65536 / w.max(1)).max(1);
    let px_over = |s: Px| -> [u8; 4] {
        let k = 1.0 - s[3];
        unpremul8([s[0] + bg[0] * k, s[1] + bg[1] * k, s[2] + bg[2] * k, s[3] + bg[3] * k])
    };

    // Whole-pixel copy (same resolution, pixel-aligned crop), as in `draw_raster_impl`.
    let aligned = src.iter().all(|v| (v - v.round()).abs() < 1e-6);
    if let (Some((dx, dy)), true) = (inv.integer_translation(), aligned) {
        let (sx0, sy0, sx1, sy1) = (src[0] as i64, src[1] as i64, src[2] as i64, src[3] as i64);
        let xa = (sx0 - dx).max(region.x as i64);
        let xb = (sx1 - dx).min(region.right() as i64).max(xa);
        out.par_chunks_mut(w * 4).with_min_len(min_rows).enumerate().for_each(|(y, line)| {
            let y = y as i64;
            let sy = y + dy;
            for px in line.as_chunks_mut::<4>().0 {
                *px = bg8;
            }
            if y < region.y as i64 || y >= region.bottom() as i64 || sy < sy0 || sy >= sy1 {
                return;
            }
            let row = sy as usize * pw * 4;
            for x in xa..xb {
                let sx = (x + dx) as usize;
                let o = &mut line[x as usize * 4..x as usize * 4 + 4];
                let p = &data[row + sx * 4..row + sx * 4 + 4];
                if !premul && p[3] == 255 && opacity >= 1.0 {
                    // An opaque pixel over anything: itself (what the f32 path rounds back to).
                    o.copy_from_slice(p);
                    continue;
                }
                let mut s = fetch(data, pw, sx, sy as usize, premul);
                if opacity < 1.0 {
                    for v in s.iter_mut() {
                        *v *= opacity;
                    }
                }
                o.copy_from_slice(&px_over(s));
            }
        });
        return Some(out);
    }

    let gx = (inv.a * inv.a + inv.c * inv.c).sqrt().max(1e-9);
    let gy = (inv.b * inv.b + inv.d * inv.d).sqrt().max(1e-9);
    let tx0 = src[0].floor() as i64;
    let ty0 = src[1].floor() as i64;
    let tx1 = (src[2].ceil() as i64 - 1).min(pw as i64 - 1);
    let ty1 = (src[3].ceil() as i64 - 1).min(pic.h as i64 - 1);
    // Columns: the same span for every row (no skew), then per-column sample position and coverage.
    let (a0, a1) = span_1d(inv.a, inv.e, src[0] - 0.5 * gx, src[2] + 0.5 * gx);
    struct Col {
        cx: f64,
        nx: usize,
        x0: usize,
        x1: usize,
        tw: f32,
    }
    let cols: Vec<Col> = (0..w)
        .map(|x| {
            let ox = x as f64 + 0.5;
            let sx = inv.a * ox + inv.c * 0.0 + inv.e;
            let cx = (((sx - src[0]).min(src[2] - sx)) / gx + 0.5).clamp(0.0, 1.0);
            let fx = sx - 0.5;
            let ixf = fx.floor();
            let ix = ixf as i64;
            Col {
                cx,
                nx: (sx.floor() as i64).clamp(tx0, tx1) as usize,
                x0: ix.clamp(tx0, tx1) as usize,
                x1: (ix + 1).clamp(tx0, tx1) as usize,
                tw: (fx - ixf) as f32,
            }
        })
        .collect();
    out.par_chunks_mut(w * 4).with_min_len(min_rows).enumerate().for_each(|(y, line)| {
        for px in line.as_chunks_mut::<4>().0 {
            *px = bg8;
        }
        let y = y as i32;
        if y < region.y || y >= region.bottom() {
            return;
        }
        let oy = y as f64 + 0.5;
        // As `draw_raster_impl` (with b = c = 0 the column span does not depend on the row).
        let (b0, b1) = span_1d(inv.b, inv.d * oy + inv.f, src[1] - 0.5 * gy, src[3] + 0.5 * gy);
        let lo = a0.max(b0);
        let hi = a1.min(b1);
        if !(lo < hi) {
            return;
        }
        let xa = ((lo - 0.5).floor() as i64).max(region.x as i64);
        let xb = ((hi - 0.5).ceil() as i64 + 1).min(region.right() as i64).max(xa);
        let sy = inv.b * 0.0 + inv.d * oy + inv.f;
        let cy = (((sy - src[1]).min(src[3] - sy)) / gy + 0.5).clamp(0.0, 1.0);
        let fy = sy - 0.5;
        let iyf = fy.floor();
        let tyw = (fy - iyf) as f32;
        let iy = iyf as i64;
        let y0 = iy.clamp(ty0, ty1) as usize;
        let y1 = (iy + 1).clamp(ty0, ty1) as usize;
        let ny = (sy.floor() as i64).clamp(ty0, ty1) as usize;
        let (r0, r1) = (&data[y0 * pw * 4..(y0 + 1) * pw * 4], &data[y1 * pw * 4..(y1 + 1) * pw * 4]);
        for x in xa..xb {
            let c = &cols[x as usize];
            let cov = (c.cx * cy) as f32;
            if cov <= 0.0 {
                continue;
            }
            let (i0, i1) = (c.x0 * 4, c.x1 * 4);
            if !d.nearest && cov == 1.0 && opacity == 1.0 && r0[i0 + 3] == 255 && r0[i1 + 3] == 255 && r1[i0 + 3] == 255 && r1[i1 + 3] == 255 {
                // Four opaque taps, fully covered: every premultiply, the blend over the
                // background and the unpremultiply are exact no-ops, leaving a plain lerp.
                let o = &mut line[x as usize * 4..x as usize * 4 + 4];
                for k in 0..3 {
                    let (p00, p10, p01, p11) = (U8_F32[r0[i0 + k] as usize], U8_F32[r0[i1 + k] as usize], U8_F32[r1[i0 + k] as usize], U8_F32[r1[i1 + k] as usize]);
                    let a = p00 + (p10 - p00) * c.tw;
                    let b = p01 + (p11 - p01) * c.tw;
                    let v = a + (b - a) * tyw;
                    o[k] = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                }
                o[3] = 255;
                continue;
            }
            let s = if d.nearest {
                fetch(data, pw, c.nx, ny, premul)
            } else {
                let p00 = fetch(data, pw, c.x0, y0, premul);
                let p10 = fetch(data, pw, c.x1, y0, premul);
                let p01 = fetch(data, pw, c.x0, y1, premul);
                let p11 = fetch(data, pw, c.x1, y1, premul);
                let mut o = [0f32; 4];
                for k in 0..4 {
                    let a = p00[k] + (p10[k] - p00[k]) * c.tw;
                    let b = p01[k] + (p11[k] - p01[k]) * c.tw;
                    o[k] = a + (b - a) * tyw;
                }
                o
            };
            let k = cov * opacity;
            let s = [s[0] * k, s[1] * k, s[2] * k, s[3] * k];
            let i = x as usize * 4;
            line[i..i + 4].copy_from_slice(&px_over(s));
        }
    });
    Some(out)
}

#[inline]
fn put(o: &mut Px, s: Px, mode: BlendMode, fill: Option<Px>) {
    match fill {
        Some(bg) => {
            let k = 1.0 - s[3];
            *o = [s[0] + bg[0] * k, s[1] + bg[1] * k, s[2] + bg[2] * k, s[3] + bg[3] * k];
        }
        None => {
            if mode == BlendMode::Normal && s[3] >= 1.0 {
                *o = s;
            } else {
                blend_px(mode, o, s);
            }
        }
    }
}

/// Returns false when nothing could be drawn (and, with `fill`, nothing was written).
fn draw_raster_impl(dst: &mut Layer, d: &RasterDraw, mode: BlendMode, fill: Option<Px>) -> bool {
    let Some(inv) = d.inv.to_affine() else { return draw_projective(dst, d, mode, fill) };
    let pic = d.pic;
    if pic.w == 0 || pic.h == 0 || d.opacity <= 0.0 || dst.rect.is_empty() {
        return false;
    }
    let mut src = d.src;
    src[0] = src[0].max(0.0);
    src[1] = src[1].max(0.0);
    src[2] = src[2].min(pic.w as f64);
    src[3] = src[3].min(pic.h as f64);
    if src[2] <= src[0] || src[3] <= src[1] {
        return false;
    }
    let region = d.bounds().intersect(&dst.rect);
    if region.is_empty() {
        return false;
    }
    let gx = (inv.a * inv.a + inv.c * inv.c).sqrt().max(1e-9);
    let gy = (inv.b * inv.b + inv.d * inv.d).sqrt().max(1e-9);
    let (pw, ph) = (pic.w as usize, pic.h as usize);
    let tx0 = src[0].floor() as i64;
    let ty0 = src[1].floor() as i64;
    let tx1 = (src[2].ceil() as i64 - 1).min(pw as i64 - 1);
    let ty1 = (src[3].ceil() as i64 - 1).min(ph as i64 - 1);
    let data: &[u8] = &pic.data;
    let premul = pic.premul;
    let opacity = d.opacity;
    let dr = dst.rect;
    let dw = dst.stride();

    // Fast path: a whole-pixel copy (same resolution, no rotation, pixel-aligned crop).
    let aligned = src.iter().all(|v| (v - v.round()).abs() < 1e-6);
    if let (Some((dx, dy)), true) = (inv.integer_translation(), aligned) {
        let (sx0, sy0, sx1, sy1) = (src[0] as i64, src[1] as i64, src[2] as i64, src[3] as i64);
        dst.px.par_chunks_mut(dw).enumerate().for_each(|(row, line)| {
            let y = dr.y as i64 + row as i64;
            let sy = y + dy;
            if y < region.y as i64 || y >= region.bottom() as i64 || sy < sy0 || sy >= sy1 {
                if let Some(bg) = fill {
                    line.fill(bg);
                }
                return;
            }
            let xa = (sx0 - dx).max(region.x as i64);
            let xb = (sx1 - dx).min(region.right() as i64).max(xa);
            if let Some(bg) = fill {
                line[..(xa - dr.x as i64).max(0) as usize].fill(bg);
                line[((xb - dr.x as i64).max(0) as usize).min(dw)..].fill(bg);
            }
            for x in xa..xb {
                let sx = (x + dx) as usize;
                let mut s = fetch(data, pw, sx, sy as usize, premul);
                if opacity < 1.0 {
                    for v in s.iter_mut() {
                        *v *= opacity;
                    }
                }
                put(&mut line[(x - dr.x as i64) as usize], s, mode, fill);
            }
        });
        return true;
    }

    dst.px.par_chunks_mut(dw).enumerate().for_each(|(row, line)| {
        let y = dr.y + row as i32;
        if y < region.y || y >= region.bottom() {
            if let Some(bg) = fill {
                line.fill(bg);
            }
            return;
        }
        let oy = y as f64 + 0.5;
        // Columns where the picture can cover this row.
        let (a0, a1) = span_1d(inv.a, inv.c * oy + inv.e, src[0] - 0.5 * gx, src[2] + 0.5 * gx);
        let (b0, b1) = span_1d(inv.b, inv.d * oy + inv.f, src[1] - 0.5 * gy, src[3] + 0.5 * gy);
        let lo = a0.max(b0);
        let hi = a1.min(b1);
        if !(lo < hi) {
            if let Some(bg) = fill {
                line.fill(bg);
            }
            return;
        }
        let xa = ((lo - 0.5).floor() as i64).max(region.x as i64);
        let xb = ((hi - 0.5).ceil() as i64 + 1).min(region.right() as i64).max(xa);
        if let Some(bg) = fill {
            line[..(xa - dr.x as i64).max(0) as usize].fill(bg);
            line[((xb - dr.x as i64).max(0) as usize).min(dw)..].fill(bg);
        }
        for x in xa..xb {
            let ox = x as f64 + 0.5;
            let sx = inv.a * ox + inv.c * oy + inv.e;
            let sy = inv.b * ox + inv.d * oy + inv.f;
            let cx = (((sx - src[0]).min(src[2] - sx)) / gx + 0.5).clamp(0.0, 1.0);
            let cy = (((sy - src[1]).min(src[3] - sy)) / gy + 0.5).clamp(0.0, 1.0);
            let cov = (cx * cy) as f32;
            if cov <= 0.0 {
                if let Some(bg) = fill {
                    line[(x - dr.x as i64) as usize] = bg;
                }
                continue;
            }
            let s = if d.nearest {
                let ix = (sx.floor() as i64).clamp(tx0, tx1) as usize;
                let iy = (sy.floor() as i64).clamp(ty0, ty1) as usize;
                fetch(data, pw, ix, iy, premul)
            } else {
                let fx = sx - 0.5;
                let fy = sy - 0.5;
                let ixf = fx.floor();
                let iyf = fy.floor();
                let txw = (fx - ixf) as f32;
                let tyw = (fy - iyf) as f32;
                let ix = ixf as i64;
                let iy = iyf as i64;
                let x0 = ix.clamp(tx0, tx1) as usize;
                let x1 = (ix + 1).clamp(tx0, tx1) as usize;
                let y0 = iy.clamp(ty0, ty1) as usize;
                let y1 = (iy + 1).clamp(ty0, ty1) as usize;
                let p00 = fetch(data, pw, x0, y0, premul);
                let p10 = fetch(data, pw, x1, y0, premul);
                let p01 = fetch(data, pw, x0, y1, premul);
                let p11 = fetch(data, pw, x1, y1, premul);
                let mut o = [0f32; 4];
                for k in 0..4 {
                    let a = p00[k] + (p10[k] - p00[k]) * txw;
                    let b = p01[k] + (p11[k] - p01[k]) * txw;
                    o[k] = a + (b - a) * tyw;
                }
                o
            };
            let k = cov * opacity;
            let s = [s[0] * k, s[1] * k, s[2] * k, s[3] * k];
            put(&mut line[(x - dr.x as i64) as usize], s, mode, fill);
        }
    });
    true
}

/// Converts a tiny-skia pixmap placed at canvas `(x, y)` into a layer.
pub(crate) fn pixmap_layer(cw: u32, ch: u32, pm: &tiny_skia::Pixmap, x: i32, y: i32) -> Layer {
    let rect = IRect::new(x, y, pm.width() as i32, pm.height() as i32);
    let vis = rect.intersect(&IRect::of_size(cw, ch));
    let mut out = Layer::new(cw, ch, vis);
    if vis.is_empty() {
        return out;
    }
    let data = pm.data();
    let pw = pm.width() as usize;
    let ow = out.stride();
    out.px.par_chunks_mut(ow).enumerate().for_each(|(row, line)| {
        let sy = (vis.y - y) as usize + row;
        let sx0 = (vis.x - x) as usize;
        for (i, o) in line.iter_mut().enumerate() {
            let j = (sy * pw + sx0 + i) * 4;
            const K: f32 = 1.0 / 255.0;
            *o = [data[j] as f32 * K, data[j + 1] as f32 * K, data[j + 2] as f32 * K, data[j + 3] as f32 * K];
        }
    });
    out
}

// ---------------------------------------------------------------------------------------------
// Projective drawing and weighted accumulation (3-D cards, motion blur, motion transitions)
// ---------------------------------------------------------------------------------------------

/// Bilinear sample of picture texels clamped to `[tx0..=tx1] × [ty0..=ty1]`, premultiplied.
#[inline]
#[allow(clippy::too_many_arguments)]
fn pic_bilinear(data: &[u8], pw: usize, premul: bool, sx: f64, sy: f64, tx: (i64, i64), ty: (i64, i64)) -> Px {
    let fx = sx - 0.5;
    let fy = sy - 0.5;
    let ixf = fx.floor();
    let iyf = fy.floor();
    let txw = (fx - ixf) as f32;
    let tyw = (fy - iyf) as f32;
    let (ix, iy) = (ixf as i64, iyf as i64);
    let x0 = ix.clamp(tx.0, tx.1) as usize;
    let x1 = (ix + 1).clamp(tx.0, tx.1) as usize;
    let y0 = iy.clamp(ty.0, ty.1) as usize;
    let y1 = (iy + 1).clamp(ty.0, ty.1) as usize;
    let p00 = fetch(data, pw, x0, y0, premul);
    let p10 = fetch(data, pw, x1, y0, premul);
    let p01 = fetch(data, pw, x0, y1, premul);
    let p11 = fetch(data, pw, x1, y1, premul);
    let mut o = [0f32; 4];
    for k in 0..4 {
        let a = p00[k] + (p10[k] - p00[k]) * txw;
        let b = p01[k] + (p11[k] - p01[k]) * txw;
        o[k] = a + (b - a) * tyw;
    }
    o
}

/// How source coordinates change per output pixel at an output point under a projective inverse:
/// returns `(sx, sy, gx, gy)` (source position and footprint along source x / y), or `None`
/// behind the camera.
#[inline]
fn proj_footprint(inv: &Proj, ox: f64, oy: f64) -> Option<(f64, f64, f64, f64)> {
    let m = &inv.m;
    let (u, v, w) = inv.apply_h(ox, oy);
    if w <= W_EPS {
        return None;
    }
    let (sx, sy) = (u / w, v / w);
    let a = (m[0] - sx * m[6]) / w;
    let b = (m[1] - sx * m[7]) / w;
    let c = (m[3] - sy * m[6]) / w;
    let d = (m[4] - sy * m[7]) / w;
    Some((sx, sy, a.hypot(b).max(1e-9), c.hypot(d).max(1e-9)))
}

/// One picture sample under a projective map, with anti-aliased crop edges and box-filtered
/// supersampling where the picture is shrunk (the far side of a 3-D card).
#[inline]
fn proj_pic_sample(pic: &Picture, src: &[f64; 4], tx: (i64, i64), ty: (i64, i64), inv: &Proj, ox: f64, oy: f64, ss: bool) -> Px {
    let Some((sx, sy, gx, gy)) = proj_footprint(inv, ox, oy) else { return [0.0; 4] };
    let cov = |s: f64, lo: f64, hi: f64, g: f64| (((s - lo).min(hi - s)) / g + 0.5).clamp(0.0, 1.0);
    let c = (cov(sx, src[0], src[2], gx) * cov(sy, src[1], src[3], gy)) as f32;
    if c <= 0.0 {
        return [0.0; 4];
    }
    let data: &[u8] = &pic.data;
    let pw = pic.w as usize;
    let n = if ss { gx.max(gy).ceil().clamp(1.0, 4.0) as usize } else { 1 };
    let mut o = if n <= 1 {
        pic_bilinear(data, pw, pic.premul, sx, sy, tx, ty)
    } else {
        let mut acc = [0f32; 4];
        let mut k = 0usize;
        for j in 0..n {
            for i in 0..n {
                let qx = ox + (i as f64 + 0.5) / n as f64 - 0.5;
                let qy = oy + (j as f64 + 0.5) / n as f64 - 0.5;
                if let Some((x, y)) = inv.apply(qx, qy) {
                    let p = pic_bilinear(data, pw, pic.premul, x, y, tx, ty);
                    for q in 0..4 {
                        acc[q] += p[q];
                    }
                    k += 1;
                }
            }
        }
        let inv_k = 1.0 / k.max(1) as f32;
        acc.map(|v| v * inv_k)
    };
    for v in o.iter_mut() {
        *v *= c;
    }
    o
}

fn texel_range(pic: &Picture, src: &[f64; 4]) -> ((i64, i64), (i64, i64)) {
    let (pw, ph) = (pic.w as i64, pic.h as i64);
    let tx = (src[0].floor() as i64, (src[2].ceil() as i64 - 1).min(pw - 1));
    let ty = (src[1].floor() as i64, (src[3].ceil() as i64 - 1).min(ph - 1));
    (tx, ty)
}

/// Canvas bounds of a picture's visible part under the inverse map `inv` (canvas → picture);
/// the whole canvas when part of it is behind the camera.
pub(crate) fn proj_bounds(inv: &Proj, src: &[f64; 4], canvas: IRect) -> IRect {
    match inv.invert().and_then(|f| f.bounds_of(src[0], src[1], src[2], src[3])) {
        Some(b) => IRect::from_bounds(b).expand(1).intersect(&canvas),
        None => canvas,
    }
}

/// Draws a picture through a projective map (`d.inv` not affine).
fn draw_projective(dst: &mut Layer, d: &RasterDraw, mode: BlendMode, fill: Option<Px>) -> bool {
    let pic = d.pic;
    let mut src = d.src;
    src[0] = src[0].max(0.0);
    src[1] = src[1].max(0.0);
    src[2] = src[2].min(pic.w as f64);
    src[3] = src[3].min(pic.h as f64);
    if pic.w == 0 || pic.h == 0 || src[2] <= src[0] || src[3] <= src[1] || d.opacity <= 0.0 || dst.rect.is_empty() {
        return false;
    }
    if let Some(bg) = fill {
        dst.fill(bg);
    }
    let region = proj_bounds(&d.inv, &src, dst.rect);
    if region.is_empty() {
        return fill.is_some();
    }
    let (tx, ty) = texel_range(pic, &src);
    let inv = d.inv;
    let dr = dst.rect;
    let dw = dst.stride();
    let opacity = d.opacity;
    dst.px.par_chunks_mut(dw).enumerate().for_each(|(row, line)| {
        let y = dr.y + row as i32;
        if y < region.y || y >= region.bottom() {
            return;
        }
        let oy = y as f64 + 0.5;
        for x in region.x..region.right() {
            let s = proj_pic_sample(pic, &src, tx, ty, &inv, x as f64 + 0.5, oy, true);
            if s[3] <= 0.0 {
                continue;
            }
            let s = [s[0] * opacity, s[1] * opacity, s[2] * opacity, s[3] * opacity];
            put(&mut line[(x - dr.x) as usize], s, mode, None);
        }
    });
    true
}

/// What an accumulation item samples.
pub(crate) enum AccSource<'a> {
    /// A picture's `src` rectangle (anti-aliased edges).
    Pic { pic: &'a Picture, src: [f64; 4] },
    /// A canvas-space layer; with `mirror`, coordinates outside that canvas rectangle reflect back
    /// into it (mirrored edges, so a spinning or shaking full-frame picture never shows a border).
    Layer { layer: &'a Layer, mirror: Option<[f64; 4]> },
}

/// One weighted draw: `weight × sample(inv(p))`, colours × `shade`.
pub(crate) struct AccItem<'a> {
    pub src: AccSource<'a>,
    /// Output canvas pixels → source coordinates (picture pixels or canvas pixels of the layer).
    pub inv: Proj,
    pub weight: f32,
    pub shade: f32,
}

/// Supersampling where a picture is shrunk is only needed when few items are averaged (many
/// sub-frame samples already filter it).
fn supersample_ok(items: &[AccItem]) -> bool {
    items.len() <= 4
}

#[inline]
fn reflect(v: f64, lo: f64, hi: f64) -> f64 {
    let w = hi - lo;
    if w <= 0.0 {
        return lo;
    }
    let mut u = (v - lo).rem_euclid(2.0 * w);
    if u > w {
        u = 2.0 * w - u;
    }
    lo + u
}

impl AccItem<'_> {
    fn bounds(&self, canvas: IRect) -> IRect {
        match &self.src {
            AccSource::Pic { src, .. } => proj_bounds(&self.inv, src, canvas),
            AccSource::Layer { mirror: Some(_), .. } => canvas,
            AccSource::Layer { layer, mirror: None } => {
                let r = layer.rect;
                proj_bounds(&self.inv, &[r.x as f64, r.y as f64, r.right() as f64, r.bottom() as f64], canvas)
            }
        }
    }

    #[inline]
    fn sample(&self, ox: f64, oy: f64, tex: ((i64, i64), (i64, i64)), ss: bool) -> Px {
        match &self.src {
            AccSource::Pic { pic, src } => proj_pic_sample(pic, src, tex.0, tex.1, &self.inv, ox, oy, ss),
            AccSource::Layer { layer, mirror } => {
                let Some((sx, sy, gx, gy)) = proj_footprint(&self.inv, ox, oy) else { return [0.0; 4] };
                let one = |x: f64, y: f64| -> Px {
                    match mirror {
                        Some(m) => layer.sample(reflect(x, m[0], m[2]), reflect(y, m[1], m[3]), (true, true, true, true)),
                        None => layer.sample(x, y, (false, false, false, false)),
                    }
                };
                let n = if ss { gx.max(gy).ceil().clamp(1.0, 3.0) as usize } else { 1 };
                if n <= 1 {
                    return one(sx, sy);
                }
                let mut acc = [0f32; 4];
                let mut k = 0;
                for j in 0..n {
                    for i in 0..n {
                        let qx = ox + (i as f64 + 0.5) / n as f64 - 0.5;
                        let qy = oy + (j as f64 + 0.5) / n as f64 - 0.5;
                        if let Some((x, y)) = self.inv.apply(qx, qy) {
                            let p = one(x, y);
                            for q in 0..4 {
                                acc[q] += p[q];
                            }
                            k += 1;
                        }
                    }
                }
                let inv_k = 1.0 / k.max(1) as f32;
                acc.map(|v| v * inv_k)
            }
        }
    }
}

/// Like [`accumulate`], computed at `1 / div` resolution and scaled back up (`div` 2 or 4: for
/// heavy smears, whose detail is gone anyway).
pub(crate) fn accumulate_res(cw: u32, ch: u32, items: Vec<AccItem>, limit: IRect, div: u32) -> Layer {
    let div = div.clamp(1, 8);
    if div == 1 || cw < 16 * div || ch < 16 * div {
        return accumulate(cw, ch, &items, limit);
    }
    let k = div as f64;
    let (hw, hh) = (cw.div_ceil(div), ch.div_ceil(div));
    let up = Proj::from_affine(&Affine::scale(k, k));
    let small: Vec<AccItem> = items.into_iter().map(|it| AccItem { inv: up.then(&it.inv), ..it }).collect();
    let lim = IRect::from_bounds([limit.x as f64 / k, limit.y as f64 / k, limit.right() as f64 / k, limit.bottom() as f64 / k]);
    let lo = accumulate(hw, hh, &small, lim);
    if lo.is_empty() {
        return Layer::empty(cw, ch);
    }
    let r = lo.rect;
    let d = div as i32;
    let rect = IRect::new(r.x * d, r.y * d, r.w * d, r.h * d).intersect(&IRect::of_size(cw, ch)).intersect(&limit);
    let mut out = Layer::new(cw, ch, rect);
    let w = out.stride();
    let clamp = lo.touches_edges();
    out.px.par_chunks_mut(w.max(1)).enumerate().for_each(|(row, line)| {
        let y = (rect.y + row as i32) as f64 + 0.5;
        for (i, o) in line.iter_mut().enumerate() {
            let x = (rect.x + i as i32) as f64 + 0.5;
            *o = lo.sample(x / k, y / k, clamp);
        }
    });
    out
}

/// Sampling plan for a smear of `moved` output pixels across the shutter: (resolution divisor,
/// number of samples). Samples land about 1.5 pixels apart at the chosen resolution; larger
/// smears are sampled at lower resolution where their detail is gone anyway.
pub(crate) fn smear_plan(moved: f64, max_n: usize, draft: bool) -> (u32, usize) {
    let (div, cap) = if moved > 160.0 {
        (4, 64)
    } else if moved > 24.0 {
        (2, 48)
    } else {
        (1, 32)
    };
    let cap = if draft { 8 } else { cap.min(max_n.max(2)) };
    let n = ((moved / (1.5 * div as f64)).ceil() as usize).clamp(2, cap);
    (div, n)
}

/// `Σ weight × sample` of every item into a new layer limited to `limit` (premultiplied sums: the
/// average of `n` sub-frame renders when each weight is `1/n`).
pub(crate) fn accumulate(cw: u32, ch: u32, items: &[AccItem], limit: IRect) -> Layer {
    let canvas = IRect::of_size(cw, ch).intersect(&limit);
    let bounds: Vec<IRect> = items.iter().map(|it| it.bounds(canvas)).collect();
    let rect = bounds.iter().fold(IRect::default(), |a, b| a.union(b)).intersect(&canvas);
    let mut out = Layer::new(cw, ch, rect);
    if rect.is_empty() {
        return out;
    }
    let tex: Vec<((i64, i64), (i64, i64))> = items
        .iter()
        .map(|it| match &it.src {
            AccSource::Pic { pic, src } => texel_range(pic, src),
            AccSource::Layer { .. } => ((0, 0), (0, 0)),
        })
        .collect();
    let w = out.stride();
    let ss = supersample_ok(items);
    out.px.par_chunks_mut(w).enumerate().for_each(|(row, line)| {
        let y = rect.y + row as i32;
        let oy = y as f64 + 0.5;
        for (k, it) in items.iter().enumerate() {
            let b = &bounds[k];
            if it.weight == 0.0 || y < b.y || y >= b.bottom() {
                continue;
            }
            let (xa, xb) = (b.x.max(rect.x), b.right().min(rect.right()));
            for x in xa..xb {
                let s = it.sample(x as f64 + 0.5, oy, tex[k], ss);
                if s[3] == 0.0 && s[0] == 0.0 && s[1] == 0.0 && s[2] == 0.0 {
                    continue;
                }
                let o = &mut line[(x - rect.x) as usize];
                let (wc, wa) = (it.weight * it.shade, it.weight);
                o[0] += s[0] * wc;
                o[1] += s[1] * wc;
                o[2] += s[2] * wc;
                o[3] += s[3] * wa;
            }
        }
    });
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Tests that inspect the process-wide pool run one at a time.
    pub(crate) static POOL_TEST: Mutex<()> = Mutex::new(());

    fn pic(w: u32, h: u32, c: [u8; 4]) -> Picture {
        Picture { w, h, data: Arc::new(c.iter().copied().cycle().take((w * h * 4) as usize).collect()), premul: false }
    }

    /// A noisy picture; `alpha`: some pixels translucent (else all opaque).
    fn noise_pic(w: u32, h: u32, seed: u64, alpha: bool, premul: bool) -> Picture {
        let mut s = seed | 1;
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let a = if alpha { [255u8, 255, 128, 0, 30][(s % 5) as usize] } else { 255 };
            for k in 0..3 {
                let c = ((s >> (8 * (k + 1))) & 0xff) as u8;
                v.push(if premul { (c as u32 * a as u32 / 255) as u8 } else { c });
            }
            v.push(a);
        }
        Picture { w, h, data: Arc::new(v), premul }
    }

    #[test]
    fn straight_rgba8_fast_path_matches_the_canvas_path() {
        // Every axis-aligned case the compositor meets: same size (whole-pixel copy), down- and
        // upscaling, fractional offsets (soft edges), crops, fades, drafts (nearest), translucent
        // and premultiplied pictures, letterbox bars and transparent backgrounds.
        let (cw, ch) = (97u32, 61u32);
        let mut cases = 0;
        for (pi, p) in [noise_pic(97, 61, 1, false, false), noise_pic(64, 40, 2, false, false), noise_pic(200, 120, 3, true, false), noise_pic(50, 50, 4, true, true)].iter().enumerate() {
            for &(sx, sy, ex, ey) in &[(1.0, 1.0, 0.0, 0.0), (1.0, 1.0, 3.0, -2.0), (0.5, 0.5, 0.25, 0.75), (2.0, 2.0, -7.3, 1.1), (1.37, 0.91, 5.5, -3.25), (0.333, 0.333, 0.0, 0.0), (1.0, 1.0, 0.4, 0.0)] {
                for src in [[0.0, 0.0, p.w as f64, p.h as f64], [3.0, 2.0, p.w as f64 - 5.0, p.h as f64 - 1.0], [1.5, 0.25, p.w as f64 * 0.8, p.h as f64 * 0.7]] {
                    for &(opacity, nearest) in &[(1.0f32, false), (0.6, false), (1.0, true)] {
                        for bg in [[0.1f32, 0.2, 0.3, 1.0], [0.0, 0.0, 0.0, 0.0]] {
                            let inv = Proj::from_affine(&Affine { a: sx, b: 0.0, c: 0.0, d: sy, e: ex, f: ey });
                            let d = RasterDraw { pic: p, inv, src, opacity, nearest };
                            let mut l = Layer::dirty(cw, ch);
                            draw_raster_over(&mut l, &d, bg);
                            let want = l.to_rgba8();
                            let got = raster_over_rgba8(&d, bg, cw, ch).expect("axis-aligned");
                            let diff = want.iter().zip(&got).position(|(a, b)| a != b);
                            assert!(diff.is_none(), "picture {pi}, inv ({sx},{sy},{ex},{ey}), src {src:?}, opacity {opacity}, nearest {nearest}, bg {bg:?}: first difference at byte {diff:?}");
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert!(cases > 400);
        // Rotated or projective draws are left to the general path.
        let p = noise_pic(10, 10, 5, false, false);
        let d = RasterDraw { pic: &p, inv: Proj::from_affine(&Affine { a: 0.9, b: 0.1, c: -0.1, d: 0.9, e: 0.0, f: 0.0 }), src: [0.0, 0.0, 10.0, 10.0], opacity: 1.0, nearest: false };
        assert!(raster_over_rgba8(&d, [0.0; 4], 16, 16).is_none());
    }

    #[test]
    fn identity_copy_and_scaled_draw() {
        let p = pic(4, 4, [255, 0, 0, 255]);
        let mut l = Layer::full(8, 8, [0.0, 0.0, 0.0, 1.0]);
        let d = RasterDraw { pic: &p, inv: Proj::from_affine(&Affine::translate(-2.0, -2.0)), src: [0.0, 0.0, 4.0, 4.0], opacity: 1.0, nearest: false };
        draw_raster(&mut l, &d, BlendMode::Normal);
        assert_eq!(unpremul8(l.get(2, 2)), [255, 0, 0, 255]);
        assert_eq!(unpremul8(l.get(5, 5)), [255, 0, 0, 255]);
        assert_eq!(unpremul8(l.get(6, 6)), [0, 0, 0, 255]);
        assert_eq!(unpremul8(l.get(1, 1)), [0, 0, 0, 255]);
        // 2x upscale through the general path.
        let mut l = Layer::full(8, 8, [0.0, 0.0, 0.0, 1.0]);
        let d = RasterDraw { pic: &p, inv: Proj::from_affine(&Affine::scale(0.5, 0.5)), src: [0.0, 0.0, 4.0, 4.0], opacity: 0.5, nearest: false };
        draw_raster(&mut l, &d, BlendMode::Normal);
        let px = unpremul8(l.get(3, 3));
        assert!((px[0] as i32 - 128).abs() <= 1, "{px:?}");
        assert_eq!(px[1], 0);
    }

    #[test]
    fn pool_compaction_cap_and_trim() {
        let _g = POOL_TEST.lock().unwrap_or_else(|p| p.into_inner());
        // A small layer may borrow a pooled canvas while it is drawn, but compacting it (before it
        // is cached) gives the canvas back.
        clear_pool();
        let mut big = Vec::with_capacity(2_000_001);
        big.resize(50_000, [0.0; 4]);
        let mut small = Layer { cw: 4000, ch: 4000, rect: crate::geom::IRect::new(0, 0, 250, 200), px: big };
        small.compact();
        assert!(small.px.capacity() < 60_000, "compacted to {} pixels", small.px.capacity());
        assert!(lock(&POOL).iter().any(|b| b.capacity() == 2_000_001), "the canvas is back in the pool");
        assert_eq!(small.get(10, 10), [0.0; 4]);
        // Exports raise the cap for their frames and restore it; trimming frees idle buffers.
        // (Other unit tests use the pool concurrently and may borrow these buffers: only check
        // the cap and that trimming leaves none of them idle.)
        let big_ones = || lock(&POOL).iter().filter(|b| b.capacity() == 8_000_000).count();
        let limit = || POOL_LIMIT.load(std::sync::atomic::Ordering::Relaxed);
        set_pool_limit(Some(100_000_000));
        assert_eq!(limit(), 100_000_000);
        for _ in 0..4 {
            give_buffer(Vec::with_capacity(8_000_000));
        }
        set_pool_limit(Some(1));
        assert_eq!(limit(), POOL_MAX_PIXELS, "never below the default");
        set_pool_limit(None);
        assert_eq!(limit(), POOL_MAX_PIXELS);
        trim_pool(POOL_IDLE_PIXELS);
        assert_eq!(big_ones(), 0, "trimming frees the large idle buffers");
    }

    #[test]
    fn blend_modes() {
        let grey = [0.5, 0.5, 0.5, 1.0];
        let mut d = grey;
        blend_px(BlendMode::Multiply, &mut d, grey);
        assert!((d[0] - 0.25).abs() < 1e-6);
        let mut d = grey;
        blend_px(BlendMode::Screen, &mut d, grey);
        assert!((d[0] - 0.75).abs() < 1e-6);
        let mut d = [0.2, 0.2, 0.2, 1.0];
        blend_px(BlendMode::Add, &mut d, [0.3, 0.3, 0.3, 1.0]);
        assert!((d[0] - 0.5).abs() < 1e-6 && (d[3] - 1.0).abs() < 1e-6);
        let mut d = [0.8, 0.8, 0.8, 1.0];
        blend_px(BlendMode::Difference, &mut d, [0.3, 0.3, 0.3, 1.0]);
        assert!((d[0] - 0.5).abs() < 1e-6);
        let mut d = [0.8, 0.8, 0.8, 1.0];
        blend_px(BlendMode::Darken, &mut d, [0.3, 0.9, 0.3, 1.0]);
        assert!((d[0] - 0.3).abs() < 1e-6 && (d[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn transform_and_expand() {
        let mut l = Layer::new(10, 10, IRect::new(2, 2, 2, 2));
        l.px.iter_mut().for_each(|p| *p = [1.0, 1.0, 1.0, 1.0]);
        let m = transform_layer(&l, &Affine::translate(3.0, 1.0));
        assert_eq!(m.rect, IRect::new(5, 3, 2, 2));
        assert_eq!(m.get(5, 3)[3], 1.0);
        l.expand_to(IRect::new(0, 0, 10, 10));
        assert_eq!(l.rect, IRect::new(0, 0, 10, 10));
        assert_eq!(l.get(2, 2)[3], 1.0);
        assert_eq!(l.get(0, 0)[3], 0.0);
    }
}
