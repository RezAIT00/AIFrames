//! Waveform peaks for drawing: per bin (200 per second) the min and max sample over both
//! channels and the RMS. Cached on disk next to the audio cache.

use crate::audio::AudioData;
use crate::error::{MediaError, Result};
use std::io::Write;
use std::path::Path;

/// Bins per second of `Waveform`.
pub const WAVEFORM_BINS_PER_SEC: f64 = 200.0;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Waveform {
    pub bins_per_sec: f64,
    /// Seconds covered (the audio's duration).
    pub duration: f64,
    /// Per bin: lowest sample (-1..0), highest sample (0..1), RMS (0..1), over both channels.
    pub min: Vec<f32>,
    pub max: Vec<f32>,
    pub rms: Vec<f32>,
}

impl Waveform {
    pub fn len(&self) -> usize {
        self.max.len()
    }

    pub fn is_empty(&self) -> bool {
        self.max.is_empty()
    }

    fn bin_range(&self, t0: f64, t1: f64) -> Option<(usize, usize)> {
        if self.is_empty() || !t0.is_finite() || !t1.is_finite() {
            return None;
        }
        let (t0, t1) = if t1 < t0 { (t1, t0) } else { (t0, t1) };
        let a = (t0 * self.bins_per_sec).floor().max(0.0) as usize;
        let b = ((t1 * self.bins_per_sec).ceil() as usize).max(a + 1).min(self.len());
        (a < b).then_some((a, b))
    }

    /// (min, max) over source seconds `[t0, t1)` — what to draw for one pixel column at any zoom.
    /// `(0, 0)` outside the audio.
    pub fn range(&self, t0: f64, t1: f64) -> (f32, f32) {
        match self.bin_range(t0, t1) {
            Some((a, b)) => {
                let mn = self.min[a..b].iter().copied().fold(0.0f32, f32::min);
                let mx = self.max[a..b].iter().copied().fold(0.0f32, f32::max);
                (mn, mx)
            }
            None => (0.0, 0.0),
        }
    }

    /// RMS over source seconds `[t0, t1)`.
    pub fn rms_range(&self, t0: f64, t1: f64) -> f32 {
        match self.bin_range(t0, t1) {
            Some((a, b)) => {
                let s: f64 = self.rms[a..b].iter().map(|r| (*r as f64) * (*r as f64)).sum();
                (s / (b - a) as f64).sqrt() as f32
            }
            None => 0.0,
        }
    }

    /// Peaks of decoded audio.
    pub fn from_audio(a: &AudioData) -> Waveform {
        let rate = a.rate() as f64;
        let per_bin = (rate / WAVEFORM_BINS_PER_SEC).round().max(1.0) as u64;
        let bins_per_sec = rate / per_bin as f64;
        let n_bins = a.frames().div_ceil(per_bin) as usize;
        let mut w = Waveform {
            bins_per_sec,
            duration: a.duration(),
            min: Vec::with_capacity(n_bins),
            max: Vec::with_capacity(n_bins),
            rms: Vec::with_capacity(n_bins),
        };
        let (mut mn, mut mx, mut sq, mut count) = (0f32, 0f32, 0f64, 0u64);
        a.for_each_chunk(|chunk| {
            for fr in chunk.as_chunks::<2>().0 {
                let (l, r) = (fr[0], fr[1]);
                mn = mn.min(l).min(r);
                mx = mx.max(l).max(r);
                sq += (l as f64 * l as f64 + r as f64 * r as f64) * 0.5;
                count += 1;
                if count == per_bin {
                    w.min.push(mn);
                    w.max.push(mx);
                    w.rms.push((sq / count as f64).sqrt() as f32);
                    (mn, mx, sq, count) = (0.0, 0.0, 0.0, 0);
                }
            }
        });
        if count > 0 {
            w.min.push(mn);
            w.max.push(mx);
            w.rms.push((sq / count as f64).sqrt() as f32);
        }
        w
    }

    pub(crate) fn empty(duration: f64) -> Waveform {
        Waveform { bins_per_sec: WAVEFORM_BINS_PER_SEC, duration: duration.max(0.0), ..Default::default() }
    }

    const MAGIC: &'static [u8; 8] = b"AIFWAVE1";

    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        let mut b = Vec::with_capacity(32 + self.len() * 12);
        b.extend_from_slice(Self::MAGIC);
        b.extend_from_slice(&self.bins_per_sec.to_le_bytes());
        b.extend_from_slice(&self.duration.to_le_bytes());
        b.extend_from_slice(&(self.len() as u64).to_le_bytes());
        for v in [&self.min, &self.max, &self.rms] {
            for x in v.iter() {
                b.extend_from_slice(&x.to_le_bytes());
            }
        }
        let tmp = crate::util::temp_sibling(path);
        let mut f = std::fs::File::create(&tmp).map_err(|e| MediaError::io(&tmp, "create the waveform cache", &e))?;
        f.write_all(&b).map_err(|e| MediaError::io(&tmp, "write the waveform cache", &e))?;
        drop(f);
        crate::util::commit_temp(&tmp, path)
    }

    pub(crate) fn load(path: &Path) -> Option<Waveform> {
        let b = std::fs::read(path).ok()?;
        if b.len() < 32 || &b[..8] != Self::MAGIC {
            return None;
        }
        let f64_at = |i: usize| f64::from_le_bytes(b[i..i + 8].try_into().unwrap_or([0; 8]));
        let bins_per_sec = f64_at(8);
        let duration = f64_at(16);
        let n = u64::from_le_bytes(b[24..32].try_into().ok()?) as usize;
        if b.len() != 32 + n * 12 || bins_per_sec.is_nan() || bins_per_sec <= 0.0 {
            return None;
        }
        let read = |k: usize| -> Vec<f32> {
            let off = 32 + k * n * 4;
            b[off..off + n * 4].as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
        };
        Some(Waveform { bins_per_sec, duration, min: read(0), max: read(1), rms: read(2) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_and_ranges() {
        let rate = 48_000u32;
        // 1 s silence, then 0.5 s of a 0.5-amplitude square wave
        let mut s = vec![0f32; rate as usize * 2];
        for i in 0..(rate / 2) as usize {
            let v = if (i / 24) % 2 == 0 { 0.5 } else { -0.5 };
            s.push(v);
            s.push(v);
        }
        let a = AudioData::from_samples("x", rate, s);
        let w = Waveform::from_audio(&a);
        assert_eq!(w.bins_per_sec, 200.0);
        assert_eq!(w.len(), 300);
        assert_eq!(w.range(0.0, 0.99), (0.0, 0.0));
        assert_eq!(w.range(1.0, 1.5), (-0.5, 0.5));
        assert!((w.rms_range(1.1, 1.4) - 0.5).abs() < 1e-4);
        assert_eq!(w.range(5.0, 6.0), (0.0, 0.0));

        let dir = crate::util::TempDir::new("aiframes-wf-unit").unwrap();
        let p = dir.0.join("w.wf");
        w.save(&p).unwrap();
        assert_eq!(Waveform::load(&p).unwrap(), w);
    }
}
