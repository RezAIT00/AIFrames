//! Progressive audio decoding: instant `audio()`, exact reads while decoding, crash-safe cache.
mod common;

use aiframes_media::{AudioData, MediaPool};
use common::*;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LONG: Duration = Duration::from_secs(30);

/// Timing-sensitive tests run alone (they measure latency).
static SERIAL: Mutex<()> = Mutex::new(());
fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

fn fresh(name: &str) -> Arc<MediaPool> {
    MediaPool::with_cache_dir(16 << 20, scratch(&format!("prog-{name}")))
}

fn read(a: &AudioData, start: i64, frames: usize) -> Vec<f32> {
    let mut v = vec![0f32; frames * 2];
    assert!(a.read_frames_wait(start, &mut v, LONG), "read at {start} timed out");
    v
}

fn final_cache_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".f32")).collect()).unwrap_or_default()
}

#[test]
fn progressive_reads_match_the_full_decode_exactly() {
    let _s = serial();
    let path = long_aac();
    // Reference: a complete decode in its own cache.
    let reference = fresh("ref").audio_complete(&path, 48_000).unwrap();
    assert!(reference.is_complete());
    let total = reference.frames() as i64;
    assert!((total - 120 * 48_000).abs() < 2048, "{total}");

    let pool = fresh("exact");
    let regions_before = aiframes_media::audio_regions_started();
    let a = pool.audio(&path, 48_000).unwrap();
    // Far-ahead positions first (served by region decoders), then near ones, odd lengths, the end.
    let mut spots: Vec<(i64, usize)> =
        vec![(100 * 48_000 + 17, 4096), (60 * 48_000, 48_000), (5 * 48_000 + 3, 999), (0, 1024), (115 * 48_000, 7777), (total - 500, 2000), (-300, 1000)];
    let got: Vec<Vec<f32>> = spots.iter().map(|&(s, n)| read(&a, s, n)).collect();
    assert!(aiframes_media::audio_regions_started() > regions_before, "far reads were served by region decoders");
    // Concurrent readers at random positions while decoding continues.
    let mut hs = Vec::new();
    for k in 0..4u64 {
        let a = a.clone();
        hs.push(std::thread::spawn(move || {
            let mut x = 0x9e37_79b9_7f4a_7c15u64 ^ k;
            (0..25)
                .map(|_| {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    let s = (x >> 33) as i64 % (120 * 48_000);
                    (s, read(&a, s, 2048))
                })
                .collect::<Vec<_>>()
        }));
    }
    let mut more = Vec::new();
    for h in hs {
        more.extend(h.join().unwrap());
    }
    for ((s, n), v) in spots.drain(..).zip(got) {
        assert_eq!(v, read(&reference, s, n), "read at frame {s} differs from the full decode");
    }
    for (s, v) in more {
        assert_eq!(v, read(&reference, s, 2048), "concurrent read at frame {s} differs from the full decode");
    }
    a.wait_complete().unwrap();
    assert!(a.is_complete());
    assert_eq!(a.progress(), 1.0);
    assert_eq!(a.frames(), reference.frames());
    // after completion the whole signal is identical too
    let n = a.frames() as usize;
    assert_eq!(read(&a, 0, n), read(&reference, 0, n));
}

#[test]
fn burst_far_ahead_is_found_at_its_exact_sample() {
    let _s = serial();
    let pool = fresh("burst");
    let a = pool.audio(&late_burst_wav(), 48_000).unwrap();
    assert!(!a.is_complete() || a.progress() == 1.0);
    let start = 85 * 48_000 - 4800; // 84.9 s
    let v = read(&a, start, 9600);
    let onset = v.as_chunks::<2>().0.iter().position(|f| f[0].abs() > 0.25).map(|i| start + i as i64);
    assert_eq!(onset, Some(4_080_000));
    assert_eq!(v[4800 * 2], 0.5);
    assert!(a.ready_at(84.95, 0.1));
    // the start of the file (main decoder) is right too
    let head = read(&a, 0, 4800);
    assert!(head.iter().all(|s| s.abs() <= 0.0101));
    a.wait_complete().unwrap();
    assert_eq!(a.frames(), 90 * 48_000);
}

