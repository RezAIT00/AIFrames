//! Decode-session memory under an AI-snapshot burst (read-only inputs).
//!
//! `cargo run --release -p aiframes-media --example session_memory -- <folder-with-videos> [--files 12]`
//!
//! Takes the first `--files` videos of the folder. Phase A: four threads request every file at
//! three sizes (1920x1080, 960x540, 320x180) at five random times each, while phase B plays one
//! file sequentially at 960x540. A PowerShell sampler records this process's private memory and
//! the number and working set of its ffmpeg children; `PoolStats` gives sessions and readahead.

use aiframes_media::MediaPool;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = PathBuf::from(args.first().expect("usage: session_memory <folder> [--files 12]"));
    let n_files: usize = args.iter().position(|a| a == "--files").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(12);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4")))
        .collect();
    files.sort();
    files.truncate(n_files);
    let durations: Vec<f64> = files.iter().map(|f| aiframes_media::probe(f).unwrap().1.duration).collect();
    println!("{} files from {}", files.len(), dir.display());

    let cache_dir = std::env::temp_dir().join(format!("aiframes-session-memory-{}", std::process::id()));
    let pool = MediaPool::with_cache_dir(64 << 20, &cache_dir);

    // Sampler: private bytes of this process, ffmpeg children (count, working set).
    let script = format!(
        "$p={}; while ($true) {{ $me = Get-Process -Id $p -ErrorAction SilentlyContinue; if (!$me) {{ break }}; \
         $k = @(Get-CimInstance Win32_Process -Filter \"ParentProcessId=$p AND Name='ffmpeg.exe'\"); \
         $ws = ($k | Measure-Object -Property WorkingSetSize -Sum).Sum; if (!$ws) {{ $ws = 0 }}; \
         \"$($me.PrivateMemorySize64) $($k.Count) $ws\"; Start-Sleep -Milliseconds 150 }}",
        std::process::id()
    );
    let mut sampler = Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).stdout(Stdio::piped()).spawn().unwrap();
    let samples: Arc<Mutex<Vec<(u64, u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let s2 = samples.clone();
    let out = sampler.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let v: Vec<u64> = line.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if v.len() == 3 {
                s2.lock().unwrap().push((v[0], v[1], v[2]));
            }
        }
    });
    let stats_peak = Arc::new(Mutex::new((0usize, 0usize)));
    let done = Arc::new(AtomicBool::new(false));
    {
        let (pool, peak, done) = (pool.clone(), stats_peak.clone(), done.clone());
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let s = pool.stats();
                let mut p = peak.lock().unwrap();
                p.0 = p.0.max(s.sessions);
                p.1 = p.1.max(session_bytes(&s));
                drop(p);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });
    }
    std::thread::sleep(std::time::Duration::from_millis(1500)); // sampler warm-up

    let t0 = Instant::now();
    // Phase B: playback of the first file at 960x540, concurrently with phase A.
    let play = {
        let (pool, f, d) = (pool.clone(), files[0].clone(), durations[0]);
        std::thread::spawn(move || {
            let start = d * 0.4;
            let t = Instant::now();
            for n in 0..300 {
                pool.video_frame(&f, start + n as f64 / 30.0, Some((960, 540))).unwrap();
            }
            300.0 / t.elapsed().as_secs_f64()
        })
    };
    // Phase A: AI snapshots at three sizes, four threads.
    let jobs: Vec<(usize, (u32, u32), f64)> = {
        let mut v = Vec::new();
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        for (i, d) in durations.iter().enumerate() {
            for size in [(1920u32, 1080u32), (960, 540), (320, 180)] {
                for _ in 0..5 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    v.push((i, size, (x % 10_000) as f64 / 10_000.0 * (d - 1.0)));
                }
            }
        }
        v
    };
    let next = Arc::new(AtomicUsize::new(0));
    let jobs = Arc::new(jobs);
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let (pool, files, jobs, next) = (pool.clone(), files.clone(), jobs.clone(), next.clone());
            std::thread::spawn(move || loop {
                let k = next.fetch_add(1, Ordering::Relaxed);
                let Some(&(i, size, t)) = jobs.get(k) else { break };
                pool.video_frame(&files[i], t, Some(size)).unwrap();
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    let snap_wall = t0.elapsed().as_secs_f64();
    let play_fps = play.join().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(600));
    done.store(true, Ordering::Relaxed);
    let end_stats = pool.stats();
    let _ = sampler.kill();
    let _ = sampler.wait();
    let s = samples.lock().unwrap().clone();
    let base = s.first().map(|x| x.0).unwrap_or(0);
    let peak_private = s.iter().map(|x| x.0).max().unwrap_or(0);
    let peak_children = s.iter().map(|x| x.1).max().unwrap_or(0);
    let peak_child_ws = s.iter().map(|x| x.2).max().unwrap_or(0);
    let (peak_sessions, peak_session_bytes) = *stats_peak.lock().unwrap();
    let mb = |b: u64| b as f64 / (1 << 20) as f64;
    println!("{} snapshot requests in {snap_wall:.1} s while playback ran at {play_fps:.0} fps (960x540)", jobs.len());
    println!("peak live sessions {peak_sessions}, peak session memory (PoolStats) {:.0} MB", mb(peak_session_bytes as u64));
    println!(
        "process private memory: start {:.0} MB, peak {:.0} MB (+{:.0}); ffmpeg children: peak {peak_children}, peak working set {:.0} MB",
        mb(base),
        mb(peak_private),
        mb(peak_private.saturating_sub(base)),
        mb(peak_child_ws)
    );
    println!("end: {end_stats:?}");
    drop(pool);
    let _ = std::fs::remove_dir_all(cache_dir);
}

/// Session memory reported by `PoolStats`.
fn session_bytes(s: &aiframes_media::PoolStats) -> usize {
    s.session_bytes
}
