//! Regression tests ported from the independent review (review-render/tests/render_*.rs), plus the
//! follow-up fixes (no-break-space words, continuous audio positions, flattened alpha).

mod common;
use aiframes_core::{resolve, ExportSettings, Project};
use aiframes_render::{export, FrameOptions, Renderer};
use common::*;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

/// Tests that export or inspect the temp folder / child processes run one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

fn proj(v: Value) -> Project {
    serde_json::from_value(v).expect("project json")
}

fn base(w: u32, h: u32, clips: Value) -> Project {
    proj(json!({"format": 1, "settings": {"width": w, "height": h, "fps": 30}, "tracks": [{"id": "v1", "kind": "video"}], "clips": clips}))
}

fn frame(r: &Renderer, p: &Project, t: f64) -> aiframes_render::Image {
    let res = resolved(p);
    r.frame(Path::new("."), p, &res, t, &FrameOptions::default()).expect("frame")
}

fn rms(v: &[f32]) -> f32 {
    (v.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt() as f32
}

fn bars_asset(path: &Path, secs: f64) -> Value {
    json!({"id": "m1", "path": path.to_string_lossy(), "kind": "video",
           "info": {"duration": secs, "width": 320, "height": 180, "fps": 30.0, "has_video": true, "has_audio": true}})
}

fn audio_asset(id: &str, path: &Path, secs: f64) -> Value {
    json!({"id": id, "path": path.to_string_lossy(), "kind": "audio", "info": {"duration": secs, "has_audio": true, "audio_channels": 2, "audio_rate": 48000}})
}

fn wav(name: &str, expr: &str, secs: f64) -> std::path::PathBuf {
    fixture(&format!("{name}.wav"), |out| {
        ffmpeg(&["-f", "lavfi", "-i", &format!("aevalsrc=exprs='{expr}':s=48000:d={secs}:c=stereo"), "-c:a", "pcm_f32le", out]);
    })
}

// ---------------------------------------------------------------------------------------------
// Pixels (review render_pixels.rs)
// ---------------------------------------------------------------------------------------------

/// Review #1: c3 starts at 0.1 + 0.2 = 0.30000000000000004 s; frame 9 (0.3 s) must show it.
#[test]
fn cut_after_float_accumulated_start_shows_the_new_clip() {
    let p = base(16, 16, json!([
        {"id": "c1", "track": "v1", "start": 0.0, "duration": 0.1, "source": {"type": "solid", "color": "#FF0000"}},
        {"id": "c2", "track": "v1", "start": {"after": "c1"}, "duration": 0.2, "source": {"type": "solid", "color": "#00FF00"}},
        {"id": "c3", "track": "v1", "start": {"after": "c2"}, "duration": 0.5, "source": {"type": "solid", "color": "#0000FF"}},
    ]));
    let r = renderer();
    assert_eq!(px(&frame(&r, &p, 9.0 / 30.0), 8, 8), [0, 0, 255, 255]);
    assert_eq!(px(&frame(&r, &p, 3.0 / 30.0), 8, 8), [0, 255, 0, 255], "c2 starts at 0.1 exactly");
    assert_eq!(px(&frame(&r, &p, 8.0 / 30.0), 8, 8), [0, 255, 0, 255]);
    // layers_at agrees, and the mixer's clip edges use the same tolerance.
    let res = resolved(&p);
    let l = r.layers_at(Path::new("."), &p, &res, 9.0 / 30.0);
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].clip, "c3");
}

/// Review #7: partial edge blocks are averaged over their on-canvas pixels.
#[test]
fn pixelate_keeps_partial_edge_blocks_opaque() {
    let p = base(100, 100, json!([
        {"id": "c1", "track": "v1", "start": 0.0, "duration": 1.0, "source": {"type": "solid", "color": "#FFFFFF"},
         "effects": [{"kind": "pixelate", "params": {"size": 30}}]},
    ]));
    let img = frame(&renderer(), &p, 0.0);
    assert_eq!(px(&img, 15, 15), [255, 255, 255, 255]);
    assert_eq!(px(&img, 95, 15), [255, 255, 255, 255]);
    assert_eq!(px(&img, 95, 95), [255, 255, 255, 255]);
}

