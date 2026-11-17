//! The audio mixer: audio clips and the sound of video clips, clip volume × track volume × fades,
//! speed (varispeed resampling, ramps, reverse; freeze frames are silent), solo/mute,
//! constant-power crossfades across transitions, sidechain ducking and a soft-clip safety.
//!
//! `mix` is stateless (any block of any length at any time gives the same samples), so playback
//! can pull small blocks and export can mix in parallel chunks. Ducking stays deterministic by
//! re-running its envelope over a short look-back from a cached per-hop level of the trigger.

use crate::timeline::{TrackInfo, Timeline};
use crate::util::{lock, KeyHasher, Lru};
use crate::{RenderError, Renderer};
use aiframes_core::eval::{audio_gain, sample, source_time};
use aiframes_core::{Clip, Ducking, MediaKind, Param, Project, Resolved, Source, Span};
use aiframes_media::AudioData;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Samples per ducking analysis hop.
const HOP: usize = 256;
/// Hops per cached level chunk.
const CHUNK_HOPS: i64 = 2048;
/// The ducked level is held this long after the trigger falls below the threshold (bridges the
/// gaps between words).
pub(crate) const DUCK_HOLD: f64 = 0.3;
/// Sub-block length for speed and gain evaluation.
const SUB: usize = 256;

pub(crate) struct MixOpts<'a> {
    pub soft_clip: bool,
    pub strict: bool,
    /// Mix only this track (with its ducking).
    pub only_track: Option<&'a str>,
}

/// Cached trigger levels: signature → chunk index → per-hop peak (dBFS).
pub(crate) type DuckCache = Mutex<Lru<Arc<Mutex<HashMap<i64, Arc<Vec<f32>>>>>>>;

pub(crate) fn new_duck_cache() -> DuckCache {
    Mutex::new(Lru::new(usize::MAX, 16))
}

pub(crate) fn rate_of(p: &Project) -> u32 {
    p.settings.sample_rate.clamp(8000, 192_000)
}

struct Mixer<'a> {
    rd: &'a Renderer,
    doc_dir: &'a Path,
    tl: Timeline<'a>,
    rate: u32,
    strict: bool,
    warnings: Mutex<Vec<String>>,
    /// Set once a read timed out on audio still being decoded: later reads in this call don't wait,
    /// so a playback block is late by one short wait at most.
    stalled: AtomicBool,
}

/// How long one playback read may wait for samples still being decoded.
const READ_WAIT: Duration = Duration::from_millis(150);

impl Mixer<'_> {
    fn problem(&self, e: RenderError) -> Result<(), RenderError> {
        if self.strict {
            return Err(e);
        }
        let m = e.to_string();
        let mut w = lock(&self.warnings);
        if !w.contains(&m) {
            w.push(m);
        }
        Ok(())
    }

    /// The decoded sound of a clip (the sound stream it plays), if it has any.
    fn clip_audio(&self, clip: &Clip) -> Result<Option<Arc<AudioData>>, RenderError> {
        let Source::Media { asset, stream, .. } = &clip.source else { return Ok(None) };
        if clip.muted || !clip.enabled {
            return Ok(None);
        }
        let Some(a) = self.tl.assets.get(asset.as_str()) else {
            self.problem(RenderError::Missing(format!("clip {} uses media asset `{asset}`, which is not in the project", clip.id)))?;
            return Ok(None);
        };
        match a.kind {
            MediaKind::Image => return Ok(None),
            MediaKind::Video if !a.info.has_audio => return Ok(None),
            _ => {}
        }
        let path = self.doc_dir.join(&a.path);
        match self.rd.pool().audio_stream(&path, self.rate, *stream) {
            Ok(d) => {
                // Export (strict) always mixes complete, exact audio; playback reads what is decoded
                // (reads of samples still decoding wait briefly inside the pool).
                if self.strict && !d.is_complete() {
                    if let Err(e) = d.wait_complete() {
                        self.problem(RenderError::Media(e))?;
                        return Ok(None);
                    }
                }
                Ok(Some(d))
            }
            Err(e) => {
                let err = if !path.exists() {
                    RenderError::Missing(format!("media file {} (clip {}) is offline", path.display(), clip.id))
                } else if *stream > 0 {
                    RenderError::Missing(format!("clip {} plays sound stream {stream} of {}: {e}", clip.id, a.id))
                } else {
                    RenderError::Media(e)
                };
                self.problem(err)?;
                Ok(None)
            }
        }
    }

    /// Adds one track's sound (track volume included, no ducking) for `[t0, t0 + n)` into `buf`.
    /// Returns false when a source was still being decoded (the result may miss samples).
    fn mix_track(&self, info: &TrackInfo, t0: f64, buf: &mut [f32]) -> Result<bool, RenderError> {
        let vol = if info.track.volume.is_finite() { info.track.volume.max(0.0) } else { 1.0 } as f32;
        if vol <= 0.0 {
            return Ok(true);
        }
        let mut complete = true;
        let n = buf.len() / 2;
        let rate = self.rate as f64;
        let t1 = t0 + n as f64 / rate;
        for (i, (clip, span)) in info.clips.iter().enumerate() {
            if !clip.enabled || clip.muted || !matches!(clip.source, Source::Media { .. }) {
                continue;
            }
            let (mut a, mut b) = (span.start, span.end);
            let mut win_in = None;
            let mut win_out = None;
            for x in &info.trans {
                if !info.clips[x.from].0.enabled || !info.clips[x.to].0.enabled {
                    continue;
                }
                if x.to == i {
                    a = a.min(x.start());
                    win_in = Some((x.start(), x.end()));
                }
                if x.from == i {
                    b = b.max(x.end());
                    win_out = Some((x.start(), x.end()));
                }
            }
            if b <= t0 || a >= t1 {
                continue;
            }
            let Some(data) = self.clip_audio(clip)? else { continue };
            complete &= data.is_complete();
            render_clip(clip, *span, &data, (a, b), win_in, win_out, vol, t0, rate, buf, &self.stalled);
        }
        Ok(complete)
    }
}

