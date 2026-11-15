//! Export: the mix (rendered in parallel chunks) is measured, gained to the loudness target and
//! true-peak limited into a WAV; frames render on several threads (with an in-order media
//! read-ahead thread so decoders stream) and are pushed to the encoder in order.

use crate::audio::{self, MixOpts};
use crate::compose::{FrameJob, OutputView};
use crate::timeline::Timeline;
use crate::util::{frame_index, frame_time, lock};
use crate::{ExportProgress, ExportReport, RenderError, Renderer};
use aiframes_core::{resolve, ExportSettings, Project, Resolved};
use aiframes_media::{EncodeOptions, Encoder, Loudness, LoudnessMeter, MediaPool, Quality, VideoCodec, WavFormat, WavWriter};
use rayon::prelude::*;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// Mix chunk length (seconds) for parallel rendering.
const CHUNK_SECS: f64 = 1.0;
/// Mixes up to this long are kept in memory between the measure and apply passes.
const KEEP_SECS: f64 = 15.0 * 60.0;

// ---------------------------------------------------------------------------------------------
// True-peak limiter
// ---------------------------------------------------------------------------------------------

/// Taps per phase of the 4× true-peak interpolator.
const TP_TAPS: usize = 12;

/// Polyphase coefficients for the inter-sample points at 1/4, 2/4 and 3/4 between two samples:
/// a Kaiser-windowed sinc over 12 input samples per phase (48-tap 4× upsampler, the structure of
/// ITU-R BS.1770 Annex 2), each phase normalised to unity gain at DC.
fn tp_phases() -> &'static [[f32; TP_TAPS]; 3] {
    static P: std::sync::OnceLock<[[f32; TP_TAPS]; 3]> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        fn bessel_i0(x: f64) -> f64 {
            let (mut sum, mut term) = (1.0, 1.0);
            for k in 1..40 {
                term *= (x / (2.0 * k as f64)).powi(2);
                sum += term;
            }
            sum
        }
        let beta = 7.0;
        let half = TP_TAPS as f64 / 2.0 + 0.5;
        let mut out = [[0f32; TP_TAPS]; 3];
        for (k, phase) in out.iter_mut().enumerate() {
            let f = (k + 1) as f64 / 4.0;
            let mut c = [0f64; TP_TAPS];
            for (j, cj) in c.iter_mut().enumerate() {
                // Input sample i + (j - 5) contributes to the point at i + f.
                let x = (j as f64 - 5.0) - f;
                let sinc = if x.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
                let r = x / half;
                let w = if r.abs() >= 1.0 { 0.0 } else { bessel_i0(beta * (1.0 - r * r).sqrt()) / bessel_i0(beta) };
                *cj = sinc * w;
            }
            let sum: f64 = c.iter().sum();
            for (o, v) in phase.iter_mut().zip(c) {
                *o = (v / sum) as f32;
            }
        }
        out
    })
}

/// Peak (absolute) of the band-limited signal over the interval `[i, i+1)`, from the 12 samples
/// `x[i-5..=i+6]` of one channel.
#[inline]
fn interval_peak(x: &[f32; TP_TAPS]) -> f32 {
    let mut pk = x[5].abs();
    for c in tp_phases() {
        let mut y = 0f32;
        for j in 0..TP_TAPS {
            y += c[j] * x[j];
        }
        pk = pk.max(y.abs());
    }
    pk
}

/// Streaming look-ahead limiter with inter-sample (true) peak detection by 4× polyphase FIR
/// oversampling. Output lags input by `look + 6` samples; `flush` drains it.
pub(crate) struct Limiter {
    ceil: f32,
    look: usize,
    rel: f32,
    /// The last 12 input frames.
    raw: VecDeque<[f32; 2]>,
    n_raw: u64,
    /// Peak of the previous interval (a sample's peak covers the intervals on both sides).
    ip_prev: f32,
    mins: VecDeque<(u64, f32)>,
    hbox: VecDeque<f32>,
    hsum: f64,
    delay: VecDeque<[f32; 2]>,
    env: f32,
    n_in: u64,
    n_out: u64,
}

impl Limiter {
    pub fn new(ceiling_db: f64, rate: u32) -> Limiter {
        let look = ((rate as f64) * 0.005).round().max(1.0) as usize;
        let rel = 1.0 - (-1.0 / (0.1 * rate as f64)).exp() as f32;
        let mut raw = VecDeque::with_capacity(TP_TAPS + 1);
        raw.extend([[0.0; 2]; TP_TAPS - 1]);
        Limiter {
            ceil: 10f64.powf(ceiling_db / 20.0) as f32,
            look,
            rel,
            raw,
            n_raw: 0,
            ip_prev: 0.0,
            mins: VecDeque::new(),
            hbox: VecDeque::with_capacity(look + 1),
            hsum: 0.0,
            delay: VecDeque::with_capacity(look + 8),
            env: 1.0,
            n_in: 0,
            n_out: 0,
        }
    }