/// Review #14: sharpening a flat clip that does not fill the frame adds no rim at its edge.
#[test]
fn sharpen_leaves_flat_clip_edges_flat() {
    let p = base(200, 200, json!([
        {"id": "c1", "track": "v1", "start": 0.0, "duration": 1.0, "source": {"type": "solid", "color": "#808080"},
         "transform": {"scale": 0.5}, "effects": [{"kind": "sharpen", "params": {"amount": 2.0, "radius": 2.0}}]},
    ]));
    let img = frame(&renderer(), &p, 0.0);
    assert!((px(&img, 100, 100)[0] as i32 - 128).abs() <= 1);
    for x in [50, 51, 148, 149] {
        let q = px(&img, x, 100);
        assert!((q[0] as i32 - 128).abs() <= 2, "column {x}: {q:?}");
    }
}

/// Review #18: the pivot of a flipped clip with an off-centre anchor is the transform's fixed point,
/// and corners are listed in visual order.
#[test]
fn layers_at_pivot_and_corners_of_flipped_clips() {
    let mk = |scale: f64| {
        base(100, 100, json!([
            {"id": "c1", "track": "v1", "start": 0.0, "duration": 1.0,
             "source": {"type": "shape", "kind": "rect", "width": 40, "height": 20, "fill": "#FFFFFF"},
             "transform": {"anchor_x": 0.0, "flip_x": true, "scale": scale}},
        ]))
    };
    let r = renderer();
    for scale in [1.0, 2.0] {
        let p = mk(scale);
        let l = &r.layers_at(Path::new("."), &p, &resolved(&p), 0.0)[0];
        assert!((l.bounds[0] - 30.0).abs() < 1e-3, "left edge stays: {:?}", l.bounds);
        assert!((l.pivot[0] - 30.0).abs() < 1e-3 && (l.pivot[1] - 50.0).abs() < 1e-3, "{:?}", l.pivot);
        // Top-left corner is the visual top-left.
        assert!((l.corners[0][0] - 30.0).abs() < 1e-3 && l.corners[1][0] > l.corners[0][0]);
        // And the drawing matches the box.
        let img = frame(&r, &p, 0.0);
        let ink = bbox(&img, |q| q[0] > 128).unwrap();
        let right = (l.bounds[0] + l.bounds[2]).min(100.0);
        assert!((ink[0] as f32 - l.bounds[0]).abs() <= 1.0 && (ink[2] as f32 - right).abs() <= 1.0, "{ink:?} vs {:?}", l.bounds);
    }
}

// ---------------------------------------------------------------------------------------------
// Timing (review render_timing.rs)
// ---------------------------------------------------------------------------------------------

#[test]
fn transition_uses_exact_handle_frames() {
    let b = bars(30, 12);
    let p = proj(json!({
        "format": 1, "settings": {"width": 320, "height": 180, "fps": 30}, "media": [bars_asset(&b, 12.0)],
        "tracks": [{"id": "v1", "kind": "video"}],
        "clips": [
            {"id": "a", "track": "v1", "start": 0.0, "duration": 1.0, "source": {"type": "media", "asset": "m1", "in_point": 2.0}},
            {"id": "b", "track": "v1", "start": {"after": "a"}, "duration": 1.0, "source": {"type": "media", "asset": "m1", "in_point": 5.0}},
        ],
        "transitions": [{"id": "t1", "from": "a", "to": "b", "kind": "wipe_down", "duration": 0.4, "params": {"softness": 0.0}}],
    }));
    let r = renderer();
    for k in [25u32, 27, 29, 30, 31, 33] {
        let img = frame(&r, &p, k as f64 / 30.0);
        assert_eq!(code(&img, 0, 160, 320, 20), 60 + k, "frame {k}: A (bottom) plays its handle");
        assert_eq!(code(&img, 0, 0, 320, 20), 120 + k, "frame {k}: B (top) plays its handle");
    }
}

