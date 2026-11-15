//! Mixer: levels, fades, speed (pitch/duration), reverse, freeze, mute/solo, video sound,
//! constant-power transitions, ducking, block independence, soft clip.

mod common;
use aiframes_core::{Ducking, Project, Transition};
use common::*;
use serde_json::json;
use std::path::Path;

const RATE: f64 = 48_000.0;

fn mix(p: &Project, t0: f64, secs: f64) -> Vec<f32> {
    let r = renderer();
    let res = resolved(p);
    let mut out = vec![0f32; (secs * RATE) as usize * 2];
    let warn = r.mix_checked(Path::new("."), p, &res, t0, &mut out).unwrap();
    assert!(warn.is_empty(), "{warn:?}");
    out
}

fn audio_clip(id: &str, track: &str, asset: &str, start: f64, dur: f64, extra: serde_json::Value) -> aiframes_core::Clip {
    let mut v = json!({"id": id, "track": track, "start": start, "duration": dur, "source": {"type": "media", "asset": asset}});
    for (k, x) in extra.as_object().unwrap() {
        if k == "in_point" {
            v["source"]["in_point"] = x.clone();
        } else {
            v[k] = x.clone();
        }
    }
    clip(v)
}

fn sine_project(extra: serde_json::Value) -> Project {
    let mut p = project(64, 36, 30.0);
    add_media(&mut p, "s", &sine_wav("sine1k_m20", 1000.0, 0.1, 6.0));
    p.clips = vec![audio_clip("c1", "a1", "s", 0.0, 4.0, extra)];
    p
}

#[test]
fn levels_follow_clip_and_track_volume() {
    let full = mix(&sine_project(json!({})), 1.0, 1.0);
    assert!((peak_db(&full) + 20.0).abs() < 0.1, "{}", peak_db(&full));
    let half = mix(&sine_project(json!({"volume": 0.5})), 1.0, 1.0);
    assert!((peak_db(&half) + 26.02).abs() < 0.1, "a -20 dBFS sine at volume 0.5: {}", peak_db(&half));
    assert!((rms_db(&half) - rms_db(&full) + 6.02).abs() < 0.05);
    let mut p = sine_project(json!({"volume": 0.5}));
    p.tracks[3].volume = 0.5;
    assert!((peak_db(&mix(&p, 1.0, 1.0)) + 32.04).abs() < 0.1);
    // Animated volume keys.
    let p = sine_project(json!({"volume": {"keys": [{"t": 0, "v": 0}, {"t": 2, "v": 1}]}}));
    let a = mix(&p, 0.99, 0.02);
    assert!((rms_db(&a) - (-20.0 - 3.01 + 20.0 * 0.5f64.log10())).abs() < 0.15, "{}", rms_db(&a));
}

#[test]
fn fades_shape_the_level() {
    let p = sine_project(json!({"fade_in": 1.0, "fade_out": 1.0}));
    let at = |t: f64| rms_db(&mix(&p, t - 0.01, 0.02));
    let steady = at(2.0);
    assert!((at(0.5) - steady + 6.02).abs() < 0.3, "{} {}", at(0.5), steady);
    assert!((at(3.5) - steady + 6.02).abs() < 0.3);
    assert!(rms_db(&mix(&p, 0.0, 0.002)) < steady - 30.0);
    assert!(rms_db(&mix(&p, 4.0, 0.5)) < -100.0, "silent after the clip");
}

