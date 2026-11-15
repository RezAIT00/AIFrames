//! Encoder preset measurement on real footage (read-only source; outputs under `--out`).
//!
//! `cargo run --release -p aiframes-media --example encode_quality -- <video> --out <dir> [--start 300] [--secs 30] [--codec h264] [--label after]`
//!
//! Decodes `secs` seconds from `start` through `MediaPool` (like an export), encodes them with every
//! quality preset on the GPU encoder and on the software encoder, and reports bitrate, export
//! speed (decode + encode, pipelined) and quality against the source excerpt (VMAF, PSNR, SSIM).

use aiframes_media::{EncodeOptions, Encoder, MediaPool, Quality, VideoCodec};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::sync_channel;
use std::time::Instant;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn metrics(ffmpeg: &Path, encoded: &Path, source: &Path, start: f64, secs: f64) -> (f64, f64, f64) {
    // Both sides get identical colour tags: ffmpeg otherwise "converts" an untagged input to match a
    // tagged one before comparing (an untagged source vs a bt709-tagged copy of itself scores 33.7 dB).
    let tag = "setparams=colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv,format=yuv420p";
    let graph = format!(
        "[0:v]{tag},setpts=PTS-STARTPTS,split=3[a1][a2][a3];[1:v]{tag},setpts=PTS-STARTPTS,split=3[b1][b2][b3];\
         [a1][b1]libvmaf=n_threads=16[v];[a2][b2]psnr[p];[a3][b3]ssim[s]"
    );
    let out = Command::new(ffmpeg)
        .args(["-hide_banner", "-nostdin", "-nostats", "-i"])
        .arg(encoded)
        .args(["-ss", &format!("{start}"), "-t", &format!("{secs}"), "-i"])
        .arg(source)
        .args(["-filter_complex", &graph, "-map", "[v]", "-f", "null", "-", "-map", "[p]", "-f", "null", "-", "-map", "[s]", "-f", "null", "-"])
        .output()
        .expect("ffmpeg");
    let text = String::from_utf8_lossy(&out.stderr);
    let grab = |key: &str| -> f64 {
        text.rfind(key)
            .and_then(|i| text[i + key.len()..].split_whitespace().next())
            .and_then(|v| v.trim_end_matches(|c: char| !c.is_ascii_digit() && c != '.').parse().ok())
            .unwrap_or(f64::NAN)
    };
    (grab("VMAF score:"), grab("average:"), grab("All:"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let source = PathBuf::from(args.first().expect("usage: encode_quality <video> --out <dir>"));
    let out_dir = PathBuf::from(arg(&args, "--out").expect("--out <dir>"));
    let start: f64 = arg(&args, "--start").and_then(|v| v.parse().ok()).unwrap_or(300.0);
    let secs: f64 = arg(&args, "--secs").and_then(|v| v.parse().ok()).unwrap_or(30.0);
    let label = arg(&args, "--label").unwrap_or_else(|| "run".into());
    let codec = arg(&args, "--codec").and_then(|c| VideoCodec::parse(&c)).unwrap_or(VideoCodec::H264);
    let qualities: Vec<Quality> = arg(&args, "--qualities")
        .map(|v| v.split(',').filter_map(Quality::parse).collect())
        .unwrap_or_else(|| vec![Quality::Draft, Quality::Standard, Quality::High]);
    let hw_modes: Vec<bool> = match arg(&args, "--hw").as_deref() {
        Some("on") => vec![true],
        Some("off") => vec![false],
        _ => vec![true, false],
    };
    std::fs::create_dir_all(&out_dir).unwrap();
    let ffmpeg = aiframes_media::tools().unwrap().ffmpeg.clone();
    let (_, info) = aiframes_media::probe(&source).unwrap();
    let fps = info.fps;
    let frames = (secs * fps).round() as usize;
    println!("{label}: {} {}x{} {fps} fps, {frames} frames from {start}s, codec {}", source.display(), info.width, info.height, codec.name());
    println!("| preset | encoder | Mbps | export fps | VMAF | PSNR dB | SSIM |");
    println!("|---|---|---|---|---|---|---|");
    let pool = MediaPool::with_cache_dir(512 << 20, out_dir.join("cache"));
    for &hardware in &hw_modes {
        for &q in &qualities {
            let out = out_dir.join(format!("{label}-{}-{}-{}.mp4", codec.name(), if hardware { "hw" } else { "sw" }, q.name()));
            let mut o = EncodeOptions::new(&out, info.width, info.height, fps);
            o.codec = codec;
            o.quality = q;
            o.hardware = hardware;
            let t = Instant::now();
            let (tx, rx) = sync_channel(8);
            let (p2, src) = (pool.clone(), source.clone());
            let decoder = std::thread::spawn(move || {
                for n in 0..frames {
                    let f = p2.video_frame(&src, start + n as f64 / fps, None).unwrap();
                    if tx.send(f.rgba).is_err() {
                        break;
                    }
                }
            });
            let mut e = Encoder::start(&o).unwrap();
            let name = e.encoder_name().to_string();
            for rgba in rx {
                e.push(&rgba).unwrap();
            }
            e.finish().unwrap();
            decoder.join().unwrap();
            let wall = t.elapsed().as_secs_f64();
            let bytes = std::fs::metadata(&out).unwrap().len();
            let mbps = bytes as f64 * 8.0 / secs / 1e6;
            let (vmaf, psnr, ssim) = metrics(&ffmpeg, &out, &source, start, secs);
            println!("| {} | {name} | {mbps:.1} | {:.0} | {vmaf:.2} | {psnr:.2} | {ssim:.4} |", q.name(), frames as f64 / wall);
        }
    }
}