    fn feed(&mut self, x: [f32; 2], out: &mut Vec<f32>) {
        self.raw.push_back(x);
        self.n_raw += 1;
        if self.raw.len() > TP_TAPS {
            self.raw.pop_front();
        }
        // raw = x[m-11..=m] with m = n_raw - 1; it describes the interval starting at i = m - 6.
        let mut ip = 0f32;
        for ch in 0..2 {
            let mut w = [0f32; TP_TAPS];
            for (k, v) in w.iter_mut().enumerate() {
                *v = self.raw[k][ch];
            }
            ip = ip.max(interval_peak(&w));
        }
        let pk = ip.max(self.ip_prev);
        self.ip_prev = ip;
        if self.n_raw < 7 {
            return; // intervals before the first sample
        }
        let j = self.n_raw - 7;
        let xj = self.raw[5];
        let req = if pk > self.ceil { self.ceil / pk } else { 1.0 };
        self.delay.push_back(xj);
        while self.mins.back().is_some_and(|&(_, v)| v >= req) {
            self.mins.pop_back();
        }
        self.mins.push_back((j, req));
        let look = self.look as u64;
        while self.mins.front().is_some_and(|&(i, _)| i + look < j) {
            self.mins.pop_front();
        }
        let h = self.mins.front().map(|m| m.1).unwrap_or(1.0);
        self.hbox.push_back(h);
        self.hsum += h as f64;
        if self.hbox.len() > self.look {
            if let Some(old) = self.hbox.pop_front() {
                self.hsum -= old as f64;
            }
        }
        if j < look {
            return;
        }
        // Output sample k = j - look.
        let s = (self.hsum / self.hbox.len() as f64) as f32;
        self.env = s.min(self.env + (1.0 - self.env) * self.rel);
        if let Some(xk) = self.delay.pop_front() {
            if self.n_out < self.n_in {
                out.push(xk[0] * self.env);
                out.push(xk[1] * self.env);
                self.n_out += 1;
            }
        }
    }

    /// Processes interleaved stereo; appends limited output (delayed) to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        for f in input.as_chunks::<2>().0 {
            self.n_in += 1;
            self.feed(*f, out);
        }
    }

    pub fn flush(&mut self, out: &mut Vec<f32>) {
        let mut guard = 0;
        while self.n_out < self.n_in && guard < self.look + 32 {
            self.feed([0.0; 2], out);
            guard += 1;
        }
    }
}

/// True peak (dBTP) of interleaved stereo with the limiter's 4× polyphase estimator: every sample
/// plus the inter-sample points of intervals that lie fully inside the buffer.
#[allow(dead_code)]
pub(crate) fn true_peak_db(stereo: &[f32]) -> f64 {
    let frames = stereo.len() / 2;
    let mut pk = stereo.iter().fold(0f32, |a, v| a.max(v.abs()));
    for ch in 0..2 {
        for i in 5..frames.saturating_sub(6) {
            let mut w = [0f32; TP_TAPS];
            for (k, v) in w.iter_mut().enumerate() {
                *v = stereo[(i - 5 + k) * 2 + ch];
            }
            pk = pk.max(interval_peak(&w));
        }
    }
    if pk > 0.0 {
        20.0 * (pk as f64).log10()
    } else {
        -144.0
    }
}

// ---------------------------------------------------------------------------------------------
// Audio rendering
// ---------------------------------------------------------------------------------------------

/// Mixes `[t0, t1)` in parallel chunks, raw (no soft clip), in order through `sink`.
fn mix_chunks(
    rd: &Renderer,
    doc_dir: &Path,
    p: &Project,
    r: &Resolved,
    t0: f64,
    t1: f64,
    strict: bool,
    cancel: &AtomicBool,
    mut sink: impl FnMut(usize, Vec<f32>) -> Result<(), RenderError>,
) -> Result<(), RenderError> {
    let rate = audio::rate_of(p) as f64;
    let total = ((t1 - t0) * rate).round().max(0.0) as usize;
    let per = (CHUNK_SECS * rate) as usize;
    let chunks = total.div_ceil(per.max(1));
    let batch = rayon::current_num_threads().max(2) * 2;
    let mut i = 0;
    while i < chunks {
        if cancel.load(Ordering::Relaxed) {
            return Err(RenderError::Cancelled);
        }
        let hi = (i + batch).min(chunks);
        let out: Vec<Result<Vec<f32>, RenderError>> = (i..hi)
            .into_par_iter()
            .map(|c| {
                let start = c * per;
                let n = per.min(total - start);
                let mut buf = vec![0f32; n * 2];
                let ct0 = t0 + start as f64 / rate;
                audio::mix(rd, doc_dir, p, r, ct0, &mut buf, &MixOpts { soft_clip: false, strict, only_track: None })?;
                Ok(buf)
            })
            .collect();
        for (k, b) in out.into_iter().enumerate() {
            sink(i + k, b?)?;
        }
        i = hi;
    }
    Ok(())
}

/// A whole-range mix (stereo interleaved at the project rate, no soft clip).
pub(crate) fn mixdown(rd: &Renderer, doc_dir: &Path, p: &Project, r: &Resolved, t0: f64, t1: f64) -> Result<Vec<f32>, RenderError> {
    let mut all = Vec::new();
    let cancel = AtomicBool::new(false);
    mix_chunks(rd, doc_dir, p, r, t0, t1.max(t0), false, &cancel, |_, b| {
        all.extend_from_slice(&b);
        Ok(())
    })?;
    Ok(all)
}

struct AudioOut {
    loudness: Loudness,
    /// Set when the loudness target could not be reached.
    warning: Option<String>,
}

/// The raw mix of the export range and its loudness.
struct Mixed {
    /// The mix in chunks, when short enough to keep (else it is re-mixed for each pass).
    kept: Option<Vec<Vec<f32>>>,
    measured: Loudness,
    rate: u32,
}

