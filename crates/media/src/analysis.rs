//! Analysis for editing decisions: scene cuts, silences, beats.

use crate::audio::stream_pcm;
use crate::error::{MediaError, Result};
use crate::probe::probe_full;
use crate::tools::{ff_path, ffmpeg_bg, lock, spawn_error, ChildGuard, StderrPump};
use rustfft::{num_complex::Complex32, FftPlanner};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------------------------
// Scenes
// ---------------------------------------------------------------------------------------------

/// Times (source seconds) of the first frame of each new shot, using ffmpeg's scene-change score.
/// `threshold` is 0..1 (0.3 is a good default; lower finds more cuts). Values above 1 are read as
/// percentages (30 = 0.3).
pub fn detect_scenes(path: &Path, threshold: f64) -> Result<Vec<f64>> {
    let p = probe_full(path)?;
    let Some(vi) = p.video_index else {
        return Err(MediaError::unsupported(path, "has no video stream to detect scenes in"));
    };
    let mut th = if threshold.is_finite() { threshold } else { 0.3 };
    if th > 1.0 {
        th /= 100.0;
    }
    let th = th.clamp(0.001, 1.0);
    // Analyse a small copy of each frame: the scene score barely depends on resolution.
    let (w, h) = crate::util::fit_size(p.info.width, p.info.height, Some((320, 320)));
    let (w, h) = ((w / 2 * 2).max(2), (h / 2 * 2).max(2));
    let mut c = ffmpeg_bg("info")?;
    c.arg("-i").arg(ff_path(path));
    c.args(["-map", &format!("0:{vi}"), "-an", "-sn", "-dn"]);
    let vf = format!("scale={w}:{h}:flags=fast_bilinear,format=yuv420p,select=gt(scene\\,{th}),showinfo=checksum=0");
    c.args(["-vf", &vf, "-fps_mode", "passthrough", "-f", "null", "-"]);
    c.stdout(Stdio::null()).stderr(Stdio::piped());
    let child = c.spawn().map_err(|e| spawn_error("ffmpeg", e))?;
    let mut guard = ChildGuard::new(child);
    let times = Arc::new(Mutex::new(Vec::<f64>::new()));
    let tb = Arc::new(Mutex::new(0f64));
    let (t2, tb2) = (times.clone(), tb.clone());
    let pump = StderrPump::start(guard.child().stderr.take().expect("stderr"), move |line| {
        if !line.contains("Parsed_showinfo") {
            return false;
        }
        if let Some(i) = line.find("config in time_base:") {
            let tok = line[i + 20..].trim_start().split([',', ' ']).next().unwrap_or("");
            if let Some((a, b)) = tok.split_once('/') {
                if let (Ok(a), Ok(b)) = (a.parse::<f64>(), b.parse::<f64>()) {
                    if a > 0.0 && b > 0.0 {
                        *lock(&tb2) = a / b;
                    }
                }
            }
        } else if line.contains(" n:") {
            if let Some(i) = line.find(" pts:") {
                if let Ok(v) = line[i + 5..].split_whitespace().next().unwrap_or("").parse::<i64>() {
                    let tb = *lock(&tb2);
                    if tb > 0.0 {
                        lock(&t2).push(v as f64 * tb);
                    }
                }
            }
        }
        true
    });
    let status = guard.wait().map_err(|e| MediaError::io(path, "wait for ffmpeg", &e))?;
    let tail = pump.finish();
    if !status.success() {
        return Err(MediaError::tool("ffmpeg", Some(path), "detect scene changes", status.code(), tail.summary()));
    }
    let mut v = std::mem::take(&mut *lock(&times));
    v.sort_by(f64::total_cmp);
    v.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    Ok(v)
}

// ---------------------------------------------------------------------------------------------
// Silence
// ---------------------------------------------------------------------------------------------

/// Gate window for silence detection.
const SILENCE_WINDOW: f64 = 0.010;

/// `[start, end]` source seconds of every stretch at least `min_len` long whose level stays under
/// `noise_db` dBFS (RMS over 10 ms windows, the louder channel). A positive `noise_db` is read as
/// negative (40 = -40 dBFS). Files without audio are one silence over their whole duration.
pub fn detect_silence(path: &Path, noise_db: f64, min_len: f64) -> Result<Vec<[f64; 2]>> {
    let p = probe_full(path)?;
    if !p.info.has_audio {
        let d = p.info.duration;
        return Ok(if d > 0.0 && d >= min_len { vec![[0.0, d]] } else { vec![] });
    }
    let rate = 16_000u32;
    let mut gate = SilenceGate::new(rate, noise_db, min_len);
    stream_pcm(path, &p, rate, 2, |chunk| {
        gate.push(chunk);
        true
    })?;
    Ok(gate.finish())
}

