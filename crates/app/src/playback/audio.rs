//! Audio output: a cpal stream fed from a ring buffer that a mixer thread keeps ~200 ms ahead
//! with `Renderer::mix`. The number of samples the device has played is the master clock.
//!
//! * Shuttle speeds (2x, 4x) mix `k` times as much timeline audio per block and decimate it with a
//!   box filter before resampling: higher pitch, but clean (no aliasing hash).
//! * Scrub grains: while paused, a short windowed slice of the mix at the scrub position is played
//!   without touching the playback clock (Premiere-style audio scrubbing).
//!
//! One thread owns the cpal stream (it is not `Send` on every platform) and runs the mixer loop;
//! the device callback only pops samples from the ring.

use super::Snapshot;
use aiframes_render::Renderer;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Project-rate frames mixed per block at 1x.
const BLOCK: usize = 1024;
/// How far ahead of the device the ring is kept (seconds).
const AHEAD: f64 = 0.20;
/// Scrub grain length and its fade in/out (seconds).
pub const GRAIN: f64 = 0.075;
const GRAIN_FADE: f64 = 0.008;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Linear-interpolation stereo resampler that keeps its phase across blocks (no clicks).
#[derive(Clone, Debug)]
pub struct Resampler {
    /// Input frames per output frame.
    step: f64,
    /// Position of the next output frame, in input frames relative to `prev`.
    pos: f64,
    prev: [f32; 2],
    primed: bool,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Resampler { step: in_rate.max(1) as f64 / out_rate.max(1) as f64, pos: 0.0, prev: [0.0; 2], primed: false }
    }

    pub fn is_identity(&self) -> bool {
        (self.step - 1.0).abs() < 1e-12
    }

    /// Resamples interleaved stereo `input`, appending to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_identity() {
            out.extend_from_slice(input);
            return;
        }
        let n = input.len() / 2;
        if n == 0 {
            return;
        }
        if !self.primed {
            self.prev = [input[0], input[1]];
            self.primed = true;
        }
        // Sample k of the virtual sequence [prev, input[0], input[1], ...].
        let at = |k: usize| -> [f32; 2] {
            if k == 0 {
                self.prev
            } else {
                [input[(k - 1) * 2], input[(k - 1) * 2 + 1]]
            }
        };
        while self.pos < n as f64 {
            let i = self.pos.floor() as usize;
            let f = (self.pos - i as f64) as f32;
            let a = at(i);
            let b = at(i + 1);
            out.push(a[0] + (b[0] - a[0]) * f);
            out.push(a[1] + (b[1] - a[1]) * f);
            self.pos += self.step;
        }
        self.pos -= n as f64;
        self.prev = [input[(n - 1) * 2], input[(n - 1) * 2 + 1]];
    }
}

/// Averages every `k` stereo frames into one (a box low-pass + decimation): `k`x speed without the
/// aliasing of plain sample skipping.
pub fn decimate(input: &[f32], k: usize) -> Vec<f32> {
    if k <= 1 {
        return input.to_vec();
    }
    let frames = input.len() / 2;
    let mut out = Vec::with_capacity(frames / k * 2 + 2);
    for chunk in input[..frames * 2].chunks(k * 2) {
        let n = (chunk.len() / 2).max(1) as f32;
        let (mut l, mut r) = (0.0f32, 0.0f32);
        for f in chunk.as_chunks::<2>().0 {
            l += f[0];
            r += f[1];
        }
        out.push(l / n);
        out.push(r / n);
    }
    out
}

/// Raised-cosine fade in and out over `fade` frames (scrub grains start and end without clicks).
pub fn window_grain(buf: &mut [f32], fade: usize) {
    let frames = buf.len() / 2;
    let fade = fade.min(frames / 2).max(1);
    for i in 0..fade {
        let g = 0.5 - 0.5 * (std::f32::consts::PI * i as f32 / fade as f32).cos();
        for c in 0..2 {
            buf[i * 2 + c] *= g;
            buf[(frames - 1 - i) * 2 + c] *= g;
        }
    }
}