/// Review #4: a reversed clip over source [2, 3) s shows frames 89, 88, ..., 60.
#[test]
fn reversed_clip_shows_its_source_range_exactly() {
    let b = bars(30, 12);
    let p = proj(json!({
        "format": 1, "settings": {"width": 320, "height": 180, "fps": 30}, "media": [bars_asset(&b, 12.0)],
        "tracks": [{"id": "v1", "kind": "video"}],
        "clips": [{"id": "c1", "track": "v1", "start": 0.0, "duration": 1.0, "reverse": true, "source": {"type": "media", "asset": "m1", "in_point": 2.0}}],
    }));
    let r = renderer();
    let shown: Vec<u32> = (0..30).map(|k| code(&frame(&r, &p, k as f64 / 30.0), 0, 0, 320, 180)).collect();
    assert_eq!(shown, (0..30).map(|k| 89 - k).collect::<Vec<u32>>());
}

// ---------------------------------------------------------------------------------------------
// Robustness (review render_robust.rs)
// ---------------------------------------------------------------------------------------------

/// Renders on a worker thread; "ok", "error: ..", "PANIC: .." or "HANG".
fn try_render(clips: Value, transitions: Value, t: f64, secs: u64) -> String {
    let p = proj(json!({"format": 1, "settings": {"width": 64, "height": 36, "fps": 30}, "tracks": [{"id": "v1", "kind": "video"}],
        "clips": clips, "transitions": transitions}));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let r = resolve(&p);
            renderer().frame(Path::new("."), &p, &r, t, &FrameOptions::default()).map(|_| ())
        }));
        let msg = match res {
            Ok(Ok(())) => "ok".to_string(),
            Ok(Err(e)) => format!("error: {e}"),
            Err(e) => format!("PANIC: {}", e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default()),
        };
        let _ = tx.send(msg);
    });
    rx.recv_timeout(Duration::from_secs(secs)).unwrap_or_else(|_| format!("HANG (>{secs}s)"))
}