/// Decodes every source completely, mixes the range in parallel chunks and measures it.
#[allow(clippy::too_many_arguments)]
fn mix_and_measure(
    rd: &Renderer,
    doc_dir: &Path,
    p: &Project,
    r: &Resolved,
    t0: f64,
    t1: f64,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(&'static str, u64, u64) -> bool,
) -> Result<Mixed, RenderError> {
    let rate = audio::rate_of(p);
    // Exact audio: every source fully decoded before mixing.
    if !report("audio", 0, 1) {
        cancel.store(true, Ordering::Relaxed);
        return Err(RenderError::Cancelled);
    }
    audio::decode_all_sources(rd, doc_dir, p, r)?;
    let total_chunks = (((t1 - t0) * rate as f64).round() as usize).div_ceil((CHUNK_SECS * rate as f64) as usize).max(1) as u64;
    let keep = t1 - t0 <= KEEP_SECS;
    let mut kept: Vec<Vec<f32>> = Vec::new();
    let mut pre = LoudnessMeter::new(rate);
    mix_chunks(rd, doc_dir, p, r, t0, t1, true, cancel, |i, b| {
        pre.push(&b);
        if keep {
            kept.push(b);
        }
        if !report("audio", i as u64 + 1, total_chunks) {
            cancel.store(true, Ordering::Relaxed);
            return Err(RenderError::Cancelled);
        }
        Ok(())
    })?;
    Ok(Mixed { kept: keep.then_some(kept), measured: pre.result(), rate })
}

/// Gains and true-peak limits the mix into `wav` so the PCM measures `target` LUFS with its true
/// peak at or below `ceiling` dBTP (the limiter runs a little below it).
#[allow(clippy::too_many_arguments)]
fn normalize_to_wav(
    rd: &Renderer,
    doc_dir: &Path,
    p: &Project,
    r: &Resolved,
    (t0, t1): (f64, f64),
    mix: &Mixed,
    target: Option<f64>,
    ceiling: f64,
    wav: &Path,
    format: WavFormat,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(&'static str, u64, u64) -> bool,
) -> Result<AudioOut, RenderError> {
    let rate = mix.rate;
    let measured = mix.measured;
    let mut gain_db = match target {
        Some(tg) if measured.integrated_lufs > -70.0 => tg - measured.integrated_lufs,
        _ => 0.0,
    };
    let mut lim_ceiling = ceiling - 0.2;
    let mut result = Loudness::silent();
    // (gain dB, integrated LUFS) of the passes at the current limiter ceiling: with a busy limiter
    // loudness grows slower than gain, so the next gain follows the measured slope (secant).
    let mut tried: Vec<(f64, f64)> = Vec::new();
    let mut warning = None;
    for attempt in 0..LOUDNESS_PASSES {
        if !report("loudness", attempt + 1, LOUDNESS_PASSES) {
            cancel.store(true, Ordering::Relaxed);
            return Err(RenderError::Cancelled);
        }
        let g = 10f64.powf(gain_db / 20.0) as f32;
        let mut w = WavWriter::create(wav, rate, 2, format)?;
        let mut lim = Limiter::new(lim_ceiling, rate);
        let mut post = LoudnessMeter::new(rate);
        let mut out = Vec::new();
        let mut write = |buf: &[f32], flush: bool, lim: &mut Limiter, w: &mut WavWriter, post: &mut LoudnessMeter| -> Result<(), RenderError> {
            out.clear();
            let scaled: Vec<f32> = buf.iter().map(|v| v * g).collect();
            lim.process(&scaled, &mut out);
            if flush {
                lim.flush(&mut out);
            }
            post.push(&out);
            w.write(&out)?;
            Ok(())
        };
        match &mix.kept {
            Some(kept) => {
                let n = kept.len();
                for (i, b) in kept.iter().enumerate() {
                    write(b, i + 1 == n, &mut lim, &mut w, &mut post)?;
                }
                if n == 0 {
                    write(&[], true, &mut lim, &mut w, &mut post)?;
                }
            }
            None => {
                let mut pending: Option<Vec<f32>> = None;
                mix_chunks(rd, doc_dir, p, r, t0, t1, true, cancel, |_, b| {
                    if let Some(prev) = pending.replace(b) {
                        write(&prev, false, &mut lim, &mut w, &mut post)?;
                    }
                    Ok(())
                })?;
                write(&pending.take().unwrap_or_default(), true, &mut lim, &mut w, &mut post)?;
            }
        }
        w.finish()?;
        result = post.result();
        if result.integrated_lufs <= -70.0 {
            break;
        }
        let miss = target.map(|tg| tg - result.integrated_lufs).unwrap_or(0.0);
        let over = result.true_peak_db - ceiling;
        if miss.abs() <= 0.1 && over <= 0.0 {
            break;
        }
        if attempt + 1 == LOUDNESS_PASSES {
            if miss.abs() > 0.2 || over > 0.1 {
                warning = Some(format!(
                    "loudness normalisation ended at {:.2} LUFS / {:.2} dBTP (target {} LUFS, ceiling {ceiling:.1} dBTP): the programme cannot reach it without more limiting",
                    result.integrated_lufs,
                    result.true_peak_db,
                    target.map(|t| format!("{t:.1}")).unwrap_or_else(|| "none".into())
                ));
            }
            break;
        }
        if over > 0.0 {
            // The ceiling moves: earlier passes no longer describe the curve.
            lim_ceiling -= over + 0.05;
            tried.clear();
        }
        if target.is_some() {
            tried.push((gain_db, result.integrated_lufs));
            let slope = match tried.len() {
                n if n >= 2 => {
                    let (g0, l0) = tried[n - 2];
                    let (g1, l1) = tried[n - 1];
                    if (g1 - g0).abs() > 1e-6 {
                        ((l1 - l0) / (g1 - g0)).clamp(0.05, 1.5)
                    } else {
                        1.0
                    }
                }
                _ => 1.0,
            };
            gain_db = (gain_db + (miss / slope).clamp(-24.0, 24.0)).min(80.0);
        }
    }
    Ok(AudioOut { loudness: result, warning })
}

/// Normalisation passes (each limits the whole mix once).
const LOUDNESS_PASSES: u64 = 10;

/// Memory for frames in flight (canvases of the workers, finished frames waiting for the encoder).
/// Export runs inside the GUI process, so it must stay modest.
const EXPORT_BUFFER_BUDGET: usize = 600 << 20;
/// Full-size f32 canvases a worker holds at once on a typical frame (the composite; clip and effect
/// layers are sized to their content, transitions briefly need more).
const WORKER_CANVASES: usize = 1;
/// Cache of rendered titles/shapes for an export (static titles repeat; animated ones do not).
const EXPORT_VECTOR_CACHE: usize = 64 << 20;

/// Extra encode passes after measuring the delivered file (lossy audio codecs overshoot).
const DELIVERY_PASSES: usize = 2;
/// The delivered true peak must be this far under the ceiling (meters and decoders differ slightly).
const DELIVERY_TP_GUARD: f64 = 0.05;

fn tool_command(program: &Path) -> std::process::Command {
    let mut c = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c.stdin(std::process::Stdio::null());
    c
}

/// Loudness of a file's first audio stream exactly as ffmpeg's `ebur128` filter measures it
/// (integrated, true peak with its 4× oversampling, LRA, max momentary).
pub(crate) fn delivered_loudness(path: &Path) -> Result<Loudness, RenderError> {
    let ffmpeg = &aiframes_media::tools()?.ffmpeg;
    let out = tool_command(ffmpeg)
        .args(["-hide_banner", "-nostats", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-af", "ebur128=peak=true:metadata=1,ametadata=mode=print:file=-", "-f", "null", "-"])
        .output()
        .map_err(|e| RenderError::Other(format!("could not run ffmpeg to measure {}: {e}", path.display())))?;
    if !out.status.success() {
        return Err(RenderError::Other(format!(
            "ffmpeg could not measure the loudness of {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (mut i, mut tp, mut lra, mut mm) = (None, None, 0.0, f64::NEG_INFINITY);
    for line in text.lines() {
        let Some((k, v)) = line.trim().split_once('=') else { continue };
        let Ok(v) = v.trim().parse::<f64>() else { continue };
        match k {
            "lavfi.r128.I" => i = Some(v),
            "lavfi.r128.true_peak" => tp = Some(v),
            "lavfi.r128.LRA" => lra = v,
            "lavfi.r128.M" if v.is_finite() => mm = mm.max(v),
            _ => {}
        }
    }
    let (Some(i), Some(tp)) = (i, tp) else {
        return Err(RenderError::Other(format!("ffmpeg reported no loudness for {}", path.display())));
    };
    let floor = |v: f64| if v.is_finite() { v.max(-70.0) } else { -70.0 };
    Ok(Loudness {
        integrated_lufs: floor(i),
        true_peak_db: if tp > 0.0 { (20.0 * tp.log10()).max(-144.0) } else { -144.0 },
        lra: if lra.is_finite() { lra.max(0.0) } else { 0.0 },
        max_momentary_lufs: floor(mm),
    })
}

/// Saves the audio of a delivered file to a temp file (a container's audio stream copied as is, or
/// a whole WAV).
fn save_audio(file: &Path, container: bool) -> Result<TempFile, RenderError> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let ext = if container { file.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "mp4".into()) } else { "wav".into() };
    let tmp = TempFile(std::env::temp_dir().join(format!("aiframes-export-{}-best{}.{ext}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))));
    if !container {
        std::fs::copy(file, &tmp.0).map_err(|e| RenderError::Encode(format!("could not keep a copy of {}: {e}", file.display())))?;
        return Ok(tmp);
    }
    let ffmpeg = &aiframes_media::tools()?.ffmpeg;
    let res = tool_command(ffmpeg)
        .args(["-hide_banner", "-nostats", "-loglevel", "error", "-y", "-i"])
        .arg(file)
        .args(["-vn", "-map", "0:a:0", "-c:a", "copy"])
        .arg(&tmp.0)
        .output()
        .map_err(|e| RenderError::Encode(format!("could not run ffmpeg to keep the audio: {e}")))?;
    if !res.status.success() {
        return Err(RenderError::Encode(format!("ffmpeg could not copy the audio of {}: {}", file.display(), String::from_utf8_lossy(&res.stderr).trim())));
    }
    Ok(tmp)
}

/// Puts audio saved by `save_audio` back into the delivered file (streams copied, nothing re-encoded).
fn restore_audio(file: &Path, saved: &Path, container: bool) -> Result<(), RenderError> {
    if !container {
        std::fs::copy(saved, file).map_err(|e| RenderError::Encode(format!("could not restore {}: {e}", file.display())))?;
        return Ok(());
    }
    let ffmpeg = &aiframes_media::tools()?.ffmpeg;
    let ext = file.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "mp4".into());
    let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "export".into());
    let tmp = file.with_file_name(format!("{stem}.audio-restore.part.{ext}"));
    let mut c = tool_command(ffmpeg);
    c.args(["-hide_banner", "-nostats", "-loglevel", "error", "-y", "-i"]).arg(file).arg("-i").arg(saved);
    c.args(["-map", "0:v:0", "-map", "1:a:0", "-map_metadata", "0", "-c", "copy"]);
    if matches!(ext.to_ascii_lowercase().as_str(), "mp4" | "mov" | "m4v") {
        c.args(["-movflags", "+faststart"]);
    }
    c.arg(&tmp);
    let res = c.output().map_err(|e| RenderError::Encode(format!("could not run ffmpeg to restore the audio: {e}")))?;
    if !res.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(RenderError::Encode(format!("ffmpeg could not restore the audio of {}: {}", file.display(), String::from_utf8_lossy(&res.stderr).trim())));
    }
    std::fs::rename(&tmp, file).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        RenderError::Encode(format!("could not replace {} (is it open in another program?): {e}", file.display()))
    })
}

/// Replaces the audio of a finished file with `wav` (video copied untouched).
fn remux_audio(out: &Path, wav: &Path, codec: VideoCodec, kbps: u32) -> Result<(), RenderError> {
    let ffmpeg = &aiframes_media::tools()?.ffmpeg;
    let ext = out.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "mp4".into());
    let stem = out.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "export".into());
    let tmp = out.with_file_name(format!("{stem}.audio-pass.part.{ext}"));
    let mut c = tool_command(ffmpeg);
    c.args(["-hide_banner", "-nostats", "-loglevel", "error", "-y", "-i"]).arg(out).arg("-i").arg(wav);
    c.args(["-map", "0:v:0", "-map", "1:a:0", "-map_metadata", "0", "-c:v", "copy"]);
    if codec == VideoCodec::ProRes {
        c.args(["-c:a", "pcm_s24le"]);
    } else {
        c.args(["-c:a", "aac", "-b:a", &format!("{}k", kbps.clamp(32, 1024))]);
    }
    if matches!(ext.to_ascii_lowercase().as_str(), "mp4" | "mov" | "m4v") {
        c.args(["-movflags", "+faststart"]);
    }
    c.arg(&tmp);
    let res = c.output().map_err(|e| RenderError::Encode(format!("could not run ffmpeg to replace the audio: {e}")))?;
    if !res.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(RenderError::Encode(format!("ffmpeg could not replace the audio of {}: {}", out.display(), String::from_utf8_lossy(&res.stderr).trim())));
    }
    std::fs::rename(&tmp, out).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        RenderError::Encode(format!("could not replace {} (is it open in another program?): {e}", out.display()))
    })
}