/// Constant-power crossfade gain at `t` for a clip entering over `win_in` / leaving over `win_out`.
#[inline]
fn trans_gain(t: f64, win_in: Option<(f64, f64)>, win_out: Option<(f64, f64)>) -> f64 {
    let mut g = 1.0;
    if let Some((s, e)) = win_in {
        if t < e {
            let p = ((t - s) / (e - s)).clamp(0.0, 1.0);
            g *= (p * std::f64::consts::FRAC_PI_2).sin();
        }
    }
    if let Some((s, e)) = win_out {
        if t >= s {
            let p = ((t - s) / (e - s)).clamp(0.0, 1.0);
            g *= (p * std::f64::consts::FRAC_PI_2).cos();
        }
    }
    g
}

#[inline]
fn cubic(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    let a = -0.5 * p0 + 1.5 * p1 - 1.5 * p2 + 0.5 * p3;
    let b = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
    let c = -0.5 * p0 + 0.5 * p2;
    ((a * t + b) * t + c) * t + p1
}

#[allow(clippy::too_many_arguments)]
fn render_clip(
    clip: &Clip,
    span: Span,
    data: &AudioData,
    (a, b): (f64, f64),
    win_in: Option<(f64, f64)>,
    win_out: Option<(f64, f64)>,
    vol: f32,
    t0: f64,
    rate: f64,
    buf: &mut [f32],
    stalled: &AtomicBool,
) {
    let n = buf.len() / 2;
    let read = |start: i64, out: &mut [f32]| {
        let wait = if stalled.load(Ordering::Relaxed) { Duration::ZERO } else { READ_WAIT };
        if !data.read_frames_wait(start, out, wait) {
            stalled.store(true, Ordering::Relaxed);
        }
    };
    // 1e-6 of a sample of slack: float-accumulated `after` chains must not drop a clip's first sample.
    let i0 = (((a - t0) * rate - 1e-6).ceil().max(0.0) as usize).min(n);
    let i1 = (((b - t0) * rate - 1e-6).ceil().max(0.0) as usize).min(n);
    if i1 <= i0 {
        return;
    }
    let const_speed = match &clip.speed {
        Param::Value(v) => Some(*v),
        Param::Animated { keys } if keys.len() <= 1 => Some(sample(&clip.speed, 0.0)),
        _ => None,
    };
    // Speed 1: source sample index = timeline sample index + this.
    let src_offset = ((source_time(clip, 0.0).unwrap_or(0.0) - span.start) * rate).round() as i64;
    let static_gain = matches!(clip.volume, Param::Value(_)) && clip.fade_in <= 0.0 && clip.fade_out <= 0.0 && win_in.is_none() && win_out.is_none();
    let mut tmp: Vec<f32> = Vec::new();
    let mut j0 = i0;
    while j0 < i1 {
        let j1 = (j0 + SUB).min(i1);
        let m = j1 - j0;
        let tl0 = t0 + j0 as f64 / rate - span.start;
        let dt = m as f64 / rate;
        let (Some(s0), Some(s1)) = (source_time(clip, tl0), source_time(clip, tl0 + dt)) else { return };
        let ds = (s1 - s0) / dt; // source seconds per second
        if !ds.is_finite() || ds.abs() < 1e-4 {
            j0 = j1; // freeze frame: silence
            continue;
        }
        let gain_at = |j: usize| -> f32 {
            let tl = t0 + j as f64 / rate - span.start;
            (audio_gain(clip, tl) * trans_gain(tl + span.start, win_in, win_out)) as f32 * vol
        };
        let g_static = if static_gain { Some(gain_at(j0)) } else { None };
        let out = &mut buf[j0 * 2..j1 * 2];
        if const_speed == Some(1.0) && !clip.reverse {
            // Sample-aligned copy: source sample = timeline sample + a per-clip constant, so
            // consecutive sub-blocks and calls never skip or repeat a sample.
            let start = (t0 * rate).round() as i64 + j0 as i64 + src_offset;
            tmp.resize(m * 2, 0.0);
            read(start, &mut tmp);
            for k in 0..m {
                let g = g_static.unwrap_or_else(|| gain_at(j0 + k));
                out[2 * k] += tmp[2 * k] * g;
                out[2 * k + 1] += tmp[2 * k + 1] * g;
            }
        } else {
            let speed = ds.abs();
            let box_n = if speed >= 1.5 { speed.round() as i64 } else { 1 };
            let (lo, hi) = (s0.min(s1), s0.max(s1));
            let start = (lo * rate).floor() as i64 - 2 - box_n;
            let end = (hi * rate).ceil() as i64 + 3 + box_n;
            let len = (end - start).max(1) as usize;
            if len > 1 << 22 {
                // Absurd speeds (validation reports them): skip rather than allocate gigabytes.
                j0 = j1;
                continue;
            }
            tmp.resize(len * 2, 0.0);
            read(start, &mut tmp);
            let at = |idx: i64, ch: usize| -> f32 {
                let i = (idx - start).clamp(0, len as i64 - 1) as usize;
                tmp[i * 2 + ch]
            };
            for k in 0..m {
                let pos = (s0 + ds * k as f64 / rate) * rate;
                let g = g_static.unwrap_or_else(|| gain_at(j0 + k));
                let mut lr = [0f32; 2];
                for (ch, v) in lr.iter_mut().enumerate() {
                    if box_n > 1 {
                        // Cheap anti-alias for fast-forward: average the samples the step skips.
                        let c = pos.floor() as i64;
                        let mut acc = 0.0;
                        for q in 0..box_n {
                            acc += at(c - box_n / 2 + q, ch);
                        }
                        *v = acc / box_n as f32;
                    } else {
                        let ip = pos.floor();
                        let f = (pos - ip) as f32;
                        let ip = ip as i64;
                        *v = cubic(at(ip - 1, ch), at(ip, ch), at(ip + 1, ch), at(ip + 2, ch), f);
                    }
                }
                out[2 * k] += lr[0] * g;
                out[2 * k + 1] += lr[1] * g;
            }
        }
        j0 = j1;
    }
}