/// Review #5 (debug and release): absurd effect sizes neither panic nor hang.
#[test]
fn huge_effect_sizes_do_not_panic_or_hang() {
    let solid = |e: Value| json!([{"id": "c1", "track": "v1", "start": 0.0, "duration": 1.0, "source": {"type": "solid", "color": "#808080"}, "transform": {"scale": 0.5}, "effects": [e]}]);
    let mut cases: Vec<(String, Value, Value, f64)> = [
        json!({"kind": "blur", "params": {"radius": 1e9}}),
        json!({"kind": "glow", "params": {"radius": 1e9, "intensity": 1.0}}),
        json!({"kind": "drop_shadow", "params": {"blur": 1e9}}),
        json!({"kind": "drop_shadow", "params": {"distance": 1e12}}),
        json!({"kind": "sharpen", "params": {"radius": 1e9}}),
        json!({"kind": "pixelate", "params": {"size": 1e10}}),
        json!({"kind": "rgb_split", "params": {"amount": 1e12}}),
        json!({"kind": "shake", "params": {"amount": 1e12}}),
        json!({"kind": "zoom_blur", "params": {"amount": 1e9}}),
        json!({"kind": "mask_rect", "params": {"feather": 1e12, "radius": 1e12, "width": 1e9}}),
    ]
    .into_iter()
    .map(|e| (e.to_string(), solid(e), json!([]), 0.0))
    .collect();
    cases.push((
        "text shadow blur 1e9".into(),
        json!([{"id": "c1", "track": "v1", "start": 0.0, "duration": 1.0, "source": {"type": "text", "text": "Hi", "size": 20, "shadow": {"color": "#000000", "blur": 1e9, "x": 1e12}, "stroke": {"color": "#FF0000", "width": 1e9}}}]),
        json!([]),
        0.0,
    ));
    cases.push((
        "blur_dissolve radius 1e9".into(),
        json!([
            {"id": "a", "track": "v1", "start": 0.0, "duration": 1.0, "source": {"type": "solid", "color": "#FF0000"}},
            {"id": "b", "track": "v1", "start": {"after": "a"}, "duration": 1.0, "source": {"type": "solid", "color": "#0000FF"}},
        ]),
        json!([{"id": "t1", "from": "a", "to": "b", "kind": "blur_dissolve", "duration": 1.0, "params": {"radius": 1e9}}]),
        1.0,
    ));
    let bad: Vec<String> = cases
        .into_iter()
        .filter_map(|(name, clips, tr, t)| {
            let r = try_render(clips, tr, t, 20);
            (r.starts_with("PANIC") || r.starts_with("HANG")).then(|| format!("{name}: {r}"))
        })
        .collect();
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn odd_text_and_shapes_do_not_panic() {
    let sources = [
        json!({"type": "text", "text": "", "background": {"color": "#000000"}}),
        json!({"type": "text", "text": "\n\n\r\n"}),
        json!({"type": "text", "text": "Hello", "font": "No Such Font 123"}),
        json!({"type": "text", "text": "Hello", "letter_spacing": 1e9}),
        json!({"type": "text", "text": "Hello world", "max_width": 1.0}),
        json!({"type": "text", "text": "Hello", "size": 0.0}),
        json!({"type": "text", "text": "a\nb", "line_height": 0.0}),
        json!({"type": "text", "text": "héllo 👋🏽 שלום مرحبا", "reveal": {"kind": "typewriter", "duration": 1.0},
               "highlight": {"color": "#FFFF00", "words": [[0, 1], [1, 2]], "background": "#000000"}}),
        json!({"type": "text", "text": "a b c", "reveal": {"kind": "pop_words", "duration": 0.5}}),
        json!({"type": "shape", "kind": "ellipse", "width": 1e9, "height": 1e9, "fill": "#FFFFFF", "stroke": {"color": "#FF0000", "width": 1e6}}),
        json!({"type": "shape", "kind": "arrow", "width": 0.0, "height": 0.0}),
        json!({"type": "shape", "kind": "rect", "width": 10.0, "height": 10.0, "radius": 1e9, "stroke": {"color": "#FF0000", "width": 2.0}, "draw": 0.5}),
    ];
    let bad: Vec<String> = sources
        .into_iter()
        .filter_map(|src| {
            let r = try_render(json!([{"id": "c1", "track": "v1", "start": 0.0, "duration": 1.0, "source": src.clone()}]), json!([]), 0.3, 20);
            (r.starts_with("PANIC") || r.starts_with("HANG")).then(|| format!("{src}: {r}"))
        })
        .collect();
    assert!(bad.is_empty(), "{bad:#?}");
}

// ---------------------------------------------------------------------------------------------
// Export (review render_export.rs)
// ---------------------------------------------------------------------------------------------

fn settings(v: Value) -> ExportSettings {
    serde_json::from_value(v).unwrap()
}

/// Decodes every frame of a file to 320x180 grey and reads the bars code.
fn codes_of(file: &Path) -> Vec<u32> {
    let out = std::process::Command::new(&aiframes_media::tools().unwrap().ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(file)
        .args(["-vf", "scale=320:180", "-f", "rawvideo", "-pix_fmt", "gray", "pipe:1"])
        .output()
        .unwrap();
    out.stdout
        .chunks(320 * 180)
        .map(|f| (0..9).filter(|k| f[90 * 320 + ((*k as f64 + 0.5) * 320.0 / 9.0) as usize] > 128).map(|k| 1 << k).sum())
        .collect()
}

fn bars_project(b: &Path, dur: f64) -> Project {
    proj(json!({
        "format": 1, "settings": {"width": 320, "height": 180, "fps": 30}, "media": [bars_asset(b, 10.0)],
        "tracks": [{"id": "v1", "kind": "video"}],
        "clips": [{"id": "c1", "track": "v1", "start": 0.0, "duration": dur, "source": {"type": "media", "asset": "m1"}}],
    }))
}

#[test]
fn export_range_has_exact_frames() {
    let _g = serial();
    let p = bars_project(&bars(30, 4), 4.0);
    let out = scratch("reg-range").join("range.mp4");
    let s = settings(json!({"codec": "h264", "quality": "draft", "hardware": false, "loudness_lufs": null, "range": [0.5, 1.5]}));
    export(Path::new("."), &p, pool(), &s, &out, &mut |_| true).unwrap();
    let codes = codes_of(&out);
    assert_eq!((codes.len(), codes[0], *codes.last().unwrap()), (30, 15, 44));
}

/// Review #10: an odd size is rounded up to even for 4:2:0 codecs; the extra column/row is the
/// background colour (not encoder black) and the report states the real file size. (The review's
/// literal "file is 101x75" cannot hold: H.264 4:2:0 needs even dimensions.)
#[test]
fn export_odd_size_pads_with_background_and_reports_file_size() {
    let _g = serial();
    let p = proj(json!({
        "format": 1, "settings": {"width": 101, "height": 75, "fps": 30, "background": "#FFFFFF"},
        "tracks": [{"id": "v1", "kind": "video"}],
        "clips": [{"id": "c1", "track": "v1", "start": 0.0, "duration": 0.5, "source": {"type": "solid", "color": "#FFFFFF"}}],
    }));
    let out = scratch("reg-odd").join("odd.mp4");
    let s = settings(json!({"codec": "h264", "quality": "high", "hardware": false, "loudness_lufs": null}));
    let rep = export(Path::new("."), &p, pool(), &s, &out, &mut |_| true).unwrap();
    let probe = ffprobe_json(&out);
    let v = probe["streams"].as_array().unwrap().iter().find(|s| s["codec_type"] == "video").unwrap().clone();
    assert_eq!((rep.width, rep.height), (102, 76));
    assert_eq!((v["width"].as_u64(), v["height"].as_u64()), (Some(102), Some(76)));
    let f = fresh_pool("reg-odd").video_frame(&out, 0.0, None).unwrap();
    for (x, y) in [(50, 37), (101, 37), (50, 75), (101, 75)] {
        let q = f.pixel(x, y).unwrap();
        assert!(q[0] > 245 && q[1] > 245 && q[2] > 245, "({x},{y}) = {q:?}: padding must be the background");
    }
}

#[test]
fn export_cancel_cleans_up() {
    let _g = serial();
    let p = bars_project(&bars(30, 4), 4.0);
    let dir = scratch("reg-cancel");
    let out = dir.join("cancel.mp4");
    let s = settings(json!({"codec": "h264", "quality": "draft", "hardware": false, "loudness_lufs": -14.0}));
    let r = export(Path::new("."), &p, pool(), &s, &out, &mut |pr| !(pr.stage == "video" && pr.frame >= 10));
    assert!(matches!(r, Err(aiframes_render::RenderError::Cancelled)), "{r:?}");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "nothing left in the output folder");
    let pid = std::process::id();
    let temp_wavs = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&format!("aiframes-export-{pid}-")))
        .count();
    assert_eq!(temp_wavs, 0);
    #[cfg(windows)]
    {
        let o = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!("Get-CimInstance Win32_Process -Filter \"ParentProcessId={pid}\" | Where-Object {{ $_.Name -eq 'ffmpeg.exe' -and $_.CommandLine -like '*reg-cancel*' }} | ForEach-Object {{ $_.ProcessId }}")])
            .output()
            .unwrap();
        // Decode sessions of the shared pool (other tests) may live on; the export's encoder must not.
        let kids = String::from_utf8_lossy(&o.stdout).split_whitespace().count();
        assert_eq!(kids, 0, "the export's ffmpeg encoder is still running");
    }
}

