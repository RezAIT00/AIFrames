//! (Added) Effect previews: what an effect, transition or title/shape preset would look like, without
//! editing anything.
//!
//! The window's effects-browser thumbnails, its live hover preview in the program viewer and the AI's
//! `preview_effects` tool all work the same way: the edit is applied to a *scratch copy* of the project
//! (never the document, so nothing enters the history) and that copy is drawn by the one renderer
//! ([`Renderer::frame`]), exactly as export would draw it.
//!
//! When nothing suitable is selected, previews use a sample scene: two painted stills (generated once
//! into the media pool's cache folder, about 0.5 MB) on one track with a cut between them.

use aiframes_core::{apply_batch, resolve, transition_spec, EffectArg, Id, Op, Project, Resolved};
use aiframes_render::{encode_png, FrameOptions, Image, Renderer};
use serde_json::json;
use std::path::{Path, PathBuf};

/// A copy of a project with an edit applied (the document itself is untouched).
pub struct Scratch {
    pub project: Project,
    pub resolved: Resolved,
}

/// Applies `ops` to a copy of `p`.
pub fn scratch(p: &Project, ops: &[Op]) -> Result<Scratch, String> {
    let mut project = p.clone();
    if !ops.is_empty() {
        apply_batch(&mut project, ops).map_err(|e| e.to_string())?;
    }
    let resolved = resolve(&project);
    Ok(Scratch { project, resolved })
}

/// The render scale that makes the project's frame `width` pixels wide.
pub fn scale_for(p: &Project, width: u32) -> f32 {
    (width.max(8) as f32 / p.settings.width.max(1) as f32).clamp(0.005, 1.0)
}

/// Renders the scratch project at timeline time `at`, `width` pixels wide.
pub fn render(rd: &Renderer, dir: &Path, s: &Scratch, at: f64, width: u32) -> Result<Image, String> {
    let o = FrameOptions { scale: scale_for(&s.project, width), guides: false, draft: false };
    rd.frame(dir, &s.project, &s.resolved, at, &o).map_err(|e| format!("render failed: {e}"))
}

/// The op that puts a default `kind` effect on `clip` (as the effects browser does).
pub fn effect_op(clip: &str, kind: &str) -> Op {
    Op::AddEffect { clip: clip.into(), effect: EffectArg::Kind(kind.into()), index: None }
}

/// The cut next to `clip` on its track: `(from, to)` with the clip that follows it, else with the one
/// before it.
pub fn cut_of(p: &Project, r: &Resolved, clip: &str) -> Option<(Id, Id)> {
    let c = p.clips.iter().find(|c| c.id == clip)?;
    let s = r.span(clip)?;
    let on = r.on_track(p, &c.track);
    if let Some((n, _)) = on.iter().find(|(k, ks)| k.id != clip && (ks.start - s.end).abs() < 1e-3) {
        return Some((clip.to_string(), n.id.clone()));
    }
    on.iter().find(|(k, ks)| k.id != clip && (ks.end - s.start).abs() < 1e-3).map(|(k, _)| (k.id.clone(), clip.to_string()))
}

/// The ops for a `kind` transition on the cut `from|to` (changing the kind of one already there), the
/// timeline time of the cut (where the transition is half way) and the transition's duration.
pub fn transition_ops(p: &Project, r: &Resolved, from: &str, to: &str, kind: &str) -> Option<(Vec<Op>, f64, f64)> {
    let at = r.span(to)?.start;
    if let Some(t) = p.transitions.iter().find(|t| t.from == from && t.to == to) {
        return Some((vec![Op::SetTransition { transition: t.id.clone(), kind: Some(kind.into()), duration: None, params: Default::default() }], at, t.duration));
    }
    let spec = transition_spec(kind)?;
    let max = r.span(from)?.duration().min(r.span(to)?.duration());
    let d = spec.default_duration.min(max);
    Some((vec![Op::AddTransition { from: from.into(), to: to.into(), kind: kind.into(), duration: Some(d), id: None, params: Default::default() }], at, d))
}

/// Where a still of a transition is taken: 35 % of the way through (half way, a dip is only its colour
/// and a push only the outgoing edge). The effects browser uses the same moment.
pub fn transition_still(cut: f64, duration: f64) -> f64 {
    cut - duration / 2.0 + duration * 0.35
}

// ---------------------------------------------------------------------------------------------
// The sample scene
// ---------------------------------------------------------------------------------------------

