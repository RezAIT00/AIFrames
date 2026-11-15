//! Re-exports a project file with its export settings and prints the report plus the process's
//! peak private memory (sampled every 10 ms) while exporting.
//!
//! `cargo run --release -p aiframes-render --example reexport -- <project.aifr> <out.mp4> [--cache-mb N] [--size WxH]`

use aiframes_media::MediaPool;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut cache_mb = 512usize;
    if let Some(i) = args.iter().position(|a| a == "--cache-mb") {
        cache_mb = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(cache_mb);
        args.drain(i..(i + 2).min(args.len()));
    }
    let mut size: Option<(u32, u32)> = None;
    if let Some(i) = args.iter().position(|a| a == "--size") {
        size = args.get(i + 1).and_then(|v| v.split_once('x')).and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
        args.drain(i..(i + 2).min(args.len()));
    }
    let (Some(project), Some(out)) = (args.first().map(PathBuf::from), args.get(1).map(PathBuf::from)) else {
        eprintln!("usage: reexport <project.aifr> <out.mp4> [--cache-mb N]");
        std::process::exit(2);
    };
    let doc = aiframes_core::Document::open(&project).expect("open project");
    let dir = doc.project_dir();
    // Like the app: a shared pool with its cache budget (the app uses 256..512 MB).
    let pool = MediaPool::with_cache_dir(cache_mb << 20, std::env::temp_dir().join("aiframes-reexport-cache"));
    let mut s = doc.project.export.clone();
    if let Some((w, h)) = size {
        s.width = Some(w);
        s.height = Some(h);
    }
    println!("export settings: codec {} quality {} hw {} {:?} LUFS, TP {} dB; pool cache {cache_mb} MB", s.codec, s.quality, s.hardware, s.loudness_lufs, s.true_peak_db);
    let mem = || memory_stats::memory_stats().map(|m| (m.virtual_mem, m.physical_mem)).unwrap_or((0, 0));
    let (start_priv, start_ws) = mem();
    let peak_priv = Arc::new(AtomicUsize::new(start_priv));
    let peak_ws = Arc::new(AtomicUsize::new(start_ws));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (pp, pw, stop) = (peak_priv.clone(), peak_ws.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let (p, w) = mem();
                pp.fetch_max(p, Ordering::Relaxed);
                pw.fetch_max(w, Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        })
    };
    let started = std::time::Instant::now();
    let rep = aiframes_render::export(&dir, &doc.project, pool, &s, &out, &mut |_| true).expect("export");
    let secs = started.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    let _ = sampler.join();
    let (end_priv, end_ws) = mem();
    let mb = |b: usize| b as f64 / (1 << 20) as f64;
    println!(
        "{} frames {}x{} with {} in {:.1} s ({:.0} fps); loudness {:?}; warnings {:?}; report peak memory {:?} MB",
        rep.frames,
        rep.width,
        rep.height,
        rep.encoder,
        secs,
        rep.frames as f64 / secs,
        rep.loudness.map(|l| (l.integrated_lufs, l.true_peak_db)),
        rep.warnings,
        rep.peak_memory_bytes.map(|b| b >> 20)
    );
    println!(
        "memory: private start {:.0} MB, peak {:.0} MB (+{:.0}), end {:.0} MB; working set start {:.0} MB, peak {:.0} MB, end {:.0} MB",
        mb(start_priv),
        mb(peak_priv.load(Ordering::Relaxed)),
        mb(peak_priv.load(Ordering::Relaxed) - start_priv),
        mb(end_priv),
        mb(start_ws),
        mb(peak_ws.load(Ordering::Relaxed)),
        mb(end_ws)
    );
}
