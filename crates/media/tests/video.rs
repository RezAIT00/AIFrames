mod common;

use aiframes_core::model::MediaKind;
use aiframes_media::{probe, MediaError};
use common::*;
use std::path::Path;

#[test]
fn probe_video_audio_image() {
    let (k, i) = probe(&cfr()).unwrap();
    assert_eq!(k, MediaKind::Video);
    assert_eq!((i.width, i.height), (320, 180));
    assert_eq!(i.fps, 30.0);
    assert!((i.duration - 12.0).abs() < 0.05, "{}", i.duration);
    assert!(i.has_video && !i.has_audio);
    assert_eq!(i.codec, "h264");

    let (_, n) = probe(&ntsc()).unwrap();
    assert_eq!(n.fps, 29.97);

    let (_, v) = probe(&vfr()).unwrap();
    assert!(v.fps > 20.0 && v.fps <= 31.0, "vfr fps {}", v.fps);

    let (k, a) = probe(&burst_wav()).unwrap();
    assert_eq!(k, MediaKind::Audio);
    assert!(a.has_audio && !a.has_video);
    assert_eq!((a.audio_channels, a.audio_rate), (2, 48000));
    assert!((a.duration - 3.0).abs() < 1e-3);

    let (k, av) = probe(&av_offset()).unwrap();
    assert_eq!(k, MediaKind::Video);
    assert!(av.has_audio && av.has_video);

    for img in [png(), jpg()] {
        let (k, i) = probe(&img).unwrap();
        assert_eq!(k, MediaKind::Image, "{}", img.display());
        assert_eq!((i.width, i.height), (64, 48));
        assert_eq!(i.duration, 0.0);
    }

    let (_, r) = probe(&rotated()).unwrap();
    assert_eq!((r.width, r.height), (180, 320));
    assert_eq!(r.rotation, 270);
}

#[test]
fn probe_errors_are_actionable() {
    let missing = root().join("does-not-exist.mp4");
    let e = probe(&missing).unwrap_err();
    assert!(matches!(e, MediaError::NotFound { .. }), "{e}");
    assert!(e.to_string().contains("does-not-exist.mp4"));

    let dir = scratch("probe-errors");
    let junk = dir.join("junk.mp4");
    std::fs::write(&junk, b"this is not a video file at all").unwrap();
    let e = probe(&junk).unwrap_err();
    let s = e.to_string();
    assert!(s.contains("junk.mp4") && s.contains("ffprobe"), "{s}");
}

/// Timestamp tolerance of `video_frame`: a frame starting within 0.5 ms after `t` counts as at `t`.
const EPS: f64 = 0.0005;

/// Ground truth for the 30 fps fixture: frame shown at t.
fn cfr_frame_at(t: f64) -> u32 {
    (((t + EPS) * 30.0).floor() as i64).clamp(0, 359) as u32
}

#[test]
fn frame_accuracy_sequential_forward_and_backward() {
    let pool = common::pool("seq", 64 << 20);
    let path = cfr();
    // Exact frame times, mid-frame times and just-before-next times, forward.
    for n in 0..360u32 {
        for (frac, label) in [(0.0, "start"), (0.5, "mid"), (0.97, "end")] {
            let t = (n as f64 + frac) / 30.0;
            let f = pool.video_frame(&path, t, None).unwrap();
            assert_eq!(code(&f), n, "forward {label} t={t:.6}");
            assert!((f.time - n as f64 / 30.0).abs() < 1e-3, "pts {} for frame {n}", f.time);
        }
    }
    // Backward over the whole file.
    let pool = common::pool("seq-back", 64 << 20);
    for n in (0..360u32).rev().step_by(7) {
        let t = n as f64 / 30.0 + 0.01;
        assert_eq!(code(&pool.video_frame(&path, t, None).unwrap()), n, "backward t={t}");
    }
    // Clamping at both ends.
    assert_eq!(code(&pool.video_frame(&path, -5.0, None).unwrap()), 0);
    assert_eq!(code(&pool.video_frame(&path, 999.0, None).unwrap()), 359);
    assert_eq!(code(&pool.video_frame(&path, f64::NAN, None).unwrap()), 0);
}

