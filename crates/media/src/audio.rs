//! Audio: decode once to a disk cache of stereo f32 PCM, then serve random-access reads without
//! loading long files into RAM. Also the streaming PCM decoder used by the analysis functions, and
//! WAV writing for the export pipeline.
//!
//! A file not cached yet is decoded *progressively* (see `progressive.rs`): `MediaPool::audio`
//! returns at once, reads wait briefly for samples still being decoded, and `wait_complete` waits
//! for the whole file (export does that, so it always gets exact audio).
//!
//! Timing: PCM sample 0 is source time 0 (the container start, the same origin as video frame
//! times). `aresample=async=1:first_pts=0` pads audio that starts late and trims encoder priming,
//! so audio stays in sync with video.

use crate::error::{MediaError, Result};
use crate::probe::Probed;
use crate::progressive::{Prog, READ_WAIT};
use crate::tools::{ff_path, ffmpeg_at, lock, spawn_error, ChildGuard, Priority, StderrPump};
use crate::util::{cache_name, ensure_dir, read_exact_at, FileKey};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Decoded audio of one file at one sample rate: stereo interleaved f32.
pub struct AudioData {
    rate: u32,
    frames: u64,
    /// Media duration used for files without audio (they read as silence).
    silent_duration: f64,
    source: PathBuf,
    store: Store,
}

enum Store {
    Silent,
    Memory(Vec<f32>),
    File { file: File, blocks: Mutex<Blocks> },
    /// Still being decoded (or decoded during this run).
    Progressive(Arc<Prog>),
}

/// Frames per cached block (32768 stereo frames = 256 KiB) and blocks kept per file.
pub(crate) const BLOCK_FRAMES: u64 = 32_768;
const MAX_BLOCKS: usize = 24;
/// Cache files up to this size are loaded fully into memory.
const RESIDENT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct Blocks {
    tick: u64,
    list: Vec<(u64, Vec<f32>, u64)>, // (block index, samples, last use)
}

impl std::fmt::Debug for AudioData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioData")
            .field("source", &self.source)
            .field("rate", &self.rate)
            .field("frames", &self.frames())
            .field("duration", &self.duration())
            .field("complete", &self.is_complete())
            .finish()
    }
}

impl AudioData {
    /// Wraps stereo interleaved samples already in memory.
    pub fn from_samples(source: impl Into<PathBuf>, rate: u32, stereo: Vec<f32>) -> AudioData {
        let frames = (stereo.len() / 2) as u64;
        AudioData { rate: rate.max(1), frames, silent_duration: 0.0, source: source.into(), store: Store::Memory(stereo) }
    }

    pub(crate) fn silent(source: &Path, rate: u32, duration: f64) -> AudioData {
        AudioData { rate: rate.max(1), frames: 0, silent_duration: duration.max(0.0), source: source.to_path_buf(), store: Store::Silent }
    }

    /// Opens a cache file of raw f32le stereo PCM.
    pub(crate) fn open_cache(source: &Path, cache: &Path, rate: u32) -> Result<AudioData> {
        let file = File::open(cache).map_err(|e| MediaError::io(cache, "open the audio cache file", &e))?;
        let len = file.metadata().map_err(|e| MediaError::io(cache, "read the audio cache file", &e))?.len();
        let frames = len / 8;
        let store = if len <= RESIDENT_BYTES {
            let mut bytes = Vec::with_capacity(len as usize);
            (&file).read_to_end(&mut bytes).map_err(|e| MediaError::io(cache, "read the audio cache file", &e))?;
            Store::Memory(bytes_to_f32(&bytes[..(frames * 8) as usize]))
        } else {
            Store::File { file, blocks: Mutex::new(Blocks::default()) }
        };
        Ok(AudioData { rate, frames, silent_duration: 0.0, source: source.to_path_buf(), store })
    }

