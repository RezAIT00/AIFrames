mod common;

use aiframes_media::{
    analyze_beats, detect_beats, detect_scenes, detect_silence, loudness_of_file, loudness_of_pcm, write_wav, AudioData, MediaPool,
};
use common::*;
use std::process::Command;

/// First stereo frame index (from `from`) whose left sample exceeds `thr` in magnitude.
fn onset(a: &AudioData, from_sec: f64, len_sec: f64, thr: f32) -> Option<i64> {
    let n = (len_sec * a.rate() as f64) as usize;
    let mut buf = vec![0f32; n * 2];
    a.read(from_sec, &mut buf);
    let start = (from_sec * a.rate() as f64).round() as i64;
    buf.as_chunks::<2>().0.iter().position(|f| f[0].abs() > thr).map(|i| start + i as i64)
}

#[test]
fn audio_read_is_sample_accurate() {
    let pool = shared_pool();
    let a = pool.audio(&burst_wav(), 48_000).unwrap();
    assert_eq!(a.rate(), 48_000);
    assert_eq!(a.frames(), 144_000);
    assert!((a.duration() - 3.0).abs() < 1e-9);
    assert!(a.has_audio());
    assert_eq!(onset(&a, 0.5, 1.0, 0.25), Some(48_000));
    // reads straddling the onset at odd offsets
    for start in [47_990i64, 47_999, 48_000, 48_001] {
        let mut b = vec![0f32; 64];
        a.read_frames(start, &mut b);
        let first = b.as_chunks::<2>().0.iter().position(|f| f[0].abs() > 0.25).map(|i| start + i as i64);
        assert_eq!(first, Some(48_000.max(start)), "start {start}");
        if start <= 48_000 {
            assert!((b[(48_000 - start) as usize * 2] - 0.5).abs() < 1e-6, "peak at sample 48000");
        }
    }
    // outside the file: zeros
    let mut b = vec![1f32; 100];
    a.read(-1.0, &mut b);
    assert!(b.iter().all(|v| *v == 0.0));
    a.read(10.0, &mut b);
    assert!(b.iter().all(|v| *v == 0.0));

    // resampled: onset stays at 1.0 s within 2 samples
    let r = pool.audio(&burst_wav(), 44_100).unwrap();
    let o = onset(&r, 0.5, 1.0, 0.25).unwrap();
    assert!((o - 44_100).abs() <= 2, "44.1k onset at {o}");
    assert!((r.duration() - 3.0).abs() < 1e-3);
}

#[test]
fn audio_stays_in_sync_with_video_through_aac_and_offsets() {
    // The burst is at 1.0 s in the WAV and the audio track starts 0.5 s late: source time 1.5 s.
    let pool = shared_pool();
    let a = pool.audio(&av_offset(), 48_000).unwrap();
    let o = onset(&a, 1.0, 1.0, 0.3).unwrap() as f64 / 48_000.0;
    assert!((o - 1.5).abs() < 0.002, "burst at {o:.5}s, expected 1.5s");
    // and the leading 0.5 s is silence
    let mut b = vec![1f32; 48_000];
    a.read(0.0, &mut b);
    assert!(b.iter().all(|v| v.abs() < 1e-3));
}

#[test]
fn mono_plays_on_both_channels_at_unity() {
    let pool = shared_pool();
    let a = pool.audio(&mono_wav(), 48_000).unwrap();
    let mut b = vec![0f32; 48_000];
    a.read(0.5, &mut b);
    let (mut l, mut r) = (0f32, 0f32);
    for f in b.as_chunks::<2>().0 {
        assert!((f[0] - f[1]).abs() < 1e-6);
        l = l.max(f[0].abs());
        r = r.max(f[1].abs());
    }
    assert!((l - 0.25).abs() < 0.01 && (r - 0.25).abs() < 0.01, "{l} {r}");
}

