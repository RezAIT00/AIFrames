//! Time to first audio on freshly imported media (read-only input, fresh disk cache each run).
//!
//! `cargo run --release -p aiframes-media --example audio_latency -- <media> [seek-seconds]`
//!
//! Reports how long `MediaPool::audio` + the first `read_frames` take at the start of the file and
//! at a later position (as when pressing Play after seeking), and how long the full decode takes.

use aiframes_media::MediaPool;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next().map(PathBuf::from) else {
        eprintln!("usage: audio_latency <media> [seek-seconds]");
        std::process::exit(2);
    };
    let seek: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(f64::NAN);
    let (_, info) = aiframes_media::probe(&path).expect("probe");
    let seek = if seek.is_finite() { seek } else { info.duration * 0.8 };
    println!("{}: {:.1} s, audio {} Hz x{}", path.display(), info.duration, info.audio_rate, info.audio_channels);

    for (label, at) in [("start", 0.0), ("seek", seek)] {
        let dir = std::env::temp_dir().join(format!("aiframes-audio-latency-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pool = MediaPool::with_cache_dir(64 << 20, &dir);
        let t = Instant::now();
        let a = pool.audio(&path, 48_000).expect("audio");
        let opened = t.elapsed();
        let mut buf = vec![0f32; 1024 * 2];
        a.read(at, &mut buf);
        let first = t.elapsed();
        let peak = buf.iter().fold(0f32, |m, v| m.max(v.abs()));
        println!(
            "{label:>5} @ {at:7.1}s: audio() {:6.1} ms, first 1024-frame block {:6.1} ms (peak {peak:.3})",
            opened.as_secs_f64() * 1e3,
            first.as_secs_f64() * 1e3
        );
        drop(a);
        drop(pool);
        let _ = std::fs::remove_dir_all(&dir);
    }
    let dir = std::env::temp_dir().join(format!("aiframes-audio-latency-{}-full", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let pool = MediaPool::with_cache_dir(64 << 20, &dir);
    let t = Instant::now();
    let a = pool.audio_complete(&path, 48_000).expect("audio");
    println!("full decode: {:.0} ms ({} frames)", t.elapsed().as_secs_f64() * 1e3, a.frames());
    drop(a);
    drop(pool);
    let _ = std::fs::remove_dir_all(&dir);
}