#[test]
fn bounded_wait_ready_at_and_prefetch() {
    let _s = serial();
    let pool = fresh("bounded");
    let path = ten_minutes();
    let a = pool.audio(&path, 48_000).unwrap();
    assert!(!a.is_complete(), "a 10-minute file is decoded in the background");
    assert!(a.progress() < 1.0);
    // Not decoded yet and no time to wait: silence, reported, immediately.
    let mut buf = vec![1f32; 2048];
    let t = Instant::now();
    let ok = a.read_frames_wait(400 * 48_000, &mut buf, Duration::ZERO);
    assert!(t.elapsed() < Duration::from_millis(50), "{:?}", t.elapsed());
    if !ok {
        assert!(buf.iter().all(|v| *v == 0.0));
    }
    // prefetch makes it ready soon without anyone reading
    a.prefetch(300.0);
    let t = Instant::now();
    while !a.ready_at(300.0, 1.0) {
        assert!(t.elapsed() < Duration::from_secs(5), "prefetched range never became ready");
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut buf = vec![0f32; 48_000 * 2];
    assert!(a.read_frames_wait(300 * 48_000, &mut buf, Duration::ZERO), "ready_at promised no wait");
    assert!(buf.iter().any(|v| v.abs() > 0.05));
    // pool-level prefetch returns the same data object
    let b = pool.prefetch_audio(&path, 48_000, 500.0).unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    assert!(a.wait_complete_timeout(Duration::from_secs(60)).unwrap());
    assert!(a.is_complete() && a.ready_at(0.0, 600.0));
    assert!((a.duration() - 600.0).abs() < 0.05, "{}", a.duration());
}

#[test]
fn ten_minute_file_plays_within_a_few_hundred_ms() {
    let _s = serial();
    let path = ten_minutes();
    let mut report = String::new();
    for (label, at) in [("start", 0.0), ("seek", 420.0)] {
        let pool = fresh(&format!("timing-{label}"));
        let t = Instant::now();
        let a = pool.audio(&path, 48_000).unwrap();
        let opened = t.elapsed();
        let mut buf = vec![0f32; 1024 * 2];
        a.read(at, &mut buf);
        let first = t.elapsed();
        assert!(buf.iter().any(|v| v.abs() > 0.01), "{label}: first block is silent");
        report.push_str(&format!("{label}: audio() {opened:?}, first block {first:?}; "));
        assert!(first < Duration::from_millis(400), "{label}: first block took {first:?}");
    }
    let pool = fresh("timing-full");
    let t = Instant::now();
    pool.audio_complete(&path, 48_000).unwrap();
    eprintln!("{report}full decode {:?}", t.elapsed());
}

#[test]
fn crash_mid_decode_never_leaves_a_complete_looking_cache() {
    let _s = serial();
    let dir = scratch("crash-cache");
    // A child process starts decoding into `dir` and aborts half-way.
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_child", "--ignored", "--nocapture", "--test-threads", "1"])
        .env("AIFRAMES_TEST_CRASH_DIR", &dir)
        .status()
        .unwrap();
    assert!(!status.success(), "the child should have aborted");
    let audio_dir = dir.join("audio");
    let names: Vec<String> = std::fs::read_dir(&audio_dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(names.iter().any(|n| n.ends_with(".part")), "{names:?}");
    assert!(final_cache_files(&audio_dir).is_empty(), "a partial decode got the cache name: {names:?}");

    // The next run ignores the partial file and decodes completely and correctly.
    let pool = MediaPool::with_cache_dir(16 << 20, &dir);
    let a = pool.audio_complete(&ten_minutes(), 48_000).unwrap();
    let finals = final_cache_files(&audio_dir);
    assert_eq!(finals.len(), 1, "{finals:?}");
    assert_eq!(std::fs::metadata(audio_dir.join(&finals[0])).unwrap().len(), a.frames() * 8);
    let reference = fresh("crash-ref").audio_complete(&ten_minutes(), 48_000).unwrap();
    for s in [0i64, 123_456, 300 * 48_000, a.frames() as i64 - 1000] {
        assert_eq!(read(&a, s, 1000), read(&reference, s, 1000));
    }
    // A later run reuses the complete cache instantly.
    let again = MediaPool::with_cache_dir(16 << 20, &dir).audio(&ten_minutes(), 48_000).unwrap();
    assert!(again.is_complete());
}

/// Runs only inside `crash_mid_decode_never_leaves_a_complete_looking_cache`.
#[test]
#[ignore]
fn crash_child() {
    let Some(dir) = std::env::var_os("AIFRAMES_TEST_CRASH_DIR") else { return };
    let pool = MediaPool::with_cache_dir(16 << 20, dir);
    let a = pool.audio(&ten_minutes(), 48_000).unwrap();
    let t = Instant::now();
    while a.progress() < 0.1 && t.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!a.is_complete());
    std::process::abort();
}

#[test]
fn stale_part_files_from_crashed_runs_are_removed() {
    let dir = scratch("stale-parts");
    let audio = dir.join("audio");
    std::fs::create_dir_all(&audio).unwrap();
    let old = audio.join("x-0000000000000000-48000.f32.999-0.part");
    let fresh_part = audio.join("y-0000000000000000-48000.f32.998-0.part");
    std::fs::write(&old, [0u8; 64]).unwrap();
    std::fs::write(&fresh_part, [0u8; 64]).unwrap();
    let f = std::fs::File::options().write(true).open(&old).unwrap();
    f.set_modified(std::time::SystemTime::now() - Duration::from_secs(3 * 3600)).unwrap();
    drop(f);
    let pool = MediaPool::with_cache_dir(16 << 20, &dir);
    let a = pool.audio(&long_aac(), 48_000).unwrap();
    assert!(!old.exists(), "an hour-old .part file is a crashed decode");
    assert!(fresh_part.exists(), "a recent .part file may belong to another running instance");
    a.wait_complete().unwrap();
}

#[test]
fn data_ending_early_does_not_hang_readers() {
    let _s = serial();
    let pool = fresh("truncated");
    let a = pool.audio(&truncated(), 48_000).unwrap();
    // Far past where the data actually ends: must return (silence) once the decode is over,
    // not wait the full 30 s.
    let t = Instant::now();
    let mut buf = vec![1f32; 4096];
    a.read_frames_wait(560 * 48_000, &mut buf, LONG);
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    assert!(buf.iter().all(|v| *v == 0.0));
    a.wait_complete().unwrap();
    assert!(a.frames() < 500 * 48_000, "{}", a.frames());
    assert!(a.frames() > 100 * 48_000, "{}", a.frames());
}

#[test]
fn short_files_are_complete_when_audio_returns() {
    let pool = fresh("short");
    let a = pool.audio(&burst_wav(), 48_000).unwrap();
    assert!(a.is_complete());
    assert_eq!(a.progress(), 1.0);
    assert!(a.ready_at(0.0, 3.0));
    assert!(a.error().is_none());
}