/// Silence detection on stereo interleaved PCM (e.g. a rendered mix); same rules as `detect_silence`.
pub fn silence_of_pcm(stereo: &[f32], rate: u32, noise_db: f64, min_len: f64) -> Vec<[f64; 2]> {
    let mut gate = SilenceGate::new(rate, noise_db, min_len);
    gate.push(stereo);
    gate.finish()
}

struct SilenceGate {
    window: usize,
    threshold_sq: f64,
    min_len: f64,
    rate: f64,
    // current window accumulators
    n: usize,
    sl: f64,
    sr: f64,
    windows: u64,
    run_start: Option<u64>,
    out: Vec<[f64; 2]>,
}

impl SilenceGate {
    fn new(rate: u32, noise_db: f64, min_len: f64) -> Self {
        let db = if noise_db.is_finite() { -noise_db.abs() } else { -50.0 };
        let window = ((rate as f64 * SILENCE_WINDOW).round() as usize).max(1);
        SilenceGate {
            window,
            threshold_sq: 10f64.powf(db / 10.0),
            min_len: if min_len.is_finite() { min_len.max(0.0) } else { 0.5 },
            rate: rate.max(1) as f64,
            n: 0,
            sl: 0.0,
            sr: 0.0,
            windows: 0,
            run_start: None,
            out: Vec::new(),
        }
    }

    fn push(&mut self, stereo: &[f32]) {
        for fr in stereo.as_chunks::<2>().0 {
            self.sl += fr[0] as f64 * fr[0] as f64;
            self.sr += fr[1] as f64 * fr[1] as f64;
            self.n += 1;
            if self.n == self.window {
                self.close_window();
            }
        }
    }

    fn close_window(&mut self) {
        let ms = (self.sl.max(self.sr)) / self.n.max(1) as f64;
        let silent = ms < self.threshold_sq;
        let idx = self.windows;
        self.windows += 1;
        self.n = 0;
        self.sl = 0.0;
        self.sr = 0.0;
        match (silent, self.run_start) {
            (true, None) => self.run_start = Some(idx),
            (false, Some(s)) => {
                self.emit(s, idx);
                self.run_start = None;
            }
            _ => {}
        }
    }

    fn emit(&mut self, start_w: u64, end_w: u64) {
        let w = self.window as f64 / self.rate;
        let (a, b) = (start_w as f64 * w, end_w as f64 * w);
        if b - a + 1e-9 >= self.min_len {
            self.out.push([a, b]);
        }
    }

    fn finish(mut self) -> Vec<[f64; 2]> {
        let total = self.windows as f64 * self.window as f64 + self.n as f64;
        if self.n > 0 {
            self.close_window();
        }
        if let Some(s) = self.run_start.take() {
            let w = self.window as f64 / self.rate;
            let a = s as f64 * w;
            let b = total / self.rate;
            if b - a + 1e-9 >= self.min_len {
                self.out.push([a, b]);
            }
        }
        self.out
    }
}

// ---------------------------------------------------------------------------------------------
// Beats
// ---------------------------------------------------------------------------------------------

/// Beat analysis result.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Beats {
    /// Estimated tempo in beats per minute (0 when no rhythm was found).
    pub bpm: f64,
    /// Beat times in source seconds.
    pub beats: Vec<f64>,
    /// Detected note onsets (transients) in source seconds — a superset useful for cutting on hits.
    pub onsets: Vec<f64>,
}

/// Beat times (source seconds) of the file's music.
pub fn detect_beats(path: &Path) -> Result<Vec<f64>> {
    analyze_beats(path).map(|b| b.beats)
}

/// Tempo, beats and onsets of the file's audio.
pub fn analyze_beats(path: &Path) -> Result<Beats> {
    let p = probe_full(path)?;
    if !p.info.has_audio {
        return Err(MediaError::unsupported(path, "has no audio stream to detect beats in"));
    }
    let mut mono: Vec<f32> = Vec::new();
    stream_pcm(path, &p, BEAT_RATE, 1, |chunk| {
        mono.extend_from_slice(chunk);
        true
    })?;
    Ok(beats_of_mono(&mono, BEAT_RATE))
}