/// Clip ids of the sample scene: `fx_sample_a` (0–4 s) then `fx_sample_b` (4–8 s) on track `v1`.
pub const SAMPLE_A: &str = "fx_sample_a";
pub const SAMPLE_B: &str = "fx_sample_b";
/// Where effects are shown on the sample (the middle of the first still).
pub const SAMPLE_AT: f64 = 2.0;
const SAMPLE_W: u32 = 640;
const SAMPLE_H: u32 = 360;
/// Bump when the painting changes (new file names; old ones are left to the cache pruning).
const SAMPLE_VERSION: u32 = 1;

/// The sample scene for a project of `width`×`height` at `fps` (so pixel-sized effect parameters look
/// as they would in that project): two painted stills, filling the frame, with a cut at 4 s. Their
/// files are written once into `cache_dir/fx-sample`.
pub fn sample_project(cache_dir: &Path, width: u32, height: u32, fps: f64) -> Result<Project, String> {
    let (a, b) = sample_files(cache_dir)?;
    let media = |id: &str, path: &Path| json!({"id": id, "path": path.to_string_lossy().replace('\\', "/"), "kind": "image", "info": {"width": SAMPLE_W, "height": SAMPLE_H, "has_video": true}});
    let clip = |id: &str, asset: &str, start: f64| json!({"id": id, "track": "v1", "start": start, "duration": 4.0, "transform": {"fit": "cover"}, "source": {"type": "media", "asset": asset}});
    serde_json::from_value(json!({
        "format": aiframes_core::FORMAT_VERSION,
        "name": "Effect preview sample",
        "settings": {"width": width.max(16), "height": height.max(16), "fps": if fps > 0.0 { fps } else { 30.0 }},
        "media": [media("fx_sample_ma", &a), media("fx_sample_mb", &b)],
        "tracks": [{"id": "v1", "kind": "video", "role": "main"}, {"id": "v2", "kind": "video", "role": "overlay"}],
        "clips": [clip(SAMPLE_A, "fx_sample_ma", 0.0), clip(SAMPLE_B, "fx_sample_mb", 4.0)],
        "id_counters": {"c": 100, "v": 2, "m": 100},
    }))
    .map_err(|e| format!("sample project: {e}"))
}

/// The two sample stills (written on first use).
fn sample_files(cache_dir: &Path) -> Result<(PathBuf, PathBuf), String> {
    let dir = cache_dir.join("fx-sample");
    let a = dir.join(format!("sample-a-v{SAMPLE_VERSION}.png"));
    let b = dir.join(format!("sample-b-v{SAMPLE_VERSION}.png"));
    for (f, paint) in [(&a, paint_landscape as fn(u32, u32) -> Vec<u8>), (&b, paint_sea)] {
        if f.is_file() {
            continue;
        }
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let img = Image { width: SAMPLE_W, height: SAMPLE_H, rgba: paint(SAMPLE_W, SAMPLE_H) };
        // Written under a unique name, then renamed: concurrent first uses never see half a file.
        let tmp = dir.join(format!("{}.{}.tmp", f.file_name().and_then(|n| n.to_str()).unwrap_or("sample"), std::process::id()));
        std::fs::write(&tmp, encode_png(&img)).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        if std::fs::rename(&tmp, f).is_err() {
            let _ = std::fs::remove_file(&tmp);
            if !f.is_file() {
                return Err(format!("cannot write {}", f.display()));
            }
        }
    }
    Ok((a, b))
}

fn hash(x: i32, y: i32, seed: u32) -> f32 {
    let mut h = (x as u32).wrapping_mul(374_761_393) ^ (y as u32).wrapping_mul(668_265_263) ^ seed.wrapping_mul(2_246_822_519);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    ((h ^ (h >> 16)) & 0xffff) as f32 / 65535.0
}