    pub(crate) fn progressive(source: &Path, prog: Arc<Prog>) -> AudioData {
        AudioData { rate: prog.rate(), frames: 0, silent_duration: 0.0, source: source.to_path_buf(), store: Store::Progressive(prog) }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Seconds of audio (for files without an audio stream: the media duration, all silence).
    /// While still decoding this is the expected length; exact once `is_complete`.
    pub fn duration(&self) -> f64 {
        let frames = self.frames();
        if frames > 0 || self.has_audio() {
            frames as f64 / self.rate as f64
        } else {
            self.silent_duration
        }
    }

    /// Number of stereo sample frames (expected while still decoding, exact once `is_complete`).
    pub fn frames(&self) -> u64 {
        match &self.store {
            Store::Progressive(p) => p.frames(),
            _ => self.frames,
        }
    }

    /// True when every sample is decoded (always true for cached files and files without audio).
    pub fn is_complete(&self) -> bool {
        match &self.store {
            Store::Progressive(p) => p.is_complete(),
            _ => true,
        }
    }

    /// Fraction of the file decoded from its start, 0..1 (1 when complete).
    pub fn progress(&self) -> f64 {
        match &self.store {
            Store::Progressive(p) => p.progress(),
            _ => 1.0,
        }
    }

    /// Blocks until the whole file is decoded; the decode's error if it failed. Export calls
    /// this before mixing so it always gets complete, exact audio. Moves this file to the front
    /// of the decode queue.
    pub fn wait_complete(&self) -> Result<()> {
        match &self.store {
            Store::Progressive(p) => p.wait_complete(None).map(|_| ()),
            _ => Ok(()),
        }
    }

    /// Like `wait_complete` with a time limit: `Ok(true)` when complete, `Ok(false)` on timeout.
    pub fn wait_complete_timeout(&self, timeout: Duration) -> Result<bool> {
        match &self.store {
            Store::Progressive(p) => p.wait_complete(Some(timeout)),
            _ => Ok(true),
        }
    }

    /// The background decode's error, if it failed (reads past what was decoded are then silence).
    pub fn error(&self) -> Option<MediaError> {
        match &self.store {
            Store::Progressive(p) => p.error(),
            _ => None,
        }
    }

    /// Non-blocking: can `[start_sec, start_sec + len_sec)` be read without waiting? (Use it to show
    /// "buffering".) True outside the audio.
    pub fn ready_at(&self, start_sec: f64, len_sec: f64) -> bool {
        let Store::Progressive(p) = &self.store else { return true };
        let (a, b) = self.frame_range(start_sec, len_sec);
        b <= a || p.ready(a, b)
    }

    /// Non-blocking: prioritises decoding around `start_sec` (call it on seek, before playing).
    pub fn prefetch(&self, start_sec: f64) {
        if let Store::Progressive(p) = &self.store {
            let (a, b) = self.frame_range(start_sec, 2.0);
            p.prefetch(a, b.max(a + 1));
        }
    }

    fn frame_range(&self, start_sec: f64, len_sec: f64) -> (u64, u64) {
        let r = self.rate as f64;
        let a = if start_sec.is_finite() { (start_sec * r).round().max(0.0) } else { 0.0 };
        let len = if len_sec.is_finite() { (len_sec * r).round().max(0.0) } else { 0.0 };
        (a as u64, (a + len) as u64)
    }

    /// False for files without an audio stream (reads return silence).
    pub fn has_audio(&self) -> bool {
        !matches!(self.store, Store::Silent)
    }

    /// The media file this was decoded from.
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Fills `out` (stereo interleaved) from source time `start_sec` (rounded to the nearest sample);
    /// zeros outside the audio.
    pub fn read(&self, start_sec: f64, out: &mut [f32]) {
        let start = if start_sec.is_finite() { (start_sec * self.rate as f64).round() as i64 } else { 0 };
        self.read_frames(start, out);
    }

    /// Sample-exact variant of `read`: `start` is a stereo frame index (may be negative).
    /// Samples still being decoded are waited for up to 150 ms, then read as silence.
    pub fn read_frames(&self, start: i64, out: &mut [f32]) {
        self.read_frames_wait(start, out, READ_WAIT);
    }

    /// `read_frames` with an explicit wait for samples not decoded yet. Returns true when every
    /// requested sample was available (or lies outside the audio), false if some were filled with
    /// silence because they did not arrive within `max_wait`.
    pub fn read_frames_wait(&self, start: i64, out: &mut [f32], max_wait: Duration) -> bool {
        if let Store::Progressive(p) = &self.store {
            return p.read(start, out, max_wait);
        }
        self.read_static(start, out);
        true
    }

    fn read_static(&self, start: i64, out: &mut [f32]) {
        let n = (out.len() / 2) as i64;
        let a = start.max(0);
        let b = (start + n).min(self.frames as i64);
        if b <= a {
            out.fill(0.0);
            return;
        }
        let lead = ((a - start) * 2) as usize;
        let tail_from = ((b - start) * 2) as usize;
        out[..lead].fill(0.0);
        out[tail_from..].fill(0.0);
        let dst = &mut out[lead..tail_from];
        match &self.store {
            Store::Silent | Store::Progressive(_) => dst.fill(0.0),
            Store::Memory(v) => dst.copy_from_slice(&v[(a * 2) as usize..(b * 2) as usize]),
            Store::File { file, blocks } => {
                let mut blocks = lock(blocks);
                let mut pos = a as u64;
                let mut written = 0usize;
                while pos < b as u64 {
                    let bi = pos / BLOCK_FRAMES;
                    let block_start = bi * BLOCK_FRAMES;
                    let in_block = (pos - block_start) as usize;
                    let take = ((b as u64 - pos) as usize).min(BLOCK_FRAMES as usize - in_block);
                    match blocks.get(file, bi, self.frames) {
                        Some(samples) => {
                            let avail = samples.len() / 2;
                            let real = take.min(avail.saturating_sub(in_block));
                            dst[written..written + real * 2].copy_from_slice(&samples[in_block * 2..(in_block + real) * 2]);
                            dst[written + real * 2..written + take * 2].fill(0.0);
                        }
                        None => dst[written..written + take * 2].fill(0.0),
                    }
                    written += take * 2;
                    pos += take as u64;
                }
            }
        }
    }

    /// Visits the whole signal sequentially in chunks (bounded memory), stereo interleaved.
    /// Waits for a progressive decode to finish first.
    pub(crate) fn for_each_chunk(&self, mut f: impl FnMut(&[f32])) {
        let _ = self.wait_complete();
        let total = self.frames();
        let chunk = BLOCK_FRAMES as usize;
        let mut buf = vec![0f32; chunk * 2];
        let mut pos = 0u64;
        while pos < total {
            let n = ((total - pos) as usize).min(chunk);
            self.read_frames(pos as i64, &mut buf[..n * 2]);
            f(&buf[..n * 2]);
            pos += n as u64;
        }
    }
}

impl Blocks {
    pub(crate) fn get(&mut self, file: &File, bi: u64, total_frames: u64) -> Option<&Vec<f32>> {
        self.tick += 1;
        let tick = self.tick;
        if let Some(i) = self.list.iter().position(|(b, _, _)| *b == bi) {
            self.list[i].2 = tick;
            return Some(&self.list[i].1);
        }
        let start = bi * BLOCK_FRAMES;
        let n = BLOCK_FRAMES.min(total_frames.saturating_sub(start));
        let mut bytes = vec![0u8; (n * 8) as usize];
        read_exact_at(file, &mut bytes, start * 8).ok()?;
        let samples = bytes_to_f32(&bytes);
        if self.list.len() >= MAX_BLOCKS {
            if let Some(oldest) = self.list.iter().enumerate().min_by_key(|(_, (_, _, t))| *t).map(|(i, _)| i) {
                self.list.swap_remove(oldest);
            }
        }
        self.list.push((bi, samples, tick));
        self.list.last().map(|(_, s, _)| s)
    }
}

pub(crate) fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

/// The audio filter chain: mono becomes dual-mono at unity gain (not -3 dB), more channels are
/// downmixed with ffmpeg's standard matrix; output starts at source time 0.
fn audio_filter(probed: &Probed, rate: u32, channels: u32) -> String {
    let mut f = String::new();
    if channels == 2 && probed.info.audio_channels == 1 {
        f.push_str("pan=stereo|c0=c0|c1=c0,");
    }
    f.push_str(&format!("aresample={rate}:async=1:first_pts=0"));
    f
}

/// The audio of `path` at `rate`: from the disk cache when it is there, else a progressive decode
/// into it (returns at once; the decode runs in the background).
pub(crate) fn open_or_start(path: &Path, probed: &Probed, key: &FileKey, rate: u32, dir: &Path) -> Result<AudioData> {
    if probed.audio_index.is_none() || !probed.info.has_audio {
        return Ok(AudioData::silent(path, rate, probed.info.duration));
    }
    ensure_dir(dir)?;
    let cache = dir.join(cache_name(path, key, &format!("-{rate}.f32")));
    let valid = std::fs::metadata(&cache).map(|m| m.is_file() && m.len() % 8 == 0).unwrap_or(false);
    if valid {
        return AudioData::open_cache(path, &cache, rate);
    }
    Ok(AudioData::progressive(path, Prog::start(path, probed, rate, &cache)?))
}

/// Streams a file's audio as interleaved f32 (`channels` = 1 or 2) at `rate` to `f` in chunks;
/// `f` returns false to stop early. Returns the number of frames delivered. Files without audio
/// deliver nothing.
pub(crate) fn stream_pcm(path: &Path, probed: &Probed, rate: u32, channels: u32, mut f: impl FnMut(&[f32]) -> bool) -> Result<u64> {
    let mut samples: Vec<f32> = Vec::new();
    stream_raw(path, probed, rate, channels, 0, Priority::Background, |bytes| {
        samples.clear();
        samples.extend(bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)));
        f(&samples)
    })
}