/// Review #11: a peaky programme (loud transients over a quiet bed) lands on -14 and -9 LUFS under
/// a -1.5 dBTP ceiling.
#[test]
fn loudness_normalisation_lands_on_target_with_a_busy_limiter() {
    let _g = serial();
    let w = wav("peaky", "0.02*sin(2*PI*220*t)+0.9*sin(2*PI*1800*t)*exp(-40*mod(t,0.5))", 12.0);
    let p = proj(json!({
        "format": 1, "settings": {"width": 64, "height": 36, "fps": 30}, "media": [audio_asset("a1m", &w, 12.0)],
        "tracks": [{"id": "a1", "kind": "audio"}],
        "clips": [{"id": "c1", "track": "a1", "start": 0.0, "duration": 12.0, "source": {"type": "media", "asset": "a1m"}}],
    }));
    let dir = scratch("reg-loud");
    for target in [-14.0, -9.0] {
        let out = dir.join(format!("loud{}.wav", -target as i32));
        let s = settings(json!({"codec": "wav", "loudness_lufs": target, "true_peak_db": -1.5}));
        let rep = export(Path::new("."), &p, pool(), &s, &out, &mut |_| true).unwrap();
        let m = aiframes_media::loudness_of_file(&out).unwrap();
        assert!((m.integrated_lufs - target).abs() <= 0.5, "target {target}: {:.2}", m.integrated_lufs);
        assert!(m.true_peak_db <= -1.5 + 0.2, "true peak {:.2}", m.true_peak_db);
        assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
    }
    // An unreachable target is reported, not silently missed.
    let out = dir.join("impossible.wav");
    let s = settings(json!({"codec": "wav", "loudness_lufs": 0.0, "true_peak_db": -20.0}));
    let rep = export(Path::new("."), &p, pool(), &s, &out, &mut |_| true).unwrap();
    assert!(rep.warnings.iter().any(|w| w.contains("loudness")), "{:?}", rep.warnings);
}

