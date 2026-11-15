//! Export round trips: probe duration/size/fps, loudness target, ranges, cancel, WAV, cleanup.

mod common;
use aiframes_core::{ExportSettings, Project};
use aiframes_render::{export, ExportProgress, RenderError};
use common::*;
use serde_json::json;
use std::path::Path;

/// Export tests check the temp folder, so they run one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

fn edit() -> Project {
    let mut p = project(640, 360, 30.0);
    add_media(&mut p, "v", &bars(30, 6));
    add_media(&mut p, "music", &sine_wav("sine440_m10", 440.0, 0.3, 6.0));
    p.clips = vec![
        clip(json!({"id": "c1", "track": "v1", "start": 0, "duration": 2.0, "muted": true, "source": {"type": "media", "asset": "v"}})),
        clip(json!({"id": "t1", "track": "v2", "start": 0.5, "duration": 1.0, "source": {"type": "text", "text": "Export", "size": 60}})),
        clip(json!({"id": "a1", "track": "a1", "start": 0, "duration": 2.0, "fade_out": 0.5, "source": {"type": "media", "asset": "music"}})),
    ];
    p
}

fn fps_of(stream: &serde_json::Value) -> f64 {
    let s = stream["r_frame_rate"].as_str().unwrap();
    let (a, b) = s.split_once('/').unwrap();
    a.parse::<f64>().unwrap() / b.parse::<f64>().unwrap()
}

fn temp_wavs() -> usize {
    std::fs::read_dir(std::env::temp_dir())
        .map(|rd| rd.flatten().filter(|e| e.file_name().to_string_lossy().starts_with(&format!("aiframes-export-{}-", std::process::id()))).count())
        .unwrap_or(0)
}

#[test]
fn export_round_trip_hits_size_fps_duration_and_loudness() {
    let _g = serial();
    let p = edit();
    let dir = scratch("export");
    let out = dir.join("out.mp4");
    let s = ExportSettings { loudness_lufs: Some(-16.0), true_peak_db: -1.0, ..ExportSettings::default() };
    let mut stages: Vec<&'static str> = Vec::new();
    let mut last = 0u64;
    let report = export(Path::new("."), &p, pool(), &s, &out, &mut |pr: ExportProgress| {
        if stages.last() != Some(&pr.stage) {
            stages.push(pr.stage);
        }
        if pr.stage == "video" {
            assert!(pr.frame >= last);
            last = pr.frame;
        }
        true
    })
    .unwrap();
    assert_eq!(report.frames, 60);
    #[cfg(windows)]
    assert!(report.peak_memory_bytes.is_some_and(|b| b > 1 << 20), "{:?}", report.peak_memory_bytes);
    assert_eq!((report.width, report.height), (640, 360));
    assert!((report.duration - 2.0).abs() < 1e-9);
    for st in ["prepare", "audio", "loudness", "video", "mux", "done"] {
        assert!(stages.contains(&st), "stage {st} reported: {stages:?}");
    }
    let l = report.loudness.unwrap();
    assert!((l.integrated_lufs + 16.0).abs() < 0.5, "{l:?}");
    assert!(l.true_peak_db <= -1.0 + 0.1, "{l:?}");
    assert_eq!(temp_wavs(), 0, "temporary WAV removed");
    let probe = ffprobe_json(&out);
    let streams = probe["streams"].as_array().unwrap();
    let v = streams.iter().find(|s| s["codec_type"] == "video").unwrap();
    let a = streams.iter().find(|s| s["codec_type"] == "audio").unwrap();
    assert_eq!((v["width"].as_u64(), v["height"].as_u64()), (Some(640), Some(360)));
    assert!((fps_of(v) - 30.0).abs() < 0.01);
    let dur: f64 = probe["format"]["duration"].as_str().unwrap().parse().unwrap();
    assert!((dur - 2.0).abs() < 0.08, "{dur}");
    assert_eq!(a["sample_rate"].as_str(), Some("48000"));
    let lf = aiframes_media::loudness_of_file(&out).unwrap();
    assert!((lf.integrated_lufs + 16.0).abs() < 0.5, "file loudness {lf:?}");
    // The first frame of the file shows frame 0 of the bars at the right place.
    let pool = fresh_pool("export-read");
    let f = pool.video_frame(&out, 1.0, None).unwrap();
    let img = aiframes_render::Image { width: f.width, height: f.height, rgba: f.rgba.to_vec() };
    assert_eq!(code(&img, 0, 0, 640, 360), 30);
}

#[test]
fn export_range_wav_and_cancel() {
    let _g = serial();
    let p = edit();
    let dir = scratch("export2");
    // Range.
    let out = dir.join("range.mp4");
    let s = ExportSettings { range: Some([0.5, 1.5]), loudness_lufs: None, hardware: false, quality: "draft".into(), ..ExportSettings::default() };
    let rep = export(Path::new("."), &p, pool(), &s, &out, &mut |_| true).unwrap();
    assert_eq!(rep.frames, 30);
    assert!(rep.encoder.contains("264"));
    let dur: f64 = ffprobe_json(&out)["format"]["duration"].as_str().unwrap().parse().unwrap();
    assert!((dur - 1.0).abs() < 0.08, "{dur}");
    // WAV only.
    let wav = dir.join("mix.wav");
    let s = ExportSettings { codec: "wav".into(), loudness_lufs: Some(-20.0), ..ExportSettings::default() };
    let rep = export(Path::new("."), &p, pool(), &s, &wav, &mut |_| true).unwrap();
    let l = aiframes_media::loudness_of_file(&wav).unwrap();
    assert!((l.integrated_lufs + 20.0).abs() < 0.5, "{l:?}");
    assert!((rep.loudness.unwrap().integrated_lufs + 20.0).abs() < 0.5);
    // Cancel during video: no output file.
    let out = dir.join("cancel.mp4");
    let s = ExportSettings::default();
    let res = export(Path::new("."), &p, pool(), &s, &out, &mut |pr| !(pr.stage == "video" && pr.frame >= 5));
    assert!(matches!(res, Err(RenderError::Cancelled)), "{res:?}");
    assert!(!out.exists());
    assert_eq!(temp_wavs(), 0);
    // Unknown codec and empty range are errors.
    let s = ExportSettings { codec: "mpeg1".into(), ..ExportSettings::default() };
    assert!(export(Path::new("."), &p, pool(), &s, &dir.join("x.mp4"), &mut |_| true).is_err());
    let s = ExportSettings { range: Some([3.0, 3.0]), ..ExportSettings::default() };
    assert!(export(Path::new("."), &p, pool(), &s, &dir.join("y.mp4"), &mut |_| true).is_err());
}

#[test]
fn export_fails_loudly_on_missing_media() {
    let _g = serial();
    let mut p = edit();
    p.media[0].path = "C:/nope/missing.mp4".into();
    let dir = scratch("export3");
    let out = dir.join("m.mp4");
    let err = export(Path::new("."), &p, pool(), &ExportSettings::default(), &out, &mut |_| true).unwrap_err();
    assert!(err.to_string().contains("missing") || err.to_string().contains("offline"), "{err}");
    assert!(!out.exists());
}
