//! The compositor: a frame at time t. Video tracks bottom → top; each active clip's picture goes
//! crop → fit → transform → effects → opacity × fade → blend onto the canvas. Adjustment clips
//! apply their effects to the composite below. Transitions combine both clips around the cut.

use crate::canvas::{accumulate, accumulate_res, blend_layer, draw_raster, draw_raster_over, mix_layers, over_union, premul, transform_layer, AccItem, AccSource, Layer, Picture, Px, RasterDraw};
use crate::effects::{apply_effects, shake_offset, FxCtx};
use crate::geom::{m3_apply, m3_mul, plane_homography, rot_x, rot_y, rot_z, Affine, IRect, Proj};
use crate::text::{self, TextLayout, TextState};
use crate::timeline::{covers, local_time, Active, Timeline, TransInfo};
use crate::transition::{self, TCtx};
use crate::util::{frame_index, frame_time, KeyHasher};
use crate::{FrameOptions, Image, LayerBox, RenderError, Renderer};
use aiframes_core::eval::{sample, source_time, visual_opacity};
use aiframes_media::FrameCaching;
use aiframes_core::{BlendMode, Clip, Color, Fit, MediaKind, Project, Resolved, Settings, Source, Span};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

pub(crate) struct Ctx<'a> {
    pub rd: &'a Renderer,
    pub doc_dir: &'a Path,
    pub p: &'a Project,
    pub tl: &'a Timeline<'a>,
    pub t: f64,
    pub frame: i64,
    pub out_w: u32,
    pub out_h: u32,
    /// Output pixels per project pixel.
    pub sx: f64,
    pub sy: f64,
    /// Canvas position of the project frame's top-left corner (non-zero for exports whose output
    /// crops the frame).
    pub ox: f64,
    pub oy: f64,
    pub draft: bool,
    pub strict: bool,
    /// How decoded media frames are cached (export reads at low priority).
    pub caching: FrameCaching,
    /// Only fetch the media the frame needs (export read-ahead); draw nothing.
    pub prefetch: bool,
    pub warnings: Mutex<Vec<String>>,
    /// Frame-blended pictures of this frame (shared by motion-blur sub-samples).
    pub blends: Mutex<HashMap<String, Picture>>,
}

impl Ctx<'_> {
    pub fn warn(&self, msg: String) {
        let mut w = crate::util::lock(&self.warnings);
        if !w.contains(&msg) {
            w.push(msg);
        }
    }
    /// A problem that fails strict renders (export) and is a warning otherwise.
    pub fn problem(&self, err: RenderError) -> Result<(), RenderError> {
        if self.strict {
            return Err(err);
        }
        self.warn(err.to_string());
        Ok(())
    }
    fn px_scale(&self) -> f64 {
        (self.sx + self.sy) / 2.0
    }
    fn fw(&self) -> f64 {
        self.p.settings.width.max(1) as f64
    }
    fn fh(&self) -> f64 {
        self.p.settings.height.max(1) as f64
    }
    /// Project pixels → canvas pixels.
    fn out_affine(&self) -> Affine {
        Affine::scale(self.sx, self.sy).then(&Affine::translate(self.ox, self.oy))
    }
    /// The project frame on the canvas: `[x, y, w, h]` (may extend past the canvas).
    pub fn frame_rect(&self) -> [f64; 4] {
        [self.ox, self.oy, self.fw() * self.sx, self.fh() * self.sy]
    }
}

// ---------------------------------------------------------------------------------------------
// Content boxes and placement (shared with layers_at)
// ---------------------------------------------------------------------------------------------

pub(crate) enum BoxKind {
    Media { path: PathBuf, kind: MediaKind, sw: f64, sh: f64, crop: [f64; 4] },
    Solid { color: Color },
    Text { layout: Arc<TextLayout> },
    Shape,
    Adjustment,
}

pub(crate) struct ClipBox {
    pub kind: BoxKind,
    pub bw: f64,
    pub bh: f64,
}

fn crop_of(clip: &Clip, t_local: f64) -> [f64; 4] {
    let c = &clip.transform.crop;
    let f = |p: &aiframes_core::Param| {
        let v = sample(p, t_local);
        if v.is_finite() {
            v.clamp(0.0, 0.999)
        } else {
            0.0
        }
    };
    let (mut l, mut t, mut r, mut b) = (f(&c.left), f(&c.top), f(&c.right), f(&c.bottom));
    if l + r > 0.999 {
        let k = 0.999 / (l + r);
        l *= k;
        r *= k;
    }
    if t + b > 0.999 {
        let k = 0.999 / (t + b);
        t *= k;
        b *= k;
    }
    [l, t, r, b]
}

fn fit_box(fit: Fit, cw: f64, ch: f64, fw: f64, fh: f64) -> (f64, f64) {
    match fit {
        Fit::Contain => {
            let k = (fw / cw).min(fh / ch);
            (cw * k, ch * k)
        }
        Fit::Cover => {
            let k = (fw / cw).max(fh / ch);
            (cw * k, ch * k)
        }
        Fit::Stretch => (fw, fh),
        Fit::None => (cw, ch),
    }
}

/// The content box of a clip at clip-local time `t_local` (in project pixels before transform).
pub(crate) fn clip_box(
    rd: &Renderer,
    doc_dir: &Path,
    tl: &Timeline,
    clip: &Clip,
    t_local: f64,
    warn: &dyn Fn(RenderError, bool) -> Result<(), RenderError>,
) -> Result<Option<ClipBox>, RenderError> {
    let s = &tl.p.settings;
    let (fw, fh) = (s.width.max(1) as f64, s.height.max(1) as f64);
    Ok(Some(match &clip.source {
        Source::Media { asset, .. } => {
            let Some(a) = tl.assets.get(asset.as_str()) else {
                warn(RenderError::Missing(format!("clip {} uses media asset `{asset}`, which is not in the project", clip.id)), true)?;
                return Ok(Some(ClipBox { kind: BoxKind::Solid { color: OFFLINE }, bw: fw, bh: fh }));
            };
            if a.kind == MediaKind::Audio {
                return Ok(None);
            }
            let path = doc_dir.join(&a.path);
            let (mut sw, mut sh) = (a.info.width as f64, a.info.height as f64);
            if sw < 1.0 || sh < 1.0 {
                match rd.pool().probe(&path) {
                    Ok((_, info)) if info.width > 0 && info.height > 0 => {
                        sw = info.width as f64;
                        sh = info.height as f64;
                    }
                    _ => {
                        sw = fw;
                        sh = fh;
                    }
                }
            }
            let crop = crop_of(clip, t_local);
            let cw = sw * (1.0 - crop[0] - crop[2]);
            let ch = sh * (1.0 - crop[1] - crop[3]);
            let (bw, bh) = fit_box(clip.transform.fit, cw, ch, fw, fh);
            ClipBox { kind: BoxKind::Media { path, kind: a.kind, sw, sh, crop }, bw, bh }
        }
        Source::Solid { color } => {
            // A solid is the frame itself; cropping shrinks it (no re-fit).
            let crop = crop_of(clip, t_local);
            let (bw, bh) = match clip.transform.fit {
                Fit::Stretch => (fw, fh),
                _ => (fw * (1.0 - crop[0] - crop[2]), fh * (1.0 - crop[1] - crop[3])),
            };
            ClipBox { kind: BoxKind::Solid { color: *color }, bw, bh }
        }
        Source::Text(ts) => {
            let (layout, w) = rd.text_layout(doc_dir, ts);
            if let Some(w) = w {
                warn(RenderError::Other(format!("clip {}: {w}", clip.id)), false)?;
            }
            let (bw, bh, _) = text::content_size(ts, &layout);
            ClipBox { kind: BoxKind::Text { layout }, bw, bh }
        }
        Source::Shape(sh) => {
            let (bw, bh) = crate::shape::content_size(sh);
            ClipBox { kind: BoxKind::Shape, bw, bh }
        }
        Source::Adjustment => ClipBox { kind: BoxKind::Adjustment, bw: fw, bh: fh },
    }))
}

