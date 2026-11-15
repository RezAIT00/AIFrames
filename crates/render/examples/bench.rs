//! Renderer benchmark on a synthetic 1080p30 edit: three video layers (full frame + two rotated
//! picture-in-pictures, one with a drop shadow), a title (pill, stroke, shadow) and a karaoke
//! caption, music + video sound with ducking and a transition.
//!
//! `cargo run --release -p aiframes-render --example bench -- [--frames N] [--export-secs S] [--size WxH] [--dump DIR] [--no-hw]`
//!
//! `--size 3840x2160` exports at that output size (frames rendered natively at that scale).
//!
//! Reports per-frame render time at scale 1 and 0.5 (sequential playback with decoding, and with
//! every source cached), mix speed, and export fps.

use aiframes_core::{resolve, Ducking, ExportSettings, MediaAsset, Project, Transition};
use aiframes_media::MediaPool;
use aiframes_render::{encode_png, export, FrameOptions, Renderer};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn ffmpeg(args: &[&str]) {
    let out = Command::new(&aiframes_media::tools().expect("ffmpeg on PATH").ffmpeg)
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "ffmpeg failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn fixture(dir: &Path, name: &str, make: impl FnOnce(&str)) -> PathBuf {
    let p = dir.join(name);
    if !p.is_file() {
        let tmp = dir.join(format!("tmp-{name}"));
        make(tmp.to_str().unwrap());
        std::fs::rename(&tmp, &p).unwrap();
    }
    p
}

fn stats(mut v: Vec<f64>) -> String {
    v.sort_by(f64::total_cmp);
    let q = |f: f64| v[((v.len() - 1) as f64 * f).round() as usize];
    format!("median {:.1} ms, p90 {:.1} ms, min {:.1} ms, max {:.1} ms", q(0.5), q(0.9), q(0.0), q(1.0))
}