/// Where the device was at its last callback.
#[derive(Clone, Copy, Debug)]
struct Stamp {
    wall: Instant,
    /// Real (non-underrun) frames played before this callback.
    before: u64,
    /// ... and after it.
    after: u64,
    /// Seconds from the callback until its first sample is heard.
    delay: f64,
}

struct Ctl {
    active: bool,
    gen: u64,
    /// Timeline seconds of the next block to mix.
    t: f64,
    /// Integer playback speed (1, 2, 4).
    speed: u32,
    snap: Option<Arc<Snapshot>>,
    resampler: Resampler,
    /// A scrub grain to play (timeline seconds), while not playing.
    grain: Option<(f64, Arc<Snapshot>)>,
}

struct Shared {
    ring: Mutex<VecDeque<f32>>,
    ctl: Mutex<Ctl>,
    cv: Condvar,
    stamp: Mutex<Option<Stamp>>,
    played: AtomicU64,
    underruns: AtomicU64,
    volume: AtomicU32,
    active: AtomicBool,
    /// A scrub grain is in the ring (played without counting towards the clock).
    grain_active: AtomicBool,
    stop: AtomicBool,
    rate: AtomicU32,
    mix_errors: Mutex<Option<String>>,
}

impl Shared {
    fn new() -> Arc<Shared> {
        Arc::new(Shared {
            ring: Mutex::new(VecDeque::new()),
            ctl: Mutex::new(Ctl { active: false, gen: 0, t: 0.0, speed: 1, snap: None, resampler: Resampler::new(48_000, 48_000), grain: None }),
            cv: Condvar::new(),
            stamp: Mutex::new(None),
            played: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            volume: AtomicU32::new(1.0f32.to_bits()),
            active: AtomicBool::new(false),
            grain_active: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            rate: AtomicU32::new(0),
            mix_errors: Mutex::new(None),
        })
    }
}

pub struct AudioOut {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    pub device: String,
    pub error: Option<String>,
}

fn project_rate(snap: &Snapshot) -> u32 {
    snap.project.settings.sample_rate.clamp(8000, 192_000)
}

impl AudioOut {
    /// Opens the default output device and starts the mixer thread (idle until [`AudioOut::start`]).
    pub fn new(renderer: Arc<Renderer>, preferred_rate: u32) -> AudioOut {
        let shared = Shared::new();
        let (tx, rx) = std::sync::mpsc::channel::<Result<String, String>>();
        let sh = shared.clone();
        let thread = std::thread::Builder::new()
            .name("aiframes-audio".into())
            .spawn(move || {
                let stream = match open_stream(&sh, preferred_rate) {
                    Ok((s, name)) => {
                        let _ = tx.send(Ok(name));
                        s
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
                mixer_loop(&sh, &renderer);
                drop(stream);
            })
            .ok();
        let (device, error) = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(name)) => (name, None),
            Ok(Err(e)) => (String::new(), Some(e)),
            Err(_) => (String::new(), Some("the audio device did not respond".into())),
        };
        AudioOut { shared, thread, device, error }
    }

    /// No device (tests, `AIFRAMES_NO_AUDIO`): playback runs on the wall clock.
    pub fn disabled() -> AudioOut {
        AudioOut { shared: Shared::new(), thread: None, device: String::new(), error: Some("audio output disabled".into()) }
    }

    /// True when a device is open and can be the master clock.
    pub fn available(&self) -> bool {
        self.error.is_none() && self.shared.rate.load(Ordering::Relaxed) > 0
    }

    pub fn device_rate(&self) -> u32 {
        self.shared.rate.load(Ordering::Relaxed)
    }