/// Review #12: H.264 wider than NVENC allows (4160 px) falls back to the software encoder.
#[test]
fn nvenc_h264_wide_export_falls_back_to_software() {
    let _g = serial();
    let p = proj(json!({
        "format": 1, "settings": {"width": 4160, "height": 64, "fps": 30},
        "tracks": [{"id": "v1", "kind": "video"}],
        "clips": [{"id": "c1", "track": "v1", "start": 0.0, "duration": 0.2, "source": {"type": "solid", "color": "#336699"}}],
    }));
    let out = scratch("reg-nvenc").join("wide.mp4");
    let s = settings(json!({"codec": "h264", "quality": "standard", "hardware": true, "loudness_lufs": null}));
    let rep = export(Path::new("."), &p, pool(), &s, &out, &mut |_| true).unwrap();
    assert_eq!(rep.encoder, "libx264");
    assert!(out.is_file());
}

#[test]
fn png_sequence_reexport_removes_stale_frames() {
    let _g = serial();
    let mk = |dur: f64| base(32, 18, json!([{"id": "c1", "track": "v1", "start": 0.0, "duration": dur, "source": {"type": "solid", "color": "#FF0000"}}]));
    let dir = scratch("reg-png");
    let out = dir.join("seq");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("notes.txt"), "keep me").unwrap();
    let s = settings(json!({"codec": "png_sequence", "loudness_lufs": null}));
    export(Path::new("."), &mk(1.0), pool(), &s, &out, &mut |_| true).unwrap();
    let r2 = export(Path::new("."), &mk(0.5), pool(), &s, &out, &mut |_| true).unwrap();
    let pngs = std::fs::read_dir(&out).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".png")).count();
    assert_eq!(pngs as u64, r2.frames);
    assert!(out.join("notes.txt").is_file(), "other files are left alone");
}

/// Follow-up: semi-transparent pixels are composited over the background before an opaque codec.
#[test]
fn transparent_background_is_flattened_for_opaque_codecs() {
    let _g = serial();
    let mut p = base(64, 36, json!([{"id": "c1", "track": "v1", "start": 0.0, "duration": 0.3, "opacity": 0.5, "source": {"type": "solid", "color": "#FFFFFF"}}]));
    p.settings.background = aiframes_core::Color::rgba8(0, 0, 0, 0);
    let out = scratch("reg-flat").join("flat.mp4");
    let s = settings(json!({"codec": "h264", "hardware": false, "loudness_lufs": null}));
    export(Path::new("."), &p, pool(), &s, &out, &mut |_| true).unwrap();
    let f = fresh_pool("reg-flat").video_frame(&out, 0.0, None).unwrap();
    let q = f.pixel(32, 18).unwrap();
    assert!((q[0] as i32 - 128).abs() <= 6, "50% white over a transparent (black) background is grey, got {q:?}");
}