/// Beat analysis of stereo interleaved PCM at any rate.
pub fn beats_of_pcm(stereo: &[f32], rate: u32) -> Beats {
    let mono: Vec<f32> = stereo.as_chunks::<2>().0.iter().map(|f| 0.5 * (f[0] + f[1])).collect();
    if rate == BEAT_RATE {
        return beats_of_mono(&mono, rate);
    }
    // Linear resample to the analysis rate (fine for onset envelopes).
    let ratio = rate as f64 / BEAT_RATE as f64;
    let n = (mono.len() as f64 / ratio) as usize;
    let res: Vec<f32> = (0..n)
        .map(|i| {
            let x = i as f64 * ratio;
            let j = x as usize;
            let f = (x - j as f64) as f32;
            let a = mono[j.min(mono.len() - 1)];
            let b = mono[(j + 1).min(mono.len() - 1)];
            a + (b - a) * f
        })
        .collect();
    beats_of_mono(&res, BEAT_RATE)
}

const BEAT_RATE: u32 = 24_000;
const HOP: usize = 240; // 10 ms at 24 kHz -> 100 envelope frames per second
const NFFT: usize = 1024;

fn beats_of_mono(mono: &[f32], rate: u32) -> Beats {
    let fr = rate as f64 / HOP as f64;
    let env = onset_envelope(mono);
    if env.len() < (fr * 2.0) as usize {
        return Beats::default();
    }
    let onsets_idx = pick_onsets(&env, fr);
    let onsets: Vec<f64> = onsets_idx.iter().map(|&k| refine(mono, rate, k as f64 / fr)).collect();
    let Some(period) = estimate_period(&env, fr) else {
        return Beats { bpm: 0.0, beats: vec![], onsets };
    };
    let beat_frames = track_beats(&env, period);
    let mut beats: Vec<f64> = beat_frames.iter().map(|&k| refine(mono, rate, k as f64 / fr)).collect();
    beats.sort_by(f64::total_cmp);
    beats.dedup_by(|a, b| (*a - *b).abs() < 0.5 * period / fr);
    Beats { bpm: 60.0 * fr / period, beats, onsets }
}

/// Spectral flux of the log-magnitude spectrum, high-passed and normalised (one value per hop).
fn onset_envelope(mono: &[f32]) -> Vec<f32> {
    let n_frames = mono.len() / HOP + 1;
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(NFFT);
    let window: Vec<f32> = (0..NFFT).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / NFFT as f32).cos()).collect();
    let bins = NFFT / 2;
    let mut prev = vec![0f32; bins];
    let mut cur = vec![0f32; bins];
    let mut buf = vec![Complex32::new(0.0, 0.0); NFFT];
    let mut flux = Vec::with_capacity(n_frames);
    for k in 0..n_frames {
        let center = (k * HOP) as isize;
        for (i, b) in buf.iter_mut().enumerate() {
            let s = center + i as isize - (NFFT / 2) as isize;
            let v = if s >= 0 && (s as usize) < mono.len() { mono[s as usize] } else { 0.0 };
            *b = Complex32::new(v * window[i], 0.0);
        }
        fft.process(&mut buf);
        let mut f = 0f32;
        for i in 1..bins {
            cur[i] = (1.0 + 1000.0 * buf[i].norm()).ln();
            if k > 0 {
                f += (cur[i] - prev[i]).max(0.0);
            }
        }
        std::mem::swap(&mut prev, &mut cur);
        flux.push(f);
    }
    // Remove the slowly varying part (local mean over ~0.5 s) and half-wave rectify.
    let half = 25usize;
    let mut prefix = vec![0f64; flux.len() + 1];
    for (i, v) in flux.iter().enumerate() {
        prefix[i + 1] = prefix[i] + *v as f64;
    }
    let mut env: Vec<f32> = (0..flux.len())
        .map(|i| {
            let a = i.saturating_sub(half);
            let b = (i + half + 1).min(flux.len());
            let mean = (prefix[b] - prefix[a]) / (b - a) as f64;
            (flux[i] as f64 - mean).max(0.0) as f32
        })
        .collect();
    let sd = (env.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / env.len().max(1) as f64).sqrt();
    if sd > 1e-9 {
        env.iter_mut().for_each(|v| *v = (*v as f64 / sd) as f32);
    }
    env
}