#[test]
fn speed_changes_pitch_and_duration() {
    let normal = mix(&sine_project(json!({})), 1.0, 1.0);
    let fast = mix(&sine_project(json!({"speed": 2.0})), 1.0, 1.0);
    let slow = mix(&sine_project(json!({"speed": 0.5})), 1.0, 1.0);
    let f = |x: &[f32]| crossings_per_sec(x, RATE) / 2.0;
    assert!((f(&normal) - 1000.0).abs() < 5.0, "{}", f(&normal));
    assert!((f(&fast) - 2000.0).abs() < 10.0, "{}", f(&fast));
    assert!((f(&slow) - 500.0).abs() < 5.0, "{}", f(&slow));
    assert!((peak_db(&fast) + 20.0).abs() < 0.5 && (peak_db(&slow) + 20.0).abs() < 0.3);
    // Duration: a 1 s burst (then silence) at speed 2 lasts 0.5 s on the timeline.
    let mut p = project(64, 36, 30.0);
    let burst = fixture("burst1s.wav", |out| {
        ffmpeg(&["-f", "lavfi", "-i", "aevalsrc=exprs='if(lt(t,1),0.3*sin(2*PI*440*t),0)':s=48000:d=3:c=stereo", "-c:a", "pcm_f32le", out]);
    });
    add_media(&mut p, "b", &burst);
    p.clips = vec![audio_clip("c1", "a1", "b", 0.0, 1.5, json!({"speed": 2.0}))];
    assert!(rms_db(&mix(&p, 0.40, 0.08)) > -20.0);
    assert!(rms_db(&mix(&p, 0.52, 0.2)) < -90.0);
    // Reverse: silence first, then the burst.
    p.clips = vec![audio_clip("c1", "a1", "b", 0.0, 2.0, json!({"reverse": true}))];
    assert!(rms_db(&mix(&p, 0.2, 0.6)) < -90.0);
    assert!(rms_db(&mix(&p, 1.2, 0.6)) > -20.0);
    // Freeze frames are silent; ramps play.
    p.clips = vec![audio_clip("c1", "a1", "b", 0.0, 1.0, json!({"speed": 0.0}))];
    assert!(rms_db(&mix(&p, 0.1, 0.5)) < -100.0);
    p.clips = vec![audio_clip("c1", "a1", "b", 0.0, 1.0, json!({"speed": {"keys": [{"t": 0, "v": 0.5}, {"t": 1, "v": 1.5}]}}))];
    let ramp = mix(&p, 0.0, 0.9);
    assert!(rms_db(&ramp) > -20.0);
}

#[test]
fn mute_solo_and_video_sound() {
    let mut p = sine_project(json!({}));
    add_media(&mut p, "v", &bars(30, 6));
    p.clips.push(audio_clip("c2", "v1", "v", 0.0, 4.0, json!({})));
    let both = mix(&p, 1.0, 0.5);
    assert!(peak_db(&both) > -20.0 + 1.0, "video sound mixes in");
    p.tracks[3].muted = true;
    let video_only = mix(&p, 1.0, 0.5);
    assert!(rms_db(&video_only) > -30.0);
    p.clips[1].muted = true;
    assert!(rms_db(&mix(&p, 1.0, 0.5)) < -120.0);
    p.tracks[3].muted = false;
    p.clips[1].muted = false;
    p.tracks[0].solo = true;
    let solo = mix(&p, 1.0, 0.5);
    assert!((rms_db(&solo) - rms_db(&video_only)).abs() < 0.01, "solo: only the video track");
}

#[test]
fn transitions_crossfade_with_constant_power() {
    let mut p = project(64, 36, 30.0);
    add_media(&mut p, "x", &sine_wav("sine440_m10", 440.0, 0.3, 6.0));
    add_media(&mut p, "y", &sine_wav("sine1k3_m10", 1300.0, 0.3, 6.0));
    p.clips = vec![audio_clip("A", "a1", "x", 0.0, 2.0, json!({})), audio_clip("B", "a1", "y", 2.0, 2.0, json!({"in_point": 1.0}))];
    p.transitions.push(Transition { id: "t".into(), from: "A".into(), to: "B".into(), kind: "crossfade".into(), duration: 1.0, params: Default::default(), colors: Default::default() });
    let steady = rms_db(&mix(&p, 1.0, 0.3));
    for t in [1.6, 1.9, 2.0, 2.2, 2.4] {
        let lv = rms_db(&mix(&p, t - 0.02, 0.04));
        assert!((lv - steady).abs() < 0.6, "power is constant across the crossfade at {t}: {lv} vs {steady}");
    }
    // A continues past its end (its handle) at reduced gain.
    let mut only_a = p.clone();
    only_a.clips[1].muted = true;
    let a = rms_db(&mix(&only_a, 2.2, 0.02));
    let p_ = 0.7f64;
    assert!((a - (steady + 20.0 * (p_ * std::f64::consts::FRAC_PI_2).cos().log10())).abs() < 0.6, "{a}");
}

