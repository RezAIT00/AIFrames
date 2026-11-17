//! Multi-stream sources in the mix: which sound streams a project plays, measured on the mix and on
//! an exported WAV (game / mic / chat tones at 440 / 660 / 1000 Hz).

mod common;
use aiframes_core::{apply_batch, Op, Project};
use aiframes_render::{export, Renderer};
use common::*;
use serde_json::json;
use std::path::{Path, PathBuf};

/// Video + three sound streams: 0 "Game" 440 Hz stereo, 1 "Mic" 660 Hz mono, 2 "Chat" 1 kHz 5.1
/// (centre channel), each at 0.3.
fn obs_recording() -> PathBuf {
    fixture("obs-3-streams.mkv", |out| {
        ffmpeg(&[
            "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=30:duration=4",
            "-f", "lavfi", "-i", "aevalsrc=exprs='0.3*sin(2*PI*440*t)':s=48000:d=4:c=stereo",
            "-f", "lavfi", "-i", "aevalsrc=exprs='0.3*sin(2*PI*660*t)':s=48000:d=4:c=mono",
            "-f", "lavfi", "-i", "aevalsrc=exprs='0|0|0.3*sin(2*PI*1000*t)|0|0|0':s=48000:d=4:c=5.1(side)",
            "-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3:a",
            "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-c:a", "flac",
            "-metadata:s:a:0", "title=Game", "-metadata:s:a:1", "title=Mic", "-metadata:s:a:2", "title=Chat",
            out,
        ]);
    })
}