/// Local maxima of the envelope above an adaptive threshold, at least 50 ms apart.
fn pick_onsets(env: &[f32], fr: f64) -> Vec<usize> {
    let w = (0.05 * fr).round().max(1.0) as usize;
    let mut out = Vec::new();
    let mut last: Option<usize> = None;
    for i in 0..env.len() {
        let a = i.saturating_sub(w);
        let b = (i + w + 1).min(env.len());
        let local_max = env[a..b].iter().copied().fold(0.0f32, f32::max);
        let a2 = i.saturating_sub(3 * w);
        let b2 = (i + w + 1).min(env.len());
        let mean = env[a2..b2].iter().sum::<f32>() / (b2 - a2) as f32;
        if env[i] >= local_max && env[i] > mean + 0.5 && env[i] > 1.0 && last.is_none_or(|l| i - l >= w) {
            out.push(i);
            last = Some(i);
        }
    }
    out
}

/// Beat period in envelope frames: autocorrelation weighted by a log-normal tempo prior around
/// 120 BPM, refined to a fraction of a frame.
fn estimate_period(env: &[f32], fr: f64) -> Option<f64> {
    let min_lag = (60.0 / 240.0 * fr).floor() as usize;
    let max_lag = ((60.0 / 40.0 * fr).ceil() as usize).min(env.len() / 2);
    if max_lag <= min_lag + 2 {
        return None;
    }
    let ac: Vec<f64> = (0..=max_lag + 1)
        .map(|lag| {
            if lag >= env.len() {
                return 0.0;
            }
            env[..env.len() - lag].iter().zip(&env[lag..]).map(|(a, b)| (*a as f64) * (*b as f64)).sum::<f64>()
                / (env.len() - lag) as f64
        })
        .collect();
    let weight = |lag: f64| {
        let bpm = 60.0 * fr / lag;
        (-0.5 * ((bpm / 120.0).log2() / 1.0).powi(2)).exp()
    };
    let mut best = None::<(usize, f64)>;
    for (lag, a) in ac.iter().enumerate().take(max_lag + 1).skip(min_lag.max(1)) {
        let s = a * weight(lag as f64);
        if best.is_none_or(|(_, bs)| s > bs) {
            best = Some((lag, s));
        }
    }
    let (lag, score) = best?;
    if score <= 1e-6 || ac[lag] < 0.05 * ac[0] {
        return None;
    }
    // Parabolic refinement.
    let (y0, y1, y2) = (ac[lag - 1], ac[lag], ac[lag + 1]);
    let denom = y0 - 2.0 * y1 + y2;
    let delta = if denom.abs() > 1e-12 { (0.5 * (y0 - y2) / denom).clamp(-0.5, 0.5) } else { 0.0 };
    Some(lag as f64 + delta)
}

/// Dynamic-programming beat tracker (Ellis 2007): beats on strong onsets, spaced close to `period`.
fn track_beats(env: &[f32], period: f64) -> Vec<usize> {
    let n = env.len();
    // Smooth the envelope a little so near-misses still score.
    let sigma = (period / 32.0).max(0.5);
    let radius = (3.0 * sigma).ceil() as isize;
    let kernel: Vec<f64> = (-radius..=radius).map(|i| (-0.5 * (i as f64 / sigma).powi(2)).exp()).collect();
    let local: Vec<f64> = (0..n as isize)
        .map(|i| {
            kernel.iter().enumerate().map(|(j, k)| {
                let x = i + j as isize - radius;
                if x >= 0 && (x as usize) < n { env[x as usize] as f64 * k } else { 0.0 }
            }).sum()
        })
        .collect();
    let tightness = 100.0;
    let mut score = vec![0f64; n];
    let mut back = vec![usize::MAX; n];
    let lo = (period / 2.0).round() as usize;
    let hi = (2.0 * period).round() as usize;
    for t in 0..n {
        let mut best = 0.0f64;
        let mut arg = usize::MAX;
        if t >= lo {
            for (tau, st) in score.iter().enumerate().take(t - lo + 1).skip(t.saturating_sub(hi)) {
                let d = (t - tau) as f64;
                let s = st - tightness * (d / period).ln().powi(2);
                if arg == usize::MAX || s > best {
                    best = s;
                    arg = tau;
                }
            }
        }
        score[t] = local[t] + if arg == usize::MAX { 0.0 } else { best.max(0.0) };
        back[t] = if arg != usize::MAX && best > 0.0 { arg } else { usize::MAX };
    }
    // Last beat: the last local maximum of the cumulative score that is reasonably strong.
    let maxima: Vec<usize> = (1..n.saturating_sub(1)).filter(|&i| score[i] >= score[i - 1] && score[i] >= score[i + 1]).collect();
    if maxima.is_empty() {
        return vec![];
    }
    let mut vals: Vec<f64> = maxima.iter().map(|&i| score[i]).collect();
    vals.sort_by(f64::total_cmp);
    let med = vals[vals.len() / 2];
    let Some(&last) = maxima.iter().rev().find(|&&i| score[i] >= 0.5 * med) else { return vec![] };
    let mut beats = vec![last];
    let mut cur = last;
    while back[cur] != usize::MAX {
        cur = back[cur];
        beats.push(cur);
    }
    beats.reverse();
    // Trim weak beats at the edges (silence before/after the music).
    let rms = (local.iter().map(|v| v * v).sum::<f64>() / n as f64).sqrt();
    let strong = |b: &usize| local[*b] >= 0.5 * rms;
    let first = beats.iter().position(strong).unwrap_or(0);
    let last = beats.iter().rposition(strong).map(|i| i + 1).unwrap_or(beats.len());
    beats[first..last.max(first)].to_vec()
}