// ---------------------------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------------------------

/// Deletes a temp file on drop.
struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn temp_wav() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!("aiframes-export-{}-{}.wav", std::process::id(), N.fetch_add(1, Ordering::Relaxed)))
}

struct Shared {
    pushed: usize,
    prefetched: usize,
    prefetch_done: bool,
}

pub(crate) fn export(
    doc_dir: &Path,
    p: &Project,
    pool: Arc<MediaPool>,
    s: &ExportSettings,
    out: &Path,
    progress: &mut dyn FnMut(ExportProgress) -> bool,
) -> Result<ExportReport, RenderError> {
    let started = Instant::now();
    let r = resolve(p);
    if let Some(e) = r.errors.first() {
        let more = if r.errors.len() > 1 { format!(" (and {} more)", r.errors.len() - 1) } else { String::new() };
        return Err(RenderError::Other(format!("cannot export: {}{more}; run validate and fix the anchors first", e.message)));
    }
    let codec_name = s.codec.trim().to_ascii_lowercase();
    let audio_only = codec_name == "wav";
    let codec = if audio_only {
        None
    } else {
        Some(VideoCodec::parse(&codec_name).ok_or_else(|| {
            RenderError::Other(format!("unknown export codec `{}` (use h264, hevc, av1, prores, gif, png_sequence or wav)", s.codec))
        })?)
    };
    let quality = Quality::parse(&s.quality).unwrap_or_default();
    let st = &p.settings;
    let fps = if st.fps.is_finite() && st.fps > 0.0 { st.fps } else { 30.0 };
    let (a, b) = match s.range {
        Some([a, b]) => (a, b),
        None => (0.0, r.duration),
    };
    if !(a.is_finite() && b.is_finite()) || b <= a.max(0.0) {
        return Err(RenderError::Other(format!("nothing to export: range {a:.3}..{b:.3} s is empty (the project is {:.3} s long)", r.duration)));
    }
    let a = a.max(0.0);
    let first = frame_index(a, fps);
    // Frames whose start lies before `b` (exclusive end).
    let end = (b * (1.0 / frame_time(1, fps)) - 1e-6).ceil() as i64;
    let last = (end - 1).max(first);
    let frames = (last - first + 1) as u64;
    let t0 = frame_time(first, fps);
    let t1 = frame_time(last + 1, fps);
    let duration = t1 - t0;
    let cancel = AtomicBool::new(false);
    let mut last_report = Instant::now() - std::time::Duration::from_secs(1);
    let mut last_stage = "";
    // Peak private memory of the process, sampled at every progress step (cheap system call).
    let peak_mem = std::sync::atomic::AtomicU64::new(0);
    let sample_mem = || {
        if let Some(m) = memory_stats::memory_stats() {
            peak_mem.fetch_max(m.virtual_mem as u64, Ordering::Relaxed);
        }
    };
    sample_mem();
    // The canvas pool goes back to its idle size however the export ends.
    struct TrimPool;
    impl Drop for TrimPool {
        fn drop(&mut self) {
            crate::canvas::set_pool_limit(None);
            crate::canvas::trim_pool(crate::canvas::POOL_IDLE_PIXELS);
        }
    }
    let _trim = TrimPool;
    let mut report = |stage: &'static str, frame: u64, total: u64, force: bool, eta: f64| -> bool {
        sample_mem();
        let now = Instant::now();
        // Throttled to 20 per second, except stage changes and forced reports.
        if !force && stage == last_stage && now.duration_since(last_report).as_secs_f64() < 0.05 {
            return true;
        }
        last_report = now;
        last_stage = stage;
        progress(ExportProgress { stage, frame, frames: total, elapsed: started.elapsed().as_secs_f64(), eta })
    };
    if !report("prepare", 0, frames, true, 0.0) {
        return Err(RenderError::Cancelled);
    }
    // Output frame size and how the project frame maps onto it.
    let (mut w, mut h) = s.output_size(st);
    if matches!(codec, Some(VideoCodec::H264 | VideoCodec::Hevc | VideoCodec::Av1 | VideoCodec::ProRes)) {
        // Chroma subsampling needs even sizes: render the extra row/column ourselves (fitted, so it
        // is picture or background colour, never encoder black) and report the real file size.
        w += w & 1;
        h += h & 1;
    }
    let view = OutputView::fitted(st, w, h, s.fit)?;
    let mut warnings: Vec<String> = Vec::new();
    let rd = Renderer::with_vector_budget(pool.clone(), EXPORT_VECTOR_CACHE);
    let takes_audio = audio_only || matches!(codec, Some(VideoCodec::H264 | VideoCodec::Hevc | VideoCodec::Av1 | VideoCodec::ProRes));

    // Audio.
    let ceiling = if s.true_peak_db.is_finite() { s.true_peak_db.min(0.0) } else { -1.0 };
    let target = s.loudness_lufs.filter(|v| v.is_finite());
    let mut temp: Option<TempFile> = None;
    let mut loudness = None;
    let mut audio_state: Option<(Mixed, PathBuf, WavFormat)> = None;
    if takes_audio {
        let wav = if audio_only { out.to_path_buf() } else { temp_wav() };
        if !audio_only {
            temp = Some(TempFile(wav.clone()));
        }
        if let Some(dir) = wav.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        let fmt = if audio_only { WavFormat::S24 } else { WavFormat::F32 };
        let mut rep = |stage: &'static str, f: u64, n: u64| report(stage, f, n, false, 0.0);
        let res = mix_and_measure(&rd, doc_dir, p, &r, t0, t1, &cancel, &mut rep)
            .and_then(|mixed| normalize_to_wav(&rd, doc_dir, p, &r, (t0, t1), &mixed, target, ceiling, &wav, fmt, &cancel, &mut rep).map(|a| (mixed, a)));
        match res {
            Ok((mixed, a)) => {
                loudness = Some(a.loudness);
                warnings.extend(a.warning);
                audio_state = Some((mixed, wav, fmt));
            }
            Err(e) => {
                if audio_only {
                    let _ = std::fs::remove_file(&wav);
                }
                return Err(e);
            }
        }
    }
    // Measures the delivered file like ffmpeg's ebur128 and, while its true peak is over the
    // ceiling or its loudness off target (lossy audio codecs overshoot), re-encodes only the audio.
    let deliver = |file: &Path,
                   remux: Option<(VideoCodec, u32)>,
                   loudness: &mut Option<Loudness>,
                   warnings: &mut Vec<String>,
                   report: &mut dyn FnMut(&'static str, u64, u64, bool, f64) -> bool|
     -> Result<(), RenderError> {
        let Some((mixed, wav, fmt)) = &audio_state else { return Ok(()) };
        // True peak of the PCM that produced the current delivery (codec overshoot = file - this).
        let mut pcm_tp = loudness.map(|l| l.true_peak_db).unwrap_or(ceiling);
        let mut m = delivered_loudness(file)?;
        let (mut pcm_target, mut pcm_ceiling) = (target, ceiling);
        // How far a delivery is from the settings: the true-peak ceiling counts most, with a small
        // guard band so other decoders/meters also see it under the ceiling.
        let tp_limit = ceiling - DELIVERY_TP_GUARD;
        let badness = |m: &Loudness| {
            (m.true_peak_db - tp_limit).max(0.0) * 4.0 + (target.map(|t| (t - m.integrated_lufs).abs()).unwrap_or(0.0) - 0.2).max(0.0)
        };
        // The best delivery so far, with its audio stream saved (lossy codecs are not monotonic,
        // so a correction pass can come out worse than the one before).
        let mut best: Option<(f64, Loudness, TempFile)> = None;
        let mut pass = 0;
        loop {
            *loudness = Some(m);
            let bad = badness(&m);
            if m.integrated_lufs <= -70.0 || bad <= 0.0 {
                return Ok(());
            }
            if pass == DELIVERY_PASSES {
                if let Some((best_bad, best_m, saved)) = &best {
                    if *best_bad < bad {
                        restore_audio(file, &saved.0, remux.is_some())?;
                        m = *best_m;
                        *loudness = Some(m);
                    }
                }
                warnings.push(format!(
                    "the delivered audio measures {:.2} LUFS / {:.2} dBTP (target {} LUFS, ceiling {ceiling:.1} dBTP) after {} correction passes",
                    m.integrated_lufs,
                    m.true_peak_db,
                    target.map(|t| format!("{t:.1}")).unwrap_or_else(|| "none".into()),
                    DELIVERY_PASSES
                ));
                return Ok(());
            }
            if best.as_ref().is_none_or(|(b, _, _)| bad < *b) {
                best = Some((bad, m, save_audio(file, remux.is_some())?));
            }
            pass += 1;
            if !report("loudness", pass as u64, DELIVERY_PASSES as u64, true, 0.0) {
                cancel.store(true, Ordering::Relaxed);
                return Err(RenderError::Cancelled);
            }
            let over = m.true_peak_db - tp_limit;
            if over > 0.0 {
                // Leave room for the codec's measured overshoot (and always move down).
                let overshoot = (m.true_peak_db - pcm_tp).max(0.0);
                pcm_ceiling = (pcm_ceiling - over - 0.05).min(tp_limit - overshoot - 0.1);
            }
            if let (Some(t), Some(goal)) = (pcm_target.as_mut(), target) {
                *t += goal - m.integrated_lufs;
            }
            let mut quiet = |_: &'static str, _: u64, _: u64| !cancel.load(Ordering::Relaxed);
            let a = normalize_to_wav(&rd, doc_dir, p, &r, (t0, t1), mixed, pcm_target, pcm_ceiling, wav, *fmt, &cancel, &mut quiet)?;
            pcm_tp = a.loudness.true_peak_db;
            if let Some((codec, kbps)) = remux {
                remux_audio(file, wav, codec, kbps)?;
            }
            m = delivered_loudness(file)?;
        }
    };
    let Some(codec) = codec else {
        deliver(out, None, &mut loudness, &mut warnings, &mut report)?;
        report("done", frames, frames, true, 0.0);
        return Ok(ExportReport {
            path: out.to_path_buf(),
            duration,
            frames: 0,
            width: 0,
            height: 0,
            encoder: "pcm_s24le".into(),
            loudness,
            seconds: started.elapsed().as_secs_f64(),
            warnings,
            peak_memory_bytes: memory_stats::memory_stats().map(|_| peak_mem.load(Ordering::Relaxed)),
        });
    };

    // Video. A hardware encoder that cannot open at this size (H.264 NVENC is limited to 4096
    // px) or fails later makes the export start over with the software encoder.
    let tl = Timeline::new(p, &r);
    let job = FrameJob { rd: &rd, doc_dir, p, tl: &tl, strict: true, caching: aiframes_media::FrameCaching::LowPriority };
    let mut hardware = s.hardware && !(codec == VideoCodec::H264 && (w > 4096 || h > 4096));
    let flatten = !(st.background.a < 1.0 && matches!(codec, VideoCodec::ProRes | VideoCodec::PngSequence | VideoCodec::Gif));
    let encoder = loop {
        let opts = EncodeOptions {
            path: out.to_path_buf(),
            width: w,
            height: h,
            fps,
            codec,
            quality,
            hardware,
            audio: temp.as_ref().map(|t| t.0.clone()),
            audio_bitrate_kbps: s.audio_bitrate_kbps,
            alpha: !flatten,
        };
        let v = VideoJob { job: &job, view: &view, opts: &opts, first, fps, frames, flatten, background: st.background, cancel: &cancel, pool: &pool, p };
        match encode_video(&v, &mut report) {
            Ok(name) => break name,
            Err(VideoError::Encoder(e)) if hardware && !cancel.load(Ordering::Relaxed) => {
                warnings.push(format!("the hardware encoder failed ({e}); exported with the software encoder instead"));
                hardware = false;
            }
            Err(VideoError::Encoder(e)) => return Err(RenderError::Encode(e)),
            Err(VideoError::Render(e)) => return Err(e),
        }
    };
    if matches!(codec, VideoCodec::H264 | VideoCodec::Hevc | VideoCodec::Av1 | VideoCodec::ProRes) {
        deliver(out, Some((codec, s.audio_bitrate_kbps)), &mut loudness, &mut warnings, &mut report)?;
    }
    drop(temp);
    if codec == VideoCodec::PngSequence {
        remove_stale_png_frames(out, frames);
    }
    sample_mem();
    report("done", frames, frames, true, 0.0);
    Ok(ExportReport {
        path: out.to_path_buf(),
        duration,
        frames,
        width: w,
        height: h,
        encoder,
        loudness,
        seconds: started.elapsed().as_secs_f64(),
        warnings,
        peak_memory_bytes: memory_stats::memory_stats().map(|_| peak_mem.load(Ordering::Relaxed)),
    })
}

