//! Files with several sound streams (OBS game/mic/chat tracks, cameras with two mics): every
//! stream is probed and decoded on its own, with its own caches.

mod common;

use aiframes_media::{detect_silence, detect_silence_stream, loudness_of_file_stream, probe, AudioData, MediaPool};
use common::*;
use std::path::PathBuf;

/// Video + three sound streams with distinct tones: 0 "Game" 440 Hz stereo at 0.2, 1 "Mic" 660 Hz
/// mono 44.1 kHz at 0.4 with a silence from 2 to 4 s, 2 "Chat" 5.1 with 1 kHz at 0.5 in the centre
/// channel only.
fn streams(ext: &str, codec: &str) -> PathBuf {
    fixture(&format!("streams-{codec}.{ext}"), |out| {
        ffmpeg(&[
            "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=30:duration=6",
            "-f", "lavfi", "-i", "aevalsrc=exprs='0.2*sin(2*PI*440*t)':s=48000:d=6:c=stereo",
            "-f", "lavfi", "-i", "aevalsrc=exprs='0.4*sin(2*PI*660*t)*not(between(t,2,4))':s=44100:d=6:c=mono",
            "-f", "lavfi", "-i", "aevalsrc=exprs='0|0|0.5*sin(2*PI*1000*t)|0|0|0':s=48000:d=6:c=5.1(side)",
            "-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3:a",
            "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
            "-c:a", codec, "-b:a", "192k",
            "-metadata:s:a:0", "title=Game", "-metadata:s:a:1", "title=Mic", "-metadata:s:a:1", "language=eng", "-metadata:s:a:2", "title=Chat",
            out,
        ]);
    })
}

/// Power of frequency `f` in `x` (mono samples at `rate`), Goertzel, normalised to amplitude.
fn tone(x: &[f32], rate: f64, f: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * f / rate;
    let c = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &v in x {
        let s = v as f64 + c * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    let p = s1 * s1 + s2 * s2 - c * s1 * s2;
    2.0 * p.sqrt() / x.len() as f64
}

/// Left and right channels of `[from, from + len)` seconds.
fn channels(a: &AudioData, from: f64, len: f64) -> (Vec<f32>, Vec<f32>) {
    let n = (len * a.rate() as f64) as usize;
    let mut buf = vec![0f32; n * 2];
    a.read(from, &mut buf);
    let l = buf.iter().step_by(2).copied().collect();
    let r = buf.iter().skip(1).step_by(2).copied().collect();
    (l, r)
}

#[test]
fn every_sound_stream_is_probed_and_plays_its_own_sound() {
    for (ext, codec) in [("mkv", "aac"), ("mp4", "aac"), ("mkv", "flac"), ("mov", "pcm_s16le")] {
        let path = streams(ext, codec);
        let (_, info) = probe(&path).unwrap();
        assert_eq!(info.audio_stream_count(), 3, "{ext}/{codec}");
        let s = &info.audio_streams;
        assert_eq!(s.iter().map(|a| a.channels).collect::<Vec<_>>(), vec![2, 1, 6], "{ext}/{codec}");
        assert_eq!(s[1].rate, 44_100, "{ext}/{codec}");
        assert_eq!(s.iter().map(|a| a.title.as_str()).collect::<Vec<_>>(), vec!["Game", "Mic", "Chat"], "{ext}/{codec}: titles");
        assert_eq!(info.speech_stream(), 1, "the stream titled Mic holds the speech");

        let pool = MediaPool::with_cache_dir(16 << 20, scratch(&format!("streams-{ext}-{codec}")));
        let expect = [(440.0, 0.2), (660.0, 0.4), (1000.0, 0.5 * std::f64::consts::FRAC_1_SQRT_2)];
        for (k, (f, amp)) in expect.iter().enumerate() {
            let a = pool.audio_stream(&path, 48_000, k as u32).unwrap();
            assert!(a.has_audio());
            assert!((a.duration() - 6.0).abs() < 0.05, "{ext}/{codec} stream {k}: {} s", a.duration());
            let (l, r) = channels(&a, 0.5, 1.0);
            for (other, _) in expect.iter().filter(|(g, _)| g != f) {
                assert!(tone(&l, 48_000.0, *other) < 0.01, "{ext}/{codec} stream {k} leaks {other} Hz");
            }
            let (pl, pr) = (tone(&l, 48_000.0, *f), tone(&r, 48_000.0, *f));
            // Mono plays on both sides at unity gain; the 5.1 centre is downmixed to both sides.
            assert!((pl - amp).abs() < 0.05 && (pr - amp).abs() < 0.05, "{ext}/{codec} stream {k}: {f} Hz at L {pl:.3} R {pr:.3}, expected {amp:.3}");
        }
        // Stream 0 is what `audio` always gave.
        let first = pool.audio(&path, 48_000).unwrap();
        assert!(std::sync::Arc::ptr_eq(&first, &pool.audio_stream(&path, 48_000, 0).unwrap()));
        let err = pool.audio_stream(&path, 48_000, 3).unwrap_err().to_string();
        assert!(err.contains("3 sound streams") && err.contains("not stream 3"), "{err}");
    }
}

#[test]
fn waveforms_silence_and_loudness_are_per_stream() {
    let path = streams("mkv", "aac");
    let dir = scratch("streams-wave");
    let pool = MediaPool::with_cache_dir(16 << 20, &dir);
    let w0 = pool.waveform_stream(&path, 0).unwrap();
    let w1 = pool.waveform_stream(&path, 1).unwrap();
    assert!(std::sync::Arc::ptr_eq(&w0, &pool.waveform(&path).unwrap()));
    assert_ne!(*w0, *w1, "two streams draw two waveforms");
    // Reloaded from the disk cache by a fresh pool: the same per-stream peaks.
    let again = MediaPool::with_cache_dir(16 << 20, &dir);
    assert_eq!(*again.waveform_stream(&path, 1).unwrap(), *w1);
    assert_eq!(*again.waveform_stream(&path, 0).unwrap(), *w0);
    assert!(pool.waveform_stream(&path, 5).is_err());

    let quiet = detect_silence_stream(&path, 1, -40.0, 0.5).unwrap();
    assert_eq!(quiet.len(), 1, "{quiet:?}");
    assert!((quiet[0][0] - 2.0).abs() < 0.05 && (quiet[0][1] - 4.0).abs() < 0.05, "{quiet:?}");
    assert!(detect_silence(&path, -40.0, 0.5).unwrap().is_empty(), "stream 0 never goes quiet");
    let l0 = loudness_of_file_stream(&path, 0).unwrap().integrated_lufs;
    let l1 = loudness_of_file_stream(&path, 1).unwrap().integrated_lufs;
    assert!((l0 - l1).abs() > 3.0, "game {l0:.1} LUFS vs mic {l1:.1} LUFS");
}