#[test]
fn long_files_are_file_backed_and_random_access_is_exact() {
    let dir = scratch("long-audio");
    let pool = MediaPool::with_cache_dir(16 << 20, &dir);
    let a = pool.audio(&long_wav(), 48_000).unwrap();
    assert_eq!(a.frames(), 60 * 48_000);
    let mut x: u64 = 99;
    let mut buf = vec![0f32; 4096 * 2];
    // Random access while the progressive decode runs, then from the finished cache file.
    for i in 0..400 {
        if i == 200 {
            a.wait_complete().unwrap();
            assert!(a.is_complete());
            let cache_files: Vec<_> = std::fs::read_dir(dir.join("audio")).unwrap().flatten().collect();
            assert_eq!(cache_files.len(), 1, "{cache_files:?}");
            assert!(cache_files[0].file_name().to_string_lossy().ends_with("-48000.f32"));
            assert_eq!(cache_files[0].metadata().unwrap().len(), 60 * 48_000 * 8);
        }
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let start = (x >> 33) as i64 % (60 * 48_000);
        assert!(a.read_frames_wait(start, &mut buf, std::time::Duration::from_secs(20)));
        for (i, f) in buf.as_chunks::<2>().0.iter().enumerate().step_by(97) {
            let n = start + i as i64;
            if n >= 60 * 48_000 {
                assert_eq!(f[0], 0.0);
                continue;
            }
            let t = n as f64 / 48_000.0;
            let l = 0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).sin();
            let r = 0.25 * (2.0 * std::f64::consts::PI * 97.0 * t).sin();
            assert!((f[0] as f64 - l).abs() < 1e-4 && (f[1] as f64 - r).abs() < 1e-4, "frame {n}: {f:?} vs {l} {r}");
        }
    }
    // a second pool reuses the disk cache
    let pool2 = MediaPool::with_cache_dir(16 << 20, &dir);
    let t = std::time::Instant::now();
    let b = pool2.audio(&long_wav(), 48_000).unwrap();
    assert!(t.elapsed().as_millis() < 500);
    assert_eq!(b.frames(), a.frames());
}

#[test]
fn files_without_audio_read_as_silence() {
    let pool = shared_pool();
    let a = pool.audio(&cfr(), 48_000).unwrap();
    assert!(!a.has_audio());
    assert_eq!(a.frames(), 0);
    assert!((a.duration() - 12.0).abs() < 0.05);
    let mut b = vec![1f32; 1000];
    a.read(3.0, &mut b);
    assert!(b.iter().all(|v| *v == 0.0));
    let w = pool.waveform(&cfr()).unwrap();
    assert!(w.is_empty());
    assert!(pool.audio(&cfr(), 0).is_err());
}

#[test]
fn waveform_peaks_and_disk_cache() {
    let dir = scratch("waveform");
    let pool = MediaPool::with_cache_dir(16 << 20, &dir);
    let w = pool.waveform(&burst_wav()).unwrap();
    assert_eq!(w.bins_per_sec, 200.0);
    assert_eq!(w.len(), 600);
    assert!((w.duration - 3.0).abs() < 1e-9);
    assert_eq!(w.range(0.0, 0.99), (0.0, 0.0));
    let (mn, mx) = w.range(1.05, 1.45);
    assert!((mn + 0.5).abs() < 0.01 && (mx - 0.5).abs() < 0.01, "{mn} {mx}");
    assert!((w.rms_range(1.1, 1.4) - 0.5 / 2f32.sqrt()).abs() < 0.01);
    assert_eq!(w.range(2.0, 3.0), (0.0, 0.0));
    let again = pool.waveform(&burst_wav()).unwrap();
    assert!(std::sync::Arc::ptr_eq(&w, &again));
    assert_eq!(std::fs::read_dir(dir.join("waveform")).unwrap().count(), 1);
    let pool2 = MediaPool::with_cache_dir(16 << 20, &dir);
    assert_eq!(*pool2.waveform(&burst_wav()).unwrap(), *w);
}

#[test]
fn silence_detection() {
    let s = detect_silence(&silence_wav(), -40.0, 0.5).unwrap();
    assert_eq!(s.len(), 2, "{s:?}");
    for (got, want) in s.iter().zip([[1.0, 2.5], [4.0, 5.0]]) {
        assert!((got[0] - want[0]).abs() < 0.015 && (got[1] - want[1]).abs() < 0.015, "{s:?}");
    }
    // the 0.2 s pause shows up with a shorter minimum
    let s = detect_silence(&silence_wav(), 40.0, 0.1).unwrap();
    assert_eq!(s.len(), 3, "{s:?}");
    // a video without audio is one long silence
    let s = detect_silence(&cfr(), -40.0, 0.5).unwrap();
    assert_eq!(s.len(), 1);
    assert!((s[0][1] - 12.0).abs() < 0.05);
}