/// `x` for |x| ≤ 0.9, a tanh knee to ±1 above.
#[inline]
pub(crate) fn soft_clip(x: f32) -> f32 {
    let ax = x.abs();
    if ax <= 0.9 {
        x
    } else {
        x.signum() * (0.9 + 0.1 * ((ax - 0.9) / 0.1).tanh())
    }
}

fn track_signature(m: &Mixer, info: &TrackInfo) -> u64 {
    let mut h = KeyHasher::new("duck-trigger");
    h.json(info.track).u64(m.rate as u64).str(&m.doc_dir.to_string_lossy());
    let mut seen: Vec<(&str, u32)> = Vec::new();
    for (c, s) in &info.clips {
        h.json(*c).f64(s.start).f64(s.end);
        if let Source::Media { asset, stream, .. } = &c.source {
            if let Some(a) = m.tl.assets.get(asset.as_str()) {
                h.json(*a);
                if !seen.contains(&(asset.as_str(), *stream)) {
                    seen.push((asset.as_str(), *stream));
                    // The file itself: a replaced voice-over must not reuse the old levels.
                    let path = m.doc_dir.join(&a.path);
                    if let Ok(md) = std::fs::metadata(&path) {
                        let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0);
                        h.u64(md.len()).u64(mtime);
                    }
                    // And the decoded audio the pool serves (a new file version is a new `AudioData`).
                    if let Ok(data) = m.rd.pool().audio_stream(&path, m.rate, *stream) {
                        h.u64(Arc::as_ptr(&data) as usize as u64);
                    }
                }
            }
        }
    }
    for x in &info.trans {
        h.json(x.tr).f64(x.half);
    }
    h.finish()
}

