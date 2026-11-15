//! Shared test helpers: ffmpeg lavfi fixtures (cached under Cargo's tmp dir), project builders,
//! pixel and audio measurements.
#![allow(dead_code)]

use aiframes_core::{resolve, Clip, MediaAsset, Project, Resolved, Track};
use aiframes_media::MediaPool;
use aiframes_render::{FrameOptions, Image, Renderer};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

pub fn root() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("aiframes-render-fixtures");
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn scratch(name: &str) -> PathBuf {
    let d = root().join("scratch").join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn ffmpeg(args: &[&str]) {
    let out = Command::new(&aiframes_media::tools().expect("ffmpeg").ffmpeg)
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "ffmpeg {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

static LOCK: Mutex<()> = Mutex::new(());

/// Generates fixture `name` once with `make(tmp_path)`.
pub fn fixture(name: &str, make: impl FnOnce(&str)) -> PathBuf {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let p = root().join(name);
    if p.is_file() && std::fs::metadata(&p).map(|m| m.len() > 0).unwrap_or(false) {
        return p;
    }
    let ext = Path::new(name).extension().unwrap().to_string_lossy().into_owned();
    let tmp = root().join(format!("tmp-{}-{name}.{ext}", std::process::id()));
    make(tmp.to_str().unwrap());
    std::fs::rename(&tmp, &p).unwrap();
    p
}

/// Frame-number bars (9 bits, LSB left), 320x180, `fps`, `secs` long, with a 1 kHz tone.
pub fn bars(fps: u32, secs: u32) -> PathBuf {
    fixture(&format!("bars{fps}_{secs}.mp4"), |out| {
        let vf = format!(
            "color=c=black:s=320x180:r={fps}:d={secs},format=gray,geq=lum='if(bitand(N,pow(2,floor(X*9/W))),235,16)',format=yuv420p"
        );
        ffmpeg(&[
            "-f", "lavfi", "-i", &vf, "-f", "lavfi", "-i", &format!("sine=f=1000:r=48000:d={secs}"), "-shortest", "-c:v", "libx264",
            "-g", "30", "-preset", "veryfast", "-crf", "12", "-c:a", "aac", "-b:a", "192k", out,
        ]);
    })
}

/// Reads the frame number from bars rendered full-frame (row in the middle).
pub fn code(img: &Image, x0: u32, y0: u32, w: u32, h: u32) -> u32 {
    let y = y0 + h / 2;
    let mut n = 0;
    for k in 0..9 {
        let x = x0 + ((k as f64 + 0.5) * w as f64 / 9.0) as u32;
        if img.pixel(x, y).unwrap()[0] > 128 {
            n |= 1 << k;
        }
    }
    n
}

/// A flat-colour video (`color` is an ffmpeg colour), 320x180 25 fps.
pub fn flat_video(name: &str, color: &str, secs: u32) -> PathBuf {
    fixture(&format!("{name}.mp4"), |out| {
        ffmpeg(&["-f", "lavfi", "-i", &format!("color=c={color}:s=320x180:r=25:d={secs}"), "-c:v", "libx264", "-preset", "veryfast", "-crf", "10", "-pix_fmt", "yuv444p", out]);
    })
}

/// testsrc2 1920x1080 30 fps (for performance).
pub fn testsrc_1080(name: &str, secs: u32) -> PathBuf {
    fixture(&format!("{name}.mp4"), |out| {
        ffmpeg(&[
            "-f", "lavfi", "-i", &format!("testsrc2=s=1920x1080:r=30:d={secs}"), "-f", "lavfi", "-i", &format!("sine=f=440:r=48000:d={secs}"),
            "-shortest", "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-g", "60", "-pix_fmt", "yuv420p", "-c:a", "aac", out,
        ]);
    })
}

/// 64x48 PNG: left half red, right half blue.
pub fn halves_png() -> PathBuf {
    fixture("halves.png", |out| {
        ffmpeg(&["-f", "lavfi", "-i", "color=c=red:s=64x48,format=rgb24,geq=r='if(lt(X,32),255,0)':g=0:b='if(lt(X,32),0,255)'", "-frames:v", "1", out]);
    })
}

/// Stereo f32 sine WAV at 48 kHz.
pub fn sine_wav(name: &str, freq: f64, amp: f64, secs: f64) -> PathBuf {
    fixture(&format!("{name}.wav"), |out| {
        ffmpeg(&["-f", "lavfi", "-i", &format!("aevalsrc=exprs='{amp}*sin(2*PI*{freq}*t)':s=48000:d={secs}:c=stereo"), "-c:a", "pcm_f32le", out]);
    })
}

pub fn pool() -> Arc<MediaPool> {
    static P: OnceLock<Arc<MediaPool>> = OnceLock::new();
    P.get_or_init(|| MediaPool::with_cache_dir(1 << 30, root().join("cache"))).clone()
}

/// A fresh pool (own counters).
pub fn fresh_pool(name: &str) -> Arc<MediaPool> {
    MediaPool::with_cache_dir(1 << 30, scratch(&format!("cache-{name}")))
}

pub fn renderer() -> Renderer {
    Renderer::new(pool())
}

/// A project with tracks `v1` (video) and `a1` (audio) unless `tracks` given.
pub fn project(w: u32, h: u32, fps: f64) -> Project {
    serde_json::from_value(json!({
        "format": 1,
        "settings": {"width": w, "height": h, "fps": fps, "background": "#000000"},
        "tracks": [{"id": "v1", "kind": "video"}, {"id": "v2", "kind": "video"}, {"id": "v3", "kind": "video"}, {"id": "a1", "kind": "audio"}, {"id": "a2", "kind": "audio"}],
    }))
    .unwrap()
}

pub fn clip(v: Value) -> Clip {
    serde_json::from_value(v).unwrap()
}

pub fn track(v: Value) -> Track {
    serde_json::from_value(v).unwrap()
}

/// Adds a media asset (probed).
pub fn add_media(p: &mut Project, id: &str, path: &Path) {
    let (kind, info) = aiframes_media::probe(path).unwrap();
    p.media.push(MediaAsset { id: id.into(), name: String::new(), path: path.to_string_lossy().replace('\\', "/"), kind, info, tags: vec![], notes: String::new() });
}

pub fn solid(id: &str, track: &str, start: f64, dur: f64, color: &str) -> Clip {
    clip(json!({"id": id, "track": track, "start": start, "duration": dur, "source": {"type": "solid", "color": color}}))
}

pub fn render(r: &Renderer, p: &Project, t: f64, scale: f32) -> Image {
    let res = resolve(p);
    assert!(res.errors.is_empty(), "{:?}", res.errors);
    let (img, warnings) = r.frame_checked(Path::new("."), p, &res, t, &FrameOptions { scale, guides: false, draft: false }).unwrap();
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    img
}

pub fn resolved(p: &Project) -> Resolved {
    let r = resolve(p);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    r
}

pub fn px(img: &Image, x: u32, y: u32) -> [u8; 4] {
    img.pixel(x, y).unwrap()
}

pub fn near(a: [u8; 4], b: [u8; 4], tol: i32) -> bool {
    (0..4).all(|k| (a[k] as i32 - b[k] as i32).abs() <= tol)
}

/// Mean absolute difference per channel.
pub fn diff(a: &Image, b: &Image) -> f64 {
    assert_eq!((a.width, a.height), (b.width, b.height));
    let s: u64 = a.rgba.iter().zip(&b.rgba).map(|(x, y)| (*x as i64 - *y as i64).unsigned_abs()).sum();
    s as f64 / a.rgba.len() as f64
}

pub fn mean_rgb(img: &Image) -> [f64; 3] {
    let mut s = [0f64; 3];
    for p in img.rgba.as_chunks::<4>().0 {
        for k in 0..3 {
            s[k] += p[k] as f64;
        }
    }
    let n = (img.rgba.len() / 4) as f64;
    [s[0] / n, s[1] / n, s[2] / n]
}

/// Bounding box `[x0, y0, x1, y1]` (exclusive) of pixels where `f` holds.
pub fn bbox(img: &Image, f: impl Fn([u8; 4]) -> bool) -> Option<[u32; 4]> {
    let mut b = [u32::MAX, u32::MAX, 0, 0];
    let mut any = false;
    for y in 0..img.height {
        for x in 0..img.width {
            if f(px(img, x, y)) {
                any = true;
                b[0] = b[0].min(x);
                b[1] = b[1].min(y);
                b[2] = b[2].max(x + 1);
                b[3] = b[3].max(y + 1);
            }
        }
    }
    any.then_some(b)
}

/// RMS level in dBFS of interleaved stereo (both channels).
pub fn rms_db(x: &[f32]) -> f64 {
    let s: f64 = x.iter().map(|v| (*v as f64).powi(2)).sum();
    let rms = (s / x.len().max(1) as f64).sqrt();
    20.0 * rms.max(1e-12).log10()
}

/// Peak dBFS.
pub fn peak_db(x: &[f32]) -> f64 {
    let p = x.iter().fold(0f32, |a, v| a.max(v.abs())) as f64;
    20.0 * p.max(1e-12).log10()
}

/// Zero crossings per second of the left channel (≈ 2 × frequency).
pub fn crossings_per_sec(x: &[f32], rate: f64) -> f64 {
    let l: Vec<f32> = x.as_chunks::<2>().0.iter().map(|c| c[0]).collect();
    let n = l.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
    n as f64 / (l.len() as f64 / rate)
}

pub fn ffprobe_json(path: &Path) -> Value {
    let out = Command::new(&aiframes_media::tools().unwrap().ffprobe)
        .args(["-v", "error", "-show_format", "-show_streams", "-of", "json"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).unwrap()
}