/// Leading `shake` effects folded into the transform: (dx, dy, rotation, overscan scale).
fn folded_shake(clip: &Clip, t_local: f64, fw: f64, fh: f64) -> Option<(f64, f64, f64, f64)> {
    let mut acc: Option<(f64, f64, f64, f64)> = None;
    for e in clip.effects.iter().filter(|e| e.enabled) {
        if e.kind != "shake" {
            break;
        }
        let (dx, dy, r) = shake_offset(e, t_local);
        let amount = aiframes_core::effects::effect_param(e, "amount", t_local).abs().min(crate::effects::MAX_PX);
        let rot = aiframes_core::effects::effect_param(e, "rotation", t_local).abs().min(360.0).to_radians();
        let over = 1.0 + 2.0 * amount / fw.min(fh) + rot.sin() * fw.max(fh) / fw.min(fh);
        let a = acc.get_or_insert((0.0, 0.0, 0.0, 1.0));
        a.0 += dx;
        a.1 += dy;
        a.2 += r;
        a.3 *= over;
    }
    acc
}

/// Effects handled by the clip pipeline itself rather than as pixel effects.
fn is_time_effect(kind: &str) -> bool {
    matches!(kind, "echo" | "stutter")
}

/// Whether the clip has effects that need a layer (anything beyond leading shakes and stutter).
fn needs_layer(clip: &Clip) -> bool {
    clip.effects.iter().filter(|e| e.enabled).any(|e| e.kind != "shake" && e.kind != "stutter")
}

/// Whether the clip can be drawn straight onto the canvas (no layer, no time sampling).
fn plain(clip: &Clip) -> bool {
    !needs_layer(clip) && clip.matte.is_none() && clip.motion_blur.as_ref().is_none_or(|m| !(m.shutter > 0.0) || m.samples < 2)
}

/// `stutter` effects: the clip's local time quantised into steps of `1 / rate`, each frozen on its
/// first frame for `hold` of the step (picture, animation and sound-free state alike).
pub(crate) fn stutter_time(clip: &Clip, t_local: f64) -> f64 {
    let mut t = t_local;
    for e in clip.effects.iter().filter(|e| e.enabled && e.kind == "stutter") {
        let rate = aiframes_core::effects::effect_param(e, "rate", t).clamp(0.01, 1000.0);
        let hold = aiframes_core::effects::effect_param(e, "hold", t).clamp(0.0, 1.0);
        if hold <= 0.0 {
            continue;
        }
        let step = 1.0 / rate;
        let k = (t / step + 1e-9).floor();
        let within = t - k * step;
        if within < hold * step - 1e-9 {
            t = k * step;
        }
    }
    t
}

/// The evaluated transform of a clip at one instant.
struct Placement {
    s: f64,
    rot: f64,
    rx: f64,
    ry: f64,
    ax: f64,
    ay: f64,
    px: f64,
    py: f64,
    flip_x: bool,
    flip_y: bool,
    bw: f64,
    bh: f64,
    perspective: f64,
}

impl Placement {
    fn of(clip: &Clip, t_local: f64, bw: f64, bh: f64, fw: f64, fh: f64, shake: Option<(f64, f64, f64, f64)>) -> Placement {
        let tr = &clip.transform;
        let fin = |v: f64, d: f64| if v.is_finite() { v } else { d };
        let (mut x, mut y) = (fin(sample(&tr.x, t_local), 0.0), fin(sample(&tr.y, t_local), 0.0));
        let mut s = fin(sample(&tr.scale, t_local), 1.0);
        let mut rot = fin(sample(&tr.rotation, t_local), 0.0);
        if let Some((dx, dy, dr, over)) = shake {
            x += dx;
            y += dy;
            rot += dr;
            s *= over;
        }
        let (ax, ay) = (fin(tr.anchor_x, 0.5) * bw, fin(tr.anchor_y, 0.5) * bh);
        Placement {
            s,
            rot,
            rx: fin(sample(&tr.rotation_x, t_local), 0.0),
            ry: fin(sample(&tr.rotation_y, t_local), 0.0),
            ax,
            ay,
            px: fw / 2.0 + x + ax - bw / 2.0,
            py: fh / 2.0 + y + ay - bh / 2.0,
            flip_x: tr.flip_x,
            flip_y: tr.flip_y,
            bw,
            bh,
            perspective: if tr.perspective.is_finite() { tr.perspective.max(0.0) } else { 2000.0 },
        }
    }
    fn flip(&self) -> Affine {
        Affine {
            a: if self.flip_x { -1.0 } else { 1.0 },
            b: 0.0,
            c: 0.0,
            d: if self.flip_y { -1.0 } else { 1.0 },
            e: if self.flip_x { self.bw } else { 0.0 },
            f: if self.flip_y { self.bh } else { 0.0 },
        }
    }
    /// Box → project pixels (projective when the card is tilted or turned in 3-D; the camera looks
    /// at the frame centre from `perspective` pixels away), and the same without the flip.
    fn proj(&self, fw: f64, fh: f64) -> (Proj, Proj) {
        let local = Affine::translate(-self.ax, -self.ay).then(&Affine::scale(self.s, self.s));
        if self.rx.abs() < 1e-9 && self.ry.abs() < 1e-9 {
            let unflipped = local.then(&Affine::rotate_deg(self.rot)).then(&Affine::translate(self.px, self.py));
            return (Proj::from_affine(&self.flip().then(&unflipped)), Proj::from_affine(&unflipped));
        }
        let r = m3_mul(&rot_z(self.rot), &m3_mul(&rot_y(self.ry), &rot_x(self.rx)));
        let h = plane_homography(
            [self.px - fw / 2.0, self.py - fh / 2.0, 0.0],
            m3_apply(&r, [1.0, 0.0, 0.0]),
            m3_apply(&r, [0.0, 1.0, 0.0]),
            self.perspective,
            fw / 2.0,
            fh / 2.0,
        );
        let unflipped = Proj::from_affine(&local).then(&h);
        (Proj::from_affine(&self.flip()).then(&unflipped), unflipped)
    }
}

// ---------------------------------------------------------------------------------------------
// Visuals
// ---------------------------------------------------------------------------------------------

const OFFLINE: Color = Color { r: 0.55, g: 0.0, b: 0.35, a: 1.0 };

fn offline_picture() -> Picture {
    static P: OnceLock<Picture> = OnceLock::new();
    P.get_or_init(|| {
        let (w, h) = (64u32, 36u32);
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let stripe = ((x + y) / 6) % 2 == 0;
                v.extend_from_slice(if stripe { &[150, 0, 95, 255] } else { &[40, 0, 28, 255] });
            }
        }
        Picture { w, h, data: Arc::new(v), premul: false }
    })
    .clone()
}

enum VContent {
    Raster { pic: Picture, box_to_pic: Affine, src: [f64; 4] },
    Text { layout: Arc<TextLayout>, state: TextState },
    Shape { draw: f64 },
}

struct Visual<'c> {
    clip: &'c Clip,
    t_local: f64,
    /// Box coordinates → output pixels (projective for 3-D cards).
    to_out: Proj,
    bw: f64,
    bh: f64,
    opacity: f64,
    content: VContent,
}

/// Picture size to request so that roughly one or two source pixels land on each output pixel
/// (power-of-two steps of the native size so decode sessions stay stable while scale animates).
/// `k` = output pixels per box pixel (the largest local scale).
fn request_size(sw: f64, sh: f64, cw: f64, ch: f64, bw: f64, bh: f64, k: f64, draft: bool) -> Option<(u32, u32)> {
    let k = (k * bw / cw.max(1e-9)).max(k * bh / ch.max(1e-9));
    if !k.is_finite() || k <= 0.0 {
        return Some((16, 16));
    }
    let mut level = if k >= 1.0 { 0 } else { (1.0 / k).log2().floor() as i32 };
    if draft {
        level += 1;
    }
    let mut level = level.clamp(0, 8);
    while level > 0 && (sw / 2f64.powi(level) < 16.0 || sh / 2f64.powi(level) < 16.0) {
        level -= 1;
    }
    if level == 0 {
        None
    } else {
        let d = 2f64.powi(level);
        Some(((sw / d).ceil() as u32, (sh / d).ceil() as u32))
    }
}