/// Like `stream_pcm` but delivers raw little-endian f32 bytes (always whole frames), starting at
/// output frame `seek_frames` (0 = the start; sample `k` delivered is then frame `seek_frames + k`).
pub(crate) fn stream_raw(
    path: &Path,
    probed: &Probed,
    rate: u32,
    channels: u32,
    seek_frames: u64,
    priority: Priority,
    mut f: impl FnMut(&[u8]) -> bool,
) -> Result<u64> {
    let Some(ai) = probed.audio_index.filter(|_| probed.info.has_audio) else { return Ok(0) };
    let channels = channels.clamp(1, 2);
    let frame_bytes = 4 * channels as usize;
    let mut c = ffmpeg_at("error", priority)?;
    if seek_frames > 0 {
        c.args(["-ss", &format!("{:.6}", seek_frames as f64 / rate as f64)]);
    }
    c.arg("-i").arg(ff_path(path));
    c.args(["-map", &format!("0:{ai}"), "-vn", "-sn", "-dn", "-af", &audio_filter(probed, rate, channels)]);
    c.args(["-ac", &channels.to_string(), "-c:a", "pcm_f32le", "-f", "f32le", "pipe:1"]);
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = c.spawn().map_err(|e| spawn_error("ffmpeg", e))?;
    let mut guard = ChildGuard::new(child);
    let pump = StderrPump::start(guard.child().stderr.take().expect("stderr"), |_| false);
    let mut stdout = guard.child().stdout.take().expect("stdout");
    let mut buf = vec![0u8; 256 * 1024];
    let mut have = 0usize; // bytes in buf (a partial frame may be carried over)
    let mut frames = 0u64;
    loop {
        let n = match stdout.read(&mut buf[have..]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(MediaError::io(path, "read decoded audio from ffmpeg", &e)),
        };
        have += n;
        let usable = have / frame_bytes * frame_bytes;
        if usable == 0 {
            continue;
        }
        frames += (usable / frame_bytes) as u64;
        if !f(&buf[..usable]) {
            guard.kill();
            return Ok(frames);
        }
        buf.copy_within(usable..have, 0);
        have -= usable;
    }
    let status = guard.wait().map_err(|e| MediaError::io(path, "wait for ffmpeg", &e))?;
    let tail = pump.finish();
    if !status.success() {
        return Err(MediaError::tool("ffmpeg", Some(path), "decode the audio", status.code(), tail.summary()));
    }
    Ok(frames)
}