fn smooth(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

fn noise(x: f32, y: f32, seed: u32) -> f32 {
    let (xi, yi) = (x.floor() as i32, y.floor() as i32);
    let (fx, fy) = (smooth(x - x.floor()), smooth(y - y.floor()));
    let a = hash(xi, yi, seed) + (hash(xi + 1, yi, seed) - hash(xi, yi, seed)) * fx;
    let b = hash(xi, yi + 1, seed) + (hash(xi + 1, yi + 1, seed) - hash(xi, yi + 1, seed)) * fx;
    a + (b - a) * fy
}

fn fbm(x: f32, y: f32, seed: u32) -> f32 {
    let (mut s, mut amp, mut f, mut norm) = (0.0, 0.5, 1.0, 0.0);
    for o in 0..4 {
        s += amp * noise(x * f, y * f, seed + o);
        norm += amp;
        amp *= 0.5;
        f *= 2.0;
    }
    s / norm
}

type Rgb = [f32; 3];

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn sstep(a: f32, b: f32, x: f32) -> f32 {
    smooth(((x - a) / (b - a)).clamp(0.0, 1.0))
}

fn put(out: &mut [u8], i: usize, c: Rgb) {
    for k in 0..3 {
        out[i + k] = (c[k].clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    out[i + 3] = 255;
}

/// Sunset over green hills: a gradient sky with clouds and a sun, mountains, textured grass, a lake,
/// a striped balloon and a small house (colour, detail, hard edges and soft areas for every effect).
fn paint_landscape(w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (w * h * 4) as usize];
    let ar = w as f32 / h as f32;
    for y in 0..h {
        for x in 0..w {
            let (u, v) = ((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
            let mut c = mix([0.10, 0.20, 0.50], [0.98, 0.62, 0.34], (v / 0.6).powf(1.5));
            // Sun and its glow.
            let d = ((u - 0.72) * ar).hypot(v - 0.40);
            let g = 0.55 * (-(d / 0.14).powi(2)).exp();
            c = [c[0] + g, c[1] + g * 0.75, c[2] + g * 0.45];
            if d < 0.055 {
                c = [1.0, 0.96, 0.82];
            }
            // Clouds.
            if v < 0.38 {
                let cl = fbm(u * 5.0 * ar, v * 12.0, 3);
                c = mix(c, [1.0, 0.88, 0.86], sstep(0.55, 0.72, cl) * 0.75 * (1.0 - v / 0.38));
            }
            // Mountains.
            let ridge = 0.40 + 0.07 * (u * 9.0 + 0.5).sin() + 0.035 * (u * 23.0 + 1.3).sin() + 0.05 * (fbm(u * 10.0, 0.5, 7) - 0.5);
            if v > ridge {
                let k = ((v - ridge) / 0.25).clamp(0.0, 1.0);
                c = mix([0.46, 0.38, 0.62], [0.24, 0.22, 0.42], k);
                if v < ridge + 0.025 && ridge < 0.40 {
                    c = mix(c, [0.95, 0.95, 1.0], 0.85);
                }
            }
            // Hills with grass texture.
            let hill = 0.60 + 0.05 * (u * 5.0 + 2.0).sin() + 0.025 * (u * 13.0).sin();
            if v > hill {
                let k = ((v - hill) / (1.0 - hill)).clamp(0.0, 1.0);
                let t = fbm(u * 70.0, v * 30.0, 11);
                let blade = (noise(u * 300.0, v * 40.0, 5) - 0.5) * 0.25;
                c = mix([0.30, 0.66, 0.22], [0.07, 0.28, 0.09], k * 0.9 + (t - 0.5) * 0.6 + blade);
                // Lake.
                let lake = ((u - 0.30) / 0.17).powi(2) + ((v - 0.80) / 0.065).powi(2);
                if lake < 1.0 {
                    let ripple = ((v * 260.0) + 3.0 * (u * 40.0).sin()).sin() * 0.06;
                    c = mix([0.30, 0.52, 0.85], [0.95, 0.70, 0.55], 0.25 + ripple + (1.0 - lake) * 0.1);
                }
            }
            // A small house on the hill: white walls, red roof, a lit window.
            let (hx0, hx1, hy0, hy1) = (0.60, 0.67, 0.585, 0.655);
            if u > hx0 && u < hx1 && v > hy0 && v < hy1 {
                c = [0.94, 0.92, 0.88];
                if u > 0.626 && u < 0.645 && v > 0.605 && v < 0.630 {
                    c = [1.0, 0.82, 0.30];
                }
            }
            let roof = (v - (hy0 - 0.045)) / 0.045;
            if v > hy0 - 0.045 && v <= hy0 && (u - (hx0 + hx1) / 2.0).abs() < (hx1 - hx0) * 0.62 * roof {
                c = [0.62, 0.12, 0.10];
            }
            // A striped hot-air balloon with its basket.
            let (bx, by, br) = (0.24, 0.29, 0.085);
            let dx = (u - bx) * ar;
            let dy = (v - by) / 1.15;
            if dx.hypot(dy) < br {
                let band = (((dx / br) * 3.0 + 3.0).floor() as i32).rem_euclid(4);
                let base = [[0.90, 0.12, 0.15], [1.0, 0.85, 0.10], [0.10, 0.35, 0.90], [0.97, 0.97, 0.97]][band as usize];
                let shade = 1.0 - 0.45 * ((dx + dy * 0.6) / br).clamp(-1.0, 1.0).max(0.0);
                c = [base[0] * shade, base[1] * shade, base[2] * shade];
            }
            if (u - bx).abs() * ar < 0.02 && v > by + br * 1.25 && v < by + br * 1.25 + 0.035 {
                c = [0.40, 0.25, 0.12];
            }
            put(&mut out, ((y * w + x) * 4) as usize, c);
        }
    }
    out
}

/// Dusk at sea: a purple-to-pink sky, a pale moon, waves catching the light, a sailboat and a
/// lighthouse (a clearly different picture, for the incoming side of transitions).
fn paint_sea(w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (w * h * 4) as usize];
    let ar = w as f32 / h as f32;
    for y in 0..h {
        for x in 0..w {
            let (u, v) = ((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
            let mut c = mix([0.22, 0.08, 0.38], [0.98, 0.50, 0.55], (v / 0.55).powf(1.3));
            let d = ((u - 0.30) * ar).hypot(v - 0.24);
            c = mix(c, [1.0, 0.9, 0.95], 0.35 * (-(d / 0.16).powi(2)).exp());
            if d < 0.075 {
                let crater = noise(u * 60.0, v * 60.0, 2) * 0.12;
                c = [0.98 - crater, 0.96 - crater, 0.86 - crater];
            }
            if v > 0.55 {
                let k = ((v - 0.55) / 0.45).clamp(0.0, 1.0);
                c = mix([0.05, 0.32, 0.42], [0.01, 0.10, 0.18], k);
                let wave = ((v * 90.0 / (0.3 + k)) + 2.5 * (u * 30.0 + v * 10.0).sin()).sin();
                let glint = sstep(0.82, 0.98, wave) * (1.0 - ((u - 0.30) * 3.0).abs()).clamp(0.15, 1.0);
                c = mix(c, [1.0, 0.75, 0.75], glint * (1.0 - k * 0.6));
            }
            // Sailboat.
            let (sx, base) = (0.64, 0.56);
            if v < base && v > base - 0.20 && u > sx && u < sx + 0.11 * (v - (base - 0.20)) / 0.20 {
                c = [0.97, 0.96, 0.92];
            }
            if v >= base && v < base + 0.03 && u > sx - 0.04 && u < sx + 0.15 - (v - base) * 1.5 {
                c = [0.25, 0.10, 0.08];
            }
            // Lighthouse with red and white bands and a lamp.
            if u > 0.86 && u < 0.90 && v > 0.30 && v < 0.62 {
                c = if ((v - 0.30) / 0.064) as i32 % 2 == 0 { [0.88, 0.14, 0.12] } else { [0.96, 0.96, 0.94] };
            }
            let ld = ((u - 0.88) * ar).hypot(v - 0.28);
            c = mix(c, [1.0, 0.95, 0.6], (-(ld / 0.04).powi(2)).exp());
            put(&mut out, ((y * w + x) * 4) as usize, c);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Labelled sheets
// ---------------------------------------------------------------------------------------------

/// A strip `w`×`h` with `text` drawn by the renderer (a one-clip text project).
pub fn label_image(rd: &Renderer, text: &str, w: u32, h: u32) -> Result<Image, String> {
    let p: Project = serde_json::from_value(json!({
        "format": aiframes_core::FORMAT_VERSION,
        "settings": {"width": w.max(16), "height": h.max(8), "fps": 30, "background": "#1C1C20"},
        "tracks": [{"id": "v1", "kind": "video"}],
        "clips": [{"id": "l", "track": "v1", "start": 0.0, "duration": 1.0,
                   "source": {"type": "text", "text": text, "size": (h as f64 * 0.58).round().max(6.0), "weight": 600, "color": "#E8E8EC", "max_width": w as f64 - 6.0}}],
    }))
    .map_err(|e| e.to_string())?;
    let r = resolve(&p);
    rd.frame(Path::new("."), &p, &r, 0.0, &FrameOptions::default()).map_err(|e| format!("label: {e}"))
}

fn blit(dst: &mut Image, src: &Image, x0: u32, y0: u32) {
    for y in 0..src.height {
        let ty = y0 + y;
        if ty >= dst.height {
            break;
        }
        let n = src.width.min(dst.width.saturating_sub(x0)) as usize * 4;
        let s = (y * src.width) as usize * 4;
        let d = (ty * dst.width + x0) as usize * 4;
        dst.rgba[d..d + n].copy_from_slice(&src.rgba[s..s + n]);
    }
}

/// A grid of equally sized tiles, each with its label strip underneath, `cols` per row.
pub fn grid(rd: &Renderer, tiles: &[(Image, String)], cols: u32) -> Result<Image, String> {
    let Some((first, _)) = tiles.first() else { return Ok(Image::new(1, 1)) };
    let (tw, th) = (first.width, first.height);
    let lh = (tw / 11).clamp(14, 28);
    let gap = 4;
    let cols = cols.clamp(1, tiles.len() as u32);
    let rows = (tiles.len() as u32).div_ceil(cols);
    let mut out = Image::new(cols * (tw + gap) + gap, rows * (th + lh + gap) + gap);
    for px in out.rgba.chunks_exact_mut(4) {
        px.copy_from_slice(&[0x12, 0x12, 0x15, 0xFF]);
    }
    for (i, (img, label)) in tiles.iter().enumerate() {
        let (c, r) = (i as u32 % cols, i as u32 / cols);
        let (x, y) = (gap + c * (tw + gap), gap + r * (th + lh + gap));
        blit(&mut out, img, x, y);
        blit(&mut out, &label_image(rd, label, tw, lh)?, x, y + th);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiframes_media::MediaPool;

    fn mean(img: &Image, x0: f32, y0: f32, x1: f32, y1: f32) -> [f64; 3] {
        let mut s = [0.0; 3];
        let mut n: f64 = 0.0;
        for y in (img.height as f32 * y0) as u32..(img.height as f32 * y1) as u32 {
            for x in (img.width as f32 * x0) as u32..(img.width as f32 * x1) as u32 {
                let p = img.pixel(x, y).unwrap();
                for k in 0..3 {
                    s[k] += p[k] as f64;
                }
                n += 1.0;
            }
        }
        s.map(|v| v / n.max(1.0))
    }

    #[test]
    fn sample_scene_renders_and_scratch_edits_leave_the_project_alone() {
        let dir = std::env::temp_dir().join(format!("aiframes-fxsample-{}", std::process::id()));
        let pool = MediaPool::with_cache_dir(64 << 20, &dir);
        let rd = Renderer::new(pool);
        let p = sample_project(&dir, 1920, 1080, 30.0).unwrap();
        let before = p.clone();
        let plain = render(&rd, &dir, &scratch(&p, &[]).unwrap(), SAMPLE_AT, 192).unwrap();
        assert_eq!((plain.width, plain.height), (192, 108));
        // The painted scene is not flat: sky (top) brighter blue-ish than the grass (bottom).
        let (sky, grass) = (mean(&plain, 0.4, 0.02, 0.6, 0.1), mean(&plain, 0.0, 0.9, 1.0, 1.0));
        assert!(sky[2] > grass[2] + 20.0, "sky {sky:?} grass {grass:?}");
        // An inverted copy differs; the project is untouched (nothing enters a document).
        let inv = render(&rd, &dir, &scratch(&p, &[effect_op(SAMPLE_A, "invert")]).unwrap(), SAMPLE_AT, 192).unwrap();
        let isky = mean(&inv, 0.4, 0.02, 0.6, 0.1);
        assert!((isky[2] - (255.0 - sky[2])).abs() < 12.0, "{isky:?} vs {sky:?}");
        assert_eq!(p, before);
        // A transition on the sample cut renders half way at the cut time.
        let r = resolve(&p);
        assert_eq!(cut_of(&p, &r, SAMPLE_A), Some((SAMPLE_A.to_string(), SAMPLE_B.to_string())));
        let (ops, at, d) = transition_ops(&p, &r, SAMPLE_A, SAMPLE_B, "wipe_left").unwrap();
        assert_eq!((at, d), (4.0, 0.5));
        let mid = render(&rd, &dir, &scratch(&p, &ops).unwrap(), at, 192).unwrap();
        let (l, rr) = (mean(&mid, 0.0, 0.0, 0.3, 0.3), mean(&mid, 0.7, 0.0, 1.0, 0.3));
        let (al, br) = (mean(&plain, 0.0, 0.0, 0.3, 0.3), mean(&render(&rd, &dir, &scratch(&p, &[]).unwrap(), 6.0, 192).unwrap(), 0.7, 0.0, 1.0, 0.3));
        let near = |a: [f64; 3], b: [f64; 3]| (0..3).all(|k| (a[k] - b[k]).abs() < 25.0);
        assert!(near(l, al) && near(rr, br), "a wipe left shows the outgoing clip on the left and the incoming on the right half way");
        // Labelled grid.
        let g = grid(&rd, &[(plain.clone(), "Plain".into()), (inv, "Invert".into()), (mid, "Wipe left".into())], 2).unwrap();
        assert!(g.width > 2 * 192 && g.height > 2 * 108);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