#[test]
fn frame_accuracy_random_jumps() {
    let pool = common::pool("random", 32 << 20);
    let path = cfr();
    let mut x: u64 = 0x1234_5678;
    for i in 0..120 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let t = (x % 12_000) as f64 / 1000.0;
        let f = pool.video_frame(&path, t, None).unwrap();
        assert_eq!(code(&f), cfr_frame_at(t), "jump {i} t={t}");
    }
    let s = pool.stats();
    assert!(s.seeks > 10, "{s:?}");
}

#[test]
fn frame_accuracy_ntsc_rate() {
    let pool = shared_pool();
    let path = ntsc();
    let dur = 1001.0 / 30000.0;
    for n in (0..119u32).chain((0..119u32).rev().step_by(5)) {
        let t = n as f64 * dur; // exact frame boundary
        assert_eq!(code(&pool.video_frame(&path, t, None).unwrap()), n, "t={t}");
        let t2 = (n as f64 + 0.98) * dur; // 0.67 ms before the next frame (outside the tolerance)
        assert_eq!(code(&pool.video_frame(&path, t2, None).unwrap()), n, "t2={t2}");
    }
}

#[test]
fn frame_accuracy_variable_frame_rate() {
    let pool = common::pool("vfr", 64 << 20);
    let path = vfr();
    let truth = |t: f64| -> u32 {
        let ms = t * 1000.0;
        (0..VFR_FRAMES).rev().find(|&n| vfr_pts_ms(n) as f64 <= ms + 1e-6).unwrap_or(0)
    };
    // forward through every gap edge
    for n in 0..VFR_FRAMES - 1 {
        let a = vfr_pts_ms(n) as f64 / 1000.0;
        let b = vfr_pts_ms(n + 1) as f64 / 1000.0;
        for t in [a, (a + b) / 2.0, b - 0.0015] {
            let f = pool.video_frame(&path, t, None).unwrap();
            assert_eq!(code(&f), truth(t), "vfr forward t={t}");
            assert!((f.time - a).abs() < 1e-3, "pts {} vs {a}", f.time);
        }
    }
    // random access into the middle of long gaps
    let pool = common::pool("vfr-rand", 64 << 20);
    for n in [119u32, 4, 60, 9, 59, 3, 100, 14] {
        let t = vfr_pts_ms(n) as f64 / 1000.0 + 0.030;
        assert_eq!(code(&pool.video_frame(&path, t, None).unwrap()), truth(t), "vfr random t={t}");
    }
}

#[test]
fn scaled_frames_keep_aspect_and_timing() {
    let pool = shared_pool();
    let path = cfr();
    let f = pool.video_frame(&path, 2.0, Some((160, 90))).unwrap();
    assert_eq!((f.width, f.height), (160, 90));
    assert_eq!(f.rgba.len(), 160 * 90 * 4);
    assert_eq!(code(&f), 60);
    let f = pool.video_frame(&path, 3.5, Some((100, 100))).unwrap();
    assert_eq!((f.width, f.height), (100, 56));
    assert_eq!(code(&f), 105);
    // never upscaled
    let f = pool.video_frame(&path, 1.0, Some((4000, 4000))).unwrap();
    assert_eq!((f.width, f.height), (320, 180));
    // straight RGBA, opaque
    assert_eq!(f.pixel(0, 0).unwrap()[3], 255);

    let r = pool.video_frame(&rotated(), 1.0, None).unwrap();
    assert_eq!((r.width, r.height), (180, 320));
}

#[test]
fn cache_is_bounded_and_eviction_is_correct() {
    let frame_bytes = 320 * 180 * 4 + 64;
    let pool = common::pool("evict", frame_bytes * 5);
    let path = cfr();
    for n in 0..40u32 {
        let f = pool.video_frame(&path, n as f64 / 30.0, None).unwrap();
        assert_eq!(code(&f), n);
        let s = pool.stats();
        assert!(s.cache_bytes <= s.cache_budget, "{s:?}");
        assert!(s.cached_frames <= 5, "{s:?}");
    }
    // repeated request hits the cache
    let before = pool.stats();
    let f = pool.video_frame(&path, 39.0 / 30.0, None).unwrap();
    assert_eq!(code(&f), 39);
    let after = pool.stats();
    assert_eq!(after.cache_hits, before.cache_hits + 1);
    assert_eq!(after.frames_decoded, before.frames_decoded);
    // an evicted frame is decoded again, correctly
    assert_eq!(code(&pool.video_frame(&path, 0.0, None).unwrap()), 0);
    pool.set_cache_bytes(0);
    assert_eq!(pool.stats().cache_bytes, 0);
}