#[test]
fn ducking_lowers_the_target_only_while_the_trigger_plays() {
    let mut p = project(64, 36, 30.0);
    add_media(&mut p, "music", &sine_wav("sine440_m10", 440.0, 0.3, 6.0));
    add_media(&mut p, "voice", &sine_wav("sine1k3_m10", 1300.0, 0.3, 6.0));
    p.clips = vec![audio_clip("m", "a1", "music", 0.0, 6.0, json!({})), audio_clip("v", "a2", "voice", 2.0, 1.5, json!({}))];
    p.ducking.push(Ducking { id: "d".into(), target: "a1".into(), trigger: "a2".into(), amount_db: -12.0, threshold_db: -45.0, attack: 0.1, release: 0.4 });
    let r = renderer();
    let res = resolved(&p);
    let music = |t: f64| {
        let mut out = vec![0f32; 4800 * 2];
        r.mix_track(Path::new("."), &p, &res, "a1", t, &mut out).unwrap();
        rms_db(&out)
    };
    let before = music(1.0);
    let during = music(2.6);
    let after = music(5.0);
    assert!((during - before + 12.0).abs() < 0.3, "ducked by 12 dB: {before} → {during}");
    assert!((after - before).abs() < 0.05, "recovers after release: {after}");
    // The trigger itself is untouched.
    let mut out = vec![0f32; 4800 * 2];
    r.mix_track(Path::new("."), &p, &res, "a2", 2.6, &mut out).unwrap();
    assert!((peak_db(&out) - 20.0 * 0.3f64.log10()).abs() < 0.1);
    // Muting the trigger removes the ducking.
    let mut q = p.clone();
    q.tracks[4].muted = true;
    let qres = resolved(&q);
    let mut out = vec![0f32; 4800 * 2];
    r.mix_track(Path::new("."), &q, &qres, "a1", 2.6, &mut out).unwrap();
    assert!((rms_db(&out) - before).abs() < 0.05);
}

#[test]
fn small_blocks_equal_one_big_block() {
    let mut p = project(64, 36, 30.0);
    add_media(&mut p, "music", &sine_wav("sine440_m10", 440.0, 0.3, 6.0));
    add_media(&mut p, "voice", &sine_wav("sine1k3_m10", 1300.0, 0.3, 6.0));
    p.clips = vec![
        audio_clip("m", "a1", "music", 0.0, 2.0, json!({"fade_in": 0.3, "speed": 1.25})),
        audio_clip("m2", "a1", "music", 2.0, 2.0, json!({"in_point": 0.5})),
        audio_clip("v", "a2", "voice", 1.0, 1.5, json!({"volume": 0.7})),
    ];
    p.transitions.push(Transition { id: "t".into(), from: "m".into(), to: "m2".into(), kind: "crossfade".into(), duration: 0.5, params: Default::default(), colors: Default::default() });
    p.ducking.push(Ducking { id: "d".into(), target: "a1".into(), trigger: "a2".into(), amount_db: -10.0, threshold_db: -45.0, attack: 0.12, release: 0.45 });
    let r = renderer();
    let res = resolved(&p);
    let t0 = 0.5;
    let n = 48_000 * 3;
    let mut big = vec![0f32; n * 2];
    r.mix(Path::new("."), &p, &res, t0, &mut big).unwrap();
    let mut small = vec![0f32; n * 2];
    let block = 1024;
    let mut i = 0;
    let started = std::time::Instant::now();
    while i < n {
        let m = block.min(n - i);
        r.mix(Path::new("."), &p, &res, t0 + i as f64 / RATE, &mut small[i * 2..(i + m) * 2]).unwrap();
        i += m;
    }
    let per_block = started.elapsed().as_secs_f64() / (n as f64 / block as f64);
    let max_err = big.iter().zip(&small).fold(0f32, |a, (x, y)| a.max((x - y).abs()));
    assert!(max_err < 1e-4, "block size changes the mix by {max_err}");
    // Real time: a 21 ms block must mix far faster than it plays (debug builds included).
    assert!(per_block < 0.021, "{:.2} ms per 1024-frame block", per_block * 1000.0);
}

#[test]
fn soft_clip_keeps_hot_mixes_in_range() {
    let mut p = project(64, 36, 30.0);
    add_media(&mut p, "x", &sine_wav("sine440_m10", 440.0, 0.3, 6.0));
    p.clips = vec![audio_clip("A", "a1", "x", 0.0, 2.0, json!({"volume": 6.0}))];
    let out = mix(&p, 0.5, 0.5);
    assert!(out.iter().all(|v| v.abs() <= 1.0));
    assert!(peak_db(&out) > -0.5);
}