enum VideoError {
    /// The encoder failed (opening, writing or finishing).
    Encoder(String),
    /// Rendering failed or was cancelled.
    Render(RenderError),
}

struct VideoJob<'a> {
    job: &'a FrameJob<'a>,
    view: &'a OutputView,
    opts: &'a EncodeOptions,
    first: i64,
    fps: f64,
    frames: u64,
    /// Composite semi-transparent pixels over the background colour (formats without alpha).
    flatten: bool,
    background: aiframes_core::Color,
    cancel: &'a AtomicBool,
    pool: &'a Arc<MediaPool>,
    p: &'a Project,
}

/// Straight RGBA over an opaque colour, in place.
fn flatten_over(rgba: &mut [u8], bg: aiframes_core::Color) {
    let [br, bgc, bb, _] = bg.to_rgba8();
    let b = [br as u32, bgc as u32, bb as u32];
    rgba.par_chunks_mut(1 << 16).for_each(|c| {
        for px in c.as_chunks_mut::<4>().0 {
            let a = px[3] as u32;
            if a == 255 {
                continue;
            }
            for k in 0..3 {
                px[k] = ((px[k] as u32 * a + b[k] * (255 - a) + 127) / 255) as u8;
            }
            px[3] = 255;
        }
    });
}

/// Renders every frame (several in flight, read-ahead in order) and encodes them; returns the
/// encoder's name.
fn encode_video(v: &VideoJob, report: &mut dyn FnMut(&'static str, u64, u64, bool, f64) -> bool) -> Result<String, VideoError> {
    let enc_err = |e: aiframes_media::MediaError| VideoError::Encoder(e.to_string());
    let mut enc = Encoder::start(v.opts).map_err(enc_err)?;
    let encoder = enc.encoder_name().to_string();
    let n = v.frames as usize;
    let (first, fps) = (v.first, v.fps);
    let time_of = |i: usize| frame_time(first + i as i64, fps);
    let (job, view, cancel) = (v.job, v.view, v.cancel);

    // Frames in flight, bounded by a byte budget: each worker holds a few premultiplied f32
    // canvases plus its finished RGBA frame; finished frames also wait for the encoder in order.
    let canvas_bytes = view.cw as usize * view.ch as usize * 16;
    let out_bytes = v.opts.width as usize * v.opts.height as usize * 4;
    let per_worker = WORKER_CANVASES * canvas_bytes + 2 * out_bytes;
    let workers = (EXPORT_BUFFER_BUDGET / per_worker.max(1)).min(rayon::current_num_threads() / 3).clamp(2, 8);
    let window = workers * 2;
    // Let the canvas pool hold the workers' canvases (re-allocating them every frame costs page
    // faults); this stays within the budget above and is undone when the export ends.
    crate::canvas::set_pool_limit(Some(workers * (canvas_bytes / 16) * 3 / 2));
    // Read-ahead distance bounded by the frame cache.
    let stats = v.pool.stats();
    let media_tracks = v.p.tracks.iter().filter(|t| job.tl.shows_picture(t)).count().clamp(1, 4);
    let frame_bytes = v.opts.width as usize * v.opts.height as usize * 4 * media_tracks;
    let ahead = (stats.cache_budget / frame_bytes.max(1) / 2).clamp(2, window + 6);

    let shared = Mutex::new(Shared { pushed: 0, prefetched: 0, prefetch_done: false });
    let cv = Condvar::new();
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let video_started = Instant::now();
    let (tx, rx) = std::sync::mpsc::channel::<(usize, Result<Vec<u8>, RenderError>)>();
    let halt = || cancel.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed);
    let outcome: Result<(), VideoError> = std::thread::scope(|sc| {
        // In-order media read-ahead.
        sc.spawn(|| {
            for i in 0..n {
                {
                    let mut g = lock(&shared);
                    while i >= g.pushed + ahead && !halt() {
                        g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
                    }
                }
                if halt() {
                    break;
                }
                job.prefetch(time_of(i), view, false);
                lock(&shared).prefetched = i + 1;
                cv.notify_all();
            }
            lock(&shared).prefetch_done = true;
            cv.notify_all();
        });
        for _ in 0..workers {
            let tx = tx.clone();
            let (shared, cv, next, halt) = (&shared, &cv, &next, &halt);
            let (flatten, background) = (v.flatten, v.background);
            sc.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n || halt() {
                    break;
                }
                {
                    let mut g = lock(shared);
                    while !halt() && (i >= g.pushed + window || (i >= g.prefetched && !g.prefetch_done)) {
                        g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
                    }
                }
                if halt() {
                    break;
                }
                let res = job.render_view(time_of(i), view, false).map(|(img, _)| {
                    let mut rgba = img.rgba;
                    if flatten && background.a < 1.0 {
                        flatten_over(&mut rgba, background);
                    }
                    rgba
                });
                if tx.send((i, res)).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        let mut pending: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
        let mut done = 0usize;
        let fail = |e: VideoError| {
            stop.store(true, Ordering::Relaxed);
            cv.notify_all();
            e
        };
        while done < n {
            let Ok((i, res)) = rx.recv() else { break };
            match res {
                Ok(px) => {
                    pending.insert(i, px);
                }
                Err(e) => return Err(fail(VideoError::Render(e))),
            }
            while let Some(px) = pending.remove(&done) {
                if let Err(e) = enc.push_owned(px) {
                    return Err(fail(enc_err(e)));
                }
                done += 1;
                lock(&shared).pushed = done;
                cv.notify_all();
                let el = video_started.elapsed().as_secs_f64();
                let eta = if done > 0 { el / done as f64 * (n - done) as f64 } else { 0.0 };
                if !report("video", done as u64, v.frames, done == n, eta) {
                    cancel.store(true, Ordering::Relaxed);
                    return Err(fail(VideoError::Render(RenderError::Cancelled)));
                }
            }
        }
        if done < n {
            return Err(fail(VideoError::Render(RenderError::Other("frame rendering stopped early".into()))));
        }
        Ok(())
    });
    if let Err(e) = outcome {
        enc.abort();
        return Err(e);
    }
    if !report("mux", v.frames, v.frames, true, 0.0) {
        enc.abort();
        cancel.store(true, Ordering::Relaxed);
        return Err(VideoError::Render(RenderError::Cancelled));
    }
    enc.finish().map_err(enc_err)?;
    Ok(encoder)
}

/// After a PNG-sequence export, frames of an earlier, longer export in the same place are removed
/// (only files named exactly like this export's frames, numbered past its end).
fn remove_stale_png_frames(out: &Path, frames: u64) {
    let s = out.to_string_lossy();
    if s.contains('%') {
        return;
    }
    let (dir, prefix) = match out.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()) {
        Some(e) if e == "png" => {
            let stem = out.file_stem().map(|x| x.to_string_lossy().into_owned()).unwrap_or_else(|| "frame".into());
            (out.parent().map(Path::to_path_buf).unwrap_or_default(), format!("{stem}_"))
        }
        _ => (out.to_path_buf(), "frame_".to_string()),
    };
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(num) = name.strip_prefix(&prefix).and_then(|r| r.strip_suffix(".png")) else { continue };
        if num.len() == 6 && num.bytes().all(|b| b.is_ascii_digit()) && num.parse::<u64>().map(|i| i >= frames).unwrap_or(false) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn true_peak_sees_between_samples() {
        // fs/4 sine at 45 degrees: every sample is +-0.707, the waveform peaks at 1.0 (0 dBTP).
        let x: Vec<f32> = (0..4800).flat_map(|n| {
            let v = (std::f64::consts::FRAC_PI_2 * n as f64 + std::f64::consts::FRAC_PI_4).sin() as f32;
            [v, v]
        }).collect();
        let tp = true_peak_db(&x);
        assert!(tp.abs() < 0.15, "{tp}");
        let sample_peak = 20.0 * (x.iter().fold(0f32, |a, v| a.max(v.abs())) as f64).log10();
        assert!((sample_peak + 3.01).abs() < 0.01);
        // A limiter at -1 dBTP brings that true peak (not just the samples) under the ceiling.
        let mut l = Limiter::new(-1.0, 48_000);
        let mut out = Vec::new();
        l.process(&x, &mut out);
        l.flush(&mut out);
        let mid = &out[2000..out.len() - 2000];
        assert!(true_peak_db(mid) <= -0.99 && true_peak_db(mid) > -1.1, "{}", true_peak_db(mid));
    }

    #[test]
    fn limiter_catches_peaks_and_keeps_length() {
        let rate = 48_000;
        let mut x = Vec::new();
        for i in 0..rate {
            let v = (i as f32 * 0.07).sin() * if i > 20_000 && i < 21_000 { 2.0 } else { 0.5 };
            x.push(v);
            x.push(-v);
        }
        let mut l = Limiter::new(-1.0, rate);
        let mut out = Vec::new();
        for c in x.chunks(999 * 2) {
            l.process(c, &mut out);
        }
        l.flush(&mut out);
        assert_eq!(out.len(), x.len());
        let ceil = 10f32.powf(-1.0 / 20.0);
        let pk = out.iter().fold(0f32, |a, v| a.max(v.abs()));
        assert!(pk <= ceil * 1.001, "{pk}");
        // Quiet parts pass unchanged (aligned, no delay).
        assert!((out[2000] - x[2000]).abs() < 1e-6);
    }
}
