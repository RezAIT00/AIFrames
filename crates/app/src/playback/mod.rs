//! The playback engine behind the program viewer.
//!
//! * Render workers (background threads) render frames with `Renderer::frame` at preview scale:
//!   ahead of the playhead while playing (a small ring keyed by frame index), the frame under the
//!   playhead while paused (a fast draft first when scrubbing, then full quality), and a few frames
//!   of pre-roll after it so Play and frame steps are instant.
//! * Audio (see [`audio`]) is mixed on its own thread; while playing at 1x its device position is the
//!   master clock and video frames are picked by audio time. Frames the renderer could not deliver
//!   in time are skipped and counted as dropped.
//! * Every document change (new revision) swaps the [`Snapshot`] and invalidates buffered frames.
//!
//! Nothing here renders or decodes on the UI thread: `update` only picks a finished frame and
//! uploads it to a texture.

pub mod audio;
pub mod clock;
pub mod prefetch;
pub mod schedule;

use aiframes_core::{Project, Resolved};
use aiframes_render::{FrameOptions, Renderer};
use clock::{Bound, Clock, Shuttle};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::timecode::{frame_index, frame_time, safe_fps};

/// Frames rendered ahead of the playhead while playing (in steps).
const LOOKAHEAD: i64 = 10;
/// Frames pre-rendered after a paused playhead.
const PREROLL: i64 = 6;
/// Upper bound on buffered frame memory.
const FRAME_BUDGET: usize = 192 << 20;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// An immutable copy of what is rendered: swapped whenever the document changes.
pub struct Snapshot {
    pub doc: u64,
    pub revision: u64,
    /// Bumped for changes that do not create a revision (recovered autosave, live drag previews).
    pub epoch: u64,
    pub project: Project,
    pub resolved: Resolved,
    pub dir: PathBuf,
}

impl Snapshot {
    pub fn fps(&self) -> f64 {
        safe_fps(self.project.settings.fps)
    }
    pub fn duration(&self) -> f64 {
        self.resolved.duration.max(0.0)
    }
    pub fn key(&self) -> (u64, u64, u64) {
        (self.doc, self.revision, self.epoch)
    }
}

/// Preview resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Quality {
    #[default]
    Auto,
    Full,
    Half,
    Quarter,
}

impl Quality {
    pub const ALL: [Quality; 4] = [Quality::Auto, Quality::Full, Quality::Half, Quality::Quarter];
    pub fn label(self) -> &'static str {
        match self {
            Quality::Auto => "Auto",
            Quality::Full => "Full",
            Quality::Half => "1/2",
            Quality::Quarter => "1/4",
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            Quality::Auto => "auto",
            Quality::Full => "full",
            Quality::Half => "half",
            Quality::Quarter => "quarter",
        }
    }
    pub fn from_id(s: &str) -> Quality {
        match s {
            "full" => Quality::Full,
            "half" => Quality::Half,
            "quarter" => Quality::Quarter,
            _ => Quality::Auto,
        }
    }
    /// Render scale for a viewer showing the frame `display_px` physical pixels wide.
    pub fn scale(self, display_px: f32, project_w: u32) -> f32 {
        match self {
            Quality::Auto => schedule::auto_scale(display_px, project_w),
            Quality::Full => 1.0,
            Quality::Half => 0.5,
            Quality::Quarter => 0.25,
        }
    }
}

struct Job {
    index: i64,
    draft: bool,
    gen: u64,
    scale: f32,
    snap: Arc<Snapshot>,
}

struct State {
    snap: Arc<Snapshot>,
    gen: u64,
    scale: f32,
    playing: bool,
    step: i64,
    next: i64,
    clock_frame: i64,
    lo: i64,
    hi: i64,
    /// Paused: the frame under the playhead (and whether a draft is acceptable first).
    want: Option<(i64, bool)>,
    frames: BTreeMap<i64, Arc<ColorImage>>,
    draft: Option<(i64, Arc<ColorImage>)>,
    pending: HashSet<(i64, bool)>,
    skipped: u64,
    render_ms: VecDeque<f32>,
    last_error: Option<String>,
    /// Render warnings of the latest finished frame (offline media, missing fonts, ...).
    warnings: Vec<String>,
    stop: bool,
}