/// Per-hop peak level (dBFS) of a trigger track for hops `[h0, h1)`.
fn trigger_levels(m: &Mixer, info: &TrackInfo, h0: i64, h1: i64) -> Result<Vec<f32>, RenderError> {
    let sig = track_signature(m, info);
    let chunks = {
        let mut c = lock(&m.rd.duck);
        match c.get(sig) {
            Some(x) => x,
            None => {
                let x = Arc::new(Mutex::new(HashMap::new()));
                c.insert(sig, x.clone(), 1);
                x
            }
        }
    };
    let hop_dt = HOP as f64 / m.rate as f64;
    let mut out = Vec::with_capacity((h1 - h0).max(0) as usize);
    let mut h = h0;
    while h < h1 {
        if h < 0 {
            out.push(f32::NEG_INFINITY);
            h += 1;
            continue;
        }
        let ci = h.div_euclid(CHUNK_HOPS);
        let chunk = { lock(&chunks).get(&ci).cloned() };
        let chunk = match chunk {
            Some(c) => c,
            None => {
                let mut buf = vec![0f32; CHUNK_HOPS as usize * HOP * 2];
                let complete = m.mix_track(info, ci as f64 * CHUNK_HOPS as f64 * hop_dt, &mut buf)?;
                let levels: Vec<f32> = buf
                    .chunks(HOP * 2)
                    .map(|c| {
                        let pk = c.iter().fold(0f32, |a, v| a.max(v.abs()));
                        if pk > 0.0 {
                            20.0 * pk.log10()
                        } else {
                            f32::NEG_INFINITY
                        }
                    })
                    .collect();
                let c = Arc::new(levels);
                // Levels of audio still being decoded are used once, never cached.
                if complete {
                    let mut g = lock(&chunks);
                    if g.len() > 4096 {
                        g.clear();
                    }
                    g.insert(ci, c.clone());
                }
                c
            }
        };
        let base = ci * CHUNK_HOPS;
        let end = ((ci + 1) * CHUNK_HOPS).min(h1);
        while h < end {
            out.push(chunk[(h - base) as usize]);
            h += 1;
        }
    }
    Ok(out)
}

/// Linear gain per sample of a ducking rule for `n` samples from `t0`.
fn duck_gains(m: &Mixer, d: &Ducking, trigger: &TrackInfo, t0: f64, n: usize) -> Result<Vec<f32>, RenderError> {
    let rate = m.rate as f64;
    let hop_dt = HOP as f64 / rate;
    let amount = if d.amount_db.is_finite() { d.amount_db.min(0.0) } else { -14.0 };
    let attack = if d.attack.is_finite() { d.attack.max(0.001) } else { 0.12 };
    let release = if d.release.is_finite() { d.release.max(0.001) } else { 0.45 };
    let thr = if d.threshold_db.is_finite() { d.threshold_db } else { -45.0 } as f32;
    let look_ahead = (attack / hop_dt).ceil() as i64;
    let hold = (DUCK_HOLD / hop_dt).ceil() as i64;
    let lookback = ((attack + release + DUCK_HOLD + 3.0) / hop_dt).ceil() as i64;
    let first = (t0 / hop_dt).floor() as i64;
    let last = ((t0 + n as f64 / rate) / hop_dt).ceil() as i64 + 1;
    let h_start = first - lookback;
    let lv = trigger_levels(m, trigger, h_start - hold, last + look_ahead + 1)?;
    let level = |h: i64| lv[(h - (h_start - hold)) as usize];
    // Active when the trigger is above threshold within [h - hold, h + look_ahead].
    let mut g = 0.0f64;
    let down = -amount / attack * hop_dt;
    let up = -amount / release * hop_dt;
    let mut at_hop: Vec<f64> = Vec::with_capacity((last - first + 2) as usize);
    for h in h_start..=last {
        let active = (h - hold..=h + look_ahead).any(|k| level(k) > thr);
        let target = if active { amount } else { 0.0 };
        if g > target {
            g = (g - down).max(target);
        } else {
            g = (g + up).min(target);
        }
        if h >= first {
            at_hop.push(g);
        }
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = t0 + i as f64 / rate;
        let x = t / hop_dt - first as f64;
        let k = (x.floor().max(0.0) as usize).min(at_hop.len().saturating_sub(2));
        let f = (x - k as f64).clamp(0.0, 1.0);
        let db = at_hop[k] + (at_hop.get(k + 1).copied().unwrap_or(at_hop[k]) - at_hop[k]) * f;
        out.push(10f64.powf(db / 20.0) as f32);
    }
    Ok(out)
}

