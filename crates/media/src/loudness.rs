//! EBU R128 / ITU-R BS.1770 loudness: integrated LUFS, true peak, loudness range, max momentary.

use crate::audio::stream_pcm;
use crate::error::Result;
use crate::probe::probe_full;
use ebur128::{EbuR128, Mode};
use std::path::Path;

/// Loudness of a programme. Silence reports `-70` LUFS (the absolute gate) and `-144` dBTP.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Loudness {
    pub integrated_lufs: f64,
    /// Maximum true (inter-sample) peak over both channels, dBTP.
    pub true_peak_db: f64,
    /// Loudness range, LU.
    pub lra: f64,
    /// Highest momentary (400 ms) loudness, LUFS.
    pub max_momentary_lufs: f64,
}

pub(crate) const LUFS_FLOOR: f64 = -70.0;
pub(crate) const PEAK_FLOOR: f64 = -144.0;

impl Loudness {
    pub fn silent() -> Loudness {
        Loudness { integrated_lufs: LUFS_FLOOR, true_peak_db: PEAK_FLOOR, lra: 0.0, max_momentary_lufs: LUFS_FLOOR }
    }

    /// Linear gain that brings this programme to `target_lufs`, limited so the true peak stays at or
    /// below `ceiling_db` (`None` when the programme is silent).
    pub fn gain_to(&self, target_lufs: f64, ceiling_db: f64) -> Option<f64> {
        if self.integrated_lufs <= LUFS_FLOOR {
            return None;
        }
        let gain_db = (target_lufs - self.integrated_lufs).min(ceiling_db - self.true_peak_db);
        Some(10f64.powf(gain_db / 20.0))
    }
}

/// Streaming loudness meter for stereo interleaved f32 (feed it a mix block by block).
pub struct LoudnessMeter {
    meter: Option<EbuR128>,
    rate: u32,
    /// Frames pushed since the last momentary reading (readings every 100 ms).
    pending: usize,
    max_momentary: f64,
    frames: u64,
}

impl LoudnessMeter {
    pub fn new(rate: u32) -> LoudnessMeter {
        let mode = Mode::I | Mode::LRA | Mode::TRUE_PEAK | Mode::M | Mode::S;
        let meter = if rate >= 8000 { EbuR128::new(2, rate, mode).ok() } else { None };
        LoudnessMeter { meter, rate, pending: 0, max_momentary: f64::NEG_INFINITY, frames: 0 }
    }

    /// Adds stereo interleaved samples.
    pub fn push(&mut self, stereo: &[f32]) {
        let Some(m) = self.meter.as_mut() else { return };
        let step = (self.rate / 10) as usize; // 100 ms
        let mut rest = &stereo[..stereo.len() / 2 * 2];
        while !rest.is_empty() {
            let take = (step - self.pending).min(rest.len() / 2);
            let (a, b) = rest.split_at(take * 2);
            if m.add_frames_f32(a).is_err() {
                return;
            }
            self.pending += take;
            self.frames += take as u64;
            rest = b;
            if self.pending == step {
                self.pending = 0;
                // The first readings cover less than 400 ms of audio; skip them like ffmpeg does.
                if self.frames as f64 >= 0.4 * self.rate as f64 {
                    if let Ok(v) = m.loudness_momentary() {
                        if v.is_finite() && v > self.max_momentary {
                            self.max_momentary = v;
                        }
                    }
                }
            }
        }
    }

    /// Momentary loudness (last 400 ms), LUFS.
    pub fn momentary(&self) -> f64 {
        self.meter.as_ref().and_then(|m| m.loudness_momentary().ok()).map(floor_lufs).unwrap_or(LUFS_FLOOR)
    }

    /// Short-term loudness (last 3 s), LUFS.
    pub fn short_term(&self) -> f64 {
        self.meter.as_ref().and_then(|m| m.loudness_shortterm().ok()).map(floor_lufs).unwrap_or(LUFS_FLOOR)
    }

