//! Showcase reel of the edit-culture toolkit: every new transition between two clips, then each new
//! effect and feature (motion blur, frame blending, 3-D cards, track mattes), labelled.
//!
//! `cargo run --release -p aiframes-render --example transitions_reel -- [--a clip.mp4] [--b clip.mp4]
//!  [--fps 60] [--size 1080x1920] [--out DIR] [--frames]`
//!
//! Writes `<out>/reel.mp4` and `<out>/reel_sheet.png` (one labelled frame per segment at its most
//! telling moment; default `<out>` = `target/render`), with `--frames` also every sheet frame as a
//! full-size PNG under `<out>/reel_frames/`. Without `--a`/`--b` it generates two colourful clips with
//! ffmpeg. Prints export speed.

use aiframes_core::{resolve, Document, ExportSettings, MediaAsset, Project};
use aiframes_media::MediaPool;
use aiframes_render::{contact_sheet, encode_png, export, FrameOptions, Image, Renderer};
use serde_json::{json, Value};
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

fn asset(id: &str, path: &Path) -> MediaAsset {
    let (kind, info) = aiframes_media::probe(path).expect("probe");
    MediaAsset { id: id.into(), name: id.into(), path: path.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"), kind, info, tags: vec![], notes: String::new() }
}

/// One segment of the reel: a label, the clips (JSON) and transitions it adds, and the time to show
/// on the contact sheet (relative to the segment start).
struct Seg {
    name: String,
    show_at: f64,
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut take = |flag: &str| -> Option<String> {
        let i = args.iter().position(|a| a == flag)?;
        let v = args.get(i + 1).cloned();
        args.drain(i..(i + 2).min(args.len()));
        v
    };
    let fps: f64 = take("--fps").and_then(|v| v.parse().ok()).unwrap_or(60.0);
    let (w, h): (u32, u32) = take("--size").and_then(|v| v.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))).unwrap_or((1080, 1920));
    let out_dir = take("--out").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/render"));
    let a_arg = take("--a").map(PathBuf::from);
    let b_arg = take("--b").map(PathBuf::from);
    let dump_frames = args.iter().any(|a| a == "--frames");
    std::fs::create_dir_all(&out_dir).unwrap();
    let media = out_dir.join("reel_media");
    std::fs::create_dir_all(&media).unwrap();
    let gen = |name: &str, filter: &str| -> PathBuf {
        let p = media.join(name);
        if !p.is_file() {
            let tmp = media.join(format!("tmp-{name}"));
            ffmpeg(&["-f", "lavfi", "-i", filter, "-c:v", "libx264", "-preset", "veryfast", "-crf", "18", "-pix_fmt", "yuv420p", tmp.to_str().unwrap()]);
            std::fs::rename(&tmp, &p).unwrap();
        }
        p
    };
    let a = a_arg.unwrap_or_else(|| gen("a.mp4", "testsrc2=s=1080x1920:r=30:d=40,hue=s=1.4"));
    let b = b_arg.unwrap_or_else(|| gen("b.mp4", "gradients=s=540x960:r=30:d=40:n=6:speed=0.05:type=spiral,format=yuv420p"));
    let music = {
        let p = media.join("beat.wav");
        if !p.is_file() {
            ffmpeg(&["-f", "lavfi", "-i", "aevalsrc=exprs='0.5*sin(2*PI*55*t)*exp(-8*mod(t,0.5))+0.15*sin(2*PI*880*t)*exp(-30*mod(t+0.25,0.5))':s=48000:d=60:c=stereo", "-c:a", "pcm_s16le", p.to_str().unwrap()]);
        }
        p
    };

    // ---- Timeline -----------------------------------------------------------------------------
    let seg_len = 1.2;
    let mut clips: Vec<Value> = Vec::new();
    let mut transitions: Vec<Value> = Vec::new();
    let mut segs: Vec<Seg> = Vec::new();
    let mut t = 0.0;
    let label = |id: String, text: &str, start: f64, dur: f64| -> Value {
        json!({"id": id, "track": "v3", "start": start, "duration": dur, "source": {"type": "text", "text": text, "size": 64, "weight": 800,
            "background": {"color": "#000000B0", "padding": 22, "radius": 16}}, "transform": {"y": -760}})
    };
    // Transitions: alternate A/B clips on v1, a transition between each pair.
    let trans: &[(&str, f64, Value)] = &[
        ("spin", 0.5, json!({})),
        ("zoom_through", 0.5, json!({})),
        ("shake", 0.4, json!({"amount": 60})),
        ("glitch", 0.4, json!({})),
        ("flash_zoom", 0.5, json!({})),
        ("film_burn", 0.9, json!({})),
        ("cube", 0.7, json!({})),
        ("flip", 0.6, json!({})),
        ("slide_blur", 0.4, json!({})),
        ("stretch", 0.4, json!({})),
        ("luma_wipe", 0.9, json!({})),
    ];
    let n = trans.len();
    for i in 0..=n {
        let asset = if i % 2 == 0 { "a" } else { "b" };
        clips.push(json!({"id": format!("c{i}"), "track": "v1", "start": t, "duration": seg_len, "muted": true,
            "source": {"type": "media", "asset": asset, "in_point": 2.0 + i as f64 * 1.3}, "transform": {"fit": "cover"}}));
        if let Some((kind, dur, params)) = trans.get(i) {
            transitions.push(json!({"id": format!("t{i}"), "from": format!("c{i}"), "to": format!("c{}", i + 1), "kind": kind, "duration": dur, "params": params}));
            clips.push(label(format!("l{i}"), kind, t + seg_len - 0.45, 0.9));
            segs.push(Seg { name: kind.to_string(), show_at: t + seg_len - t });
            segs.last_mut().unwrap().show_at = t + seg_len - if matches!(*kind, "flash_zoom" | "flip" | "cube" | "film_burn") { 0.1 } else { 0.0 }; // around the cut
        }
        t += seg_len;
    }
    // Effects and features, each on its own clip.
    let fx: Vec<(&str, Value, f64)> = vec![
        ("directional_blur", json!({"effects": [{"kind": "directional_blur", "params": {"angle": 20, "length": 80}}]}), 0.6),
        ("glitch_fx", json!({"effects": [{"kind": "glitch", "params": {"amount": 0.7, "slices": 18, "rgb": 24}}]}), 0.6),
        ("echo", json!({"transform": {"scale": 0.5, "x": {"keys": [{"t": 0, "v": -500}, {"t": 1.2, "v": 500}]}, "rotation": {"keys": [{"t": 0, "v": -20}, {"t": 1.2, "v": 20}]}},
                       "effects": [{"kind": "echo", "params": {"count": 5, "interval": 0.06, "decay": 0.6}}]}), 0.7),
        ("stutter", json!({"speed": 2.0, "effects": [{"kind": "stutter", "params": {"rate": 6, "hold": 0.8}}]}), 0.6),
        ("edge_glow", json!({"effects": [{"kind": "edge_glow", "params": {"threshold": 0.15, "radius": 14, "intensity": 2.0}}]}), 0.6),
        ("lens_distortion", json!({"effects": [{"kind": "lens_distortion", "params": {"amount": 0.6}}]}), 0.6),
        ("exposure_flash", json!({"effects": [{"kind": "exposure_flash", "params": {"amount": {"keys": [{"t": 0.3, "v": 0}, {"t": 0.45, "v": 1.2}, {"t": 0.9, "v": 0}]}}}]}), 0.72),
        ("motion_blur", json!({"motion_blur": {"shutter": 360, "samples": 24},
            "transform": {"scale": {"keys": [{"t": 0, "v": 0.35, "ease": "out"}, {"t": 0.6, "v": 1.0}]}, "rotation": {"keys": [{"t": 0, "v": -120, "ease": "out"}, {"t": 0.6, "v": 0}]},
                          "x": {"keys": [{"t": 0, "v": -700, "ease": "out"}, {"t": 0.6, "v": 0}]}}}), 0.2),
        ("frame_blend", json!({"speed": 8.0, "frame_blend": true}), 0.6),
        ("3d_card", json!({"transform": {"scale": 0.7, "rotation_y": {"keys": [{"t": 0, "v": -50}, {"t": 1.2, "v": 50}]}, "rotation_x": 12, "perspective": 1400}}), 0.35),
    ];
    for (i, (name, extra, show)) in fx.iter().enumerate() {
        // The detailed test pattern shows what each effect does; the soft gradient suits the echo card.
        let asset = if *name == "echo" { "b" } else { "a" };
        let mut c = json!({"id": format!("f{i}"), "track": "v2", "start": t, "duration": seg_len, "muted": true,
            "source": {"type": "media", "asset": asset, "in_point": 3.0 + i as f64}, "transform": {"fit": "cover"}});
        for (k, v) in extra.as_object().unwrap() {
            if k == "transform" {
                for (tk, tv) in v.as_object().unwrap() {
                    c["transform"][tk] = tv.clone();
                }
            } else {
                c[k] = v.clone();
            }
        }
        clips.push(c);
        // A dark backdrop under the cards that do not fill the frame.
        clips.push(json!({"id": format!("bg{i}"), "track": "v1", "start": t, "duration": seg_len, "source": {"type": "solid", "color": "#101018"}}));
        clips.push(label(format!("fl{i}"), name, t, seg_len));
        segs.push(Seg { name: name.to_string(), show_at: t + show });
        t += seg_len;
    }
    // Track matte: footage inside giant text.
    clips.push(json!({"id": "mbg", "track": "v1", "start": t, "duration": seg_len, "source": {"type": "solid", "color": "#0B0B12"}}));
    clips.push(json!({"id": "mt", "track": "v4", "start": t, "duration": seg_len, "source": {"type": "text", "text": "PHONK", "size": 250, "weight": 900}}));
    clips.push(json!({"id": "mf", "track": "v2", "start": t, "duration": seg_len, "muted": true, "matte": {"clip": "mt"},
        "source": {"type": "media", "asset": "a", "in_point": 5.0}, "transform": {"fit": "cover", "scale": {"keys": [{"t": 0, "v": 1.0}, {"t": 1.2, "v": 1.3}]}}}));
    clips.push(label("ml".into(), "track matte", t, seg_len));
    segs.push(Seg { name: "track matte".into(), show_at: t + 0.6 });
    t += seg_len;

    clips.push(json!({"id": "mus", "track": "a1", "start": 0.0, "duration": t, "source": {"type": "media", "asset": "music"}}));
    let p: Project = serde_json::from_value(json!({
        "format": 1,
        "settings": {"width": w, "height": h, "fps": fps},
        "tracks": [{"id": "v1", "kind": "video"}, {"id": "v2", "kind": "video"}, {"id": "v4", "kind": "video"}, {"id": "v3", "kind": "video"}, {"id": "a1", "kind": "audio"}],
        "clips": clips,
        "transitions": transitions,
    }))
    .map(|mut p: Project| {
        p.media = vec![asset("a", &a), asset("b", &b), asset("music", &music)];
        p
    })
    .expect("project");
    let res = resolve(&p);
    assert!(res.errors.is_empty(), "{:?}", res.errors);
    let doc_path = out_dir.join("reel.aifr");
    let mut d = Document::new(p.clone());
    let _ = d.save_as(&doc_path);

    // ---- Contact sheet ------------------------------------------------------------------------
    let pool = MediaPool::with_cache_dir(1 << 30, out_dir.join("reel_cache"));
    let r = Renderer::new(pool.clone());
    let doc = Path::new(".");
    let times: Vec<f64> = segs.iter().map(|s| s.show_at).collect();
    let started = Instant::now();
    let sheet = contact_sheet(&r, doc, &p, &res, &times, 6, 300).expect("sheet");
    println!("contact sheet of {} frames in {:.1} s", times.len(), started.elapsed().as_secs_f64());
    std::fs::write(out_dir.join("reel_sheet.png"), encode_png(&sheet)).unwrap();
    if dump_frames {
        let dir = out_dir.join("reel_frames");
        std::fs::create_dir_all(&dir).unwrap();
        for s in &segs {
            let img: Image = r.frame(doc, &p, &res, s.show_at, &FrameOptions { scale: 0.5, ..FrameOptions::default() }).expect("frame");
            std::fs::write(dir.join(format!("{}.png", s.name.replace(' ', "_"))), encode_png(&img)).unwrap();
        }
    }

    // ---- Per-frame cost of the heaviest moments and the export ---------------------------------
    let o = FrameOptions::default();
    for s in &segs {
        r.frame(doc, &p, &res, s.show_at, &o).unwrap();
        let st = Instant::now();
        r.frame(doc, &p, &res, s.show_at, &o).unwrap();
        println!("  {:<17} {:6.1} ms/frame at {}x{}", s.name, st.elapsed().as_secs_f64() * 1000.0, w, h);
    }
    let out = out_dir.join("reel.mp4");
    let s = ExportSettings { loudness_lufs: Some(-14.0), ..ExportSettings::default() };
    let started = Instant::now();
    let rep = export(doc, &p, pool, &s, &out, &mut |_| true).expect("export");
    let secs = started.elapsed().as_secs_f64();
    println!(
        "reel: {:.1} s at {}x{} {} fps exported in {:.1} s = {:.0} fps ({:.2}x real time) with {}; peak memory {:?} MB",
        rep.duration,
        rep.width,
        rep.height,
        fps,
        secs,
        rep.frames as f64 / secs,
        rep.duration / secs,
        rep.encoder,
        rep.peak_memory_bytes.map(|b| b >> 20)
    );
    println!("wrote {} and {}", out.display(), out_dir.join("reel_sheet.png").display());
}