    /// Starts playing the project's sound from timeline time `t` at an integer `speed` (1, 2, 4).
    pub fn start(&self, t: f64, snap: Arc<Snapshot>, speed: u32) {
        if !self.available() {
            return;
        }
        let out_rate = self.device_rate();
        {
            let mut c = lock(&self.shared.ctl);
            c.gen += 1;
            c.t = t;
            c.speed = speed.max(1);
            c.resampler = Resampler::new(project_rate(&snap), out_rate);
            c.snap = Some(snap);
            c.grain = None;
            c.active = true;
            self.shared.grain_active.store(false, Ordering::SeqCst);
            lock(&self.shared.ring).clear();
            self.shared.played.store(0, Ordering::SeqCst);
            *lock(&self.shared.stamp) = None;
            self.shared.active.store(true, Ordering::SeqCst);
        }
        self.shared.cv.notify_all();
    }

    /// Stops the sound; the clock position goes back to 0.
    pub fn stop(&self) {
        {
            let mut c = lock(&self.shared.ctl);
            c.active = false;
            c.gen += 1;
            self.shared.active.store(false, Ordering::SeqCst);
            lock(&self.shared.ring).clear();
            self.shared.played.store(0, Ordering::SeqCst);
            *lock(&self.shared.stamp) = None;
        }
        self.shared.cv.notify_all();
    }

    /// Plays a short scrub grain of the mix at timeline time `t` (only while not playing).
    pub fn grain(&self, t: f64, snap: Arc<Snapshot>) {
        if !self.available() {
            return;
        }
        let mut c = lock(&self.shared.ctl);
        if c.active {
            return;
        }
        c.grain = Some((t, snap));
        drop(c);
        self.shared.cv.notify_all();
    }

    /// Swaps the project being mixed without restarting (already queued sound plays out).
    pub fn set_snapshot(&self, snap: Arc<Snapshot>) {
        let mut c = lock(&self.shared.ctl);
        if c.active {
            c.snap = Some(snap);
        }
    }