#[test]
fn concurrent_sessions_for_different_files_and_ranges() {
    let pool = common::pool("concurrent", 128 << 20);
    let a = cfr();
    let b = ntsc();
    let mut hs = Vec::new();
    for (i, (path, fps, frames)) in [(a.clone(), 30.0, 360u32), (a.clone(), 30.0, 360), (b.clone(), 30000.0 / 1001.0, 119)].into_iter().enumerate() {
        let pool = pool.clone();
        hs.push(std::thread::spawn(move || {
            let start = [0u32, 200, 10][i];
            for n in start..(start + 100).min(frames) {
                let t = n as f64 / fps + 0.001;
                let f = pool.video_frame(&path, t, Some((160, 90))).unwrap();
                assert_eq!(code(&f), n, "thread {i} t={t}");
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    // the two interleaved readers of the same file used separate sessions instead of thrashing
    let s = pool.stats();
    assert!(s.seeks < 40, "{s:?}");
    assert!(s.sessions >= 2, "{s:?}");
    pool.clear();
    assert_eq!(pool.stats().sessions, 0);
}

#[test]
fn alternating_two_points_in_one_file_does_not_thrash() {
    // A transition between two parts of the same file: requests alternate between two times.
    let pool = common::pool("transition", 4 << 20);
    let path = cfr();
    for n in 0..60u32 {
        let t1 = 1.0 + n as f64 / 30.0;
        let t2 = 8.0 + n as f64 / 30.0;
        assert_eq!(code(&pool.video_frame(&path, t1, Some((160, 90))).unwrap()), 30 + n);
        assert_eq!(code(&pool.video_frame(&path, t2, Some((160, 90))).unwrap()), 240 + n);
    }
    let s = pool.stats();
    assert!(s.seeks <= 4, "{s:?}");
}

#[test]
fn images_and_alpha() {
    let pool = shared_pool();
    let f = pool.image(&png(), None).unwrap();
    assert_eq!((f.width, f.height), (64, 48));
    // same pixels as a direct decode
    let direct = image::open(png()).unwrap().into_rgba8();
    assert_eq!(f.rgba.as_slice(), direct.as_raw().as_slice());
    let small = pool.image(&jpg(), Some((32, 32))).unwrap();
    assert_eq!((small.width, small.height), (32, 24));
    // video_frame on a still returns the still
    let v = pool.video_frame(&png(), 5.0, None).unwrap();
    assert_eq!((v.width, v.height), (64, 48));

    let a = pool.image(&alpha_png(), Some((32, 24))).unwrap();
    assert_eq!((a.width, a.height), (32, 24));
    let p = a.pixel(28, 20).unwrap(); // semi-transparent red region
    assert!(p[0] > 240 && p[1] < 10 && p[2] < 10, "red stays red after resize: {p:?}");
    assert!((p[3] as i32 - 128).abs() <= 2, "{p:?}");
    let edge = a.pixel(3, 3).unwrap(); // inside the opaque blue square (8x8 after the resize)
    assert_eq!(edge[3], 255);
    assert!(edge[2] > 240, "{edge:?}");

    let e = pool.video_frame(&burst_wav(), 0.0, None).unwrap_err();
    assert!(matches!(e, MediaError::Unsupported { .. }), "{e}");
}

#[test]
fn webm_with_alpha_keeps_its_transparency() {
    let path = alpha_webm();
    let pool = shared_pool();
    let f = pool.video_frame(&path, 0.5, None).unwrap();
    assert_eq!((f.width, f.height), (64, 48));
    let clear = f.pixel(8, 24).unwrap();
    assert!(clear[3] < 10, "left half is transparent: {clear:?}");
    let solid = f.pixel(56, 24).unwrap();
    assert!(solid[3] > 245, "right half is opaque: {solid:?}");
    assert!(solid[0] > 200 && solid[1] < 60, "and red: {solid:?}");
}

#[test]
fn thumbnails_are_small_cached_and_on_disk() {
    let dir = scratch("thumb-cache");
    let pool = aiframes_media::MediaPool::with_cache_dir(32 << 20, &dir);
    let path = cfr();
    let t0 = std::time::Instant::now();
    let f = pool.thumbnail(&path, 5.0, 36).unwrap();
    let first = t0.elapsed();
    assert_eq!((f.width, f.height), (64, 36));
    assert_eq!(code(&f), 150);
    let t1 = std::time::Instant::now();
    let g = pool.thumbnail(&path, 5.02, 36).unwrap(); // same 0.1 s grid cell
    assert!(t1.elapsed() < first);
    assert!(std::sync::Arc::ptr_eq(&f.rgba, &g.rgba));
    let n_files = std::fs::read_dir(dir.join("thumbs")).unwrap().count();
    assert_eq!(n_files, 1);
    // a new pool reads it from disk without decoding
    let pool2 = aiframes_media::MediaPool::with_cache_dir(32 << 20, &dir);
    let h = pool2.thumbnail(&path, 5.0, 36).unwrap();
    assert_eq!(h.rgba, f.rgba);
    assert_eq!(pool2.stats().seeks, 0);
    // a filmstrip across the file
    for i in 0..12 {
        let t = i as f64;
        let f = pool.thumbnail(&path, t, 36).unwrap();
        assert_eq!(code(&f), (t * 30.0) as u32, "thumb at {t}");
    }
    // stills scale to the height
    let s = pool.thumbnail(&png(), 0.0, 24).unwrap();
    assert_eq!((s.width, s.height), (32, 24));
}

#[test]
fn release_forgets_a_file() {
    let pool = common::pool("release", 16 << 20);
    let path = cfr();
    pool.video_frame(&path, 1.0, None).unwrap();
    assert!(pool.stats().cached_frames > 0);
    pool.release(Path::new(&path));
    let s = pool.stats();
    assert_eq!(s.cached_frames, 0);
    assert_eq!(s.sessions, 0);
    assert_eq!(code(&pool.video_frame(&path, 1.0, None).unwrap()), 30);
}

#[test]
fn stepping_backward_frame_by_frame_is_mostly_cache_hits() {
    let pool = common::pool("backstep", 256 << 20);
    let path = cfr();
    assert_eq!(code(&pool.video_frame(&path, 9.0, Some((160, 90))).unwrap()), 270);
    let seeks0 = pool.stats().seeks;
    for k in 1..=60u32 {
        let t = (270 - k) as f64 / 30.0;
        assert_eq!(code(&pool.video_frame(&path, t, Some((160, 90))).unwrap()), 270 - k, "step back {k}");
    }
    let s = pool.stats();
    // one re-seek per second of stepping (1 s window), not one per frame
    assert!(s.seeks - seeks0 <= 3, "{s:?}");
}

#[test]
fn gpu_decode_modes_give_identical_timing() {
    // GPU paths are opt-in; whether or not this machine has one, frames must be the same ones
    // (a failing GPU path falls back to the CPU).
    for hw in [aiframes_media::HwAccel::Cuda, aiframes_media::HwAccel::D3d11va] {
        let pool = common::pool(&format!("hw-{hw:?}"), 32 << 20);
        pool.set_hwaccel(hw);
        let path = cfr();
        for n in [0u32, 1, 2, 47, 48, 49, 200, 359, 100, 5] {
            let t = n as f64 / 30.0 + 0.002;
            assert_eq!(code(&pool.video_frame(&path, t, Some((160, 90))).unwrap()), n, "{hw:?} t={t}");
        }
    }
}

#[test]
fn overlapping_crossfade_in_one_file_does_not_thrash() {
    // Clip B's source runs 1 s behind clip A's (within the backward-step distance).
    let pool = common::pool("overlap", 64 << 20);
    let path = cfr();
    for n in 0..60u32 {
        let a = 4.0 + n as f64 / 30.0;
        let b = 3.0 + n as f64 / 30.0;
        assert_eq!(code(&pool.video_frame(&path, a, Some((160, 90))).unwrap()), 120 + n);
        assert_eq!(code(&pool.video_frame(&path, b, Some((160, 90))).unwrap()), 90 + n);
    }
    let s = pool.stats();
    assert!(s.seeks <= 4, "{s:?}");
}
