//! Safe-area guides, labels, contact sheets and PNG encoding.

use crate::canvas::{unpremul8, Layer};
use crate::geom::Affine;
use crate::text;
use crate::{FrameOptions, Image, RenderError, Renderer};
use aiframes_core::{Color, Project, Resolved, Settings, TextSource};
use rayon::prelude::*;
use std::path::Path;

fn blend8(px: &mut [u8], c: [u8; 4]) {
    let a = c[3] as u32;
    for k in 0..3 {
        px[k] = ((px[k] as u32 * (255 - a) + c[k] as u32 * a + 127) / 255) as u8;
    }
    px[3] = px[3].max(c[3]);
}

fn hline(rgba: &mut [u8], w: u32, h: u32, x0: i64, x1: i64, y: i64, th: i64, c: [u8; 4]) {
    for yy in y..y + th {
        if yy < 0 || yy >= h as i64 {
            continue;
        }
        for x in x0.max(0)..x1.min(w as i64) {
            let i = ((yy as usize) * w as usize + x as usize) * 4;
            blend8(&mut rgba[i..i + 4], c);
        }
    }
}

fn vline(rgba: &mut [u8], w: u32, h: u32, x: i64, y0: i64, y1: i64, th: i64, c: [u8; 4]) {
    for yy in y0.max(0)..y1.min(h as i64) {
        for x in x..x + th {
            if x < 0 || x >= w as i64 {
                continue;
            }
            let i = ((yy as usize) * w as usize + x as usize) * 4;
            blend8(&mut rgba[i..i + 4], c);
        }
    }
}

/// Title-safe (yellow) and action-safe (white) rectangles plus a centre cross, drawn onto straight
/// RGBA8 output.
pub(crate) fn draw_guides(rgba: &mut [u8], w: u32, h: u32, s: &Settings) {
    let th = ((w.min(h) as f64 / 540.0).round() as i64).max(1);
    for (frac, c) in [(s.action_safe, [255, 255, 255, 150]), (s.title_safe, [255, 210, 0, 190])] {
        let f = if frac.is_finite() { frac.clamp(0.05, 1.0) } else { continue };
        let rw = (w as f64 * f).round() as i64;
        let rh = (h as f64 * f).round() as i64;
        let x0 = (w as i64 - rw) / 2;
        let y0 = (h as i64 - rh) / 2;
        hline(rgba, w, h, x0, x0 + rw, y0, th, c);
        hline(rgba, w, h, x0, x0 + rw, y0 + rh - th, th, c);
        vline(rgba, w, h, x0, y0, y0 + rh, th, c);
        vline(rgba, w, h, x0 + rw - th, y0, y0 + rh, th, c);
    }
    let arm = (w.min(h) as i64 / 40).max(4);
    let (cx, cy) = (w as i64 / 2, h as i64 / 2);
    let c = [255, 255, 255, 170];
    hline(rgba, w, h, cx - arm, cx + arm, cy - th / 2, th, c);
    vline(rgba, w, h, cx - th / 2, cy - arm, cy + arm, th, c);
}

/// A centred one-line label of `size` px in a transparent `w`×`h` layer.
pub(crate) fn label_layer(label: &str, size: f64, color: Color, w: u32, h: u32) -> Layer {
    let layout = text::simple_layout(label, size, 600);
    let ts = TextSource { text: label.to_string(), size, weight: 600, color, ..TextSource::default() };
    let (bw, bh, _) = text::content_size(&ts, &layout);
    let state = text::state(&ts, &layout, 0.0);
    let to_out = Affine::translate(((w as f64 - bw) / 2.0).round(), ((h as f64 - bh) / 2.0).round());
    let td = text::TextDraw { ts: &ts, layout: &layout, state: &state, to_out, px_scale: 1.0, canvas: (w, h) };
    text::render(&td)
}

pub(crate) fn contact_sheet(r: &Renderer, doc_dir: &Path, p: &Project, res: &Resolved, times: &[f64], cols: u32, thumb_w: u32) -> Result<Image, RenderError> {
    let s = &p.settings;
    let n = times.len();
    let thumb_w = thumb_w.clamp(16, 4096);
    let scale = thumb_w as f32 / s.width.max(1) as f32;
    let o = FrameOptions { scale, guides: false, draft: false };
    let (tw, th) = crate::compose::output_size(s, scale)?;
    if n == 0 {
        return Ok(Image::new(tw, 1));
    }
    let cols = (cols.max(1) as usize).min(n);
    let rows = n.div_ceil(cols);
    let label_h = ((th as f64 * 0.14).round() as u32).clamp(14, 48);
    let gap = 4u32;
    let w = cols as u32 * tw + (cols as u32 + 1) * gap;
    let h = rows as u32 * (th + label_h) + (rows as u32 + 1) * gap;
    if (w as u64) * (h as u64) > 16384 * 16384 {
        return Err(RenderError::Other(format!("contact sheet of {w}x{h} pixels is too large; use fewer times or a smaller thumb_w")));
    }
    let frames: Vec<Result<Image, RenderError>> = times.par_iter().map(|&t| r.frame(doc_dir, p, res, t, &o)).collect();
    let mut sheet = Image::new(w, h);
    for px in sheet.rgba.as_chunks_mut::<4>().0 {
        *px = [24, 24, 28, 255];
    }
    let labels: Vec<Layer> = times
        .par_iter()
        .map(|&t| label_layer(&crate::util::timecode(t, s.fps), label_h as f64 * 0.72, Color::rgba8(235, 235, 235, 255), tw, label_h))
        .collect();
    for (i, f) in frames.into_iter().enumerate() {
        let img = f?;
        let (c, rr) = ((i % cols) as u32, (i / cols) as u32);
        let x0 = gap + c * (tw + gap);
        let y0 = gap + rr * (th + label_h + gap);
        for y in 0..img.height.min(th) {
            let src = &img.rgba[(y * img.width) as usize * 4..((y * img.width + img.width.min(tw)) as usize) * 4];
            let di = ((y0 + y) * w + x0) as usize * 4;
            sheet.rgba[di..di + src.len()].copy_from_slice(src);
        }
        let lab = &labels[i];
        for y in 0..label_h {
            for x in 0..tw {
                let pxl = lab.get(x as i32, y as i32);
                if pxl[3] <= 0.0 {
                    continue;
                }
                let c8 = unpremul8(pxl);
                let di = ((y0 + th + y) * w + x0 + x) as usize * 4;
                blend8(&mut sheet.rgba[di..di + 4], c8);
            }
        }
    }
    Ok(sheet)
}

/// PNG of an image (fast compression; frames are previews and AI snapshots).
pub(crate) fn encode_png(img: &Image) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, img.width.max(1), img.height.max(1));
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        if let Ok(mut w) = enc.write_header() {
            let need = img.width as usize * img.height as usize * 4;
            if img.rgba.len() == need && need > 0 {
                let _ = w.write_image_data(&img.rgba);
            } else {
                let _ = w.write_image_data(&vec![0u8; img.width.max(1) as usize * img.height.max(1) as usize * 4]);
            }
        }
    }
    out
}