    pub fn set_volume(&self, v: f32) {
        self.shared.volume.store(v.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    /// Seconds of sound heard since [`AudioOut::start`] (interpolated between device callbacks).
    /// At speed `k` this is `1/k` of the timeline time covered.
    pub fn position(&self) -> f64 {
        let rate = self.device_rate().max(1) as f64;
        let Some(s) = *lock(&self.shared.stamp) else { return 0.0 };
        let pos = s.before as f64 / rate + s.wall.elapsed().as_secs_f64() - s.delay;
        pos.clamp(0.0, s.after as f64 / rate)
    }

    pub fn underruns(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    /// The last mixing error (render crate), if any.
    pub fn mix_error(&self) -> Option<String> {
        lock(&self.shared.mix_errors).clone()
    }
}

impl Drop for AudioOut {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn open_stream(sh: &Arc<Shared>, preferred_rate: u32) -> Result<(cpal::Stream, String), String> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no audio output device")?;
    let name = device.name().unwrap_or_else(|_| "audio device".into());
    let default = device.default_output_config().map_err(|e| format!("audio device {name}: {e}"))?;
    // Prefer the project's rate (no resampling) when the device supports it.
    let mut chosen = default.clone();
    if let Ok(configs) = device.supported_output_configs() {
        for c in configs {
            if c.channels() >= 2 && c.sample_format() == cpal::SampleFormat::F32 && c.min_sample_rate().0 <= preferred_rate && c.max_sample_rate().0 >= preferred_rate {
                chosen = c.with_sample_rate(cpal::SampleRate(preferred_rate));
                break;
            }
        }
    }
    let format = chosen.sample_format();
    let config: cpal::StreamConfig = chosen.config();
    let stream = match format {
        cpal::SampleFormat::F32 => build::<f32>(&device, &config, sh.clone()),
        cpal::SampleFormat::I16 => build::<i16>(&device, &config, sh.clone()),
        cpal::SampleFormat::U16 => build::<u16>(&device, &config, sh.clone()),
        cpal::SampleFormat::I32 => build::<i32>(&device, &config, sh.clone()),
        other => return Err(format!("audio device {name}: unsupported sample format {other:?}")),
    }
    .map_err(|e| format!("audio device {name}: {e}"))?;
    stream.play().map_err(|e| format!("audio device {name}: {e}"))?;
    sh.rate.store(config.sample_rate.0, Ordering::SeqCst);
    Ok((stream, format!("{name} ({} Hz, {} ch)", config.sample_rate.0, config.channels)))
}

fn build<T>(device: &cpal::Device, config: &cpal::StreamConfig, sh: Arc<Shared>) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = config.channels.max(1) as usize;
    device.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let now = Instant::now();
            let ts = info.timestamp();
            let delay = ts.playback.duration_since(&ts.callback).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            let frames = data.len() / channels;
            let silence = T::from_sample(0.0f32);
            let active = sh.active.load(Ordering::Relaxed);
            let grain = sh.grain_active.load(Ordering::Relaxed);
            if !active && !grain {
                data.fill(silence);
                return;
            }
            let vol = f32::from_bits(sh.volume.load(Ordering::Relaxed));
            let mut real = 0u64;
            let mut starved = false;
            {
                let mut ring = lock(&sh.ring);
                for f in 0..frames {
                    let out = &mut data[f * channels..(f + 1) * channels];
                    if ring.len() >= 2 {
                        let l = ring.pop_front().unwrap_or(0.0) * vol;
                        let r = ring.pop_front().unwrap_or(0.0) * vol;
                        if channels == 1 {
                            out[0] = T::from_sample((l + r) * 0.5);
                        } else {
                            out[0] = T::from_sample(l);
                            out[1] = T::from_sample(r);
                            for o in out.iter_mut().skip(2) {
                                *o = silence;
                            }
                        }
                        real += 1;
                    } else {
                        out.fill(silence);
                        starved = true;
                    }
                }
            }
            if !active {
                // A scrub grain: no clock; done once the ring is empty.
                if starved {
                    sh.grain_active.store(false, Ordering::Relaxed);
                }
                return;
            }
            if starved && real > 0 {
                sh.underruns.fetch_add(1, Ordering::Relaxed);
            }
            let before = sh.played.fetch_add(real, Ordering::SeqCst);
            if real > 0 || lock(&sh.stamp).is_some() {
                *lock(&sh.stamp) = Some(Stamp { wall: now, before, after: before + real, delay });
            }
            sh.cv.notify_all();
        },
        |e| eprintln!("aiframes: audio stream error: {e}"),
        None,
    )
}

enum Job {
    Block { gen: u64, t: f64, speed: u32, snap: Arc<Snapshot> },
    Grain { t: f64, snap: Arc<Snapshot> },
}

fn mixer_loop(sh: &Arc<Shared>, renderer: &Renderer) {
    let mut mixed: Vec<f32> = Vec::new();
    let mut out: Vec<f32> = Vec::new();
    while !sh.stop.load(Ordering::SeqCst) {
        let out_rate = sh.rate.load(Ordering::Relaxed).max(1);
        let want = (AHEAD * out_rate as f64) as usize * 2;
        let job = {
            let mut c = lock(&sh.ctl);
            loop {
                if sh.stop.load(Ordering::SeqCst) {
                    return;
                }
                if c.active {
                    let queued = lock(&sh.ring).len();
                    if let (Some(snap), true) = (&c.snap, queued < want) {
                        break Job::Block { gen: c.gen, t: c.t, speed: c.speed, snap: snap.clone() };
                    }
                } else if let Some((t, snap)) = c.grain.take() {
                    break Job::Grain { t, snap };
                }
                c = sh.cv.wait_timeout(c, Duration::from_millis(20)).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0);
            }
        };
        match job {
            Job::Block { gen, t, speed, snap } => {
                let rate = project_rate(&snap);
                let frames = BLOCK * speed as usize;
                mixed.clear();
                mixed.resize(frames * 2, 0.0);
                mix_into(renderer, &snap, t, &mut mixed, sh);
                let block = decimate(&mixed, speed as usize);
                let mut c = lock(&sh.ctl);
                if c.gen != gen || !c.active {
                    continue;
                }
                out.clear();
                c.resampler.process(&block, &mut out);
                c.t = t + frames as f64 / rate as f64;
                lock(&sh.ring).extend(out.iter().copied());
            }
            Job::Grain { t, snap } => {
                let rate = project_rate(&snap);
                let frames = (GRAIN * rate as f64) as usize;
                mixed.clear();
                mixed.resize(frames * 2, 0.0);
                mix_into(renderer, &snap, t, &mut mixed, sh);
                window_grain(&mut mixed, (GRAIN_FADE * rate as f64) as usize);
                out.clear();
                Resampler::new(rate, out_rate).process(&mixed, &mut out);
                let c = lock(&sh.ctl);
                if c.active || c.grain.is_some() {
                    continue; // playback started, or a newer grain is waiting
                }
                let mut ring = lock(&sh.ring);
                ring.clear();
                ring.extend(out.iter().copied());
                sh.grain_active.store(true, Ordering::SeqCst);
            }
        }
    }
}

