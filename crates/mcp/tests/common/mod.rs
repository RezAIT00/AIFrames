//! Fixtures generated with `ffmpeg -f lavfi` (kept between runs under Cargo's test tmp dir) and helpers
//! to drive the backend headlessly.
#![allow(dead_code)]

use aiframes_mcp::{AiframesBackend, Services};
use serde_json::{json, Value as J};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use suite_mcp::Backend;

pub fn root() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("aiframes-mcp-fixtures");
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A fresh, empty folder for one test.
pub fn scratch(name: &str) -> PathBuf {
    let d = root().join("scratch").join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Services with a private media cache under the test tmp dir.
pub fn services() -> Arc<Services> {
    Services::with_pool(aiframes_media::MediaPool::with_cache_dir(256 << 20, root().join("cache")))
}

pub fn ffmpeg(args: &[&str]) {
    let out = Command::new(&aiframes_media::tools().expect("ffmpeg").ffmpeg)
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "ffmpeg {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

static LOCK: Mutex<()> = Mutex::new(());

fn fixture(name: &str, make: impl FnOnce(&str)) -> PathBuf {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = root().join("media");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    if std::fs::metadata(&p).map(|m| m.len() > 0).unwrap_or(false) {
        return p;
    }
    let tmp = dir.join(format!("tmp-{}-{name}", std::process::id()));
    make(tmp.to_str().unwrap());
    std::fs::rename(&tmp, &p).unwrap();
    p
}

/// 640x360, 30 fps, 6 s testsrc2 with a quiet 440 Hz tone.
pub fn video() -> PathBuf {
    fixture("video.mp4", |out| {
        ffmpeg(&[
            "-f", "lavfi", "-i", "testsrc2=s=640x360:r=30:d=6", "-f", "lavfi", "-i", "sine=f=440:r=48000:d=6", "-af", "volume=0.1", "-c:v", "libx264", "-preset",
            "veryfast", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest", "-f", "mp4", out,
        ])
    })
}

/// 4 s: 2 s of testsrc2 then a hard cut to 2 s of smptebars (scene change at 2.0 s).
pub fn cut_video() -> PathBuf {
    fixture("cut.mp4", |out| {
        ffmpeg(&[
            "-f", "lavfi", "-i", "testsrc2=s=320x180:r=30:d=2", "-f", "lavfi", "-i", "smptebars=s=320x180:r=30:d=2", "-filter_complex", "[0:v][1:v]concat=n=2:v=1[v]",
            "-map", "[v]", "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p", "-f", "mp4", out,
        ])
    })
}

/// 10 s "speech": a tone while mod(t, 2.5) is in [0.2, 1.7], silence otherwise (internal silences of
/// 1.0 s at 1.7–2.7, 4.2–5.2, 6.7–7.7, and 9.2–10).
pub fn talk() -> PathBuf {
    fixture("talk.wav", |out| {
        ffmpeg(&["-f", "lavfi", "-i", "aevalsrc='0.5*sin(2*PI*220*t)*between(mod(t,2.5),0.2,1.7)':s=48000:d=10", "-c:a", "pcm_s16le", "-f", "wav", out])
    })
}

/// 8 s of clicks at 120 BPM over a quiet drone.
pub fn music() -> PathBuf {
    fixture("music.wav", |out| {
        ffmpeg(&["-f", "lavfi", "-i", "aevalsrc='0.6*sin(2*PI*1000*t)*exp(-40*mod(t,0.5))+0.05*sin(2*PI*110*t)':s=48000:d=8", "-c:a", "pcm_s16le", "-f", "wav", out])
    })
}

/// The 12 words of [`talk`]: three per burst at +0.05, +0.55, +1.05 s, each 0.4 s long.
pub fn talk_transcript() -> J {
    let names = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet", "kilo", "lima"];
    let words: Vec<J> = names
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let start = (i / 3) as f64 * 2.5 + 0.25 + (i % 3) as f64 * 0.5;
            json!({"text": format!(" {w}"), "start": start, "end": start + 0.4})
        })
        .collect();
    json!({"language": "en", "words": words})
}

pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Calls a tool, panicking with the error.
pub fn call(b: &AiframesBackend, name: &str, args: J) -> J {
    match b.call(name, &args) {
        Ok(v) => v,
        Err(e) => panic!("{name} {args} failed: {e}"),
    }
}

/// Calls a tool that must fail; returns the error.
pub fn fails(b: &AiframesBackend, name: &str, args: J) -> String {
    match b.call(name, &args) {
        Ok(v) => panic!("{name} {args} should fail but returned {v}"),
        Err(e) => e,
    }
}

/// Whether the renderer is still the orchestrator stub (export not implemented).
pub fn render_is_stub(message: &str) -> bool {
    message.contains("not implemented")
}