/// Frame blending: the average of every source frame an output frame covers (clip playing faster
/// than 1×), premultiplied; `None` when blending does not apply.
fn blended_frame(cx: &Ctx, clip: &Clip, path: &Path, t_frame: f64, max: Option<(u32, u32)>, src_fps: f64) -> Result<Option<Picture>, RenderError> {
    let dt = frame_time(1, cx.p.settings.fps);
    let (Some(s0), Some(s1)) = (source_time(clip, t_frame), source_time(clip, t_frame + dt)) else { return Ok(None) };
    let fps = if src_fps > 0.0 { src_fps } else { 30.0 };
    let covered = (s1 - s0).abs() * fps;
    if !(covered > 1.05) {
        return Ok(None);
    }
    let key = format!("{}@{:.9}", clip.id, t_frame);
    if let Some(p) = crate::util::lock(&cx.blends).get(&key) {
        return Ok(Some(p.clone()));
    }
    let n = (covered.ceil() as usize).clamp(2, 16);
    let mut frames: Vec<(f64, aiframes_media::Frame)> = Vec::with_capacity(n);
    for j in 0..n {
        let s = s0 + (s1 - s0) * (j as f64 + 0.5) / n as f64;
        let f = cx.rd.pool().video_frame_with(path, s, max, cx.caching)?;
        match frames.iter_mut().find(|(_, g)| Arc::ptr_eq(&g.rgba, &f.rgba)) {
            Some((w, _)) => *w += 1.0,
            None => frames.push((1.0, f)),
        }
    }
    let (w, h) = (frames[0].1.width, frames[0].1.height);
    frames.retain(|(_, f)| f.width == w && f.height == h);
    let total: f64 = frames.iter().map(|(k, _)| k).sum();
    let mut out = vec![0u8; w as usize * h as usize * 4];
    {
        use rayon::prelude::*;
        let frames = &frames;
        out.par_chunks_mut(w as usize * 4).enumerate().for_each(|(y, row)| {
            let off = y * w as usize * 4;
            for x in 0..w as usize {
                let mut acc = [0f32; 4];
                for (k, f) in frames.iter() {
                    let p = &f.rgba[off + x * 4..off + x * 4 + 4];
                    let a = p[3] as f32 / 255.0;
                    let wgt = (*k / total) as f32;
                    acc[0] += p[0] as f32 * a * wgt;
                    acc[1] += p[1] as f32 * a * wgt;
                    acc[2] += p[2] as f32 * a * wgt;
                    acc[3] += p[3] as f32 * wgt;
                }
                for k in 0..4 {
                    row[x * 4 + k] = (acc[k] + 0.5).clamp(0.0, 255.0) as u8;
                }
            }
        });
    }
    let pic = Picture { w, h, data: Arc::new(out), premul: true };
    let mut g = crate::util::lock(&cx.blends);
    if g.len() > 8 {
        g.clear();
    }
    g.insert(key, pic.clone());
    Ok(Some(pic))
}

/// The clip at clip-local time `t_local`: its content and where it lands. `t_frame`: the local time
/// of the output frame (frame blending covers the whole frame whatever the sub-frame sample).
fn visual_at<'c>(cx: &Ctx, clip: &'c Clip, t_local: f64, t_frame: f64) -> Result<Option<Visual<'c>>, RenderError> {
    let opacity = visual_opacity(clip, t_local);
    if !(opacity > 0.0) {
        return Ok(None);
    }
    let warn = |e: RenderError, fatal: bool| if fatal { cx.problem(e) } else { cx.warn(e.to_string()); Ok(()) };
    let Some(b) = clip_box(cx.rd, cx.doc_dir, cx.tl, clip, t_local, &warn)? else { return Ok(None) };
    if matches!(b.kind, BoxKind::Adjustment) || !(b.bw > 0.0 && b.bh > 0.0) {
        return Ok(None);
    }
    let (fw, fh) = (cx.fw(), cx.fh());
    let shake = folded_shake(clip, t_local, fw, fh);
    let (to_proj, _) = Placement::of(clip, t_local, b.bw, b.bh, fw, fh, shake).proj(fw, fh);
    let to_out = to_proj.then(&Proj::from_affine(&cx.out_affine()));
    if !to_out.is_finite() || to_out.invert().is_none() {
        return Ok(None);
    }
    let k = to_out.max_scale(0.0, 0.0, b.bw, b.bh);
    let content = match b.kind {
        BoxKind::Media { path, kind, sw, sh, crop } => {
            let cw = sw * (1.0 - crop[0] - crop[2]);
            let chh = sh * (1.0 - crop[1] - crop[3]);
            let max = request_size(sw, sh, cw, chh, b.bw, b.bh, k, cx.draft);
            let got = match kind {
                MediaKind::Image => cx.rd.pool().image(&path, max).map(|f| Picture { w: f.width, h: f.height, data: f.rgba, premul: false }),
                _ => {
                    let blend = if clip.frame_blend {
                        let fps = cx.tl.assets.values().find(|a| cx.doc_dir.join(&a.path) == path).map(|a| a.info.fps).unwrap_or(30.0);
                        blended_frame(cx, clip, &path, t_frame, max, fps)
                    } else {
                        Ok(None)
                    };
                    match blend {
                        Ok(Some(p)) => Ok(p),
                        Ok(None) => {
                            // A reversed clip shows, during each output frame, the source frame under the
                            // middle of that frame (sampling at its start would land on the next frame up).
                            let at = if clip.reverse { t_local + 0.5 * frame_time(1, cx.p.settings.fps) } else { t_local };
                            let s = source_time(clip, at).unwrap_or(0.0);
                            cx.rd.pool().video_frame_with(&path, s, max, cx.caching).map(|f| Picture { w: f.width, h: f.height, data: f.rgba, premul: false })
                        }
                        Err(e) => Err(match e {
                            RenderError::Media(m) => m,
                            other => return Err(other),
                        }),
                    }
                }
            };
            let pic = match got {
                Ok(p) => p,
                Err(e) => {
                    let err = if path.exists() {
                        RenderError::Media(e)
                    } else {
                        RenderError::Missing(format!("media file {} (clip {}) is offline", path.display(), clip.id))
                    };
                    cx.problem(err)?;
                    offline_picture()
                }
            };
            // Far more source pixels than output pixels (the pool could not downscale): box-filter
            // halvings keep bilinear sampling from aliasing.
            let mut pic = pic;
            loop {
                let ratio = ((pic.w as f64 / sw) * (cw / b.bw) / k.max(1e-9)).max((pic.h as f64 / sh) * (chh / b.bh) / k.max(1e-9));
                if !(ratio > 2.5) || pic.w < 4 || pic.h < 4 {
                    break;
                }
                pic = pic.half();
            }
            let (pw, ph) = (pic.w as f64, pic.h as f64);
            let box_to_pic = Affine { a: cw / b.bw * pw / sw, b: 0.0, c: 0.0, d: chh / b.bh * ph / sh, e: crop[0] * pw, f: crop[1] * ph };
            let src = [crop[0] * pw, crop[1] * ph, (1.0 - crop[2]) * pw, (1.0 - crop[3]) * ph];
            VContent::Raster { pic, box_to_pic, src }
        }
        BoxKind::Solid { color } => {
            let pic = Picture::solid(color);
            VContent::Raster { pic, box_to_pic: Affine::scale(1.0 / b.bw, 1.0 / b.bh), src: [0.0, 0.0, 1.0, 1.0] }
        }
        BoxKind::Text { layout } => {
            let Source::Text(ts) = &clip.source else { return Ok(None) };
            let state = text::state(ts, &layout, t_local);
            if state.invisible() {
                return Ok(None);
            }
            VContent::Text { layout, state }
        }
        BoxKind::Shape => {
            let Source::Shape(sh) = &clip.source else { return Ok(None) };
            VContent::Shape { draw: sample(&sh.draw, t_local) }
        }
        BoxKind::Adjustment => return Ok(None),
    };
    Ok(Some(Visual { clip, t_local, to_out, bw: b.bw, bh: b.bh, opacity, content }))
}