// ---------------------------------------------------------------------------------------------
// Audio (review render_audio.rs)
// ---------------------------------------------------------------------------------------------

#[test]
fn mix_is_block_size_invariant_with_ducking() {
    let music = wav("music440", "0.3*sin(2*PI*440*t)", 8.0);
    let voice = wav("voicebursts", "if(lt(mod(t,1.3),0.6),0.4*sin(2*PI*900*t),0)", 8.0);
    let p = proj(json!({
        "format": 1, "settings": {"width": 64, "height": 36, "fps": 30},
        "media": [audio_asset("m", &music, 8.0), audio_asset("v", &voice, 8.0)],
        "tracks": [{"id": "a1", "kind": "audio"}, {"id": "a2", "kind": "audio"}],
        "clips": [
            // A fractional in-point exercises the speed-1 path's source position.
            {"id": "c1", "track": "a1", "start": 0.0, "duration": 8.0, "source": {"type": "media", "asset": "m", "in_point": 0.123_456_789}},
            {"id": "c2", "track": "a2", "start": 0.5, "duration": 7.0, "source": {"type": "media", "asset": "v"}},
        ],
        "ducking": [{"id": "d1", "target": "a1", "trigger": "a2", "amount_db": -14.0, "attack": 0.12, "release": 0.45}],
    }));
    let res = resolved(&p);
    let r = renderer();
    let (t0, n) = (1.0, 48_000 * 4);
    let mut whole = vec![0f32; n * 2];
    r.mix(Path::new("."), &p, &res, t0, &mut whole).unwrap();
    let mut pieces = vec![0f32; n * 2];
    let (mut pos, mut k) = (0usize, 0usize);
    while pos < n {
        let len = [1, 37, 512, 4096, 999, 2048][k % 6].min(n - pos);
        r.mix(Path::new("."), &p, &res, t0 + pos as f64 / 48_000.0, &mut pieces[pos * 2..(pos + len) * 2]).unwrap();
        pos += len;
        k += 1;
    }
    let maxdiff = whole.iter().zip(&pieces).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    assert!(maxdiff < 1e-5, "block-size dependent mix: {maxdiff}");
}

#[test]
fn speed_two_doubles_pitch_over_the_right_range() {
    let tone = wav("tone440_long", "0.5*sin(2*PI*440*t)", 10.0);
    let p = proj(json!({
        "format": 1, "settings": {"width": 64, "height": 36, "fps": 30}, "media": [audio_asset("m", &tone, 10.0)],
        "tracks": [{"id": "a1", "kind": "audio"}],
        "clips": [{"id": "c1", "track": "a1", "start": 0.0, "duration": 2.0, "speed": 2.0, "source": {"type": "media", "asset": "m", "in_point": 1.0}}],
    }));
    let mut out = vec![0f32; 48_000 * 2];
    renderer().mix_track(Path::new("."), &p, &resolved(&p), "a1", 0.5, &mut out).unwrap();
    let left: Vec<f32> = out.iter().step_by(2).copied().collect();
    let zc = left.windows(2).filter(|w| (w[0] <= 0.0) != (w[1] <= 0.0)).count();
    assert!((zc as i64 - 1760).abs() <= 4, "{zc}");
    assert!((rms(&left) - 0.5 / 2f32.sqrt()).abs() < 0.03);
}