fn tone(x: &[f32], rate: f64, f: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * f / rate;
    let c = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &v in x {
        let s = v as f64 + c * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    2.0 * (s1 * s1 + s2 * s2 - c * s1 * s2).sqrt() / x.len() as f64
}

/// The amplitude of each test tone in the left channel of the mix over `[1, 2)` s.
fn heard(r: &Renderer, p: &Project) -> [f64; 3] {
    let res = resolved(p);
    let mut out = vec![0f32; 48_000 * 2];
    r.mix(Path::new("."), p, &res, 1.0, &mut out).unwrap();
    let left: Vec<f32> = out.iter().step_by(2).copied().collect();
    [440.0, 660.0, 1000.0].map(|f| tone(&left, 48_000.0, f))
}

fn on(level: f64) -> bool {
    level > 0.1
}

fn ops(v: serde_json::Value) -> Vec<Op> {
    v.as_array().unwrap().iter().map(|o| Op::from_json(o).unwrap()).collect()
}

fn obs_project() -> Project {
    let path = obs_recording();
    let (kind, info) = aiframes_media::probe(&path).unwrap();
    assert_eq!(info.audio_stream_count(), 3);
    let mut p: Project = serde_json::from_value(json!({"format": 1, "settings": {"width": 160, "height": 90, "fps": 30}})).unwrap();
    apply_batch(
        &mut p,
        &ops(json!([
            {"op": "add_media", "id": "m1", "path": path.to_string_lossy(), "kind": kind, "info": info},
            {"op": "add_track", "id": "v1", "kind": "video"},
            {"op": "add_clip", "clip": {"id": "c1", "track": "v1", "asset": "m1", "start": 0.0}},
        ])),
    )
    .unwrap();
    p
}

#[test]
fn the_mix_plays_the_streams_the_clips_choose() {
    let r = renderer();
    let mut p = obs_project();
    // An old-style video clip plays its first stream only.
    let h = heard(&r, &p);
    assert!(on(h[0]) && !on(h[1]) && !on(h[2]), "plain video clip: {h:?}");

    // One linked clip per stream, each on its own new audio track named after the stream.
    let out = apply_batch(&mut p, &ops(json!([{"op": "detach_streams", "clip": "c1"}]))).unwrap();
    let made: Vec<String> = out[0].created.clone();
    assert_eq!(p.tracks.iter().filter(|t| t.kind == aiframes_core::TrackKind::Audio).map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["Game", "Mic", "Chat"]);
    let streams: Vec<(String, u32, String)> = p
        .clips
        .iter()
        .filter_map(|c| match &c.source {
            aiframes_core::Source::Media { stream, .. } if c.id != "c1" => Some((c.id.clone(), *stream, c.track.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(streams.len(), 3, "{made:?}");
    assert_eq!(streams.iter().map(|s| s.1).collect::<Vec<_>>(), vec![0, 1, 2]);
    assert!(p.clips.iter().find(|c| c.id == "c1").unwrap().muted, "the video's own sound is muted");
    let h = heard(&r, &p);
    assert!(on(h[0]) && on(h[1]) && on(h[2]), "every stream plays: {h:?}");
    // 5.1 centre reaches both sides at -3 dB.
    assert!((h[2] - 0.3 * std::f64::consts::FRAC_1_SQRT_2).abs() < 0.03 && (h[1] - 0.3).abs() < 0.03, "{h:?}");

    // Each linked clip mutes and has its own volume.
    let (game, mic, chat) = (streams[0].0.clone(), streams[1].0.clone(), streams[2].0.clone());
    apply_batch(&mut p, &ops(json!([{"op": "set_clip", "clip": mic, "patch": {"muted": true}}, {"op": "set_param", "clip": chat, "path": "volume", "value": 0.5}]))).unwrap();
    let h = heard(&r, &p);
    assert!(on(h[0]) && !on(h[1]), "mic muted: {h:?}");
    assert!((h[2] - 0.15 * std::f64::consts::FRAC_1_SQRT_2).abs() < 0.02, "chat at half volume: {h:?}");

    // Choosing another stream for an existing clip.
    apply_batch(&mut p, &ops(json!([{"op": "set_audio_stream", "clip": game, "stream": 1}]))).unwrap();
    let h = heard(&r, &p);
    assert!(!on(h[0]) && on(h[1]), "game clip now plays the mic stream: {h:?}");
    let e = apply_batch(&mut p, &ops(json!([{"op": "set_audio_stream", "clip": game, "stream": 3}]))).unwrap_err().message;
    assert!(e.contains("3 sound streams (0–2)"), "{e}");

    // The links hold: trimming the video trims every stream; splitting splits them all.
    let res = resolved(&p);
    let before = res.span("c1").unwrap();
    apply_batch(&mut p, &ops(json!([{"op": "trim_clip", "clip": "c1", "edge": "end", "delta": -1.0}]))).unwrap();
    let res = resolved(&p);
    for (id, _, _) in &streams {
        assert!((res.span(id).unwrap().end - (before.end - 1.0)).abs() < 1e-6, "{id} follows the trim");
    }
    apply_batch(&mut p, &ops(json!([{"op": "split_clip", "clip": "c1", "at": 1.5}]))).unwrap();
    assert_eq!(p.clips.len(), 8, "the picture and its three streams split together");
}

#[test]
fn every_stream_in_use_reaches_the_exported_mix() {
    let mut p = obs_project();
    apply_batch(&mut p, &ops(json!([{"op": "detach_streams", "clip": "c1", "streams": [1, 2]}]))).unwrap();
    // Stream 0 is not used now (the video is muted and only 1 and 2 were detached).
    let out = scratch("streams-export").join("mix.wav");
    let s = aiframes_core::ExportSettings { codec: "wav".into(), loudness_lufs: None, ..Default::default() };
    let rep = export(Path::new("."), &p, pool(), &s, &out, &mut |_| true).unwrap();
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
    let (data, rate) = read_wav_left(&out);
    let a = (rate as usize, 2 * rate as usize);
    let x = &data[a.0..a.1];
    let h = [440.0, 660.0, 1000.0].map(|f| tone(x, rate as f64, f));
    assert!(!on(h[0]) && on(h[1]) && on(h[2]), "exported mix: {h:?}");
}

/// Left channel of a 32-bit float or 16-bit WAV.
fn read_wav_left(path: &Path) -> (Vec<f32>, u32) {
    let b = std::fs::read(path).unwrap();
    let mut i = 12;
    let (mut fmt, mut ch, mut rate, mut bits) = (0u16, 0u16, 0u32, 0u16);
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &b[i + 8..(i + 8 + len).min(b.len())];
        if id == b"fmt " {
            fmt = u16::from_le_bytes([body[0], body[1]]);
            ch = u16::from_le_bytes([body[2], body[3]]);
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            let step = (bits / 8) as usize * ch as usize;
            let v = body
                .chunks_exact(step)
                .map(|f| match (fmt, bits) {
                    (3, 32) | (0xFFFE, 32) => f32::from_le_bytes(f[0..4].try_into().unwrap()),
                    (_, 16) => i16::from_le_bytes([f[0], f[1]]) as f32 / 32768.0,
                    (_, 24) => (i32::from_le_bytes([0, f[0], f[1], f[2]]) >> 8) as f32 / 8_388_608.0,
                    _ => panic!("unexpected wav format {fmt}/{bits}"),
                })
                .collect();
            return (v, rate);
        }
        i += 8 + len + (len & 1);
    }
    panic!("no data chunk in {}", path.display());
}
