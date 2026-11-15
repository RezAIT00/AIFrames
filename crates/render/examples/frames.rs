//! Renders frames of a project file to PNGs (for looking at an edit without the app).
//!
//! `cargo run --release -p aiframes-render --example frames -- <project.aifr> <out_dir> <t> [<t> ...] [--scale S]`
//!
//! A time `a..b` renders every frame from `a` to `b`. PNGs are named by time (`f_19.866.png`).
//! `--time` renders without writing PNGs and prints each frame's render time instead.

use aiframes_media::MediaPool;
use aiframes_render::{encode_png, FrameOptions, Renderer};
use std::path::PathBuf;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut scale = 0.5f32;
    if let Some(i) = args.iter().position(|a| a == "--scale") {
        scale = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(scale);
        args.drain(i..(i + 2).min(args.len()));
    }
    let timing = args.iter().any(|a| a == "--time");
    args.retain(|a| a != "--time");
    if args.len() < 3 {
        eprintln!("usage: frames <project.aifr> <out_dir> <t|a..b> ... [--scale S]");
        std::process::exit(2);
    }
    let doc = aiframes_core::Document::open(PathBuf::from(&args[0])).expect("open project");
    let out = PathBuf::from(&args[1]);
    std::fs::create_dir_all(&out).unwrap();
    let p = &doc.project;
    let res = aiframes_core::resolve(p);
    let fps = p.settings.fps;
    let mut times = Vec::new();
    for a in &args[2..] {
        match a.split_once("..") {
            Some((x, y)) => {
                let (x, y): (f64, f64) = (x.parse().unwrap(), y.parse().unwrap());
                let (i0, i1) = ((x * fps).round() as i64, (y * fps).round() as i64);
                times.extend((i0..=i1).map(|i| i as f64 / fps));
            }
            None => times.push(a.parse().unwrap()),
        }
    }
    let r = Renderer::new(MediaPool::with_cache_dir(512 << 20, std::env::temp_dir().join("aiframes-frames-cache")));
    let dir = doc.project_dir();
    for t in times {
        let started = std::time::Instant::now();
        let (img, warnings) = r.frame_checked(&dir, p, &res, t, &FrameOptions { scale, guides: false, draft: false }).expect("frame");
        if !warnings.is_empty() {
            eprintln!("{t:.3}: {warnings:?}");
        }
        if timing {
            println!("{t:7.3} s  {:6.1} ms", started.elapsed().as_secs_f64() * 1000.0);
            continue;
        }
        std::fs::write(out.join(format!("f_{t:.3}.png")), encode_png(&img)).unwrap();
    }
}
