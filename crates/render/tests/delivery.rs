//! The delivered file's loudness, as ffmpeg's ebur128 filter measures it, meets the export settings
//! even through a lossy AAC encode (which overshoots true peaks).

mod common;
use aiframes_core::{ExportSettings, Project};
use aiframes_render::export;
use common::*;
use serde_json::json;
use std::path::Path;

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// (integrated LUFS, true peak dBTP) of a file's audio per ffmpeg `ebur128=peak=true`.
fn ffmpeg_ebur128(path: &Path) -> (f64, f64) {
    let out = std::process::Command::new(&aiframes_media::tools().unwrap().ffmpeg)
        .args(["-hide_banner", "-nostats", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-af", "ebur128=peak=true:metadata=1,ametadata=mode=print:file=-", "-f", "null", "-"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let last = |key: &str| -> f64 {
        text.lines().filter_map(|l| l.trim().strip_prefix(key)).filter_map(|v| v.parse::<f64>().ok()).next_back().unwrap()
    };
    (last("lavfi.r128.I="), 20.0 * last("lavfi.r128.true_peak=").log10())
}

/// Loud high-frequency transients over a quiet bed: hard for the limiter.
fn peaky_project() -> Project {
    let wav = fixture("peaky_hf.wav", |out| {
        ffmpeg(&[
            "-f", "lavfi", "-i",
            "aevalsrc=exprs='0.03*sin(2*PI*220*t)+0.95*sin(2*PI*3100*t)*exp(-30*mod(t,0.4))+0.4*sin(2*PI*7300*t)*exp(-60*mod(t+0.13,0.4))':s=48000:d=10:c=stereo",
            "-c:a", "pcm_f32le", out,
        ]);
    });
    audio_project(&wav)
}

/// Pink-noise bursts: AAC overshoots their limited peaks by 1-2 dB and lowers their loudness, so
/// the delivered file needs the post-encode correction passes.
fn noise_bursts_project() -> Project {
    let wav = fixture("noise_bursts.wav", |out| {
        ffmpeg(&[
            "-f", "lavfi", "-i",
            "anoisesrc=color=pink:amplitude=0.9:d=10:seed=5,volume='0.1+0.9*lt(mod(t,0.7),0.15)':eval=frame,aformat=channel_layouts=stereo",
            "-ar", "48000", "-c:a", "pcm_f32le", out,
        ]);
    });
    audio_project(&wav)
}

fn audio_project(wav: &Path) -> Project {
    serde_json::from_value(json!({
        "format": 1,
        "settings": {"width": 64, "height": 36, "fps": 30},
        "media": [{"id": "m", "path": wav.to_string_lossy(), "kind": "audio", "info": {"duration": 10.0, "has_audio": true, "audio_channels": 2, "audio_rate": 48000}}],
        "tracks": [{"id": "v1", "kind": "video"}, {"id": "a1", "kind": "audio"}],
        "clips": [
            {"id": "bg", "track": "v1", "start": 0.0, "duration": 10.0, "source": {"type": "solid", "color": "#203040"}},
            {"id": "c1", "track": "a1", "start": 0.0, "duration": 10.0, "source": {"type": "media", "asset": "m"}},
        ],
    }))
    .unwrap()
}

#[test]
fn aac_delivery_meets_true_peak_and_loudness() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let dir = scratch("delivery");
    let cases = [("peaky", peaky_project(), -14.0, -1.5), ("peaky", peaky_project(), -9.0, -1.0), ("noise", noise_bursts_project(), -14.0, -1.5), ("noise", noise_bursts_project(), -12.0, -1.0)];
    for (name, p, target, ceiling) in cases {
        let out = dir.join(format!("{name}{}.mp4", -target as i32));
        let s = ExportSettings { loudness_lufs: Some(target), true_peak_db: ceiling, audio_bitrate_kbps: 256, hardware: false, quality: "draft".into(), ..ExportSettings::default() };
        let rep = export(Path::new("."), &p, pool(), &s, &out, &mut |_| true).unwrap();
        let (i, tp) = ffmpeg_ebur128(&out);
        assert!(tp <= ceiling + 1e-3, "target {target}: delivered true peak {tp:.2} dBTP over the {ceiling} ceiling");
        assert!((i - target).abs() <= 0.2, "target {target}: delivered {i:.2} LUFS");
        // The report states what was delivered.
        let l = rep.loudness.unwrap();
        assert!((l.integrated_lufs - i).abs() < 0.01 && (l.true_peak_db - tp).abs() < 0.01, "report {l:?} vs file {i:.3} / {tp:.3}");
        assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
        // Video untouched by the audio passes: still the full length at 30 fps.
        let probe = ffprobe_json(&out);
        let v = probe["streams"].as_array().unwrap().iter().find(|s| s["codec_type"] == "video").unwrap().clone();
        assert_eq!(v["nb_frames"].as_str(), Some("300"));
    }
}

#[test]
fn wav_delivery_is_verified_too() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let p = peaky_project();
    let out = scratch("delivery-wav").join("mix.wav");
    let s = ExportSettings { codec: "wav".into(), loudness_lufs: Some(-12.0), true_peak_db: -1.0, ..ExportSettings::default() };
    let rep = export(Path::new("."), &p, pool(), &s, &out, &mut |_| true).unwrap();
    let (i, tp) = ffmpeg_ebur128(&out);
    assert!(tp <= -1.0 + 1e-3 && (i + 12.0).abs() <= 0.2, "{i:.2} LUFS / {tp:.2} dBTP");
    let l = rep.loudness.unwrap();
    assert!((l.true_peak_db - tp).abs() < 0.01);
}
