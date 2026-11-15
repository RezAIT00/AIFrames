//! Decode benchmark on real footage (read-only).
//!
//! `cargo run --release -p aiframes-media --example bench -- <video> [second-video] [--frames N] [--hw off|cuda|d3d11va|auto]`
//!
//! Measures sequential decode fps at full size and at a 960x540 preview size, 30 fps playback of
//! high-rate sources, random seek latency, backward frame stepping, thumbnails, concurrent playback
//! of two files, and audio/waveform decode.

use aiframes_media::{HwAccel, MediaPool};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn stats(mut v: Vec<f64>) -> String {
    v.sort_by(f64::total_cmp);
    let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    format!("min {:.0} / median {:.0} / p90 {:.0} / max {:.0} ms", p(0.0), p(0.5), p(0.9), p(1.0))
}

fn fresh_pool(hw: HwAccel) -> Arc<MediaPool> {
    let dir = std::env::temp_dir().join(format!("aiframes-bench-{}", std::process::id()));
    let p = MediaPool::with_cache_dir(1 << 30, dir);
    p.set_hwaccel(hw);
    p
}

/// Plays `frames` frames at `step` seconds from `t0`; returns (first-frame latency, fps after it).
fn sequential(pool: &MediaPool, path: &Path, t0: f64, step: f64, frames: usize, max: Option<(u32, u32)>) -> (f64, f64) {
    let t = Instant::now();
    pool.video_frame(path, t0, max).expect("frame");
    let first = t.elapsed();
    let t = Instant::now();
    for n in 1..frames {
        pool.video_frame(path, t0 + n as f64 * step, max).expect("frame");
    }
    (ms(first), (frames - 1) as f64 / t.elapsed().as_secs_f64())
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut frames = 300usize;
    let mut hw_list = vec![HwAccel::Off, HwAccel::Auto];
    let mut multi = 0usize;
    if let Some(i) = args.iter().position(|a| a == "--multi") {
        multi = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(3);
        args.drain(i..(i + 2).min(args.len()));
    }
    if let Some(i) = args.iter().position(|a| a == "--frames") {
        frames = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(frames);
        args.drain(i..(i + 2).min(args.len()));
    }
    if let Some(i) = args.iter().position(|a| a == "--hw") {
        hw_list = vec![args.get(i + 1).and_then(|v| HwAccel::parse(v)).unwrap_or(HwAccel::Auto)];
        args.drain(i..(i + 2).min(args.len()));
    }
    let Some(path) = args.first().map(PathBuf::from) else {
        eprintln!("usage: bench <video> [second-video] [--frames N] [--hw off|cuda|d3d11va|auto]");
        std::process::exit(2);
    };
    let second = args.get(1).map(PathBuf::from);
    if multi > 0 {
        // Several full-size streams decoded at once (an export with stacked 1080p layers).
        let (_, info) = aiframes_media::probe(&path).expect("probe");
        let fps = info.fps.max(1.0);
        for hw in [HwAccel::Off, HwAccel::Cuda, HwAccel::D3d11va] {
            let pool = fresh_pool(hw);
            let s = Instant::now();
            let hs: Vec<_> = (0..multi)
                .map(|k| {
                    let pool = pool.clone();
                    let p = path.clone();
                    let t0 = info.duration * (0.1 + 0.25 * k as f64);
                    std::thread::spawn(move || sequential(&pool, &p, t0, 1.0 / fps, frames, None))
                })
                .collect();
            let rates: Vec<f64> = hs.into_iter().map(|h| h.join().unwrap().1).collect();
            let wall = s.elapsed().as_secs_f64();
            let slowest = rates.iter().copied().fold(f64::INFINITY, f64::min);
            println!("{multi} streams full size, {hw:?}: {:.0} frame-sets/s (slowest stream), per stream {rates:.0?}, wall {wall:.1}s", slowest);
        }
        return;
    }
    let tools = aiframes_media::tools().expect("ffmpeg");
    println!("ffmpeg: {}", tools.ffmpeg.display());
    let (kind, info) = aiframes_media::probe(&path).expect("probe");
    println!(
        "{}: {:?} {}x{} {} fps {:.1}s {} audio={}",
        path.display(),
        kind,
        info.width,
        info.height,
        info.fps,
        info.duration,
        info.codec,
        info.has_audio
    );
    let fps = info.fps.max(1.0);
    // Start points spread over the file so each run starts cold (and leaves room for `frames`).
    let span = (info.duration - frames as f64 / fps - 1.0).max(0.0);
    let at = |frac: f64| span * frac;
    let preview = Some((960, 540));

    for &hw in &hw_list {
        println!("\n== hwaccel {hw:?}");
        let pool = fresh_pool(hw);
        let (first, rate) = sequential(&pool, &path, at(0.1), 1.0 / fps, frames, None);
        println!("sequential full size  : {rate:.0} fps  (first frame {first:.0} ms)");
        let pool = fresh_pool(hw);
        let (first, rate) = sequential(&pool, &path, at(0.35), 1.0 / fps, frames, preview);
        println!("sequential 960x540    : {rate:.0} fps  (first frame {first:.0} ms)");
        if fps > 31.0 {
            let pool = fresh_pool(hw);
            let (_, rate) = sequential(&pool, &path, at(0.6), 1.0 / 30.0, frames / 2, preview);
            println!("30 fps playback 960x540 of {fps} fps source: {rate:.0} frames/s");
        }
        let pool = fresh_pool(hw);
        let mut lat = Vec::new();
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..20 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let t = (x % 10_000) as f64 / 10_000.0 * (info.duration - 1.0).max(0.0);
            let s = Instant::now();
            pool.video_frame(&path, t, preview).expect("seek");
            lat.push(ms(s.elapsed()));
        }
        println!("random seek 960x540   : {}", stats(lat));
        let pool = fresh_pool(hw);
        let mut lat = Vec::new();
        let start = at(0.85) + 2.0;
        pool.video_frame(&path, start, preview).expect("frame");
        for n in 1..=30 {
            let s = Instant::now();
            pool.video_frame(&path, start - n as f64 / fps, preview).expect("frame");
            lat.push(ms(s.elapsed()));
        }
        println!("backward frame steps  : {}", stats(lat));
    }

    let pool = fresh_pool(HwAccel::Auto);
    let mut lat = Vec::new();
    for i in 0..20 {
        let t = info.duration * i as f64 / 20.0;
        let s = Instant::now();
        pool.thumbnail(&path, t, 64).expect("thumb");
        lat.push(ms(s.elapsed()));
    }
    println!("\nthumbnails (64 px, cold): {}", stats(lat));
    let s = Instant::now();
    for i in 0..20 {
        pool.thumbnail(&path, info.duration * i as f64 / 20.0, 64).expect("thumb");
    }
    println!("thumbnails (memory hit): {:.3} ms each", ms(s.elapsed()) / 20.0);

    if let Some(second) = second {
        let pool = fresh_pool(HwAccel::Auto);
        let (_, i2) = aiframes_media::probe(&second).expect("probe second");
        let s = Instant::now();
        let hs: Vec<_> = [(path.clone(), fps, info.duration), (second.clone(), i2.fps.max(1.0), i2.duration)]
            .into_iter()
            .map(|(p, f, d)| {
                let pool = pool.clone();
                std::thread::spawn(move || sequential(&pool, &p, d * 0.5, 1.0 / f, frames, Some((960, 540))).1)
            })
            .collect();
        let rates: Vec<f64> = hs.into_iter().map(|h| h.join().unwrap()).collect();
        println!(
            "\ntwo files concurrently at 960x540: {:.0} + {:.0} fps ({:.1} s wall)",
            rates[0],
            rates[1],
            s.elapsed().as_secs_f64()
        );
    }

    if info.has_audio {
        let pool = fresh_pool(HwAccel::Auto);
        let s = Instant::now();
        let a = pool.audio(&path, 48_000).expect("audio");
        println!("\naudio decode to cache: {:.0} ms for {:.0} s of audio", ms(s.elapsed()), a.duration());
        let s = Instant::now();
        let mut buf = vec![0f32; 1024 * 2];
        let mut t = 0.0;
        for _ in 0..10_000 {
            a.read(t, &mut buf);
            t += 1024.0 / 48_000.0;
        }
        println!("audio reads: {:.1} us per 1024-frame block", s.elapsed().as_secs_f64() * 1e6 / 10_000.0);
        let s = Instant::now();
        let w = pool.waveform(&path).expect("waveform");
        println!("waveform: {:.0} ms ({} bins)", ms(s.elapsed()), w.len());
        let s = Instant::now();
        let l = aiframes_media::loudness_of_file(&path).expect("loudness");
        println!("loudness: {:.0} ms -> {l:?}", ms(s.elapsed()));
    }
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("aiframes-bench-{}", std::process::id())));
}