#[test]
fn scene_detection() {
    let cuts = detect_scenes(&scenes(), 0.3).unwrap();
    assert_eq!(cuts.len(), 2, "{cuts:?}");
    assert!((cuts[0] - 2.0).abs() < 0.041 && (cuts[1] - 4.0).abs() < 0.041, "{cuts:?}");
    // percentage form
    assert_eq!(detect_scenes(&scenes(), 30.0).unwrap(), cuts);
    assert!(detect_scenes(&burst_wav(), 0.3).is_err());
}

#[test]
fn beat_detection_on_click_track() {
    let b = analyze_beats(&clicks_wav()).unwrap();
    assert!((b.bpm - 100.0).abs() < 1.5, "bpm {}", b.bpm);
    let expected: Vec<f64> = (0..20).map(|k| 0.25 + 0.6 * k as f64).filter(|t| *t < 11.9).collect();
    let matched = expected.iter().filter(|e| b.beats.iter().any(|x| (x - *e).abs() < 0.015)).count();
    assert!(matched >= expected.len() * 9 / 10, "{matched}/{}: {:?}", expected.len(), b.beats);
    assert!(b.onsets.len() >= expected.len() * 9 / 10);
    assert_eq!(detect_beats(&clicks_wav()).unwrap(), b.beats);
    assert!(detect_beats(&cfr()).is_err());
}

/// ffmpeg's own EBU R128 measurement: (integrated, true peak, LRA).
fn ffmpeg_ebur128(path: &std::path::Path) -> (f64, f64, f64) {
    let out = Command::new(&aiframes_media::tools().unwrap().ffmpeg)
        .args(["-hide_banner", "-nostdin", "-nostats", "-i"])
        .arg(path)
        .args(["-af", "ebur128=peak=true", "-f", "null", "-"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stderr);
    let summary = &text[text.rfind("Summary:").expect("ebur128 summary")..];
    let grab = |key: &str| -> f64 {
        let i = summary.find(key).unwrap_or_else(|| panic!("{key} in {summary}")) + key.len();
        summary[i..].split_whitespace().next().unwrap().parse().unwrap()
    };
    (grab("I:"), grab("Peak:"), grab("LRA:"))
}

#[test]
fn loudness_matches_ffmpeg_ebur128() {
    for path in [sine20_wav(), swing_wav()] {
        let ours = loudness_of_file(&path).unwrap();
        let (i, tp, lra) = ffmpeg_ebur128(&path);
        assert!((ours.integrated_lufs - i).abs() < 0.2, "{}: I {} vs ffmpeg {i}", path.display(), ours.integrated_lufs);
        assert!((ours.true_peak_db - tp).abs() < 0.3, "{}: TP {} vs ffmpeg {tp}", path.display(), ours.true_peak_db);
        assert!((ours.lra - lra).abs() < 1.0, "{}: LRA {} vs ffmpeg {lra}", path.display(), ours.lra);
        assert!(ours.max_momentary_lufs >= ours.integrated_lufs - 0.2);
    }
    let s = loudness_of_file(&sine20_wav()).unwrap();
    assert!((s.integrated_lufs + 20.0).abs() < 0.3, "{s:?}");
    assert!(s.lra < 0.5);
    // PCM entry point agrees with the file path
    let pool = shared_pool();
    let a = pool.audio(&sine20_wav(), 48_000).unwrap();
    let mut pcm = vec![0f32; a.frames() as usize * 2];
    a.read_frames(0, &mut pcm);
    let p = loudness_of_pcm(&pcm, 48_000);
    assert!((p.integrated_lufs - s.integrated_lufs).abs() < 0.05);
    // silence
    assert_eq!(loudness_of_file(&cfr()).unwrap().integrated_lufs, -70.0);
}

#[test]
fn wav_writer_round_trip() {
    let dir = scratch("wav");
    let p = dir.join("mix.wav");
    let rate = 48_000;
    let pcm: Vec<f32> = (0..rate * 2).flat_map(|i| {
        let v = 0.3 * (i as f32 * 0.05).sin();
        [v, -v]
    }).collect();
    write_wav(&p, &pcm, rate as u32).unwrap();
    let (k, info) = aiframes_media::probe(&p).unwrap();
    assert_eq!(k, aiframes_core::model::MediaKind::Audio);
    assert_eq!((info.audio_channels, info.audio_rate), (2, 48_000));
    assert!((info.duration - 2.0).abs() < 1e-3);
    let pool = common::pool("wav", 1 << 20);
    let a = pool.audio(&p, 48_000).unwrap();
    let mut back = vec![0f32; pcm.len()];
    a.read_frames(0, &mut back);
    assert_eq!(back, pcm);
}