/// Review #13: replacing the trigger's file on disk re-computes the ducking levels.
#[test]
fn ducking_follows_a_replaced_trigger_file() {
    let a = wav("voice_at_1s", "if(between(t,1,2),0.5*sin(2*PI*700*t),0)", 8.0);
    let b = wav("voice_at_4s", "if(between(t,4,5),0.5*sin(2*PI*700*t),0)", 8.0);
    let music = wav("music440", "0.3*sin(2*PI*440*t)", 8.0);
    let dir = scratch("reg-duckstale");
    let voice = dir.join("voice.wav");
    std::fs::copy(&a, &voice).unwrap();
    let p = proj(json!({
        "format": 1, "settings": {"width": 64, "height": 36, "fps": 30},
        "media": [audio_asset("m", &music, 8.0), audio_asset("v", &voice, 8.0)],
        "tracks": [{"id": "a1", "kind": "audio"}, {"id": "a2", "kind": "audio"}],
        "clips": [
            {"id": "c1", "track": "a1", "start": 0.0, "duration": 8.0, "source": {"type": "media", "asset": "m"}},
            {"id": "c2", "track": "a2", "start": 0.0, "duration": 8.0, "source": {"type": "media", "asset": "v"}},
        ],
        "ducking": [{"id": "d1", "target": "a1", "trigger": "a2", "amount_db": -20.0}],
    }));
    let res = resolved(&p);
    let r = Renderer::new(fresh_pool("reg-duckstale"));
    let level = |t: f64| {
        let mut out = vec![0f32; 4800 * 2];
        r.mix_track(Path::new("."), &p, &res, "a1", t, &mut out).unwrap();
        rms(&out)
    };
    assert!(level(1.5) < 0.05 && level(4.5) > 0.2);
    std::thread::sleep(Duration::from_millis(1200));
    std::fs::copy(&b, &voice).unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    let (m1, m4) = (level(1.5), level(4.5));
    assert!(m1 > 0.2 && m4 < 0.05, "ducking follows the new voice: {m1:.3} at 1.5 s, {m4:.3} at 4.5 s");
}

/// Follow-up: a mixer clip starting on a float-accumulated time keeps its first samples.
#[test]
fn mixer_keeps_the_first_sample_of_float_accumulated_starts() {
    let tone = wav("tone440_long", "0.5*sin(2*PI*440*t)", 10.0);
    let p = proj(json!({
        "format": 1, "settings": {"width": 64, "height": 36, "fps": 30}, "media": [audio_asset("m", &tone, 10.0)],
        "tracks": [{"id": "v1", "kind": "video"}, {"id": "a1", "kind": "audio"}],
        "clips": [
            {"id": "c1", "track": "a1", "start": 0.0, "duration": 0.1, "muted": true, "source": {"type": "media", "asset": "m"}},
            {"id": "c2", "track": "a1", "start": {"after": "c1"}, "duration": 0.2, "muted": true, "source": {"type": "media", "asset": "m"}},
            {"id": "c3", "track": "a1", "start": {"after": "c2"}, "duration": 0.5, "source": {"type": "media", "asset": "m", "in_point": 2.0}},
        ],
    }));
    let mut out = vec![0f32; 2 * 4];
    renderer().mix_track(Path::new("."), &p, &resolved(&p), "a1", 0.3, &mut out).unwrap();
    // Sample 0 of c3 is source 2.0 s: 0.5*sin(2*PI*440*2) = 0; sample 1 is non-zero.
    assert!(out[2].abs() > 0.01, "c3's first samples are mixed: {out:?}");
}

/// Follow-up: a caption word written with a no-break space ("New York") is one highlight word.
#[test]
fn no_break_space_joins_a_highlight_word() {
    let mut p = project(640, 360, 30.0);
    p.clips = vec![clip(json!({"id": "t", "track": "v2", "start": 0, "duration": 3, "source": {"type": "text",
        "text": "in New\u{00A0}York today", "size": 50, "highlight": {"color": "#FF0000", "words": [[0, 1], [1, 2], [2, 3]]}}}))];
    let r = renderer();
    let red = |q: [u8; 4]| q[0] > 180 && q[1] < 90;
    let img = frame(&r, &p, 1.5);
    let hl = bbox(&img, red).unwrap();
    let all = bbox(&img, |q| q[0] > 100).unwrap();
    // "New York" (the middle word) is highlighted as a whole: wider than a third of the line.
    assert!((hl[2] - hl[0]) as f64 > (all[2] - all[0]) as f64 * 0.35, "{hl:?} vs {all:?}");
    let img = frame(&r, &p, 2.5);
    let last = bbox(&img, red).unwrap();
    assert!(last[0] > hl[2], "the third window lights \"today\"");
}