impl Visual<'_> {
    fn raster(&self, opacity: f32, nearest: bool) -> Option<RasterDraw<'_>> {
        let VContent::Raster { pic, box_to_pic, src } = &self.content else { return None };
        let inv = self.to_out.invert()?.then(&Proj::from_affine(box_to_pic));
        Some(RasterDraw { pic, inv, src: *src, opacity, nearest })
    }

    /// A key that is equal for sub-frame samples showing the same content (same decoded frame,
    /// text state or shape draw-on).
    fn content_key(&self) -> u64 {
        let mut h = KeyHasher::new("content");
        match &self.content {
            VContent::Raster { pic, box_to_pic, src } => {
                h.u64(Arc::as_ptr(&pic.data) as usize as u64).u64(pic.w as u64);
                h.f64(box_to_pic.a).f64(box_to_pic.d).f64(box_to_pic.e).f64(box_to_pic.f);
                src.iter().for_each(|v| {
                    h.f64(*v);
                });
            }
            VContent::Text { state, .. } => state.hash_into(&mut h),
            VContent::Shape { draw } => {
                h.f64(*draw);
            }
        }
        h.f64(self.bw).f64(self.bh);
        h.finish()
    }

    /// The content as a picture in box space, with the box → picture map and source rectangle:
    /// media frames as they are; text and shapes rendered at `k` picture pixels per box pixel
    /// (cached), for projective drawing and motion-blur sampling.
    fn picture(&self, cx: &Ctx, k: f64) -> Option<(Picture, Affine, [f64; 4])> {
        match &self.content {
            VContent::Raster { pic, box_to_pic, src } => Some((pic.clone(), *box_to_pic, *src)),
            _ => {
                // Quantise the scale (quarter octaves) so animated cards reuse the cached render.
                let k = k.clamp(0.02, 64.0);
                let k = 2f64.powf((k.log2() * 4.0).ceil() / 4.0);
                let k = k.min(4096.0 / self.bw.max(1.0)).min(4096.0 / self.bh.max(1.0)).max(0.02);
                let margin = match &self.clip.source {
                    Source::Text(ts) => {
                        let st = ts.stroke.as_ref().map(|s| s.width.clamp(0.0, 500.0)).unwrap_or(0.0);
                        let sh = ts.shadow.as_ref().map(|s| s.x.abs().min(2000.0) + s.y.abs().min(2000.0) + 1.5 * s.blur.clamp(0.0, 1000.0)).unwrap_or(0.0);
                        (st + sh + 0.4 * ts.size.clamp(1.0, 2000.0)) * k + 4.0
                    }
                    Source::Shape(s) => (2.0 * s.stroke.as_ref().map(|s| s.width.clamp(0.0, 500.0)).unwrap_or(4.0) + 8.0) * k + 4.0,
                    _ => 4.0,
                }
                .ceil()
                .min(2048.0);
                let pw = (self.bw * k + 2.0 * margin).ceil().max(1.0) as u32;
                let ph = (self.bh * k + 2.0 * margin).ceil().max(1.0) as u32;
                let to_pic = Affine::scale(k, k).then(&Affine::translate(margin, margin));
                let mut h = KeyHasher::new("box-picture");
                h.u64(self.content_key()).f64(k).f64(margin);
                let key = match &self.clip.source {
                    Source::Text(ts) => h.json(ts).finish(),
                    Source::Shape(sh) => h.json(sh).finish(),
                    _ => h.finish(),
                };
                let pic = match cx.rd.picture_get(key) {
                    Some(p) => p,
                    None => {
                        let layer = match (&self.clip.source, &self.content) {
                            (Source::Text(ts), VContent::Text { layout, state }) => {
                                let td = text::TextDraw { ts, layout, state, to_out: to_pic, px_scale: k, canvas: (pw, ph) };
                                text::render(&td)
                            }
                            (Source::Shape(sh), VContent::Shape { draw }) => crate::shape::render(sh, *draw, to_pic, (pw, ph)),
                            _ => return None,
                        };
                        let p = layer.to_premul_picture();
                        cx.rd.picture_put(key, p.clone());
                        p
                    }
                };
                Some((pic, to_pic, [0.0, 0.0, pw as f64, ph as f64]))
            }
        }
    }
}

/// Text and shape layers, cached (static titles render once). Affine placements only.
fn vector_layer(cx: &Ctx, v: &Visual, m: Affine) -> Arc<Layer> {
    let canvas = (cx.out_w, cx.out_h);
    let mut h = KeyHasher::new("vector");
    h.f64(m.a).f64(m.b).f64(m.c).f64(m.d).f64(m.e).f64(m.f).u64(canvas.0 as u64).u64(canvas.1 as u64);
    match (&v.clip.source, &v.content) {
        (Source::Text(ts), VContent::Text { layout, state }) => {
            h.json(ts).u64(Arc::as_ptr(layout) as usize as u64);
            state.hash_into(&mut h);
            let key = h.finish();
            if let Some(l) = cx.rd.vector_get(key) {
                return l;
            }
            let td = text::TextDraw { ts, layout, state, to_out: m, px_scale: m.mean_scale(), canvas };
            let mut l = text::render(&td);
            l.compact();
            let l = Arc::new(l);
            cx.rd.vector_put(key, l.clone());
            l
        }
        (Source::Shape(sh), VContent::Shape { draw }) => {
            h.json(sh).f64(*draw);
            let key = h.finish();
            if let Some(l) = cx.rd.vector_get(key) {
                return l;
            }
            let mut l = crate::shape::render(sh, *draw, m, canvas);
            l.compact();
            let l = Arc::new(l);
            cx.rd.vector_put(key, l.clone());
            l
        }
        _ => Arc::new(Layer::empty(cx.out_w, cx.out_h)),
    }
}

/// Draws a visual directly onto the canvas (no layer).
fn draw_visual(cx: &Ctx, canvas: &mut Layer, v: &Visual, opacity: f32, mode: BlendMode) {
    match (&v.content, v.to_out.to_affine()) {
        (VContent::Raster { .. }, _) => {
            if let Some(d) = v.raster(opacity, cx.draft) {
                draw_raster(canvas, &d, mode);
            }
        }
        (_, Some(m)) => {
            let l = vector_layer(cx, v, m);
            blend_layer(canvas, &l, opacity, mode);
        }
        (_, None) => {
            let k = v.to_out.max_scale(0.0, 0.0, v.bw, v.bh);
            if let Some((pic, to_pic, src)) = v.picture(cx, k) {
                if let Some(inv) = v.to_out.invert() {
                    let d = RasterDraw { pic: &pic, inv: inv.then(&Proj::from_affine(&to_pic)), src, opacity, nearest: false };
                    draw_raster(canvas, &d, mode);
                }
            }
        }
    }
}

/// The clip's picture as a layer at full opacity (no effects), grown by `reach` for its effects.
fn visual_layer(cx: &Ctx, v: &Visual, reach: i32) -> Layer {
    let reach = reach.min(1 << 20);
    match (&v.content, v.to_out.to_affine()) {
        (VContent::Raster { .. }, _) => match v.raster(1.0, cx.draft) {
            Some(d) => {
                let mut l = Layer::new(cx.out_w, cx.out_h, d.bounds().expand(reach));
                draw_raster(&mut l, &d, BlendMode::Normal);
                l
            }
            None => Layer::empty(cx.out_w, cx.out_h),
        },
        (_, Some(m)) => {
            let mut l = vector_layer(cx, v, m).as_ref().clone();
            if reach > 0 {
                l.expand_to(l.rect.expand(reach));
            }
            l
        }
        (_, None) => {
            let mut l = Layer::empty(cx.out_w, cx.out_h);
            let k = v.to_out.max_scale(0.0, 0.0, v.bw, v.bh);
            if let (Some((pic, to_pic, src)), Some(inv)) = (v.picture(cx, k), v.to_out.invert()) {
                let d = RasterDraw { pic: &pic, inv: inv.then(&Proj::from_affine(&to_pic)), src, opacity: 1.0, nearest: false };
                l = Layer::new(cx.out_w, cx.out_h, d.bounds().expand(reach));
                draw_raster(&mut l, &d, BlendMode::Normal);
            }
            l
        }
    }
}

fn fx_ctx_run(cx: &Ctx, layer: &mut Layer, clip: &Clip, t_local: f64, skip_leading_shakes: bool) -> Result<(), RenderError> {
    let doc_dir = cx.doc_dir;
    let rd = cx.rd;
    let lut = move |path: &str| rd.lut(doc_dir, path);
    let strict = cx.strict;
    let problem = |m: String| -> Result<(), String> {
        if strict {
            Err(m)
        } else {
            cx.warn(format!("clip {}: {m}", clip.id));
            Ok(())
        }
    };
    let fx = FxCtx { t_local, frame: cx.frame, rect: cx.frame_rect(), px_scale: cx.px_scale(), draft: cx.draft, lut: &lut, problem: &problem };
    apply_effects(layer, &clip.effects, &fx, skip_leading_shakes).map_err(|m| RenderError::Missing(format!("clip {}: {m}", clip.id)))
}