impl State {
    fn frame_bytes(&self) -> usize {
        self.frames.values().next().map(|f| f.size[0] * f.size[1] * 4).unwrap_or(1).max(1)
    }

    fn job(&self, index: i64, draft: bool) -> Job {
        Job { index, draft, gen: self.gen, scale: self.scale, snap: self.snap.clone() }
    }

    fn next_job(&mut self) -> Option<Job> {
        // 1. The frame the viewer is waiting for.
        if let Some((i, draft_ok)) = self.want {
            let have = self.frames.contains_key(&i);
            let have_draft = self.draft.as_ref().is_some_and(|(d, _)| *d == i);
            if !have && !self.pending.contains(&(i, false)) {
                if draft_ok && !have_draft && !self.pending.contains(&(i, true)) {
                    self.pending.insert((i, true));
                    return Some(self.job(i, true));
                }
                if !draft_ok || have_draft {
                    self.pending.insert((i, false));
                    return Some(self.job(i, false));
                }
            }
        }
        // 2. Frames ahead of the playhead.
        if self.playing {
            for _ in 0..(LOOKAHEAD * 2) {
                let c = schedule::claim(self.next, self.clock_frame, self.step, LOOKAHEAD, self.lo, self.hi);
                self.skipped += c.skipped;
                self.next = c.next;
                let i = c.index?;
                if self.frames.contains_key(&i) || self.pending.contains(&(i, false)) {
                    continue;
                }
                self.pending.insert((i, false));
                return Some(self.job(i, false));
            }
            return None;
        }
        // 3. Pre-roll after a paused playhead (only once the playhead frame itself is done).
        if self.want.is_some_and(|(i, _)| self.frames.contains_key(&i)) && self.frames.len() * self.frame_bytes() < FRAME_BUDGET / 2 {
            for k in 1..=PREROLL {
                let i = self.clock_frame + k;
                if i > self.hi {
                    break;
                }
                if !self.frames.contains_key(&i) && !self.pending.contains(&(i, false)) {
                    self.pending.insert((i, false));
                    return Some(self.job(i, false));
                }
            }
        }
        None
    }

    fn trim(&mut self) {
        if self.playing {
            schedule::drop_passed(&mut self.frames, self.clock_frame, self.step.signum(), 2);
        } else {
            let (lo, hi) = (self.clock_frame - 12, self.clock_frame + PREROLL + 12);
            self.frames.retain(|k, _| *k >= lo && *k <= hi);
        }
        let max = (FRAME_BUDGET / self.frame_bytes()).max(4);
        while self.frames.len() > max {
            // Drop the frame farthest from the playhead.
            let first = *self.frames.keys().next().expect("non-empty");
            let last = *self.frames.keys().next_back().expect("non-empty");
            let k = if (self.clock_frame - first).abs() > (last - self.clock_frame).abs() { first } else { last };
            self.frames.remove(&k);
        }
    }
}

struct Shared {
    st: Mutex<State>,
    cv: Condvar,
}

/// Playback statistics for the status bar.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Frames that should have been shown but were not (render behind, UI hitch).
    pub dropped: u64,
    /// Average render time of the last frames (ms).
    pub render_ms: f32,
    /// Frames shown per second while playing (measured).
    pub fps: f32,
    pub scale: f32,
    /// An audio device is open.
    pub audio: bool,
    /// The sound is the master clock right now (else the wall clock, with the sound resynced).
    pub audio_master: bool,
    /// Waiting for the first sound before the picture starts moving.
    pub buffering: bool,
    /// Playing at 1x on the wall clock because the sound was late; the sound follows the picture.
    pub audio_resynced: bool,
    /// How long the last Play waited for its first sound to be decoded (ms).
    pub audio_wait_ms: f32,
    /// Scrub grains played so far.
    pub grains: u64,
    pub underruns: u64,
    pub error: Option<String>,
    /// Render warnings for what is on screen (offline media, missing fonts, unknown effects).
    pub warnings: Vec<String>,
}

/// What the viewer shows this UI frame.
/// (added) Seconds the frame at the playhead may be missing before the viewer says it is loading.
const LOADING_AFTER: f64 = 0.3;

pub struct Shown {
    pub texture: Option<egui::TextureId>,
    /// Size of the rendered image in pixels.
    pub size: [usize; 2],
    pub draft: bool,
    /// (added) The frame at the playhead has been on its way for a moment (first decode of a long file,
    /// a seek far into it): the picture shown is an older one or none, and the viewer says so.
    pub loading: bool,
}

