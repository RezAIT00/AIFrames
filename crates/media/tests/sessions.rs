//! Decode-session memory: shared readahead budget, short-lived one-off sessions, reuse of larger
//! frames for small requests (round 5).
mod common;

use aiframes_media::MediaPool;
use common::*;
use std::sync::Arc;
use std::time::Duration;

const FRAME: usize = 320 * 180 * 4;

fn fresh(name: &str, cache: usize) -> Arc<MediaPool> {
    MediaPool::with_cache_dir(cache, scratch(&format!("sessions-{name}")))
}

#[test]
fn readahead_budget_is_configurable_shared_and_reported() {
    let pool = fresh("budget", 64 << 20);
    assert_eq!(pool.readahead_bytes(), aiframes_media::DEFAULT_READAHEAD_BYTES);
    assert_eq!(pool.stats().readahead_budget, 128 << 20);
    // Generous budget: a sequential reader earns several frames of readahead.
    let path = cfr();
    for n in 0..60u32 {
        assert_eq!(code(&pool.video_frame(&path, n as f64 / 30.0, None).unwrap()), n);
    }
    std::thread::sleep(Duration::from_millis(200));
    let grown = pool.stats().session_bytes;
    assert!(grown >= 6 * FRAME, "sequential readahead {grown}");
    // Tight budget: three concurrent sequential readers stay within it (plus the frame each holds).
    let pool = fresh("budget-tight", 64 << 20);
    pool.set_readahead_bytes(4 * FRAME);
    assert_eq!(pool.stats().readahead_budget, 4 * FRAME);
    let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hs: Vec<_> = [(cfr(), 0u32), (cfr(), 200), (ntsc(), 0)]
        .into_iter()
        .map(|(path, start)| {
            let (pool, peak) = (pool.clone(), peak.clone());
            std::thread::spawn(move || {
                let fps = aiframes_media::probe(&path).unwrap().1.fps;
                for n in start..start + 90 {
                    pool.video_frame(&path, n as f64 / fps + 0.001, None).unwrap();
                    peak.fetch_max(pool.stats().session_bytes, std::sync::atomic::Ordering::Relaxed);
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    let peak = peak.load(std::sync::atomic::Ordering::Relaxed);
    // each of the 3 sessions holds its current + next frame and at least one queued frame
    assert!(peak <= 4 * FRAME + 3 * 3 * FRAME, "peak session memory {peak} over a {} budget", 4 * FRAME);
}

#[test]
fn one_off_requests_do_not_pile_up_sessions() {
    let pool = fresh("oneoff", 32 << 20);
    let path = cfr();
    let mut peak = 0;
    for i in 0..45 {
        // Spread out: requests at the same size are always more than 2 s apart (one-offs).
        let t = (i as f64 * 2.3 + (i % 3) as f64 * 0.37) % 11.0;
        let size = [None, Some((240, 135)), Some((160, 90))][i % 3];
        let f = pool.video_frame(&path, t, size).unwrap();
        assert_eq!(code(&f), ((t + 0.0005) * 30.0).floor() as u32, "t={t} size {size:?}");
        peak = peak.max(pool.stats().sessions);
    }
    // Sessions used in the last 0.5 s are never closed (they may be layers of one frame), so a
    // rapid burst can hold several, but never more than the cap.
    assert!(peak <= 12, "peak sessions {peak}");
    // Once 0.5 s old, at most two isolated one-off sessions are kept (plus the newest).
    std::thread::sleep(Duration::from_millis(600));
    pool.video_frame(&path, 5.55, Some((160, 90))).unwrap();
    assert!(pool.stats().sessions <= 3, "{:?}", pool.stats());
    // provisional sessions close soon after their last use
    std::thread::sleep(Duration::from_millis(2300));
    pool.video_frame(&ntsc(), 1.0, None).unwrap(); // any call reaps
    assert!(pool.stats().sessions <= 1, "{:?}", pool.stats());
}

#[test]
fn seek_then_play_reuses_the_seek_session() {
    let pool = fresh("seekplay", 32 << 20);
    let path = cfr();
    for n in 150..240u32 {
        assert_eq!(code(&pool.video_frame(&path, n as f64 / 30.0, Some((240, 135))).unwrap()), n);
    }
    assert_eq!(pool.stats().seeks, 1, "{:?}", pool.stats());
}

#[test]
fn small_requests_and_thumbnails_reuse_larger_decoded_frames() {
    let pool = fresh("reuse", 64 << 20);
    let path = cfr();
    for n in 90..100u32 {
        pool.video_frame(&path, n as f64 / 30.0, None).unwrap(); // playback at full size
    }
    let seeks = pool.stats().seeks;
    let small = pool.video_frame(&path, 95.0 / 30.0, Some((80, 45))).unwrap();
    assert_eq!((small.width, small.height), (80, 45));
    assert_eq!(code(&small), 95);
    let th = pool.thumbnail(&path, 3.0, 36).unwrap();
    assert_eq!(code(&th), 90);
    assert_eq!(pool.stats().seeks, seeks, "served from the larger frames, no new decode");
}

#[test]
fn idle_sessions_are_closed_when_over_budget_but_not_active_ones() {
    let pool = fresh("reap", 64 << 20);
    pool.set_readahead_bytes(6 * FRAME);
    let a = cfr();
    let b = ntsc();
    // two sequential streams, then only `a` continues
    for n in 0..40u32 {
        pool.video_frame(&a, n as f64 / 30.0, None).unwrap();
        pool.video_frame(&b, n as f64 * 1001.0 / 30000.0, None).unwrap();
    }
    assert_eq!(pool.stats().sessions, 2);
    std::thread::sleep(Duration::from_millis(700)); // `b` is idle now, both over the tight budget
    for n in 40..70u32 {
        assert_eq!(code(&pool.video_frame(&a, n as f64 / 30.0, None).unwrap()), n);
    }
    let s = pool.stats();
    assert_eq!(s.sessions, 1, "the idle session was closed: {s:?}");
    // the active one kept streaming: no re-seek for `a`
    assert_eq!(s.seeks, 2, "{s:?}");
}

/// Four different files, at least 3 s of 30 fps video each.
fn four_files() -> Vec<std::path::PathBuf> {
    vec![cfr(), ntsc(), rotated(), late_video()]
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// Regression (round 5 → 6): several layers on screen at once (PiP, split screen) must each keep
/// their decode session; the provisional-session cap used to close them every frame.
#[test]
fn four_layers_read_frame_by_frame_never_reseek() {
    let pool = fresh("layers", 128 << 20);
    let files = four_files();
    let mut sets = Vec::new();
    let mut seeks_after_warmup = 0;
    for n in 0..75u32 {
        let t = 1.0 + n as f64 / 30.0 + 0.002;
        let start = std::time::Instant::now();
        for f in &files {
            let fr = pool.video_frame(f, t, Some((160, 90))).unwrap();
            assert!((fr.time - (t - 0.002)).abs() < 0.04, "{}: frame at {} for t={t}", f.display(), fr.time);
        }
        sets.push(start.elapsed().as_secs_f64() * 1000.0);
        if n == 2 {
            seeks_after_warmup = pool.stats().seeks;
        }
    }
    let s = pool.stats();
    assert_eq!(s.seeks, seeks_after_warmup, "re-seeks after warm-up: {s:?}");
    assert_eq!(s.sessions, 4, "{s:?}");
    let m = median(sets[3..].to_vec());
    assert!(m < 15.0, "median {m:.1} ms per 4-layer frame set");
}

#[test]
fn four_concurrent_sequential_readers_never_reseek() {
    let pool = fresh("readers", 128 << 20);
    let hs: Vec<_> = four_files()
        .into_iter()
        .map(|f| {
            let pool = pool.clone();
            std::thread::spawn(move || {
                let mut lat = Vec::new();
                for n in 0..75u32 {
                    let t = 1.0 + n as f64 / 30.0 + 0.002;
                    let start = std::time::Instant::now();
                    pool.video_frame(&f, t, None).unwrap();
                    lat.push(start.elapsed().as_secs_f64() * 1000.0);
                }
                lat
            })
        })
        .collect();
    let lat: Vec<Vec<f64>> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    let s = pool.stats();
    assert_eq!(s.seeks, 4, "one seek per reader, none after: {s:?}");
    for (i, l) in lat.into_iter().enumerate() {
        let m = median(l[2..].to_vec());
        assert!(m < 10.0, "reader {i}: median {m:.2} ms per frame");
    }
}

#[test]
fn uncached_and_low_priority_reads_leave_playback_frames_alone() {
    let frame = 320 * 180 * 4 + 64;
    let pool = fresh("export-cache", 20 * frame);
    let path = cfr();
    for n in 0..20u32 {
        pool.video_frame(&path, n as f64 / 30.0, None).unwrap(); // playback fills the cache
    }
    let full = pool.stats().cached_frames;
    assert_eq!(full, 20);
    // an export reading another part of the same file, uncached: nothing is inserted
    for n in 200..260u32 {
        let f = pool.video_frame_uncached(&path, n as f64 / 30.0, None).unwrap();
        assert_eq!(code(&f), n);
    }
    assert_eq!(pool.stats().cached_frames, 20);
    assert_eq!(pool.stats().cache_low_priority_bytes, 0);
    // low priority: cached (a second reader of the same time hits), but at most half the cache
    for n in 100..160u32 {
        let t = n as f64 / 30.0;
        let a = pool.video_frame_with(&path, t, None, aiframes_media::FrameCaching::LowPriority).unwrap();
        let hits = pool.stats().cache_hits;
        let b = pool.video_frame_with(&path, t, None, aiframes_media::FrameCaching::LowPriority).unwrap();
        assert_eq!(pool.stats().cache_hits, hits + 1);
        assert!(std::sync::Arc::ptr_eq(&a.rgba, &b.rgba));
    }
    let s = pool.stats();
    assert!(s.cache_low_priority_bytes <= s.cache_budget / 2, "{s:?}");
    // count surviving playback frames without inserting anything (uncached reads still hit)
    let playback_left = (0..20u32)
        .filter(|n| {
            let before = pool.stats().cache_hits;
            pool.video_frame_uncached(&path, *n as f64 / 30.0, None).unwrap();
            pool.stats().cache_hits > before
        })
        .count();
    assert!(playback_left >= 10, "half the playback frames survive the export: {playback_left}");
}