fn asset(id: &str, path: &Path) -> MediaAsset {
    let (kind, info) = aiframes_media::probe(path).unwrap();
    MediaAsset { id: id.into(), name: id.into(), path: path.to_string_lossy().replace('\\', "/"), kind, info, tags: vec![], notes: String::new() }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut take = |flag: &str| -> Option<String> {
        let i = args.iter().position(|a| a == flag)?;
        let v = args.get(i + 1).cloned();
        args.drain(i..(i + 2).min(args.len()));
        v
    };
    let frames: usize = take("--frames").and_then(|v| v.parse().ok()).unwrap_or(90);
    let export_secs: f64 = take("--export-secs").and_then(|v| v.parse().ok()).unwrap_or(10.0);
    let dump = take("--dump").map(PathBuf::from);
    let size: Option<(u32, u32)> = take("--size").and_then(|v| {
        let (w, h) = v.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    });
    let hw = !args.iter().any(|a| a == "--no-hw");

    let dir = std::env::temp_dir().join("aiframes-render-bench");
    std::fs::create_dir_all(&dir).unwrap();
    let secs = 12;
    let mk = |name: &str, src: &str| {
        fixture(&dir, name, |out| {
            ffmpeg(&[
                "-f", "lavfi", "-i", src, "-f", "lavfi", "-i", &format!("sine=f=330:r=48000:d={secs}"), "-shortest", "-c:v", "libx264",
                "-preset", "veryfast", "-crf", "20", "-g", "60", "-pix_fmt", "yuv420p", "-c:a", "aac", out,
            ])
        })
    };
    println!("preparing 1080p fixtures in {} ...", dir.display());
    let a = mk("a.mp4", &format!("testsrc2=s=1920x1080:r=30:d={secs}"));
    let b = mk("b.mp4", &format!("testsrc2=s=1920x1080:r=30:d={secs},hue=h=120"));
    let c = mk("c.mp4", &format!("smptehdbars=s=1920x1080:r=30:d={secs},drawbox=x='mod(t*400,1800)':y=500:w=120:h=120:c=yellow:t=fill"));
    let music = fixture(&dir, "music.wav", |out| {
        ffmpeg(&["-f", "lavfi", "-i", &format!("aevalsrc=exprs='0.2*sin(2*PI*220*t)+0.1*sin(2*PI*331*t)':s=48000:d={secs}:c=stereo"), "-c:a", "pcm_f32le", out])
    });

    let mut p: Project = serde_json::from_value(json!({
        "format": 1,
        "settings": {"width": 1920, "height": 1080, "fps": 30},
        "tracks": [
            {"id": "v1", "kind": "video", "role": "main"}, {"id": "v2", "kind": "video"}, {"id": "v3", "kind": "video"},
            {"id": "v4", "kind": "video", "role": "overlay"}, {"id": "v5", "kind": "video", "role": "captions"},
            {"id": "a1", "kind": "audio", "role": "music"}
        ],
    }))
    .unwrap();
    p.media = vec![asset("a", &a), asset("b", &b), asset("c", &c), asset("music", &music)];
    let clips = json!([
        {"id": "main1", "track": "v1", "start": 0, "duration": 6, "source": {"type": "media", "asset": "a"}},
        {"id": "main2", "track": "v1", "start": {"after": "main1"}, "duration": 6, "source": {"type": "media", "asset": "c", "in_point": 2}},
        {"id": "pip1", "track": "v2", "start": 0, "duration": 12, "muted": true, "source": {"type": "media", "asset": "b"},
         "transform": {"scale": 0.42, "x": -480, "y": -230, "rotation": 4}},
        {"id": "pip2", "track": "v3", "start": 0, "duration": 12, "muted": true, "source": {"type": "media", "asset": "c"},
         "transform": {"scale": {"keys": [{"t": 0, "v": 0.3}, {"t": 12, "v": 0.4}]}, "x": 520, "y": 250, "rotation": -6},
         "effects": [{"kind": "drop_shadow", "params": {"distance": 16, "blur": 24}}]},
        {"id": "title", "track": "v4", "start": 0, "duration": 12, "source": {"type": "text", "text": "AIFrames renderer", "size": 96,
          "background": {"color": "#000000A0", "padding": 24, "radius": 18}, "stroke": {"color": "#000000", "width": 3},
          "shadow": {"color": "#000000C0", "blur": 10, "x": 4, "y": 6}}, "transform": {"y": -380}},
        {"id": "cap", "track": "v5", "start": 0, "duration": 12, "source": {"type": "text", "text": "every word lights up in time with the voice",
          "size": 56, "weight": 800, "max_width": 1400, "stroke": {"color": "#000000", "width": 4},
          "highlight": {"color": "#FFD400", "words": [[0,1],[1,2],[2,3],[3,4],[4,5],[5,6],[6,7],[7,8],[8,9]]}}, "transform": {"y": 400}},
        {"id": "mus", "track": "a1", "start": 0, "duration": 12, "source": {"type": "media", "asset": "music"}}
    ]);
    p.clips = serde_json::from_value(clips).unwrap();
    p.transitions.push(Transition { id: "tr1".into(), from: "main1".into(), to: "main2".into(), kind: "crossfade".into(), duration: 0.6, params: Default::default(), colors: Default::default() });
    p.ducking.push(Ducking { id: "d1".into(), target: "a1".into(), trigger: "v1".into(), amount_db: -10.0, threshold_db: -45.0, attack: 0.12, release: 0.45 });
    let res = resolve(&p);
    assert!(res.errors.is_empty(), "{:?}", res.errors);
    let doc = Path::new(".");
    let pool = MediaPool::with_cache_dir(2 << 30, dir.join("cache"));
    let r = Renderer::new(pool.clone());
    println!("threads: {}", rayon::current_num_threads());

    for scale in [1.0f32, 0.5] {
        let o = FrameOptions { scale, ..FrameOptions::default() };
        // Warm up (fonts, decoders).
        r.frame(doc, &p, &res, 0.0, &o).unwrap();
        let mut times = Vec::new();
        for i in 1..=frames {
            let t = i as f64 / 30.0;
            let s = Instant::now();
            r.frame(doc, &p, &res, t, &o).unwrap();
            times.push(s.elapsed().as_secs_f64() * 1000.0);
        }
        println!("scale {scale}: playback (sequential, decoding):   {}", stats(times));
        let before = pool.stats();
        let mut times = Vec::new();
        for i in 1..=frames {
            let t = i as f64 / 30.0;
            let s = Instant::now();
            r.frame(doc, &p, &res, t, &o).unwrap();
            times.push(s.elapsed().as_secs_f64() * 1000.0);
        }
        let after = pool.stats();
        println!(
            "scale {scale}: sources cached (no ffmpeg):        {}  [decoded during pass: {}]",
            stats(times),
            after.frames_decoded - before.frames_decoded
        );
        // Transition frame.
        let s = Instant::now();
        r.frame(doc, &p, &res, 6.0, &o).unwrap();
        println!("scale {scale}: crossfade frame: {:.1} ms (first touch)", s.elapsed().as_secs_f64() * 1000.0);
    }
    let draft = FrameOptions { scale: 0.5, draft: true, guides: false };
    r.frame(doc, &p, &res, 3.0, &draft).unwrap();
    let s = Instant::now();
    for i in 0..30 {
        r.frame(doc, &p, &res, 3.0 + i as f64 / 30.0, &draft).unwrap();
    }
    println!("scale 0.5 draft: {:.1} ms/frame", s.elapsed().as_secs_f64() * 1000.0 / 30.0);

    if let Some(d) = &dump {
        std::fs::create_dir_all(d).unwrap();
        for t in [0.5, 3.0, 6.0, 9.0] {
            let img = r.frame(doc, &p, &res, t, &FrameOptions { guides: t == 9.0, ..FrameOptions::default() }).unwrap();
            std::fs::write(d.join(format!("frame-{t:.1}.png")), encode_png(&img)).unwrap();
        }
        let sheet = aiframes_render::contact_sheet(&r, doc, &p, &res, &[0.0, 2.0, 4.0, 6.0, 8.0, 10.0], 3, 480).unwrap();
        std::fs::write(d.join("sheet.png"), encode_png(&sheet)).unwrap();
        println!("wrote frames to {}", d.display());
    }

    // Mixer speed in playback-sized blocks.
    let mut block = vec![0f32; 1024 * 2];
    let s = Instant::now();
    let n = 470; // ~10 s
    for i in 0..n {
        r.mix(doc, &p, &res, i as f64 * 1024.0 / 48_000.0, &mut block).unwrap();
    }
    let per = s.elapsed().as_secs_f64() / n as f64;
    println!("mix: {:.3} ms per 1024-frame block ({:.0}x real time)", per * 1000.0, (1024.0 / 48_000.0) / per);

    // Throughput diagnostics: frames rendered on 8 threads (sources decoding), and the encoder alone.
    {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let n = (export_secs * 30.0) as usize;
        let (ow, oh) = size.unwrap_or((1920, 1080));
        let out_opts = FrameOptions { scale: ow as f32 / 1920.0, ..FrameOptions::default() };
        let next = AtomicUsize::new(0);
        let s = Instant::now();
        std::thread::scope(|sc| {
            for _ in 0..8 {
                sc.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    r.frame(doc, &p, &res, i as f64 / 30.0, &out_opts).unwrap();
                });
            }
        });
        println!("render only at {ow}x{oh}, 8 frames in flight: {:.0} fps", n as f64 / s.elapsed().as_secs_f64());
        let img = r.frame(doc, &p, &res, 1.0, &out_opts).unwrap();
        let mut o = aiframes_media::EncodeOptions::new(dir.join("enc-only.mp4"), img.width, img.height, 30.0);
        o.hardware = hw;
        let mut enc = aiframes_media::Encoder::start(&o).unwrap();
        let s = Instant::now();
        for _ in 0..n {
            enc.push(&img.rgba).unwrap();
        }
        enc.finish().unwrap();
        println!("encoder only ({}): {:.0} fps", if hw { "hardware" } else { "software" }, n as f64 / s.elapsed().as_secs_f64());
    }

    // Export.
    let out = dir.join("bench-export.mp4");
    let es = ExportSettings {
        range: Some([0.0, export_secs]),
        hardware: hw,
        width: size.map(|s| s.0),
        height: size.map(|s| s.1),
        ..ExportSettings::default()
    };
    let s = Instant::now();
    let mut marks: Vec<(&'static str, f64)> = Vec::new();
    let rep = export(doc, &p, pool.clone(), &es, &out, &mut |pr| {
        if marks.last().map(|m| m.0) != Some(pr.stage) {
            marks.push((pr.stage, pr.elapsed));
        }
        true
    })
    .unwrap();
    let stages: Vec<String> = marks.windows(2).map(|w| format!("{} {:.2}s", w[0].0, w[1].1 - w[0].1)).collect();
    println!("export stages: {}", stages.join(", "));
    let secs = s.elapsed().as_secs_f64();
    println!(
        "export {:.0} s of the 1080p30 edit at {}x{} with {}: {:.2} s wall = {:.0} fps ({:.1}x real time); loudness {:.2} LUFS, true peak {:.2} dBTP",
        rep.duration,
        rep.width,
        rep.height,
        rep.encoder,
        secs,
        rep.frames as f64 / secs,
        rep.duration / secs,
        rep.loudness.map(|l| l.integrated_lufs).unwrap_or(f64::NAN),
        rep.loudness.map(|l| l.true_peak_db).unwrap_or(f64::NAN),
    );
}
