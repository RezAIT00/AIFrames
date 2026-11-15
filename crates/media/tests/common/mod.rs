//! Test fixtures generated with `ffmpeg -f lavfi` (kept between runs under Cargo's test tmp dir).
#![allow(dead_code)]

use aiframes_media::{Frame, MediaPool};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

pub fn root() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("aiframes-media-fixtures");
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

pub fn ffmpeg(args: &[&str]) {
    let out = Command::new(&aiframes_media::tools().expect("ffmpeg").ffmpeg)
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "ffmpeg {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

static LOCK: Mutex<()> = Mutex::new(());

/// Generates `name` once (atomically) with `make(tmp_path)`.
pub fn fixture(name: &str, make: impl FnOnce(&str)) -> PathBuf {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let p = root().join(name);
    if p.is_file() && std::fs::metadata(&p).map(|m| m.len() > 0).unwrap_or(false) {
        return p;
    }
    let ext = Path::new(name).extension().unwrap().to_string_lossy().into_owned();
    let tmp = root().join(format!("tmp-{}-{name}.{ext}", std::process::id()));
    make(tmp.to_str().unwrap());
    std::fs::rename(&tmp, &p).unwrap();
    p
}

/// Bars encoding the frame number: `BITS` vertical bars, white = 1, LSB on the left.
pub const BITS: u32 = 9;

fn bars_filter() -> String {
    format!("format=gray,geq=lum='if(bitand(N,pow(2,floor(X*{BITS}/W))),235,16)',format=yuv420p")
}

/// Reads the frame number from the bars.
pub fn code(f: &Frame) -> u32 {
    let y = f.height / 2;
    let mut n = 0;
    for k in 0..BITS {
        let x = ((k as f64 + 0.5) * f.width as f64 / BITS as f64) as u32;
        let p = f.pixel(x, y).unwrap();
        if p[0] > 128 {
            n |= 1 << k;
        }
    }
    n
}

/// 320x180, 30 fps, 12 s (360 frames), H.264 with B-frames and 48-frame GOPs.
pub fn cfr() -> PathBuf {
    fixture("cfr30.mp4", |out| {
        let vf = format!("color=c=black:s=320x180:r=30:d=12,{}", bars_filter());
        ffmpeg(&["-f", "lavfi", "-i", &vf, "-c:v", "libx264", "-g", "48", "-bf", "3", "-preset", "veryfast", "-crf", "18", out]);
    })
}

/// 29.97 fps (30000/1001), 4 s.
pub fn ntsc() -> PathBuf {
    fixture("ntsc.mp4", |out| {
        let vf = format!("color=c=black:s=320x180:r=30000/1001:d=4,{}", bars_filter());
        ffmpeg(&["-f", "lavfi", "-i", &vf, "-c:v", "libx264", "-g", "30", "-bf", "2", "-preset", "veryfast", out]);
    })
}

/// Millisecond timestamps of the variable-rate fixture's frame `n`.
pub fn vfr_pts_ms(n: u32) -> u32 {
    33 * n + 40 * (n / 5)
}
pub const VFR_FRAMES: u32 = 120;

/// Variable frame rate: frame n at `vfr_pts_ms(n)` ms (gaps of 33 ms, every 5th 73 ms).
pub fn vfr() -> PathBuf {
    fixture("vfr.mp4", |out| {
        let vf = format!("color=c=black:s=320x180:r=30,trim=end_frame={VFR_FRAMES},{},settb=1/1000,setpts=33*N+40*floor(N/5)", bars_filter());
        ffmpeg(&[
            "-f", "lavfi", "-i", &vf, "-fps_mode", "passthrough", "-enc_time_base", "1/1000", "-video_track_timescale", "1000",
            "-c:v", "libx264", "-g", "40", "-bf", "2", "-preset", "veryfast", out,
        ]);
    })
}

/// The cfr fixture with a 90° counter-clockwise display rotation (shows as 180x320).
pub fn rotated() -> PathBuf {
    let src = cfr();
    fixture("rotated.mp4", |out| {
        ffmpeg(&["-display_rotation:v:0", "90", "-i", src.to_str().unwrap(), "-c", "copy", out]);
    })
}

/// 48 kHz stereo f32 WAV, 3 s: silence, then a 0.5-amplitude 1 kHz cosine burst on [1.0, 1.5] s
/// (first non-zero sample is exactly sample 48000).
pub fn burst_wav() -> PathBuf {
    fixture("burst.wav", |out| {
        ffmpeg(&[
            "-f", "lavfi", "-i", "aevalsrc=exprs='if(between(t,1,1.5),0.5*cos(2*PI*1000*t),0)':s=48000:d=3:c=stereo",
            "-c:a", "pcm_f32le", out,
        ]);
    })
}

/// Video (cfr bars, first 3 s) + the burst as AAC, with the audio starting 0.5 s late in the
/// container: the burst is at source time 1.5 s.
pub fn av_offset() -> PathBuf {
    let v = cfr();
    let a = burst_wav();
    fixture("av_offset.mp4", |out| {
        ffmpeg(&[
            "-t", "3", "-i", v.to_str().unwrap(), "-itsoffset", "0.5", "-i", a.to_str().unwrap(), "-map", "0:v", "-map", "1:a",
            "-c:v", "copy", "-c:a", "aac", "-b:a", "192k", out,
        ]);
    })
}

/// 60 s stereo f32 WAV with analytic content: L = 0.5 sin(2π·440·t), R = 0.25 sin(2π·97·t).
pub fn long_wav() -> PathBuf {
    fixture("long.wav", |out| {
        ffmpeg(&["-f", "lavfi", "-i", "aevalsrc=exprs='0.5*sin(2*PI*440*t)|0.25*sin(2*PI*97*t)':s=48000:d=60", "-c:a", "pcm_f32le", out]);
    })
}

/// Mono 1 kHz sine, amplitude 0.25, 2 s.
pub fn mono_wav() -> PathBuf {
    fixture("mono.wav", |out| {
        ffmpeg(&["-f", "lavfi", "-i", "aevalsrc=exprs='0.25*sin(2*PI*1000*t)':s=44100:d=2", "-c:a", "pcm_s16le", out]);
    })
}

/// Tone / silence pattern: tone 0-1, silence 1-2.5, tone 2.5-3, silence 3-3.2, tone 3.2-4, silence 4-5.
pub fn silence_wav() -> PathBuf {
    fixture("silences.wav", |out| {
        ffmpeg(&[
            "-f", "lavfi", "-i",
            "aevalsrc=exprs='0.3*sin(2*PI*440*t)*(lt(t,1)+between(t,2.5,3)+between(t,3.2,4))+0.0003*sin(2*PI*50*t)':s=48000:d=5:c=stereo",
            "-c:a", "pcm_s16le", out,
        ]);
    })
}

/// Clicks at 100 BPM starting at 0.25 s, 12 s.
pub fn clicks_wav() -> PathBuf {
    fixture("clicks100.wav", |out| {
        ffmpeg(&[
            "-f", "lavfi", "-i",
            "aevalsrc=exprs='gte(t,0.25)*0.8*sin(2*PI*1500*mod(t-0.25,0.6))*exp(-60*mod(t-0.25,0.6))':s=44100:d=12:c=stereo",
            "-c:a", "pcm_s16le", out,
        ]);
    })
}

/// 1 kHz stereo sine at -20 dBFS peak (amplitude 0.1), 10 s.
pub fn sine20_wav() -> PathBuf {
    fixture("sine20.wav", |out| {
        ffmpeg(&["-f", "lavfi", "-i", "aevalsrc=exprs='0.1*sin(2*PI*1000*t)':s=48000:d=10:c=stereo", "-c:a", "pcm_f32le", out]);
    })
}

/// Pink noise with a slow level swing (has a real loudness range), 30 s.
pub fn swing_wav() -> PathBuf {
    fixture("swing.wav", |out| {
        ffmpeg(&[
            "-f", "lavfi", "-i", "anoisesrc=color=pink:amplitude=0.3:d=30:seed=7",
            "-af", "volume='0.15+0.85*abs(sin(2*PI*t/12))':eval=frame,aformat=channel_layouts=stereo", "-ar", "48000", "-c:a", "pcm_f32le", out,
        ]);
    })
}

/// Three shots of 2 s at 25 fps: red, testsrc2, blue (cuts at 2.0 and 4.0).
pub fn scenes() -> PathBuf {
    fixture("scenes.mp4", |out| {
        ffmpeg(&[
            "-filter_complex",
            "color=c=red:s=320x240:r=25:d=2[a];testsrc2=s=320x240:r=25:d=2[b];color=c=blue:s=320x240:r=25:d=2[c];[a][b][c]concat=n=3:v=1:a=0,format=yuv420p[v]",
            "-map", "[v]", "-c:v", "libx264", "-preset", "veryfast", out,
        ]);
    })
}

pub fn png() -> PathBuf {
    fixture("still.png", |out| ffmpeg(&["-f", "lavfi", "-i", "testsrc2=s=64x48", "-frames:v", "1", out]))
}

pub fn jpg() -> PathBuf {
    fixture("still.jpg", |out| ffmpeg(&["-f", "lavfi", "-i", "testsrc2=s=64x48", "-frames:v", "1", out]))
}

/// 64x48, semi-transparent red (alpha 128) with an opaque blue 16x16 square in the top-left corner.
pub fn alpha_png() -> PathBuf {
    fixture("alpha2.png", |out| {
        let sq = "lt(X,16)*lt(Y,16)";
        let vf = format!("color=c=black:s=64x48,format=rgba,geq=r='if({sq},0,255)':g=0:b='if({sq},255,0)':a='if({sq},255,128)'");
        ffmpeg(&["-f", "lavfi", "-i", &vf, "-frames:v", "1", out]);
    })
}

/// A pool with its own disk cache folder.
pub fn pool(name: &str, bytes: usize) -> Arc<MediaPool> {
    MediaPool::with_cache_dir(bytes, scratch(&format!("cache-{name}")))
}

pub fn shared_pool() -> Arc<MediaPool> {
    static P: OnceLock<Arc<MediaPool>> = OnceLock::new();
    P.get_or_init(|| pool("shared", 512 << 20)).clone()
}

/// 120 s: small video + 44.1 kHz stereo AAC with analytic content (resampled to 48 kHz on read).
pub fn long_aac() -> PathBuf {
    fixture("long_aac441.mp4", |out| {
        ffmpeg(&[
            "-f", "lavfi", "-i", "color=c=gray:s=64x36:r=5:d=120",
            "-f", "lavfi", "-i", "aevalsrc=exprs='0.3*sin(2*PI*441*t)+0.2*sin(2*PI*3001*t*t/50)|0.25*sin(2*PI*97*t)':s=44100:d=120",
            "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-b:a", "192k", "-shortest", out,
        ]);
    })
}

/// 90 s, 48 kHz stereo f32 WAV: quiet tone, then a 0.5-amplitude cosine burst from exactly
/// 85.0 s (sample 4_080_000) to 86.0 s.
pub fn late_burst_wav() -> PathBuf {
    fixture("late_burst.wav", |out| {
        ffmpeg(&[
            "-f", "lavfi", "-i",
            "aevalsrc=exprs='if(between(t,85,86),0.5*cos(2*PI*1000*t),0.01*sin(2*PI*50*t))':s=48000:d=90:c=stereo",
            "-c:a", "pcm_f32le", out,
        ]);
    })
}

/// A 10-minute recording: 64x36 video at 5 fps + 48 kHz stereo AAC.
pub fn ten_minutes() -> PathBuf {
    fixture("ten_minutes.mp4", |out| {
        ffmpeg(&[
            "-f", "lavfi", "-i", "color=c=black:s=64x36:r=5:d=600",
            "-f", "lavfi", "-i", "aevalsrc=exprs='0.2*sin(2*PI*440*t)*(0.6+0.4*sin(2*PI*t/7))|0.2*sin(2*PI*660*t)':s=48000:d=600",
            "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-b:a", "128k", "-shortest", "-movflags", "+faststart", out,
        ]);
    })
}

/// The 10-minute recording cut off at 40 % of its bytes (moov at the front, so it still probes).
pub fn truncated() -> PathBuf {
    let src = ten_minutes();
    fixture("truncated.mp4", |out| {
        let bytes = std::fs::read(&src).unwrap();
        std::fs::write(out, &bytes[..bytes.len() * 2 / 5]).unwrap();
    })
}

/// 120 s of the analytic stereo signal at 48 kHz in `container` with `codec` (region-seek fixtures).
pub fn long_coded(name: &str, codec: &[&str]) -> PathBuf {
    fixture(name, |out| {
        let mut args = vec![
            "-f", "lavfi", "-i", "aevalsrc=exprs='0.3*sin(2*PI*441*t)+0.2*sin(2*PI*3001*t*t/50)|0.25*sin(2*PI*97*t)':s=48000:d=120",
        ];
        args.extend_from_slice(codec);
        args.push(out);
        ffmpeg(&args);
    })
}

/// Bars video (30 fps, 3 s) whose stream starts 1 s after the container start; audio 0-4 s.
pub fn late_video() -> PathBuf {
    fixture("late_video.mp4", |out| {
        let bars = root().join(format!("tmp-bars3-{}.mp4", std::process::id()));
        let vf = format!("color=c=black:s=320x180:r=30:d=3,{}", bars_filter());
        ffmpeg(&["-f", "lavfi", "-i", &vf, "-c:v", "libx264", "-g", "30", "-preset", "veryfast", bars.to_str().unwrap()]);
        ffmpeg(&[
            "-f", "lavfi", "-i", "sine=f=440:d=4", "-itsoffset", "1", "-i", bars.to_str().unwrap(), "-map", "1:v", "-map", "0:a",
            "-c:v", "copy", "-c:a", "aac", out,
        ]);
        let _ = std::fs::remove_file(bars);
    })
}

/// Matroska with 5 s of bars video and 20 s of audio (the video stream has no `duration`, only a tag).
/// 1 s of 64x48 VP9 with alpha in WebM (what background removers write): the left half is fully
/// transparent, the right half opaque red. ffmpeg tags the stream ALPHA_MODE=1 (upper case).
pub fn alpha_webm() -> PathBuf {
    fixture("alpha.webm", |out| {
        let vf = "color=c=red:s=64x48:r=30:d=1,format=yuva420p,geq=lum='lum(X,Y)':cb='cb(X,Y)':cr='cr(X,Y)':a='if(lt(X,32),0,255)'";
        ffmpeg(&["-f", "lavfi", "-i", vf, "-c:v", "libvpx-vp9", "-pix_fmt", "yuva420p", "-auto-alt-ref", "0", "-b:v", "1M", out]);
    })
}

pub fn short_video_mkv() -> PathBuf {
    fixture("short_video.mkv", |out| {
        let vf = format!("color=c=black:s=320x180:r=30:d=5,{}", bars_filter());
        ffmpeg(&["-f", "lavfi", "-i", &vf, "-f", "lavfi", "-i", "sine=f=440:d=20", "-c:v", "libx264", "-g", "30", "-preset", "veryfast", "-c:a", "aac", out]);
    })
}

/// 12 s of 30 fps bars with the index at the front, cut to half its bytes: probes as 12 s but
/// only about the first 6 s decode.
pub fn truncated_bars() -> PathBuf {
    fixture("truncated_bars.mp4", |out| {
        let full = root().join(format!("tmp-bars12fs-{}.mp4", std::process::id()));
        let vf = format!("color=c=black:s=320x180:r=30:d=12,{}", bars_filter());
        ffmpeg(&["-f", "lavfi", "-i", &vf, "-c:v", "libx264", "-g", "30", "-preset", "veryfast", "-movflags", "+faststart", full.to_str().unwrap()]);
        let bytes = std::fs::read(&full).unwrap();
        std::fs::write(out, &bytes[..bytes.len() / 2]).unwrap();
        let _ = std::fs::remove_file(full);
    })
}