// ---------------------------------------------------------------------------------------------
// WAV writing
// ---------------------------------------------------------------------------------------------

/// Sample format of a WAV file written by `WavWriter`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WavFormat {
    /// 32-bit float (lossless for the mixer's output; what `Encoder` expects).
    #[default]
    F32,
    /// 16-bit PCM (rounded, clipped).
    S16,
    /// 24-bit PCM (rounded, clipped).
    S24,
}

/// Streaming WAV writer (interleaved samples; sizes patched on `finish`).
pub struct WavWriter {
    path: PathBuf,
    out: BufWriter<File>,
    format: WavFormat,
    rate: u32,
    channels: u16,
    data_bytes: u64,
}

impl WavWriter {
    pub fn create(path: &Path, rate: u32, channels: u16, format: WavFormat) -> Result<WavWriter> {
        if rate == 0 || channels == 0 {
            return Err(MediaError::InvalidInput(format!("WAV needs a sample rate and channels (got {rate} Hz, {channels} ch)")));
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| MediaError::io(dir, "create the output folder", &e))?;
        }
        let file = File::create(path).map_err(|e| MediaError::io(path, "create", &e))?;
        let mut w = WavWriter { path: path.to_path_buf(), out: BufWriter::with_capacity(1 << 20, file), format, rate, channels, data_bytes: 0 };
        w.header(0).map_err(|e| MediaError::io(path, "write", &e))?;
        Ok(w)
    }

    fn bytes_per_sample(&self) -> u16 {
        match self.format {
            WavFormat::F32 => 4,
            WavFormat::S16 => 2,
            WavFormat::S24 => 3,
        }
    }

    fn header(&mut self, data_bytes: u64) -> std::io::Result<()> {
        let rate = self.rate;
        let bps = self.bytes_per_sample();
        let float = self.format == WavFormat::F32;
        let fmt_len: u32 = if float { 18 } else { 16 };
        let fact_len: u32 = if float { 12 } else { 0 };
        let riff = 4 + (8 + fmt_len) + fact_len + 8 + data_bytes as u32;
        let o = &mut self.out;
        o.write_all(b"RIFF")?;
        o.write_all(&riff.to_le_bytes())?;
        o.write_all(b"WAVEfmt ")?;
        o.write_all(&fmt_len.to_le_bytes())?;
        o.write_all(&(if float { 3u16 } else { 1u16 }).to_le_bytes())?;
        o.write_all(&self.channels.to_le_bytes())?;
        o.write_all(&rate.to_le_bytes())?;
        o.write_all(&(rate * self.channels as u32 * bps as u32).to_le_bytes())?;
        o.write_all(&(self.channels * bps).to_le_bytes())?;
        o.write_all(&(bps * 8).to_le_bytes())?;
        if float {
            o.write_all(&0u16.to_le_bytes())?;
            o.write_all(b"fact")?;
            o.write_all(&4u32.to_le_bytes())?;
            let frames = (data_bytes / (self.channels as u64 * bps as u64)) as u32;
            o.write_all(&frames.to_le_bytes())?;
        }
        o.write_all(b"data")?;
        o.write_all(&(data_bytes as u32).to_le_bytes())?;
        Ok(())
    }

    /// Appends interleaved samples (-1..1 nominal; floats are written unclipped).
    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        let bps = self.bytes_per_sample() as u64;
        if self.data_bytes + samples.len() as u64 * bps > u32::MAX as u64 - 64 {
            return Err(MediaError::InvalidInput(format!(
                "{} would exceed the 4 GB WAV limit; write a shorter range or use 16-bit",
                self.path.display()
            )));
        }
        let mut buf = Vec::with_capacity(samples.len() * bps as usize);
        match self.format {
            WavFormat::F32 => samples.iter().for_each(|s| buf.extend_from_slice(&s.to_le_bytes())),
            WavFormat::S16 => samples
                .iter()
                .for_each(|s| buf.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes())),
            WavFormat::S24 => samples.iter().for_each(|s| {
                let v = (s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
                buf.extend_from_slice(&v.to_le_bytes()[..3]);
            }),
        }
        self.out.write_all(&buf).map_err(|e| MediaError::io(&self.path, "write", &e))?;
        self.data_bytes += buf.len() as u64;
        Ok(())
    }

    /// Patches the header sizes and flushes.
    pub fn finish(mut self) -> Result<()> {
        let path = self.path.clone();
        let io = |e: std::io::Error| MediaError::io(&path, "write", &e);
        self.out.flush().map_err(io)?;
        self.out.seek(SeekFrom::Start(0)).map_err(io)?;
        let data = self.data_bytes;
        self.header(data).map_err(io)?;
        self.out.flush().map_err(io)?;
        Ok(())
    }
}