/// Effects and opacity on a picture layer (the clip's effects at `t_local`).
fn finish_layer(cx: &Ctx, clip: &Clip, mut l: Layer, t_local: f64, opacity: f64) -> Result<Layer, RenderError> {
    if needs_layer(clip) {
        fx_ctx_run(cx, &mut l, clip, t_local, true)?;
    }
    l.scale_alpha(opacity as f32);
    Ok(l)
}


/// Largest movement (output pixels) of the box corners and centre between two placements.
fn displacement(a: &Proj, b: &Proj, bw: f64, bh: f64) -> f64 {
    let pts = [(0.0, 0.0), (bw, 0.0), (bw, bh), (0.0, bh), (bw / 2.0, bh / 2.0)];
    pts.iter()
        .map(|&(x, y)| match (a.apply(x, y), b.apply(x, y)) {
            (Some(p), Some(q)) => (p.0 - q.0).hypot(p.1 - q.1),
            _ => 1e6,
        })
        .fold(0.0, f64::max)
}

/// Whether any of the clip's effect parameters change between two local times.
fn effects_animate(clip: &Clip, t0: f64, t1: f64) -> bool {
    clip.effects.iter().filter(|e| e.enabled && !is_time_effect(&e.kind)).any(|e| {
        e.params.values().any(|p| (sample(p, t0) - sample(p, t1)).abs() > 1e-9) || (e.kind == "shake" && t0 != t1)
    })
}

/// The clip at local time `t` (after stutter), motion-blurred when it has motion blur and moves:
/// effects and opacity applied. `t_frame`: the output frame's local time.
fn clip_core(cx: &Ctx, clip: &Clip, t: f64, t_frame: f64) -> Result<Option<Layer>, RenderError> {
    let reach = if needs_layer(clip) { crate::effects::reach(&clip.effects, t, cx.px_scale()) } else { 0 };
    let mb = clip.motion_blur.as_ref().filter(|m| m.shutter > 0.0 && m.samples >= 2);
    let Some(mb) = mb else {
        let Some(v) = visual_at(cx, clip, t, t_frame)? else { return Ok(None) };
        if cx.prefetch {
            return Ok(None);
        }
        let l = visual_layer(cx, &v, reach);
        return finish_layer(cx, clip, l, t, v.opacity).map(Some);
    };
    let shutter = (mb.shutter.clamp(0.0, 720.0) / 360.0) * frame_time(1, cx.p.settings.fps);
    let max_n = if cx.draft { mb.samples.clamp(2, 6) } else { mb.samples.clamp(2, 64) } as usize;
    let sub = |j: usize, n: usize| t - shutter / 2.0 + shutter * (j as f64 + 0.5) / n as f64;
    // Adaptive: compare the ends of the shutter; a still clip renders once.
    let first = visual_at(cx, clip, sub(0, max_n), t_frame)?;
    let last = visual_at(cx, clip, sub(max_n - 1, max_n), t_frame)?;
    let mid = visual_at(cx, clip, t, t_frame)?;
    let still = match (&first, &last, &mid) {
        (Some(a), Some(b), Some(m)) => {
            displacement(&a.to_out, &b.to_out, a.bw, a.bh) < 0.5
                && (a.bw - b.bw).abs() < 1e-6
                && (a.opacity - b.opacity).abs() < 1.0 / 512.0
                && a.content_key() == b.content_key()
                && a.content_key() == m.content_key()
        }
        (None, None, None) => return Ok(None),
        _ => false,
    };
    if cx.prefetch {
        return Ok(None);
    }
    if still {
        let Some(v) = mid else { return Ok(None) };
        let l = visual_layer(cx, &v, reach);
        return finish_layer(cx, clip, l, t, v.opacity).map(Some);
    }
    let disp = match (&first, &last) {
        (Some(a), Some(b)) => displacement(&a.to_out, &b.to_out, a.bw, a.bh),
        _ => 1e6,
    };
    // Heavy smears are sampled at lower resolution (their detail is gone anyway).
    let (div, n) = crate::canvas::smear_plan(disp, max_n, cx.draft);
    let mut states: Vec<Visual> = Vec::with_capacity(n);
    for j in 0..n {
        if let Some(v) = visual_at(cx, clip, sub(j, n), t_frame)? {
            states.push(v);
        }
    }
    if states.is_empty() {
        return Ok(None);
    }
    let canvas = IRect::of_size(cx.out_w, cx.out_h);
    // Effects that animate within the shutter are rendered per sample; otherwise the effect stack
    // runs once on the blurred picture (indistinguishable, and n times cheaper).
    if effects_animate(clip, sub(0, n), sub(n - 1, n)) {
        let mut layers = Vec::with_capacity(states.len());
        for v in &states {
            let l = visual_layer(cx, v, crate::effects::reach(&clip.effects, v.t_local, cx.px_scale()));
            layers.push(finish_layer(cx, clip, l, v.t_local, v.opacity)?);
        }
        let w = 1.0 / n as f32;
        let items: Vec<AccItem> = layers.iter().map(|l| AccItem { src: AccSource::Layer { layer: l, mirror: None }, inv: Proj::IDENTITY, weight: w, shade: 1.0 }).collect();
        return Ok(Some(accumulate(cx.out_w, cx.out_h, &items, canvas)));
    }
    let op_ref = states.iter().map(|v| v.opacity).sum::<f64>() / n as f64;
    let mut pics: Vec<(u64, (Picture, Affine, [f64; 4]))> = Vec::new();
    let mut items_src: Vec<(usize, Proj, f32)> = Vec::with_capacity(states.len());
    for v in &states {
        let key = v.content_key();
        let idx = match pics.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => {
                let k = v.to_out.max_scale(0.0, 0.0, v.bw, v.bh);
                let Some(p) = v.picture(cx, k) else { continue };
                pics.push((key, p));
                pics.len() - 1
            }
        };
        let Some(inv) = v.to_out.invert() else { continue };
        let inv = inv.then(&Proj::from_affine(&pics[idx].1 .1));
        items_src.push((idx, inv, (v.opacity / op_ref.max(1e-9) / n as f64) as f32));
    }
    let items: Vec<AccItem> = items_src
        .iter()
        .map(|(i, inv, w)| AccItem { src: AccSource::Pic { pic: &pics[*i].1 .0, src: pics[*i].1 .2 }, inv: *inv, weight: *w, shade: 1.0 })
        .collect();
    let mut l = accumulate_res(cx.out_w, cx.out_h, items, canvas, div);
    if reach > 0 {
        let r = l.rect.expand(reach.min(1 << 20));
        l.expand_to(r);
    }
    finish_layer(cx, clip, l, t, op_ref).map(Some)
}

/// The clip as drawn at the output frame: stutter, motion blur, effects, echoes, matte; opacity
/// baked in. `None` when nothing is visible.
fn clip_full(cx: &Ctx, clip: &Clip, span: Span, depth: u32) -> Result<Option<Layer>, RenderError> {
    let t_frame = local_time(cx.t, &span, cx.tl.tol);
    let t = stutter_time(clip, t_frame);
    let mut out = clip_core(cx, clip, t, t_frame)?;
    // Echo: the clip as it was k × interval earlier at decay^k, under itself (cut-outs, text) or
    // blended over it (footage that fills the frame, which would hide echoes behind it).
    if let Some(e) = clip.effects.iter().find(|e| e.enabled && e.kind == "echo") {
        use aiframes_core::effects::effect_param;
        let count = effect_param(e, "count", t).round().clamp(1.0, 32.0) as usize;
        let interval = effect_param(e, "interval", t).clamp(0.001, 60.0);
        let decay = effect_param(e, "decay", t).clamp(0.0, 1.0);
        let blend = match effect_param(e, "mode", t).round() as i64 {
            1 => false,
            2 => true,
            _ => out.as_ref().is_some_and(|o| fills_frame(cx, o)),
        };
        let mut echoes: Vec<(Layer, f64)> = Vec::new();
        for k in (1..=count).rev() {
            let te = t - k as f64 * interval;
            let w = decay.powi(k as i32);
            if te < -cx.tl.tol || w < 1.0 / 512.0 {
                continue;
            }
            if let Some(l) = clip_core(cx, clip, te.max(0.0), t_frame - k as f64 * interval)? {
                echoes.push((l, w));
            }
        }
        if blend {
            // Weighted average of the clip (weight 1) and its echoes (decay^k): ghost trails.
            let mut acc = out.take();
            let mut total = if acc.is_some() { 1.0 } else { 0.0 };
            for (l, w) in echoes.into_iter().rev() {
                total += w;
                acc = Some(match acc {
                    Some(a) => mix_layers(&a, &l, (w / total) as f32),
                    None => l,
                });
            }
            out = acc;
        } else {
            let mut acc: Option<Layer> = None;
            for (mut l, w) in echoes {
                l.scale_alpha(w as f32);
                acc = Some(match acc {
                    Some(a) => over_union(a, &l),
                    None => l,
                });
            }
            if let Some(a) = acc {
                out = Some(match out {
                    Some(o) => over_union(a, &o),
                    None => a,
                });
            }
        }
    }
    let Some(mut out) = out else { return Ok(None) };
    if let Some(m) = &clip.matte {
        apply_matte(cx, &mut out, m, depth)?;
    }
    Ok(Some(out))
}