/// Decodes the sound of every clip completely (all files in parallel) so an export mixes exact
/// audio. Missing or broken files are errors.
pub(crate) fn decode_all_sources(rd: &Renderer, doc_dir: &Path, p: &Project, r: &Resolved) -> Result<(), RenderError> {
    use rayon::prelude::*;
    let tl = Timeline::new(p, r);
    let rate = rate_of(p);
    let mut paths: Vec<(std::path::PathBuf, u32)> = Vec::new();
    for info in &tl.tracks {
        if !tl.plays_sound(info.track) {
            continue;
        }
        for (c, _) in &info.clips {
            let Source::Media { asset, stream, .. } = &c.source else { continue };
            if c.muted || !c.enabled {
                continue;
            }
            let Some(a) = tl.assets.get(asset.as_str()) else { continue };
            if a.kind == MediaKind::Image || (a.kind == MediaKind::Video && !a.info.has_audio) {
                continue;
            }
            let key = (doc_dir.join(&a.path), *stream);
            if !paths.contains(&key) {
                paths.push(key);
            }
        }
    }
    // Start every decode first (they run in the background), then wait for each.
    let offline = |path: &Path, e: aiframes_media::MediaError| {
        if path.exists() {
            RenderError::Media(e)
        } else {
            RenderError::Missing(format!("media file {} is offline", path.display()))
        }
    };
    let started: Vec<Result<Arc<AudioData>, RenderError>> =
        paths.par_iter().map(|(path, stream)| rd.pool().audio_stream(path, rate, *stream).map_err(|e| offline(path, e))).collect();
    for a in started {
        a?.wait_complete().map_err(RenderError::Media)?;
    }
    Ok(())
}

/// Mixes `[t0, t0 + out.len()/2 samples)` into `out` (stereo interleaved); returns warnings.
pub(crate) fn mix(rd: &Renderer, doc_dir: &Path, p: &Project, r: &Resolved, t0: f64, out: &mut [f32], o: &MixOpts) -> Result<Vec<String>, RenderError> {
    out.fill(0.0);
    let n = out.len() / 2;
    if n == 0 || !t0.is_finite() {
        return Ok(Vec::new());
    }
    let m = Mixer {
        rd,
        doc_dir,
        tl: Timeline::new(p, r),
        rate: rate_of(p),
        strict: o.strict,
        warnings: Mutex::new(Vec::new()),
        stalled: AtomicBool::new(false),
    };
    let mut buf = vec![0f32; n * 2];
    for info in &m.tl.tracks {
        if !m.tl.plays_sound(info.track) {
            continue;
        }
        if let Some(only) = o.only_track {
            if info.track.id != only {
                continue;
            }
        }
        if !info.clips.iter().any(|(c, _)| matches!(c.source, Source::Media { .. })) {
            continue;
        }
        buf.fill(0.0);
        m.mix_track(info, t0, &mut buf)?;
        if buf.iter().all(|v| *v == 0.0) {
            continue;
        }
        for d in p.ducking.iter().filter(|d| d.target == info.track.id && d.trigger != d.target) {
            let Some(trig) = m.tl.tracks.iter().find(|t| t.track.id == d.trigger) else {
                m.problem(RenderError::Other(format!("ducking {}: trigger track `{}` does not exist", d.id, d.trigger)))?;
                continue;
            };
            if !m.tl.plays_sound(trig.track) {
                continue;
            }
            let g = duck_gains(&m, d, trig, t0, n)?;
            for (i, gi) in g.iter().enumerate() {
                buf[2 * i] *= gi;
                buf[2 * i + 1] *= gi;
            }
        }
        for (o, v) in out.iter_mut().zip(&buf) {
            *o += v;
        }
    }
    if o.soft_clip {
        for v in out.iter_mut() {
            *v = soft_clip(*v);
        }
    }
    Ok(m.warnings.into_inner().unwrap_or_else(|p| p.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_clip_is_transparent_below_knee() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert_eq!(soft_clip(-0.9), -0.9);
        assert!(soft_clip(3.0) <= 1.0 && soft_clip(3.0) > 0.99);
        assert!(soft_clip(0.95) > 0.94 && soft_clip(0.95) < 0.95);
    }

    #[test]
    fn crossfade_is_constant_power() {
        let w = Some((1.0, 2.0));
        for i in 0..=10 {
            let t = 1.0 + i as f64 / 10.0;
            let a = trans_gain(t, None, w);
            let b = trans_gain(t, w, None);
            assert!((a * a + b * b - 1.0).abs() < 1e-9);
        }
        assert_eq!(trans_gain(0.5, None, w), 1.0);
    }
}