pub struct Engine {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
    pub audio: audio::AudioOut,
    audio_on: bool,
    clock: Clock,
    epoch: Instant,
    ctx: egui::Context,
    texture: Option<TextureHandle>,
    /// (gen, index, draft) of the frame in the texture.
    displayed: Option<(u64, i64, bool)>,
    shown_size: [usize; 2],
    /// (added) Wall time since the frame at the playhead has been missing, and whether that has lasted
    /// long enough to say so.
    loading_since: Option<f64>,
    loading: bool,
    /// Last frame index shown while playing (for the dropped-frame count).
    last_played: Option<i64>,
    pub stats: Stats,
    fps_window: VecDeque<f64>,
    /// Playback range and loop.
    pub looping: bool,
    range: Option<(f64, f64)>,
    /// Scrubbing (the playhead is being dragged): ask for drafts first.
    scrubbing: bool,
    /// (audio position, wall time) when the position last advanced.
    last_audio: (f64, f64),
    /// The audio stalled: the wall clock is the master and the sound is resynced to it.
    audio_broken: bool,
    /// Timeline time the sound was (re)started at, and when (wall).
    audio_origin: f64,
    audio_started: f64,
    last_resync: f64,
    pub volume: f32,
    pub muted: bool,
    prefetch: prefetch::Prefetcher,
    /// Playing with sound, waiting until the sound at the playhead is decoded (since this wall time).
    pending_audio: Option<f64>,
    last_prefetch: f64,
    /// Play short grains of sound while scrubbing.
    pub scrub_audio: bool,
    last_grain: (i64, f64),
}

impl Engine {
    pub fn new(ctx: egui::Context, renderer: Arc<Renderer>, workers: usize, snap: Arc<Snapshot>) -> Engine {
        let audio = std::env::var_os("AIFRAMES_NO_AUDIO").is_none();
        Engine::with_audio(ctx, renderer, workers, snap, audio)
    }

    /// `audio = false` plays silently on the wall clock (tests, machines without a device).
    pub fn with_audio(ctx: egui::Context, renderer: Arc<Renderer>, workers: usize, snap: Arc<Snapshot>, audio: bool) -> Engine {
        let rate = snap.project.settings.sample_rate.max(8000);
        let shared = Arc::new(Shared {
            st: Mutex::new(State {
                snap,
                gen: 1,
                scale: 0.5,
                playing: false,
                step: 1,
                next: 0,
                clock_frame: 0,
                lo: 0,
                hi: 0,
                want: Some((0, false)),
                frames: BTreeMap::new(),
                draft: None,
                pending: HashSet::new(),
                skipped: 0,
                render_ms: VecDeque::new(),
                last_error: None,
                warnings: Vec::new(),
                stop: false,
            }),
            cv: Condvar::new(),
        });
        let mut handles = Vec::new();
        for n in 0..workers.max(1) {
            let sh = shared.clone();
            let r = renderer.clone();
            let c = ctx.clone();
            if let Ok(h) = std::thread::Builder::new().name(format!("aiframes-render-{n}")).spawn(move || worker(sh, r, c)) {
                handles.push(h);
            }
        }
        let prefetch = prefetch::Prefetcher::new(renderer.pool().clone());
        let audio = if audio { audio::AudioOut::new(renderer, rate) } else { audio::AudioOut::disabled() };
        let audio_on = audio.available();
        Engine {
            shared,
            workers: handles,
            audio,
            audio_on,
            clock: Clock::new(0.0),
            epoch: Instant::now(),
            ctx,
            texture: None,
            displayed: None,
            loading_since: None,
            loading: false,
            shown_size: [0, 0],
            last_played: None,
            stats: Stats { audio: audio_on, ..Default::default() },
            fps_window: VecDeque::new(),
            looping: false,
            range: None,
            scrubbing: false,
            last_audio: (0.0, 0.0),
            audio_broken: false,
            audio_origin: 0.0,
            audio_started: 0.0,
            last_resync: 0.0,
            volume: 1.0,
            muted: false,
            prefetch,
            pending_audio: None,
            last_prefetch: -10.0,
            scrub_audio: true,
            last_grain: (i64::MIN, 0.0),
        }
    }