/// Moves a coarse onset time to the sharpest energy rise within ±40 ms (1 ms resolution), if any.
fn refine(mono: &[f32], rate: u32, t: f64) -> f64 {
    let ms = (rate as usize / 1000).max(1);
    let center = (t * rate as f64) as isize;
    let span = 40isize * ms as isize;
    let lookback = 5isize;
    let energy = |w: isize| -> f64 {
        let a = w * ms as isize;
        if a < 0 || a as usize + ms > mono.len() {
            return 0.0;
        }
        mono[a as usize..a as usize + ms].iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / ms as f64
    };
    let w0 = (center - span) / ms as isize;
    let w1 = (center + span) / ms as isize;
    let mut best = (0.0f64, None::<isize>);
    for w in w0..=w1 {
        let before: f64 = (1..=lookback).map(|k| energy(w - k)).sum::<f64>() / lookback as f64;
        let rise = ((energy(w) + 1e-10) / (before + 1e-10)).ln();
        if rise > best.0 {
            best = (rise, Some(w));
        }
    }
    match best {
        (r, Some(w)) if r > 4f64.ln() => (w * ms as isize) as f64 / rate as f64,
        _ => t,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clicks(bpm: f64, secs: f64, offset: f64, rate: u32) -> Vec<f32> {
        let n = (secs * rate as f64) as usize;
        let period = 60.0 / bpm;
        (0..n)
            .map(|i| {
                let t = i as f64 / rate as f64;
                if t < offset {
                    return 0.0;
                }
                let ph = (t - offset) % period;
                (0.8 * (2.0 * std::f64::consts::PI * 1500.0 * ph).sin() * (-60.0 * ph).exp()) as f32
            })
            .collect()
    }

    #[test]
    fn click_track_tempo_and_beats() {
        for bpm in [100.0, 128.0] {
            let mono = clicks(bpm, 12.0, 0.25, BEAT_RATE);
            let b = beats_of_mono(&mono, BEAT_RATE);
            assert!((b.bpm - bpm).abs() < 1.5, "bpm {} vs {bpm}", b.bpm);
            let period = 60.0 / bpm;
            let expected: Vec<f64> = (0..).map(|k| 0.25 + k as f64 * period).take_while(|t| *t < 11.9).collect();
            let matched = expected.iter().filter(|e| b.beats.iter().any(|x| (x - *e).abs() < 0.015)).count();
            assert!(matched as f64 >= expected.len() as f64 * 0.9, "{matched}/{} beats matched: {:?}", expected.len(), b.beats);
            assert!(b.beats.len() <= expected.len() + 1);
        }
    }

    #[test]
    fn silence_gate() {
        let rate = 16_000u32;
        let mut s = Vec::new();
        let tone = |secs: f64, s: &mut Vec<f32>| {
            for i in 0..(secs * rate as f64) as usize {
                let v = 0.3 * (i as f32 * 0.2).sin();
                s.push(v);
                s.push(v);
            }
        };
        let quiet = |secs: f64, s: &mut Vec<f32>| s.extend(std::iter::repeat_n(0.0005f32, (secs * rate as f64) as usize * 2));
        tone(1.0, &mut s);
        quiet(1.5, &mut s);
        tone(0.5, &mut s);
        quiet(0.2, &mut s); // too short
        tone(0.5, &mut s);
        quiet(1.0, &mut s);
        let r = silence_of_pcm(&s, rate, -40.0, 0.5);
        assert_eq!(r.len(), 2, "{r:?}");
        assert!((r[0][0] - 1.0).abs() < 0.011 && (r[0][1] - 2.5).abs() < 0.011, "{r:?}");
        assert!((r[1][0] - 3.7).abs() < 0.011 && (r[1][1] - 4.7).abs() < 0.011, "{r:?}");
    }
}