/// Whether a layer is opaque over the whole project frame (footage filling it): sampled on a grid
/// just inside the frame's edges.
fn fills_frame(cx: &Ctx, l: &Layer) -> bool {
    let [fx, fy, fw, fh] = cx.frame_rect();
    (0..5).all(|j| {
        (0..5).all(|i| {
            let x = fx + fw * (0.02 + 0.96 * i as f64 / 4.0);
            let y = fy + fh * (0.02 + 0.96 * j as f64 / 4.0);
            l.get(x as i32, y as i32)[3] >= 0.98
        })
    })
}

/// Multiplies the clip's pixels by its track matte (rendered at the same timeline time).
fn apply_matte(cx: &Ctx, layer: &mut Layer, m: &aiframes_core::Matte, depth: u32) -> Result<(), RenderError> {
    use aiframes_core::MatteMode;
    let src = cx.tl.by_id.get(m.clip.as_str()).copied().filter(|(c, s)| c.enabled && depth < 4 && covers(s, cx.t, cx.tl.tol));
    let matte = match src {
        Some((c, s)) => clip_full(cx, c, s, depth + 1)?,
        None => None,
    };
    let matte = matte.unwrap_or_else(|| Layer::empty(cx.out_w, cx.out_h));
    let inverted = matches!(m.mode, MatteMode::AlphaInverted | MatteMode::LumaInverted);
    if !inverted {
        // Outside the matte nothing shows: keep only the overlap.
        let keep = layer.rect.intersect(&matte.rect);
        if keep != layer.rect {
            let mut cut = Layer::new(layer.cw, layer.ch, keep);
            blend_layer(&mut cut, layer, 1.0, BlendMode::Normal);
            *layer = cut;
        }
    }
    let mode = m.mode;
    let r = layer.rect;
    let w = layer.stride();
    use rayon::prelude::*;
    layer.px.par_chunks_mut(w.max(1)).enumerate().for_each(|(row, line)| {
        let y = r.y + row as i32;
        for (i, o) in line.iter_mut().enumerate() {
            let q = matte.get(r.x + i as i32, y);
            let luma = (0.2126 * q[0] + 0.7152 * q[1] + 0.0722 * q[2]).clamp(0.0, 1.0);
            let f = match mode {
                MatteMode::Alpha => q[3].clamp(0.0, 1.0),
                MatteMode::AlphaInverted => 1.0 - q[3].clamp(0.0, 1.0),
                MatteMode::Luma => luma,
                MatteMode::LumaInverted => 1.0 - luma,
            };
            if f < 1.0 {
                for v in o.iter_mut() {
                    *v *= f;
                }
            }
        }
    });
    Ok(())
}

/// The clip fully processed (effects applied, opacity baked in), for transitions.
fn clip_layer(cx: &Ctx, clip: &Clip, span: Span) -> Result<Option<Layer>, RenderError> {
    if cx.prefetch {
        prefetch_clip(cx, clip, span)?;
        return Ok(None);
    }
    clip_full(cx, clip, span, 0)
}

/// Fetches the media a clip needs at this frame (export read-ahead).
fn prefetch_clip(cx: &Ctx, clip: &Clip, span: Span) -> Result<(), RenderError> {
    let t_frame = local_time(cx.t, &span, cx.tl.tol);
    let t = stutter_time(clip, t_frame);
    let _ = visual_at(cx, clip, t, t_frame)?;
    Ok(())
}

fn draw_clip(cx: &Ctx, canvas: &mut Layer, clip: &Clip, span: Span) -> Result<(), RenderError> {
    if cx.prefetch {
        return prefetch_clip(cx, clip, span);
    }
    if plain(clip) {
        let t_frame = local_time(cx.t, &span, cx.tl.tol);
        let t = stutter_time(clip, t_frame);
        if let Some(v) = visual_at(cx, clip, t, t_frame)? {
            draw_visual(cx, canvas, &v, v.opacity as f32, clip.blend);
        }
        return Ok(());
    }
    if let Some(l) = clip_full(cx, clip, span, 0)? {
        blend_layer(canvas, &l, 1.0, clip.blend);
    }
    Ok(())
}

fn draw_pair(cx: &Ctx, canvas: &mut Layer, info: &TransInfo, a: (&Clip, Span), b: (&Clip, Span), p: f64) -> Result<(), RenderError> {
    let la = clip_layer(cx, a.0, a.1)?;
    let lb = clip_layer(cx, b.0, b.1)?;
    if cx.prefetch {
        return Ok(());
    }
    let mut la = la.unwrap_or_else(|| Layer::empty(cx.out_w, cx.out_h));
    let mut lb = lb.unwrap_or_else(|| Layer::empty(cx.out_w, cx.out_h));
    let tr = info.tr;
    let tcx = TCtx {
        sx: cx.sx,
        sy: cx.sy,
        draft: cx.draft,
        frame: cx.frame_rect(),
        pw: cx.fw(),
        ph: cx.fh(),
        frame_index: cx.frame,
        frame_p: frame_time(1, cx.p.settings.fps) / (2.0 * info.half).max(1e-9),
        seed: crate::util::hash64(tr.id.bytes().fold(0u64, |h, b| h.wrapping_mul(131).wrapping_add(b as u64))),
    };
    if let Some(out) = transition::motion(tr, p, &la, &lb, &tcx) {
        let mode = if p < 0.5 { a.0.blend } else { b.0.blend };
        blend_layer(canvas, &out, 1.0, mode);
        return Ok(());
    }
    if transition::is_geometric(&tr.kind) {
        let (ga, gb) = transition::geometry(tr, p, cx.fw(), cx.fh());
        let out = cx.out_affine();
        let back = out.invert().unwrap_or(Affine::IDENTITY);
        let to_out = |g: &Affine| back.then(g).then(&out);
        if ga != Affine::IDENTITY {
            la = transform_layer(&la, &to_out(&ga));
        }
        if gb != Affine::IDENTITY {
            lb = transform_layer(&lb, &to_out(&gb));
        }
    }
    let out = transition::combine(tr, p, la, lb, &tcx);
    let mode = if p < 0.5 { a.0.blend } else { b.0.blend };
    blend_layer(canvas, &out, 1.0, mode);
    Ok(())
}