    fn wall(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64()
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        lock(&self.shared.st).snap.clone()
    }

    /// Swaps the rendered project (new revision, other document, live preview). Buffered frames
    /// are invalidated; the current picture stays until the new one is ready.
    pub fn set_snapshot(&mut self, snap: Arc<Snapshot>) {
        let fps = snap.fps();
        let dur = snap.duration();
        {
            let mut st = lock(&self.shared.st);
            if Arc::ptr_eq(&st.snap, &snap) {
                return;
            }
            st.snap = snap.clone();
            st.gen += 1;
            st.frames.clear();
            st.draft = None;
            st.warnings.clear();
            st.hi = frame_index(dur, fps).max(0);
            st.next = st.clock_frame;
        }
        if self.clock.playing() && self.audio_on {
            self.audio.set_snapshot(snap);
        }
        self.shared.cv.notify_all();
    }

    pub fn set_scale(&mut self, scale: f32) {
        let mut st = lock(&self.shared.st);
        if (st.scale - scale).abs() > 1e-4 {
            st.scale = scale;
            st.gen += 1;
            st.frames.clear();
            st.draft = None;
            st.next = st.clock_frame;
            drop(st);
            self.shared.cv.notify_all();
        }
    }

    pub fn playing(&self) -> bool {
        self.clock.playing()
    }

    pub fn rate(&self) -> f64 {
        self.clock.rate()
    }

    /// The playhead (timeline seconds) as of the last `update`.
    pub fn time(&mut self) -> f64 {
        let w = self.wall();
        let a = self.audio_pos();
        self.clock.time(w, a)
    }

    fn audio_pos(&self) -> Option<f64> {
        (self.audio_on && !self.audio_broken && clock::audible(self.clock.rate())).then(|| if self.pending_audio.is_some() { 0.0 } else { self.audio.position() })
    }

    /// Plays from `t` at a signed rate (`0` pauses). `range` limits playback (stop or loop at its end).
    pub fn play(&mut self, t: f64, rate: f64, range: Option<(f64, f64)>) {
        if rate == 0.0 {
            self.pause();
            return;
        }
        let snap = self.snapshot();
        let fps = snap.fps();
        let (lo_t, hi_t) = range.unwrap_or((0.0, snap.duration()));
        if hi_t - lo_t <= 0.0 {
            return;
        }
        let mut t = t;
        // Starting at the end plays again from the start (or the reverse).
        if rate > 0.0 && t >= hi_t - 0.5 / fps {
            t = lo_t;
        } else if rate < 0.0 && t <= lo_t + 0.5 / fps {
            t = (hi_t - 1.0 / fps).max(lo_t);
        }
        self.range = range;
        let w = self.wall();
        self.clock.start(t, w, rate);
        self.audio_broken = false;
        self.last_audio = (0.0, w);
        self.last_played = None;
        self.fps_window.clear();
        // The sound starts once what plays first is decoded (progressive media audio): until then
        // the picture holds on the first frame and the viewer says "Buffering audio".
        self.audio.stop();
        self.pending_audio = None;
        if self.audio_on && clock::audible(rate) {
            self.audio.set_volume(if self.muted { 0.0 } else { self.volume });
            self.prefetch.request(prefetch::audio_sources(&snap, t, 1.5), prefetch::mix_rate(&snap));
            self.pending_audio = Some(w);
            self.audio_origin = t;
            self.audio_started = w;
            self.last_prefetch = w;
        }
        {
            let mut st = lock(&self.shared.st);
            st.playing = true;
            st.step = schedule::step_for_rate(rate);
            st.clock_frame = frame_index(t, fps);
            st.next = st.clock_frame;
            st.lo = frame_index(lo_t, fps).max(0);
            st.hi = frame_index(hi_t - 0.5 / fps, fps).max(st.lo);
            st.want = None;
        }
        self.shared.cv.notify_all();
        self.ctx.request_repaint();
    }

    pub fn pause(&mut self) {
        let t = self.time();
        self.pause_at(t);
    }