    /// Measurement of everything pushed so far.
    pub fn result(&self) -> Loudness {
        let Some(m) = self.meter.as_ref() else { return Loudness::silent() };
        let integrated = m.loudness_global().map(floor_lufs).unwrap_or(LUFS_FLOOR);
        let lra = m.loudness_range().ok().filter(|v| v.is_finite()).unwrap_or(0.0).max(0.0);
        let tp = (0..2).filter_map(|c| m.true_peak(c).ok()).fold(0.0f64, f64::max);
        let true_peak_db = if tp > 0.0 { (20.0 * tp.log10()).max(PEAK_FLOOR) } else { PEAK_FLOOR };
        let max_momentary_lufs = if self.max_momentary.is_finite() { floor_lufs(self.max_momentary) } else { self.momentary() };
        Loudness { integrated_lufs: integrated, true_peak_db, lra, max_momentary_lufs }
    }
}

fn floor_lufs(v: f64) -> f64 {
    if v.is_finite() {
        v.max(LUFS_FLOOR)
    } else {
        LUFS_FLOOR
    }
}

/// Loudness of stereo interleaved f32 at `rate`.
pub fn loudness_of_pcm(stereo: &[f32], rate: u32) -> Loudness {
    let mut m = LoudnessMeter::new(rate);
    m.push(stereo);
    m.result()
}

/// Loudness of a file's audio as AIFrames plays it (stereo; mono as dual-mono at unity gain, so a
/// mono file measures 3 LU louder than a mono-aware meter reports). Measured at the file's own
/// sample rate. Files without audio report silence.
pub fn loudness_of_file(path: &Path) -> Result<Loudness> {
    loudness_of_file_stream(path, 0)
}

/// [`loudness_of_file`] of sound stream `stream` of a file with several (0 = the first).
pub fn loudness_of_file_stream(path: &Path, stream: u32) -> Result<Loudness> {
    let p = probe_full(path)?;
    if !p.info.has_audio {
        if stream > 0 {
            return Err(p.no_stream(path, stream));
        }
        return Ok(Loudness::silent());
    }
    let own = p.audio(stream).map(|a| a.rate).ok_or_else(|| p.no_stream(path, stream))?;
    let rate = if (8000..=384_000).contains(&own) { own } else { 48_000 };
    let mut m = LoudnessMeter::new(rate);
    stream_pcm(path, &p, stream, rate, 2, |chunk| {
        m.push(chunk);
        true
    })?;
    Ok(m.result())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amp: f32, freq: f32, secs: f32, rate: u32) -> Vec<f32> {
        let n = (secs * rate as f32) as usize;
        let mut v = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = amp * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin();
            v.push(s);
            v.push(s);
        }
        v
    }

    #[test]
    fn sine_minus_20() {
        // 1 kHz stereo sine with -20 dBFS peaks: about -20 LUFS (K-weighting is ~+0.7 dB at 1 kHz,
        // offset by the -0.691 in BS.1770), true peak -20 dBTP, no loudness range.
        let l = loudness_of_pcm(&sine(0.1, 1000.0, 10.0, 48_000), 48_000);
        assert!((l.integrated_lufs + 20.0).abs() < 0.3, "{l:?}");
        assert!((l.true_peak_db + 20.0).abs() < 0.2, "{l:?}");
        assert!(l.lra < 0.5, "{l:?}");
        assert!((l.max_momentary_lufs - l.integrated_lufs).abs() < 0.3, "{l:?}");
        let g = l.gain_to(-14.0, -1.0).unwrap();
        assert!((20.0 * g.log10() - 6.0).abs() < 0.3);
    }

    #[test]
    fn silence_is_floored() {
        let l = loudness_of_pcm(&vec![0.0; 48_000 * 4], 48_000);
        assert_eq!(l, Loudness::silent());
        assert!(l.gain_to(-14.0, -1.0).is_none());
    }
}
