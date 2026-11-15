//! Still images: decoded with the `image` crate (EXIF orientation applied), falling back to ffmpeg
//! for formats it does not handle; resizing is alpha-correct (premultiplied).

use crate::error::{MediaError, Result};
use crate::tools::{ff_path, ffmpeg, run_capture};
use image::{DynamicImage, ImageDecoder, RgbaImage};
use std::path::Path;

/// Decodes a still to straight RGBA8, upright.
pub(crate) fn load(path: &Path) -> Result<RgbaImage> {
    match load_with_image_crate(path) {
        Ok(img) => Ok(img),
        Err(first) => load_with_ffmpeg(path).map_err(|second| match second {
            MediaError::ToolMissing { .. } => MediaError::unsupported(path, format!("could not decode the image: {first}")),
            other => MediaError::unsupported(path, format!("could not decode the image ({first}); ffmpeg also failed: {other}")),
        }),
    }
}

fn load_with_image_crate(path: &Path) -> std::result::Result<RgbaImage, String> {
    let reader = image::ImageReader::open(path).map_err(|e| e.to_string())?.with_guessed_format().map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    Ok(img.into_rgba8())
}

fn load_with_ffmpeg(path: &Path) -> Result<RgbaImage> {
    let (_, info) = crate::probe::probe_full(path).map(|p| (p.kind, p.info))?;
    let (w, h) = (info.width.max(1), info.height.max(1));
    let mut c = ffmpeg("error")?;
    c.arg("-i").arg(ff_path(path));
    c.args(["-map", "0:v:0", "-frames:v", "1", "-vf", &format!("scale={w}:{h},format=rgba"), "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"]);
    let (out, _) = run_capture(c, "ffmpeg", Some(path), "decode the image")?;
    if out.len() != (w * h * 4) as usize {
        return Err(MediaError::unsupported(path, format!("ffmpeg returned {} bytes for a {w}x{h} image", out.len())));
    }
    RgbaImage::from_raw(w, h, out).ok_or_else(|| MediaError::unsupported(path, "image buffer size mismatch"))
}

/// Resizes straight RGBA to `w`x`h`. Images with transparency are resized premultiplied so
/// transparent pixels do not bleed dark fringes into the edges.
pub(crate) fn resize(img: &RgbaImage, w: u32, h: u32) -> RgbaImage {
    use image::imageops::{resize, FilterType};
    if img.width() == w && img.height() == h {
        return img.clone();
    }
    let filter = FilterType::Triangle;
    let has_alpha = img.pixels().any(|p| p.0[3] != 255);
    if !has_alpha {
        return resize(img, w, h, filter);
    }
    let mut f = DynamicImage::ImageRgba8(img.clone()).into_rgba32f();
    for p in f.pixels_mut() {
        let a = p.0[3];
        p.0[0] *= a;
        p.0[1] *= a;
        p.0[2] *= a;
    }
    let mut r = resize(&f, w, h, filter);
    for p in r.pixels_mut() {
        let a = p.0[3];
        if a > 1e-6 {
            p.0[0] /= a;
            p.0[1] /= a;
            p.0[2] /= a;
        }
    }
    DynamicImage::ImageRgba32F(r).into_rgba8()
}

/// Area-average downscale of straight RGBA (`sw`x`sh` -> `dw`x`dh`, `dw <= sw`, `dh <= sh`); used to
/// make thumbnails from larger frames already decoded.
pub(crate) fn downscale(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw.max(1) as usize, dh.max(1) as usize);
    let mut out = vec![0u8; dw * dh * 4];
    // Source column span of each output column.
    let cols: Vec<(usize, usize)> = (0..dw).map(|x| (x * sw / dw, ((x + 1) * sw / dw).max(x * sw / dw + 1).min(sw))).collect();
    let mut acc = vec![0u32; dw * 4];
    for y in 0..dh {
        let (y0, y1) = (y * sh / dh, ((y + 1) * sh / dh).max(y * sh / dh + 1).min(sh));
        acc.iter_mut().for_each(|a| *a = 0);
        for sy in y0..y1 {
            let row = &src[sy * sw * 4..(sy + 1) * sw * 4];
            for (x, &(x0, x1)) in cols.iter().enumerate() {
                let a = &mut acc[x * 4..x * 4 + 4];
                for px in row[x0 * 4..x1 * 4].as_chunks::<4>().0 {
                    a[0] += px[0] as u32;
                    a[1] += px[1] as u32;
                    a[2] += px[2] as u32;
                    a[3] += px[3] as u32;
                }
            }
        }
        for (x, &(x0, x1)) in cols.iter().enumerate() {
            let n = ((x1 - x0) * (y1 - y0)) as u32;
            let o = &mut out[(y * dw + x) * 4..(y * dw + x) * 4 + 4];
            for c in 0..4 {
                o[c] = ((acc[x * 4 + c] + n / 2) / n) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn box_downscale() {
        // 4x2 -> 2x1: averages of 2x2 blocks
        let src: Vec<u8> = [[0u8, 0, 0, 255], [100, 100, 100, 255], [200, 0, 0, 255], [200, 0, 0, 255]]
            .iter()
            .cycle()
            .take(8)
            .flatten()
            .copied()
            .collect();
        let d = super::downscale(&src, 4, 2, 2, 1);
        assert_eq!(&d[0..4], &[50, 50, 50, 255]);
        assert_eq!(&d[4..8], &[200, 0, 0, 255]);
        let big = vec![77u8; 1920 * 1080 * 4];
        assert!(super::downscale(&big, 1920, 1080, 320, 180).iter().all(|v| *v == 77));
    }
}