    fn pause_at(&mut self, t: f64) {
        self.clock.stop(t);
        self.audio.stop();
        self.pending_audio = None;
        self.range = None;
        let snap = self.snapshot();
        let mut st = lock(&self.shared.st);
        st.playing = false;
        st.lo = 0;
        st.hi = frame_index(snap.duration(), snap.fps()).max(0);
        st.clock_frame = frame_index(t, snap.fps());
        st.want = Some((st.clock_frame, false));
        drop(st);
        self.shared.cv.notify_all();
    }

    /// J/K/L/Space at playhead `t`.
    pub fn shuttle(&mut self, t: f64, s: Shuttle, range: Option<(f64, f64)>) {
        let rate = clock::shuttle(self.clock.rate(), s);
        if rate == 0.0 {
            self.pause();
        } else {
            let from = if self.playing() { self.time() } else { t };
            self.play(from, rate, range.or(self.range));
        }
    }

    /// Moves the playhead. While playing, playback continues from there.
    pub fn seek(&mut self, t: f64, scrubbing: bool) {
        let snap = self.snapshot();
        let t = t.clamp(0.0, snap.duration().max(0.0));
        self.scrubbing = scrubbing;
        if self.playing() {
            let rate = self.clock.rate();
            let range = self.range;
            self.play(t, rate, range);
        } else {
            self.pause_at(t);
            let f = {
                let mut st = lock(&self.shared.st);
                st.want = Some((st.clock_frame, scrubbing));
                st.clock_frame
            };
            // Decode the sound here now, so Play (or a scrub grain) finds it.
            self.prefetch.request(prefetch::audio_sources(&snap, t, 1.5), prefetch::mix_rate(&snap));
            let w = self.wall();
            if scrubbing && self.scrub_audio && self.audio_on && f != self.last_grain.0 && w - self.last_grain.1 > 0.045 {
                self.audio.grain(t, snap.clone());
                self.last_grain = (f, w);
                self.stats.grains += 1;
            }
        }
        self.ctx.request_repaint();
    }

    pub fn set_scrubbing(&mut self, s: bool) {
        self.scrubbing = s;
    }