fn draw_adjustment(cx: &Ctx, canvas: &mut Layer, clip: &Clip, span: Span) -> Result<(), RenderError> {
    let t_local = local_time(cx.t, &span, cx.tl.tol);
    let mix = visual_opacity(clip, t_local) as f32;
    if !(mix > 0.0) || !clip.effects.iter().any(|e| e.enabled) || cx.prefetch {
        return Ok(());
    }
    let is_mask = |k: &str| matches!(k, "mask_rect" | "mask_ellipse");
    let (masks, rest): (Vec<_>, Vec<_>) = clip.effects.iter().filter(|e| e.enabled).cloned().partition(|e| is_mask(&e.kind));
    let mut copy = canvas.clone();
    let mut c2 = clip.clone();
    c2.effects = rest;
    fx_ctx_run(cx, &mut copy, &c2, t_local, false)?;
    // Masks limit where the adjustment applies.
    let coverage = if masks.is_empty() {
        None
    } else {
        let mut m = Layer::full(cx.out_w, cx.out_h, [1.0; 4]);
        c2.effects = masks;
        fx_ctx_run(cx, &mut m, &c2, t_local, false)?;
        Some(m)
    };
    use rayon::prelude::*;
    let w = canvas.stride();
    let r = canvas.rect;
    canvas.px.par_chunks_mut(w.max(1)).enumerate().for_each(|(row, line)| {
        let y = r.y + row as i32;
        for (i, o) in line.iter_mut().enumerate() {
            let x = r.x + i as i32;
            let k = mix * coverage.as_ref().map(|m| m.get(x, y)[3]).unwrap_or(1.0);
            if k <= 0.0 {
                continue;
            }
            let c = copy.get(x, y);
            for j in 0..4 {
                o[j] += (c[j] - o[j]) * k;
            }
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Frame
// ---------------------------------------------------------------------------------------------

pub(crate) fn output_size(s: &Settings, scale: f32) -> Result<(u32, u32), RenderError> {
    let scale = if scale.is_finite() && scale > 0.0 { scale.min(8.0) } else { 1.0 };
    let w = ((s.width.max(1) as f32 * scale).round() as u32).max(1);
    let h = ((s.height.max(1) as f32 * scale).round() as u32).max(1);
    if w > 16384 || h > 16384 {
        return Err(RenderError::Other(format!("frame size {w}x{h} is too large (max 16384 per side); lower the scale")));
    }
    Ok((w, h))
}

/// Walks the tracks at `cx.t`.
/// The bottom-most clip drawn straight over the background colour (no separate fill pass) when
/// it is a plain picture: no layer effects, normal blend. Returns false if nothing was written.
fn draw_first(cx: &Ctx, canvas: &mut Layer, clip: &Clip, span: Span, bg: Px) -> Result<bool, RenderError> {
    if clip.blend != BlendMode::Normal || !plain(clip) || !matches!(clip.source, Source::Media { .. } | Source::Solid { .. }) {
        return Ok(false);
    }
    let t_frame = local_time(cx.t, &span, cx.tl.tol);
    let Some(v) = visual_at(cx, clip, stutter_time(clip, t_frame), t_frame)? else { return Ok(false) };
    let Some(d) = v.raster(v.opacity as f32, cx.draft) else { return Ok(false) };
    draw_raster_over(canvas, &d, bg);
    Ok(true)
}

/// Walks the tracks at `cx.t`. `fill`: the canvas is not initialised yet; it must end up with this
/// background colour under everything.
fn composite(cx: &Ctx, mut canvas: Option<&mut Layer>, mut fill: Option<Px>) -> Result<(), RenderError> {
    for (ti, info) in cx.tl.tracks.iter().enumerate() {
        if !cx.tl.shows_picture(info.track) {
            continue;
        }
        for act in cx.tl.active_at(ti, cx.t) {
            // Track-matte sources are only drawn through the clips that use them.
            if let Active::One(c, _) = act {
                if cx.tl.matte_sources.contains(&c.id) {
                    continue;
                }
            }
            if let (Some(bg), Some(cv)) = (fill, canvas.as_deref_mut()) {
                fill = None;
                if let Active::One(c, s) = act {
                    if draw_first(cx, cv, c, s, bg)? {
                        continue;
                    }
                }
                cv.fill(bg);
            }
            match (act, canvas.as_deref_mut()) {
                (Active::One(c, s), Some(cv)) if matches!(c.source, Source::Adjustment) => draw_adjustment(cx, cv, c, s)?,
                (Active::One(c, _), None) if matches!(c.source, Source::Adjustment) => {}
                (Active::One(c, s), Some(cv)) => draw_clip(cx, cv, c, s)?,
                (Active::One(c, s), None) => prefetch_clip(cx, c, s)?,
                (Active::Pair { info, a, b, p }, Some(cv)) => draw_pair(cx, cv, info, a, b, p)?,
                (Active::Pair { a, b, .. }, None) => {
                    prefetch_clip(cx, a.0, a.1)?;
                    prefetch_clip(cx, b.0, b.1)?;
                }
            }
        }
    }
    if let (Some(bg), Some(cv)) = (fill, canvas) {
        cv.fill(bg);
    }
    Ok(())
}

pub(crate) struct FrameJob<'a> {
    pub rd: &'a Renderer,
    pub doc_dir: &'a Path,
    pub p: &'a Project,
    pub tl: &'a Timeline<'a>,
    pub strict: bool,
    /// How decoded media frames are cached (export: `LowPriority`, so it cannot evict playback frames).
    pub caching: FrameCaching,
}

/// Where the project frame lands in a rendered image: the image is `out_w`×`out_h`; compositing
/// happens on a `cw`×`ch` canvas placed at (`cx0`, `cy0`) in it (the visible part of the frame); the
/// rest of the image is the background colour (letterbox/pillarbox bars).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct OutputView {
    pub out_w: u32,
    pub out_h: u32,
    pub cx0: u32,
    pub cy0: u32,
    pub cw: u32,
    pub ch: u32,
    /// Canvas pixels per project pixel.
    pub sx: f64,
    pub sy: f64,
    /// Canvas position of the frame's top-left corner.
    pub ox: f64,
    pub oy: f64,
}

impl OutputView {
    /// The project frame scaled by `scale` (previews, snapshots).
    pub fn scaled(s: &Settings, scale: f32) -> Result<OutputView, RenderError> {
        let (w, h) = output_size(s, scale)?;
        Ok(OutputView {
            out_w: w,
            out_h: h,
            cx0: 0,
            cy0: 0,
            cw: w,
            ch: h,
            sx: w as f64 / s.width.max(1) as f64,
            sy: h as f64 / s.height.max(1) as f64,
            ox: 0.0,
            oy: 0.0,
        })
    }

    /// The project frame mapped onto a `w`×`h` output with `fit` (export output size): contain =
    /// letterbox/pillarbox, cover = fill and centre-crop, stretch = distort, none = 1:1 centred.
    /// Frame edges are whole pixels so bars are exactly the background colour.
    pub fn fitted(s: &Settings, w: u32, h: u32, fit: Fit) -> Result<OutputView, RenderError> {
        if w == 0 || h == 0 || w > 16384 || h > 16384 {
            return Err(RenderError::Other(format!("output size {w}x{h} is out of range (1..16384 per side)")));
        }
        let (pw, ph) = (s.width.max(1) as f64, s.height.max(1) as f64);
        let (wf, hf) = (w as f64, h as f64);
        let (fw, fh) = match fit {
            Fit::Contain => {
                let k = (wf / pw).min(hf / ph);
                ((pw * k).round().clamp(1.0, wf), (ph * k).round().clamp(1.0, hf))
            }
            Fit::Cover => {
                let k = (wf / pw).max(hf / ph);
                ((pw * k).round().max(wf), (ph * k).round().max(hf))
            }
            Fit::Stretch => (wf, hf),
            Fit::None => (pw, ph),
        };
        let fx = ((wf - fw) / 2.0).round();
        let fy = ((hf - fh) / 2.0).round();
        // The visible part of the frame.
        let x0 = fx.max(0.0);
        let y0 = fy.max(0.0);
        let x1 = (fx + fw).min(wf);
        let y1 = (fy + fh).min(hf);
        if x1 <= x0 || y1 <= y0 {
            return Err(RenderError::Other(format!("the project frame does not overlap a {w}x{h} output")));
        }
        Ok(OutputView {
            out_w: w,
            out_h: h,
            cx0: x0 as u32,
            cy0: y0 as u32,
            cw: (x1 - x0) as u32,
            ch: (y1 - y0) as u32,
            sx: fw / pw,
            sy: fh / ph,
            ox: fx - x0,
            oy: fy - y0,
        })
    }
}

impl FrameJob<'_> {
    fn ctx(&self, t: f64, v: &OutputView, draft: bool, prefetch: bool) -> Ctx<'_> {
        let s = &self.p.settings;
        let frame = frame_index(t, s.fps);
        Ctx {
            rd: self.rd,
            doc_dir: self.doc_dir,
            p: self.p,
            tl: self.tl,
            t: frame_time(frame, s.fps),
            frame,
            out_w: v.cw,
            out_h: v.ch,
            sx: v.sx,
            sy: v.sy,
            ox: v.ox,
            oy: v.oy,
            draft,
            strict: self.strict,
            caching: self.caching,
            prefetch,
            warnings: Mutex::new(self.tl.problems.clone()),
            blends: Mutex::new(HashMap::new()),
        }
    }

    /// Renders the frame containing time `t`: straight RGBA8 plus warnings.
    pub fn render(&self, t: f64, o: &FrameOptions) -> Result<(Image, Vec<String>), RenderError> {
        let v = OutputView::scaled(&self.p.settings, o.scale)?;
        let (mut img, warnings) = self.render_view(t, &v, o.draft)?;
        if o.guides {
            crate::sheet::draw_guides(&mut img.rgba, img.width, img.height, &self.p.settings);
        }
        Ok((img, warnings))
    }

    /// Renders the frame containing time `t` through an output view (export sizes).
    pub fn render_view(&self, t: f64, v: &OutputView, draft: bool) -> Result<(Image, Vec<String>), RenderError> {
        let cx = self.ctx(t, v, draft, false);
        let bg = self.p.settings.background;
        // The canvas is initialised by the first layer (drawn over the background) or a fill.
        let mut canvas = Layer::dirty(cx.out_w, cx.out_h);
        composite(&cx, Some(&mut canvas), Some(premul(bg)))?;
        let rgba = canvas.to_rgba8();
        drop(canvas);
        let warnings = cx.warnings.into_inner().unwrap_or_else(|p| p.into_inner());
        if (v.cw, v.ch) == (v.out_w, v.out_h) {
            return Ok((Image { width: v.out_w, height: v.out_h, rgba }, warnings));
        }
        // Bars in the background colour around the visible frame.
        let mut out = vec![0u8; v.out_w as usize * v.out_h as usize * 4];
        let fill = bg.to_rgba8();
        let (ow, cw) = (v.out_w as usize, v.cw as usize);
        use rayon::prelude::*;
        out.par_chunks_mut(ow * 4).enumerate().for_each(|(y, row)| {
            for px in row.as_chunks_mut::<4>().0 {
                *px = fill;
            }
            let y = y as u32;
            if y >= v.cy0 && y < v.cy0 + v.ch {
                let sy = (y - v.cy0) as usize;
                let x0 = v.cx0 as usize * 4;
                row[x0..x0 + cw * 4].copy_from_slice(&rgba[sy * cw * 4..(sy + 1) * cw * 4]);
            }
        });
        Ok((Image { width: v.out_w, height: v.out_h, rgba: out }, warnings))
    }

    /// Fetches (decodes) the media the frame at `t` needs, without compositing.
    pub fn prefetch(&self, t: f64, v: &OutputView, draft: bool) {
        let cx = self.ctx(t, v, draft, true);
        let _ = composite(&cx, None, None);
    }
}