/// Writes a stereo interleaved f32 buffer to a 32-bit float WAV.
pub fn write_wav(path: &Path, stereo: &[f32], rate: u32) -> Result<()> {
    let mut w = WavWriter::create(path, rate, 2, WavFormat::F32)?;
    for chunk in stereo.chunks(1 << 18) {
        w.write(chunk)?;
    }
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_bounds_and_padding() {
        let samples: Vec<f32> = (0..20).map(|i| i as f32).collect(); // 10 frames
        let a = AudioData::from_samples("x", 10, samples);
        assert_eq!(a.duration(), 1.0);
        let mut out = vec![9.0f32; 8];
        a.read_frames(-2, &mut out);
        assert_eq!(out, vec![0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0]);
        a.read_frames(8, &mut out);
        assert_eq!(out, vec![16.0, 17.0, 18.0, 19.0, 0.0, 0.0, 0.0, 0.0]);
        a.read(0.5, &mut out);
        assert_eq!(out[0], 10.0);
        a.read_frames(100, &mut out);
        assert!(out.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn file_backed_blocks_match_memory() {
        let dir = crate::util::TempDir::new("aiframes-audio-unit").unwrap();
        let p = dir.0.join("big.f32");
        let frames = BLOCK_FRAMES * 3 + 123;
        let samples: Vec<f32> = (0..frames * 2).map(|i| (i % 9973) as f32).collect();
        let mut bytes = Vec::with_capacity(samples.len() * 4);
        samples.iter().for_each(|s| bytes.extend_from_slice(&s.to_le_bytes()));
        std::fs::write(&p, &bytes).unwrap();
        let file = File::open(&p).unwrap();
        let a = AudioData { rate: 48000, frames, silent_duration: 0.0, source: p.clone(), store: Store::File { file, blocks: Mutex::new(Blocks::default()) } };
        let mut out = vec![0f32; 70_000 * 2];
        for start in [0i64, 1, 32_767, 32_768, 65_000, (frames as i64) - 500, -100] {
            a.read_frames(start, &mut out);
            for (i, v) in out.as_chunks::<2>().0.iter().enumerate() {
                let f = start + i as i64;
                let expect = if f >= 0 && (f as u64) < frames { ((f as u64 * 2) % 9973) as f32 } else { 0.0 };
                assert_eq!(v[0], expect, "frame {f}");
            }
        }
    }

    #[test]
    fn wav_roundtrip_header() {
        let dir = crate::util::TempDir::new("aiframes-wav-unit").unwrap();
        let p = dir.0.join("x.wav");
        write_wav(&p, &[0.5, -0.5, 0.25, -0.25], 48000).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize, b.len() - 8);
        assert_eq!(&b[b.len() - 16 - 8..b.len() - 16 - 4], b"data");
        assert_eq!(f32::from_le_bytes([b[b.len() - 16], b[b.len() - 15], b[b.len() - 14], b[b.len() - 13]]), 0.5);
    }
}