    /// Per UI frame: advances the clock, handles the range end, picks the frame to show and
    /// uploads it. Returns the playhead time.
    pub fn update(&mut self) -> f64 {
        let snap = self.snapshot();
        let fps = snap.fps();
        let wall = self.wall();
        self.stats.buffering = false;
        let speed = clock::audio_speed(self.clock.rate());
        if let (true, Some(k), true) = (self.clock.playing(), speed, self.audio_on) {
            if let Some(since) = self.pending_audio {
                // Waiting for the first sound and the first pictures: start when the next half
                // second of sound is decoded (normally ~0.1 s for a file never played before) and
                // the first frames are rendered (a decoder seeking deep into a long file), so
                // picture and sound start together; after 3 s start regardless.
                let sources = prefetch::audio_sources(&snap, self.audio_origin, 0.0);
                let f0 = frame_index(self.audio_origin, fps);
                let frames_ready = {
                    let st = lock(&self.shared.st);
                    let step = st.step.max(1);
                    (0..2).all(|k| st.frames.contains_key(&(f0 + k * step)) || f0 + k * step > st.hi)
                };
                if (frames_ready && self.prefetch.ready(&sources, 0.5)) || wall - since > 3.0 {
                    self.pending_audio = None;
                    self.stats.audio_wait_ms = ((wall - since) * 1000.0) as f32;
                    self.audio.start(self.audio_origin, snap.clone(), k);
                    self.audio_started = wall;
                    self.last_audio = (0.0, wall);
                } else {
                    self.stats.buffering = wall - since > 0.15;
                }
            } else {
                let pos = self.audio.position();
                let advancing = pos > self.last_audio.0 + 1e-6;
                if advancing {
                    self.last_audio = (pos, wall);
                }
                if !self.audio_broken {
                    // The device stopped consuming sound (unplugged, driver hiccup): keep the
                    // picture going on the wall clock and keep the sound following it.
                    if !advancing && wall - self.last_audio.1 > 1.5 {
                        self.audio_broken = true;
                        self.clock.rebase(wall);
                        self.last_resync = wall;
                    }
                } else if advancing && wall - self.last_resync > 1.0 {
                    let t_clock = self.clock.time(wall, None);
                    let t_audio = self.audio_origin + pos * k as f64;
                    if (t_audio - t_clock).abs() > 0.12 {
                        self.audio.start(t_clock, snap.clone(), k);
                        self.audio_origin = t_clock;
                        self.audio_started = wall;
                        self.last_audio = (0.0, wall);
                        self.last_resync = wall;
                    }
                }
            }
        }
        // Keep the sound of what comes next decoding ahead of the playhead.
        if self.clock.playing() && self.audio_on && wall - self.last_prefetch > 0.5 {
            self.last_prefetch = wall;
            let t_now = self.clock.time(wall, self.audio_pos());
            let ahead = 3.0 * self.clock.rate().abs().max(1.0);
            self.prefetch.request(prefetch::audio_sources(&snap, t_now, ahead), prefetch::mix_rate(&snap));
        }
        let mut t = self.time();
        if self.clock.playing() {
            let (lo, hi) = self.range.unwrap_or((0.0, snap.duration()));
            match clock::bound(t, self.clock.rate(), lo, hi, self.looping, 1.0 / fps) {
                Bound::Continue => {}
                Bound::Stop(at) => {
                    let at = if at >= hi { (hi - 1.0 / fps).max(lo) } else { at };
                    self.pause_at(at);
                    t = at;
                }
                Bound::Wrap(at) => {
                    let rate = self.clock.rate();
                    let range = self.range;
                    self.play(at, rate, range);
                    t = at;
                }
            }
        }
        let f = frame_index(t, fps);
        let playing = self.clock.playing();
        let mut upload: Option<(u64, i64, bool, Arc<ColorImage>)> = None;
        let gen_now;
        {
            let mut st = lock(&self.shared.st);
            gen_now = st.gen;
            let changed = st.clock_frame != f;
            st.clock_frame = f;
            if playing {
                let dir = st.step.signum();
                if let Some(i) = schedule::pick(&st.frames, f, dir) {
                    let newer = match self.displayed {
                        Some((g, d, false)) if g == st.gen => (i - d) * dir > 0,
                        _ => true,
                    };
                    if newer {
                        upload = st.frames.get(&i).map(|img| (st.gen, i, false, img.clone()));
                    }
                }
                if changed {
                    st.trim();
                }
            } else {
                let want = Some((f, self.scrubbing));
                if st.want != want {
                    st.want = want;
                }
                let current = self.displayed.filter(|(g, _, _)| *g == st.gen);
                if let Some(img) = st.frames.get(&f) {
                    if current != Some((st.gen, f, false)) {
                        upload = Some((st.gen, f, false, img.clone()));
                    }
                } else if let Some((d, img)) = &st.draft {
                    if *d == f && current.is_none_or(|(_, i, _)| i != f) {
                        upload = Some((st.gen, *d, true, img.clone()));
                    }
                }
                if changed {
                    st.trim();
                }
            }
            self.stats.render_ms = if st.render_ms.is_empty() { 0.0 } else { st.render_ms.iter().sum::<f32>() / st.render_ms.len() as f32 };
            self.stats.scale = st.scale;
            self.stats.error = st.last_error.clone().or_else(|| self.audio.mix_error());
            if self.stats.warnings != st.warnings {
                self.stats.warnings = st.warnings.clone();
            }
        }
        self.shared.cv.notify_all();
        if let Some((gen, i, draft, img)) = upload {
            if playing && !draft {
                if let Some(prev) = self.last_played {
                    let step = schedule::step_for_rate(self.clock.rate());
                    self.stats.dropped += schedule::dropped_between(prev, i, step);
                }
                self.last_played = Some(i);
                self.fps_window.push_back(wall);
                while self.fps_window.front().is_some_and(|w| wall - w > 1.0) {
                    self.fps_window.pop_front();
                }
                self.stats.fps = self.fps_window.len() as f32;
            }
            self.shown_size = img.size;
            match &mut self.texture {
                Some(tex) => tex.set(img, TextureOptions::LINEAR),
                None => self.texture = Some(self.ctx.load_texture("aiframes-viewer", img, TextureOptions::LINEAR)),
            }
            self.displayed = Some((gen, i, draft));
        }
        if !playing {
            self.fps_window.clear();
            self.stats.fps = 0.0;
        }
        // Paused: the exact frame at the playhead (a draft counts); playing: any frame of this revision.
        let showing = self.displayed.is_some_and(|(g, i, _)| g == gen_now && (playing || i == f));
        if showing {
            self.loading_since = None;
        } else if self.loading_since.is_none() {
            self.loading_since = Some(wall);
        }
        self.loading = self.loading_since.is_some_and(|s| wall - s > LOADING_AFTER);
        if self.loading_since.is_some() {
            self.ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        self.stats.audio = self.audio_on;
        self.stats.audio_master = self.audio_on && !self.audio_broken && clock::audible(self.clock.rate());
        self.stats.audio_resynced = playing && self.audio_on && self.audio_broken && clock::audible(self.clock.rate());
        self.stats.underruns = self.audio.underruns();
        if playing {
            self.ctx.request_repaint();
        }
        t
    }

    pub fn shown(&self) -> Shown {
        Shown { texture: self.texture.as_ref().map(|t| t.id()), size: self.shown_size, draft: self.displayed.is_some_and(|d| d.2), loading: self.loading }
    }

    /// Bytes of decoded preview frames buffered by the engine.
    pub fn buffered_bytes(&self) -> usize {
        let st = lock(&self.shared.st);
        let px = |f: &ColorImage| f.size[0] * f.size[1] * 4;
        st.frames.values().map(|f| px(f)).sum::<usize>() + st.draft.as_ref().map(|(_, f)| px(f)).unwrap_or(0)
    }

    pub fn reset_stats(&mut self) {
        self.stats.dropped = 0;
        lock(&self.shared.st).skipped = 0;
    }

    pub fn set_volume(&mut self, v: f32, muted: bool) {
        self.volume = v;
        self.muted = muted;
        self.audio.set_volume(if muted { 0.0 } else { v });
    }

}

impl Drop for Engine {
    fn drop(&mut self) {
        lock(&self.shared.st).stop = true;
        self.shared.cv.notify_all();
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

pub fn to_color_image(img: &aiframes_render::Image) -> Option<ColorImage> {
    let (w, h) = (img.width as usize, img.height as usize);
    if w == 0 || h == 0 || img.rgba.len() != w * h * 4 {
        return None;
    }
    Some(ColorImage::from_rgba_unmultiplied([w, h], &img.rgba))
}

fn worker(sh: Arc<Shared>, renderer: Arc<Renderer>, ctx: egui::Context) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    loop {
        let job = {
            let mut st = lock(&sh.st);
            loop {
                if st.stop {
                    return;
                }
                if let Some(j) = st.next_job() {
                    break j;
                }
                st = sh.cv.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        };
        let fps = job.snap.fps();
        let t = frame_time(job.index, fps);
        let o = FrameOptions { scale: job.scale, guides: false, draft: job.draft };
        let started = Instant::now();
        let res = renderer.frame_checked(&job.snap.dir, &job.snap.project, &job.snap.resolved, t, &o);
        let ms = started.elapsed().as_secs_f32() * 1000.0;
        let mut warnings = Vec::new();
        let img = match res {
            Ok((img, w)) => {
                warnings = w;
                to_color_image(&img).map(Arc::new).ok_or_else(|| "renderer returned a malformed image".to_string())
            }
            Err(e) => Err(e.to_string()),
        };
        let mut st = lock(&sh.st);
        st.pending.remove(&(job.index, job.draft));
        match img {
            Ok(img) if job.gen == st.gen => {
                if job.draft {
                    st.draft = Some((job.index, img));
                } else {
                    st.frames.insert(job.index, img);
                    if st.draft.as_ref().is_some_and(|(d, _)| *d == job.index) {
                        st.draft = None;
                    }
                }
                st.render_ms.push_back(ms);
                if st.render_ms.len() > 30 {
                    st.render_ms.pop_front();
                }
                st.last_error = None;
                if !job.draft {
                    st.warnings = warnings;
                }
            }
            Ok(_) => {}
            Err(e) => {
                if !WARNED.swap(true, Ordering::Relaxed) {
                    eprintln!("aiframes: render failed at {t:.3}s: {e}");
                }
                st.last_error = Some(e);
                // Do not retry the same frame in a tight loop: store a black placeholder.
                if job.gen == st.gen {
                    let (w, h) = placeholder_size(&job.snap, job.scale);
                    st.frames.insert(job.index, Arc::new(ColorImage::filled([w, h], egui::Color32::BLACK)));
                }
            }
        }
        let paused = !st.playing;
        drop(st);
        sh.cv.notify_all();
        if paused {
            ctx.request_repaint();
        }
    }
}

fn placeholder_size(snap: &Snapshot, scale: f32) -> (usize, usize) {
    let w = ((snap.project.settings.width as f32 * scale).round() as usize).clamp(1, 8192);
    let h = ((snap.project.settings.height as f32 * scale).round() as usize).clamp(1, 8192);
    (w, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn engine() -> Engine {
        let mut p: Project = serde_json::from_value(serde_json::json!({
            "format": 1,
            "settings": {"width": 320, "height": 180, "fps": 30},
            "tracks": [{"id": "v1", "kind": "video"}],
            "clips": [{"id": "c1", "track": "v1", "start": 0.0, "duration": 2.0, "source": {"type": "solid", "color": "#FF0000"}}]
        }))
        .unwrap();
        p.name = "engine test".into();
        let resolved = aiframes_core::resolve(&p);
        let snap = Arc::new(Snapshot { doc: 1, revision: 0, epoch: 0, project: p, resolved, dir: std::env::temp_dir() });
        let pool = aiframes_media::MediaPool::new(64 << 20);
        let renderer = Arc::new(Renderer::new(pool));
        Engine::with_audio(egui::Context::default(), renderer, 2, snap, false)
    }

    fn wait_for(e: &mut Engine, mut ok: impl FnMut(&mut Engine) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            e.update();
            if ok(e) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    #[test]
    fn paused_frames_and_scale() {
        let mut e = engine();
        e.set_scale(0.5);
        e.seek(1.0, false);
        assert!(wait_for(&mut e, |e| e.shown().texture.is_some() && !e.shown().draft), "a frame arrives");
        assert_eq!(e.shown().size, [160, 90]);
        e.set_scale(0.25);
        assert!(wait_for(&mut e, |e| e.shown().size == [80, 45]), "a new scale re-renders");
        // Scrubbing asks for a draft first, then the full frame.
        e.seek(0.5, true);
        assert!(wait_for(&mut e, |e| e.shown().texture.is_some()));
        e.set_scrubbing(false);
        assert!((e.time() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn plays_on_the_wall_clock_and_stops_at_the_end() {
        let mut e = engine();
        e.set_scale(0.25);
        e.play(1.5, 1.0, None);
        assert!(e.playing());
        assert!(wait_for(&mut e, |e| !e.playing()), "stops at the end");
        let t = e.time();
        assert!((1.9..2.0).contains(&t), "pauses on the last frame, not past the end: {t}");
        assert!(e.stats.fps >= 0.0);
        // Looping a range wraps instead of stopping.
        e.looping = true;
        e.play(0.2, 1.0, Some((0.2, 0.4)));
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(500) {
            e.update();
            let t = e.time();
            assert!((0.2..0.4 + 0.05).contains(&t), "stays in the loop range: {t}");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(e.playing());
        e.pause();
        assert!(!e.playing());
    }

    #[test]
    fn shuttle_and_reverse() {
        let mut e = engine();
        e.set_scale(0.25);
        e.shuttle(1.0, Shuttle::Reverse, None);
        assert_eq!(e.rate(), -1.0);
        e.shuttle(1.0, Shuttle::Reverse, None);
        assert_eq!(e.rate(), -2.0);
        assert!(wait_for(&mut e, |e| !e.playing()), "reverse play stops at 0");
        assert!(e.time() <= 0.05);
        e.shuttle(0.0, Shuttle::Forward, None);
        assert_eq!(e.rate(), 1.0);
        e.shuttle(0.0, Shuttle::Stop, None);
        assert!(!e.playing());
    }

    #[test]
    fn new_snapshot_invalidates_frames() {
        let mut e = engine();
        e.set_scale(0.25);
        e.seek(0.5, false);
        assert!(wait_for(&mut e, |e| e.shown().texture.is_some() && !e.shown().draft));
        let mut snap = Snapshot { doc: 1, revision: 1, epoch: 0, project: e.snapshot().project.clone(), resolved: e.snapshot().resolved.clone(), dir: std::env::temp_dir() };
        snap.project.settings.width = 640;
        snap.project.settings.height = 360;
        e.set_snapshot(Arc::new(snap));
        assert!(wait_for(&mut e, |e| e.shown().size == [160, 90]), "the new revision is rendered");
    }
}