// ---------------------------------------------------------------------------------------------
// layers_at
// ---------------------------------------------------------------------------------------------

pub(crate) fn layers_at(rd: &Renderer, doc_dir: &Path, p: &Project, r: &Resolved, t: f64) -> Vec<LayerBox> {
    let tl = Timeline::new(p, r);
    let s = &p.settings;
    let t = frame_time(frame_index(t, s.fps), s.fps);
    let (fw, fh) = (s.width.max(1) as f64, s.height.max(1) as f64);
    let quiet = |_: RenderError, _: bool| Ok(());
    let mut out = Vec::new();
    let mut push = |clip: &Clip, span: Span, extra: Proj| {
        if tl.matte_sources.contains(&clip.id) {
            return;
        }
        let t_local = stutter_time(clip, local_time(t, &span, tl.tol));
        if !(visual_opacity(clip, t_local) > 0.0) {
            return;
        }
        let Ok(Some(b)) = clip_box(rd, doc_dir, &tl, clip, t_local, &quiet) else { return };
        if matches!(b.kind, BoxKind::Adjustment) {
            return;
        }
        if let (BoxKind::Text { layout }, Source::Text(ts)) = (&b.kind, &clip.source) {
            if text::state(ts, layout, t_local).invisible() {
                return;
            }
        }
        let shake = folded_shake(clip, t_local, fw, fh);
        let pl = Placement::of(clip, t_local, b.bw, b.bh, fw, fh, shake);
        let (_, m) = pl.proj(fw, fh);
        let m = m.then(&extra);
        let Some(pv) = extra.apply(pl.px, pl.py) else { return };
        // Corners in visual order (top-left, top-right, bottom-right, bottom-left of the card as
        // projected); a card partly behind the camera reports its visible extent.
        let raw = [(0.0, 0.0), (b.bw, 0.0), (b.bw, b.bh), (0.0, b.bh)];
        let pts: Vec<(f64, f64)> = raw.iter().filter_map(|&(x, y)| m.apply(x, y)).collect();
        if pts.len() < 4 {
            return;
        }
        let bb = pts.iter().fold([f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY], |b, &(x, y)| [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)]);
        out.push(LayerBox {
            clip: clip.id.clone(),
            corners: [0, 1, 2, 3].map(|i| [pts[i].0 as f32, pts[i].1 as f32]),
            bounds: [bb[0] as f32, bb[1] as f32, (bb[2] - bb[0]) as f32, (bb[3] - bb[1]) as f32],
            pivot: [pv.0 as f32, pv.1 as f32],
        });
    };
    for (ti, info) in tl.tracks.iter().enumerate() {
        if !tl.shows_picture(info.track) {
            continue;
        }
        for act in tl.active_at(ti, t) {
            match act {
                Active::One(c, sp) => push(c, sp, Proj::IDENTITY),
                Active::Pair { info, a, b, p } => {
                    if let Some((fa, fb)) = transition::face_projs(info.tr, p, fw, fh) {
                        if let Some(g) = fa {
                            push(a.0, a.1, g);
                        }
                        if let Some(g) = fb {
                            push(b.0, b.1, g);
                        }
                        continue;
                    }
                    let (ga, gb) = if transition::is_geometric(&info.tr.kind) {
                        transition::geometry(info.tr, p, fw, fh)
                    } else {
                        (Affine::IDENTITY, Affine::IDENTITY)
                    };
                    push(a.0, a.1, Proj::from_affine(&ga));
                    push(b.0, b.1, Proj::from_affine(&gb));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(w: u32, h: u32) -> Settings {
        Settings { width: w, height: h, ..Settings::default() }
    }

    #[test]
    fn output_views() {
        let s = settings(1920, 1080);
        let v = OutputView::fitted(&s, 3840, 2160, Fit::Contain).unwrap();
        assert_eq!((v.cw, v.ch, v.cx0, v.cy0, v.sx, v.ox), (3840, 2160, 0, 0, 2.0, 0.0));
        // Letterbox: 1920x1080 into 1080x1920 → a 1080x608 frame centred vertically.
        let v = OutputView::fitted(&s, 1080, 1920, Fit::Contain).unwrap();
        assert_eq!((v.cw, v.ch, v.cx0, v.cy0), (1080, 608, 0, 656));
        // Cover: the frame is 3413x1920, the canvas is the output, the frame starts 1167 px left.
        let v = OutputView::fitted(&s, 1080, 1920, Fit::Cover).unwrap();
        assert_eq!((v.cw, v.ch, v.cx0, v.cy0), (1080, 1920, 0, 0));
        assert!((v.sx - 3413.0 / 1920.0).abs() < 1e-9 && v.ox == -1167.0 && v.oy == 0.0);
        let v = OutputView::fitted(&s, 1000, 1000, Fit::Stretch).unwrap();
        assert!((v.sx - 1000.0 / 1920.0).abs() < 1e-12 && (v.sy - 1000.0 / 1080.0).abs() < 1e-12);
        let v = OutputView::fitted(&s, 960, 540, Fit::None).unwrap();
        assert_eq!((v.cw, v.ch, v.sx, v.ox, v.oy), (960, 540, 1.0, -480.0, -270.0));
        let v = OutputView::fitted(&s, 3000, 2000, Fit::None).unwrap();
        assert_eq!((v.cw, v.ch, v.cx0, v.cy0), (1920, 1080, 540, 460));
        assert!(OutputView::fitted(&s, 0, 100, Fit::Contain).is_err());
    }
}