fn mix_into(renderer: &Renderer, snap: &Snapshot, t: f64, buf: &mut [f32], sh: &Shared) {
    if let Err(e) = renderer.mix(&snap.dir, &snap.project, &snap.resolved, t, buf) {
        buf.fill(0.0);
        *lock(&sh.mix_errors) = Some(e.to_string());
    }
    for s in buf.iter_mut() {
        if !s.is_finite() {
            *s = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_passes_through() {
        let mut r = Resampler::new(48_000, 48_000);
        let mut out = Vec::new();
        r.process(&[0.1, 0.2, 0.3, 0.4], &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn downsample_halves_and_upsample_doubles() {
        let input: Vec<f32> = (0..2000).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let mut r = Resampler::new(48_000, 24_000);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert!((out.len() as i64 / 2 - 1000).abs() <= 1, "{}", out.len());
        let mut r = Resampler::new(24_000, 48_000);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert!((out.len() as i64 / 2 - 4000).abs() <= 2, "{}", out.len());
    }

    #[test]
    fn continuous_across_blocks() {
        // A ramp resampled in two blocks equals the ramp resampled at once.
        let ramp: Vec<f32> = (0..400).flat_map(|i| [i as f32, i as f32]).collect();
        let mut whole = Vec::new();
        Resampler::new(44_100, 48_000).process(&ramp, &mut whole);
        let mut parts = Vec::new();
        let mut r = Resampler::new(44_100, 48_000);
        r.process(&ramp[..300], &mut parts);
        r.process(&ramp[300..], &mut parts);
        assert_eq!(whole.len(), parts.len());
        for (a, b) in whole.iter().zip(&parts) {
            assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        }
        for w in parts.chunks(2).collect::<Vec<_>>().windows(2) {
            assert!(w[1][0] >= w[0][0]);
        }
    }

    #[test]
    fn decimation_averages_and_kills_nyquist() {
        let v: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        assert_eq!(decimate(&v, 2), vec![2.0, 3.0, 6.0, 7.0]);
        assert_eq!(decimate(&v, 1), v);
        // A tone at the old Nyquist (alternating ±1) would alias to DC/hash when skipping samples;
        // the box filter removes it.
        let alt: Vec<f32> = (0..64).flat_map(|i| { let s = if i % 2 == 0 { 1.0 } else { -1.0 }; [s, s] }).collect();
        assert!(decimate(&alt, 2).iter().all(|s| s.abs() < 1e-6));
        assert_eq!(decimate(&alt, 4).len(), 32);
    }

    #[test]
    fn grains_fade_in_and_out() {
        let mut g = vec![1.0f32; 200 * 2];
        window_grain(&mut g, 20);
        assert_eq!(g[0], 0.0);
        assert_eq!(g[g.len() - 1], 0.0);
        assert!((g[100 * 2] - 1.0).abs() < 1e-6, "the middle is untouched");
        assert!(g[10 * 2] > 0.3 && g[10 * 2] < 0.7);
    }
}
