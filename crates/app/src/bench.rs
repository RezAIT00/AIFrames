//! `aiframes --bench-playback <project.aifr> [options]`: measures the program viewer's playback
//! engine (the same [`Engine`] the window uses: render workers, media pool, clock) driven by a
//! simulated 60 Hz UI, and prints one JSON line per run.
//!
//! Tests (`--tests play,seek,scrub,step`):
//! * **play** / **playcold**: Play from `--from` (cold: 3 s later, right after seeking there) for `--secs`: frames dropped (in range, never shown), time to the
//!   first moving frame, longest hold, how far the picture lags the clock, frames shown out of order.
//! * **seek**: random seeks while paused (a click on the ruler): time to a picture and to the full frame.
//! * **scrub**: the playhead dragged back and forth at 1-4x: how often the picture is the frame under
//!   the playhead, how stale it is, and the time to the full frame after release.
//! * **step**: frame steps forward and backward: time until each stepped frame is on screen.
//! * **loop**: loop playback of a 2 s range: frames shown and longest hold after the first wrap.
//! * **jumpback**: while playing, a click 3 s earlier on the ruler: time until the picture is there.
//!
//! CPU is measured for this process and its ffmpeg children (a Windows job object), so other
//! programs on the machine do not count. `--cores N` limits the whole run (children included) to N
//! logical processors (`--core-mask` picks them). Every picture shown during play is compared with
//! an independent render of the same frame (a fresh media pool and renderer), so wrong, stale or
//! black frames are counted as glitches.

use crate::playback::{Engine, Snapshot};
use crate::timecode::{frame_index, frame_time};
use aiframes_core::Document;
use aiframes_media::MediaPool;
use aiframes_render::{FrameOptions, Renderer};
use eframe::egui::{self, ColorImage};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Pictures shown during play: (frame, width) → (hash, thumbnail).
type Shots = BTreeMap<(i64, usize), (u64, Vec<u8>)>;

struct Opts {
    project: PathBuf,
    scale: f32,
    workers: usize,
    cores: u32,
    core_mask: Option<u64>,
    from: f64,
    secs: f64,
    tests: Vec<String>,
    seeks: usize,
    audio: bool,
    hz: f64,
    verify: bool,
    label: String,
    json: Option<PathBuf>,
    cache_mb: usize,
    trace: Option<PathBuf>,
    /// Priority class of the whole run (children included): other programs' load does not steal its CPU.
    priority: String,
    /// Auto quality (the engine may lower the scale while frames drop).
    auto: bool,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        project: PathBuf::new(),
        scale: 0.75,
        workers: 3,
        cores: 0,
        core_mask: None,
        from: 1.0,
        secs: 8.0,
        tests: ["play", "seek", "scrub", "step"].map(String::from).to_vec(),
        seeks: 12,
        audio: false,
        hz: 60.0,
        verify: true,
        label: String::new(),
        json: None,
        cache_mb: 512,
        trace: None,
        priority: "above".into(),
        auto: false,
    };
    let mut it = args.iter();
    let num = |v: Option<&String>, name: &str| -> Result<f64, String> { v.and_then(|s| s.parse::<f64>().ok()).ok_or_else(|| format!("{name} needs a number")) };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--scale" => o.scale = num(it.next(), a)? as f32,
            "--workers" => o.workers = num(it.next(), a)? as usize,
            "--cores" => o.cores = num(it.next(), a)? as u32,
            "--core-mask" => {
                let v = it.next().ok_or("--core-mask needs a hex mask")?;
                o.core_mask = Some(u64::from_str_radix(v.trim_start_matches("0x"), 16).map_err(|e| e.to_string())?);
            }
            "--from" => o.from = num(it.next(), a)?,
            "--secs" => o.secs = num(it.next(), a)?,
            "--seeks" => o.seeks = num(it.next(), a)? as usize,
            "--hz" => o.hz = num(it.next(), a)?,
            "--cache-mb" => o.cache_mb = num(it.next(), a)? as usize,
            "--tests" => o.tests = it.next().ok_or("--tests needs a list")?.split(',').map(|s| s.trim().to_string()).collect(),
            "--audio" => o.audio = true,
            "--auto" => o.auto = true,
            "--no-verify" => o.verify = false,
            "--label" => o.label = it.next().cloned().unwrap_or_default(),
            "--json" => o.json = it.next().map(PathBuf::from),
            "--trace" => o.trace = it.next().map(PathBuf::from),
            "--priority" => o.priority = it.next().cloned().unwrap_or_default(),
            s if !s.starts_with("--") && o.project.as_os_str().is_empty() => o.project = PathBuf::from(s),
            s => return Err(format!("unknown option {s}")),
        }
    }
    if o.project.as_os_str().is_empty() {
        return Err("usage: aiframes --bench-playback <project.aifr> [--scale 0.75] [--workers 3] [--cores N | --core-mask HEX] [--from T] [--secs N] [--tests play,seek,scrub,step] [--seeks N] [--audio] [--no-verify] [--label L] [--json out.jsonl] [--trace play.csv]".into());
    }
    Ok(o)
}

pub fn run(args: &[String]) -> i32 {
    let o = match parse(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let mask = o.core_mask.or_else(|| (o.cores > 0).then(|| (1u64 << o.cores.min(63)) - 1));
    if let Some(m) = mask {
        // Before any thread pool starts: rayon sizes itself like it would on a machine with this many cores.
        std::env::set_var("RAYON_NUM_THREADS", m.count_ones().to_string());
    }
    let job = cpu::Job::new(mask, &o.priority);
    let doc = match Document::open(&o.project) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot open {}: {e}", o.project.display());
            return 1;
        }
    };
    let project = doc.project.clone();
    let resolved = doc.resolved().clone();
    let dir = doc.project_dir();
    let snap = Arc::new(Snapshot { doc: 1, revision: 0, epoch: 0, project, resolved, dir });
    let cache_dir = std::env::temp_dir().join(format!("aiframes-bench-{}", std::process::id()));
    let pool = MediaPool::with_cache_dir(o.cache_mb << 20, cache_dir.join("a"));
    let renderer = Arc::new(Renderer::new(pool.clone()));
    let ctx = egui::Context::default();
    let mut engine = Engine::with_audio(ctx, renderer, o.workers, snap.clone(), o.audio);
    if o.auto {
        engine.set_quality(o.scale, true);
    } else {
        engine.set_scale(o.scale);
    }
    if o.audio {
        engine.set_volume(0.0, true);
    }
    let mut out = json!({
        "label": o.label,
        "project": o.project.file_name().map(|n| n.to_string_lossy().to_string()),
        "scale": o.scale,
        "workers": o.workers,
        "cores": mask.map(|m| m.count_ones()).unwrap_or(0),
        "fps": snap.fps(),
        "priority": o.priority,
        "auto": o.auto,
    });
    let mut b = Bench { e: &mut engine, snap: snap.clone(), hz: o.hz, job: &job, pool: pool.clone() };
    // Opening the project shows its first frame.
    b.settle(0.0, 5.0);
    let mut shown_frames: Shots = BTreeMap::new();
    for t in &o.tests {
        let r = match t.as_str() {
            "play" => b.play(o.from, o.secs, true, o.verify.then_some(&mut shown_frames), o.trace.as_deref()),
            "playcold" => b.play(o.from + 3.0, o.secs, false, None, o.trace.as_ref().map(|p| p.with_extension("cold.csv")).as_deref()),
            "seek" => b.seek(o.seeks),
            "scrub" => b.scrub(o.from),
            "step" => b.step(o.from),
            "loop" => b.looping(o.from),
            "jumpback" => b.jumpback(o.from),
            "profile" => profile(&snap, o.scale, o.from, &cache_dir.join("p"), &job),
            "decode" => decode_paths(&snap, o.scale, &cache_dir.join("d"), &job),
            other => {
                eprintln!("unknown test {other}");
                continue;
            }
        };
        out[t.as_str()] = r;
    }
    drop(b);
    drop(engine);
    if o.verify && !shown_frames.is_empty() {
        out["play"]["glitches"] = verify(&snap, &shown_frames, &cache_dir.join("b"));
    }
    let _ = std::fs::remove_dir_all(&cache_dir);
    let line = out.to_string();
    println!("{line}");
    if let Some(p) = &o.json {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{line}");
        }
    }
    0
}

struct Bench<'a> {
    e: &'a mut Engine,
    snap: Arc<Snapshot>,
    hz: f64,
    job: &'a cpu::Job,
    pool: Arc<MediaPool>,
}

/// Percentile of a sorted copy (`p` in 0..=1).
fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    s[((s.len() - 1) as f64 * p).round() as usize]
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        f64::NAN
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

fn r1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// A 64-bit hash and a 96×54 RGB thumbnail of a picture.
fn fingerprint(img: &ColorImage) -> (u64, Vec<u8>) {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    img.size.hash(&mut h);
    img.as_raw().hash(&mut h);
    let (w, hh) = (img.size[0], img.size[1]);
    let (tw, th) = (96usize, 54usize);
    let mut thumb = vec![0u8; tw * th * 3];
    for ty in 0..th {
        for tx in 0..tw {
            let (x0, x1) = (tx * w / tw, ((tx + 1) * w / tw).max(tx * w / tw + 1));
            let (y0, y1) = (ty * hh / th, ((ty + 1) * hh / th).max(ty * hh / th + 1));
            let mut acc = [0u64; 3];
            let mut n = 0u64;
            for y in y0..y1.min(hh) {
                for x in x0..x1.min(w) {
                    let p = img.pixels[y * w + x];
                    acc[0] += p.r() as u64;
                    acc[1] += p.g() as u64;
                    acc[2] += p.b() as u64;
                    n += 1;
                }
            }
            for c in 0..3 {
                thumb[(ty * tw + tx) * 3 + c] = (acc[c] / n.max(1)) as u8;
            }
        }
    }
    (h.finish(), thumb)
}

fn mad(a: &[u8], b: &[u8]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).abs()).sum::<f64>() / a.len().max(1) as f64
}

impl Bench<'_> {
    fn fps(&self) -> f64 {
        self.snap.fps()
    }

    /// Ticks the UI at `hz` until `done` or `timeout` seconds; returns the seconds waited and
    /// whether `done` was reached.
    fn tick_until(&mut self, timeout: f64, mut done: impl FnMut(&mut Engine) -> bool) -> (f64, bool) {
        let start = Instant::now();
        let period = Duration::from_secs_f64(1.0 / self.hz);
        let mut next = start;
        loop {
            self.e.update();
            if done(self.e) {
                return (start.elapsed().as_secs_f64(), true);
            }
            if start.elapsed().as_secs_f64() > timeout {
                return (start.elapsed().as_secs_f64(), false);
            }
            next += period;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
    }

    /// Paused at `t`: waits for the full frame and the pre-roll after it to finish.
    fn settle(&mut self, t: f64, timeout: f64) {
        self.e.seek(t, false);
        let f = frame_index(t, self.fps());
        self.tick_until(timeout, |e| e.displayed_frame() == Some((f, false)));
        self.tick_until(1.0, |_| false);
    }

    /// `settled`: paused at `from` until the frame and its pre-roll are ready, then Play (Space);
    /// otherwise Play right after the seek (a click on the ruler and Space at once, `play from`).
    fn play(&mut self, from: f64, secs: f64, settled: bool, mut shots: Option<&mut Shots>, trace: Option<&std::path::Path>) -> Value {
        let fps = self.fps();
        let dur = self.snap.duration();
        let from = from.min((dur - secs.min(dur) - 0.1).max(0.0));
        if settled {
            self.settle(from, 10.0);
        } else {
            self.e.seek(from, false);
        }
        let start_frame = frame_index(from, fps);
        let pool0 = self.pool.stats();
        let cpu0 = self.job.cpu_seconds();
        let wall0 = Instant::now();
        self.e.reset_stats();
        self.e.play(from, 1.0, None);
        let mut shown: HashSet<i64> = HashSet::new();
        // The picture on screen is from the playhead's range (a cold start still shows the old position).
        let in_play = |i: i64, cf: i64| i >= start_frame && i <= cf.max(start_frame);
        let mut last: Option<i64> = None;
        let mut last_change = 0.0f64;
        let mut first_frame: Option<f64> = None;
        let mut first_motion: Option<f64> = None;
        let mut holds: Vec<f64> = Vec::new();
        let mut lags: Vec<f64> = Vec::new();
        let mut backwards = 0u64;
        let mut drafts = 0u64;
        let mut end_frame = start_frame;
        let mut rows: Vec<String> = Vec::new();
        let (_, _) = self.tick_until(secs + 0.05, |e| {
            let w = wall0.elapsed().as_secs_f64();
            let t = e.time();
            let cf = frame_index(t, fps);
            end_frame = end_frame.max(cf);
            let d = e.displayed_frame();
            if trace.is_some() {
                rows.push(format!("{w:.4},{t:.4},{cf},{},{}", d.map(|x| x.0).unwrap_or(-1), d.map(|x| x.1 as u8).unwrap_or(0)));
            }
            match d {
                Some((i, draft)) if in_play(i, cf) => {
                    if draft {
                        drafts += 1;
                    }
                    if first_frame.is_none() {
                        first_frame = Some(w);
                    }
                    if last != Some(i) {
                        if let Some(l) = last {
                            if i < l {
                                backwards += 1;
                            }
                            if first_motion.is_some() {
                                holds.push(w - last_change);
                            }
                        }
                        if first_motion.is_none() && i > start_frame {
                            first_motion = Some(w);
                        }
                        last = Some(i);
                        last_change = w;
                        if !draft {
                            shown.insert(i);
                            if let (Some(s), Some(img)) = (shots.as_deref_mut(), e.shown_image()) {
                                s.entry((i, img.size[0])).or_insert_with(|| fingerprint(&img));
                            }
                        }
                    }
                    lags.push(((cf - i) as f64 / fps * 1000.0).max(0.0));
                }
                // Nothing yet, or a picture from before the seek: as stale as the whole wait.
                _ => lags.push(w * 1000.0),
            }
            w >= secs
        });
        let wall = wall0.elapsed().as_secs_f64();
        let cpu = self.job.cpu_seconds() - cpu0;
        let pool1 = self.pool.stats();
        let engine_dropped = self.e.stats.dropped;
        let end_scale = self.e.stats.scale;
        self.e.pause();
        if let Some(p) = trace {
            let _ = std::fs::write(p, format!("wall,clock,clock_frame,shown,draft\n{}\n", rows.join("\n")));
        }
        let expected = (end_frame - start_frame + 1).max(1);
        let in_range = shown.iter().filter(|i| **i >= start_frame && **i <= end_frame).count() as i64;
        let dropped = expected - in_range;
        // Holds longer than 2.5 frames after the picture started moving (visible stutters).
        let stutters = holds.iter().filter(|h| **h > 2.5 / fps).count();
        json!({
            "secs": r1(wall),
            "expected": expected,
            "shown": in_range,
            "dropped": dropped,
            "dropped_pct": r1(dropped as f64 * 100.0 / expected as f64),
            "first_frame_ms": first_frame.map(|w| r1(w * 1000.0)),
            "first_motion_ms": first_motion.map(|w| r1(w * 1000.0)),
            "max_hold_ms": r1(holds.iter().cloned().fold(0.0, f64::max) * 1000.0),
            "stutters": stutters,
            "lag_ms_mean": r1(mean(&lags)),
            "lag_ms_p95": r1(pct(&lags, 0.95)),
            "backwards": backwards,
            "draft_ticks": drafts,
            "engine_dropped": engine_dropped,
            "end_scale": end_scale,
            "cpu_cores": r1(cpu / wall.max(1e-9)),
            "decoder_seeks": pool1.seeks - pool0.seeks,
            "frames_decoded": pool1.frames_decoded - pool0.frames_decoded,
        })
    }

    fn looping(&mut self, from: f64) -> Value {
        let fps = self.fps();
        let dur = self.snap.duration();
        let from = from.min((dur - 2.5).max(0.0));
        let (lo, hi) = (from, (from + 2.0).min(dur));
        self.settle(from, 10.0);
        self.e.looping = true;
        self.e.play(from, 1.0, Some((lo, hi)));
        let wall0 = Instant::now();
        let mut shown_after: HashSet<i64> = HashSet::new();
        let mut last: Option<i64> = None;
        let mut last_change = 0.0;
        let mut max_hold = 0.0f64;
        let cpu0 = self.job.cpu_seconds();
        self.tick_until(6.5, |e| {
            let w = wall0.elapsed().as_secs_f64();
            if let Some((i, false)) = e.displayed_frame() {
                if last != Some(i) {
                    if w > 2.5 {
                        max_hold = max_hold.max(w - last_change);
                    }
                    last = Some(i);
                    last_change = w;
                }
                if w > 2.5 {
                    shown_after.insert(i);
                }
            }
            false
        });
        let w = wall0.elapsed().as_secs_f64();
        max_hold = max_hold.max(w - last_change);
        let cpu = self.job.cpu_seconds() - cpu0;
        self.e.looping = false;
        self.e.pause();
        let range_frames = (frame_index(hi, fps) - frame_index(lo, fps)).max(1);
        json!({
            "range_frames": range_frames,
            "distinct_after_wrap": shown_after.len(),
            "max_hold_ms": r1(max_hold * 1000.0),
            "cpu_cores": r1(cpu / w.max(1e-9)),
        })
    }

    fn jumpback(&mut self, from: f64) -> Value {
        let fps = self.fps();
        let dur = self.snap.duration();
        let from = from.min((dur - 6.0).max(0.0));
        self.settle(from + 4.0, 10.0);
        self.e.play(from + 4.0, 1.0, None);
        self.tick_until(1.5, |_| false);
        let target = frame_index(from, fps);
        let w0 = Instant::now();
        // A click on the ruler while playing (`App::seek` → `Engine::seek`).
        self.e.seek(from, false);
        let mut first: Option<f64> = None;
        let mut shown: HashSet<i64> = HashSet::new();
        let mut end = target;
        self.tick_until(3.0, |e| {
            let w = w0.elapsed().as_secs_f64();
            let cf = frame_index(e.time(), fps);
            end = end.max(cf);
            if let Some((i, false)) = e.displayed_frame() {
                if i >= target && i <= cf {
                    first.get_or_insert(w);
                    shown.insert(i);
                }
            }
            false
        });
        self.e.pause();
        let expected = (end - target + 1).max(1);
        json!({
            "first_frame_ms": first.map(|w| r1(w * 1000.0)),
            "expected": expected,
            "dropped_pct": r1((expected - shown.len() as i64) as f64 * 100.0 / expected as f64),
        })
    }

    fn seek(&mut self, n: usize) -> Value {
        let fps = self.fps();
        let dur = self.snap.duration();
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        let mut first: Vec<f64> = Vec::new();
        let mut full: Vec<f64> = Vec::new();
        let mut timeouts = 0;
        let cpu0 = self.job.cpu_seconds();
        let wall0 = Instant::now();
        for _ in 0..n {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let t = (rng % 10_000) as f64 / 10_000.0 * (dur - 0.2).max(0.0);
            let f = frame_index(t, fps);
            let t = frame_time(f, fps);
            let w0 = Instant::now();
            self.e.seek(t, false);
            let mut got_first: Option<f64> = None;
            let (_, ok) = self.tick_until(8.0, |e| {
                let d = e.displayed_frame();
                if got_first.is_none() && d.is_some_and(|x| x.0 == f) {
                    got_first = Some(w0.elapsed().as_secs_f64());
                }
                d == Some((f, false))
            });
            if ok {
                full.push(w0.elapsed().as_secs_f64() * 1000.0);
                first.push(got_first.unwrap_or(0.0) * 1000.0);
            } else {
                timeouts += 1;
            }
            // A person looks at the frame before clicking again.
            self.tick_until(0.3, |_| false);
        }
        let wall = wall0.elapsed().as_secs_f64();
        json!({
            "n": n,
            "first_ms_median": r1(pct(&first, 0.5)),
            "full_ms_median": r1(pct(&full, 0.5)),
            "full_ms_p90": r1(pct(&full, 0.9)),
            "full_ms_max": r1(pct(&full, 1.0)),
            "timeouts": timeouts,
            "cpu_cores": r1((self.job.cpu_seconds() - cpu0) / wall.max(1e-9)),
        })
    }

    fn scrub(&mut self, from: f64) -> Value {
        let fps = self.fps();
        let dur = self.snap.duration();
        let from = from.min((dur - 6.5).max(0.0));
        self.settle(from, 10.0);
        // (wall seconds, timeline seconds moved): forward 2x, back 1.5x, fast forward 4x, slow 0.5x.
        let path = [(1.5, 3.0), (1.0, -1.5), (1.0, 4.0), (1.0, 0.5)];
        let total: f64 = path.iter().map(|p| p.0).sum();
        let target = |w: f64| -> f64 {
            let mut t = from;
            let mut acc = 0.0;
            for (len, d) in path {
                if w <= acc + len {
                    return (t + d * (w - acc) / len).clamp(0.0, dur);
                }
                acc += len;
                t += d;
            }
            t.clamp(0.0, dur)
        };
        let cpu0 = self.job.cpu_seconds();
        let wall0 = Instant::now();
        let mut exact = 0u64;
        let mut ticks = 0u64;
        let mut off_frames: Vec<f64> = Vec::new();
        let mut ages: Vec<f64> = Vec::new();
        // When each frame index was last asked for (wall seconds).
        let mut asked: BTreeMap<i64, f64> = BTreeMap::new();
        let mut distinct = 0u64;
        let mut last_shown: Option<i64> = None;
        let mut last_target = -1i64;
        self.e.set_scrubbing(true);
        let period = Duration::from_secs_f64(1.0 / self.hz);
        let mut next = Instant::now();
        loop {
            let w = wall0.elapsed().as_secs_f64();
            if w > total {
                break;
            }
            let t = target(w);
            let f = frame_index(t, fps);
            if f != last_target {
                self.e.seek(frame_time(f, fps), true);
                last_target = f;
            }
            asked.insert(f, w);
            self.e.update();
            ticks += 1;
            if let Some((i, _)) = self.e.displayed_frame() {
                if i == f {
                    exact += 1;
                }
                off_frames.push((i - f).abs() as f64);
                ages.push(asked.get(&i).map(|a| (w - a) * 1000.0).unwrap_or(1000.0));
                if last_shown != Some(i) {
                    distinct += 1;
                    last_shown = Some(i);
                }
            }
            next += period;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
        let wall = wall0.elapsed().as_secs_f64();
        let cpu = self.job.cpu_seconds() - cpu0;
        // Release: the full frame under the playhead.
        let f = last_target;
        self.e.set_scrubbing(false);
        self.e.seek(frame_time(f, fps), false);
        let (settle_s, ok) = self.tick_until(5.0, |e| e.displayed_frame() == Some((f, false)));
        json!({
            "secs": r1(wall),
            "exact_pct": r1(exact as f64 * 100.0 / ticks.max(1) as f64),
            "off_frames_mean": r1(mean(&off_frames)),
            "age_ms_mean": r1(mean(&ages)),
            "age_ms_p95": r1(pct(&ages, 0.95)),
            "updates_per_s": r1(distinct as f64 / wall.max(1e-9)),
            "release_full_ms": if ok { Some(r1(settle_s * 1000.0)) } else { None },
            "cpu_cores": r1(cpu / wall.max(1e-9)),
        })
    }

    fn step(&mut self, from: f64) -> Value {
        let fps = self.fps();
        let dur = self.snap.duration();
        let from = from.clamp(0.5, (dur - 1.0).max(0.5));
        self.settle(from, 10.0);
        let mut res = serde_json::Map::new();
        for (name, dir) in [("forward", 1i64), ("backward", -1i64)] {
            let mut f = frame_index(from, fps);
            let mut lat: Vec<f64> = Vec::new();
            let mut timeouts = 0;
            for _ in 0..12 {
                f += dir;
                let w0 = Instant::now();
                self.e.seek(frame_time(f, fps), false);
                let target = f;
                let (_, ok) = self.tick_until(5.0, |e| e.displayed_frame() == Some((target, false)));
                if ok {
                    lat.push(w0.elapsed().as_secs_f64() * 1000.0);
                } else {
                    timeouts += 1;
                }
                // Tapping an arrow key.
                self.tick_until(0.15, |_| false);
            }
            res.insert(name.into(), json!({"ms_median": r1(pct(&lat, 0.5)), "ms_max": r1(pct(&lat, 1.0)), "timeouts": timeouts}));
        }
        Value::Object(res)
    }
}

/// Where the time of one rendered frame goes, one frame at a time on one thread: decoding (the
/// first clip's media at the size the compositor asks for), compositing (decode cached), and the
/// conversion to the viewer's image.
fn profile(snap: &Snapshot, scale: f32, from: f64, cache: &std::path::Path, job: &cpu::Job) -> Value {
    let pool = MediaPool::with_cache_dir(2048 << 20, cache);
    let r = Renderer::new(pool.clone());
    let fps = snap.fps();
    let o = FrameOptions { scale, guides: false, draft: false };
    let n = 60i64;
    let f0 = frame_index(from, fps);
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
    // Warm-up: opens the decoder at `from` (the seek is not what is measured here).
    let _ = r.frame(&snap.dir, &snap.project, &snap.resolved, frame_time(f0, fps), &o);
    let cpu0 = job.cpu_seconds();
    let t0 = Instant::now();
    for k in 1..=n {
        let _ = r.frame(&snap.dir, &snap.project, &snap.resolved, frame_time(f0 + k, fps), &o);
    }
    let first_pass = ms(t0) / n as f64;
    let cpu1 = job.cpu_seconds();
    // Second pass: decoded frames come from the cache (compositing only).
    let t1 = Instant::now();
    let mut last = None;
    for k in 1..=n {
        last = r.frame(&snap.dir, &snap.project, &snap.resolved, frame_time(f0 + k, fps), &o).ok();
    }
    let compose = ms(t1) / n as f64;
    let cpu2 = job.cpu_seconds();
    let t2 = Instant::now();
    if let Some(img) = &last {
        for _ in 0..10 {
            let _ = crate::playback::to_color_image(img);
        }
    }
    let convert = ms(t2) / 10.0;
    let st = pool.stats();
    json!({
        "frames": n,
        "render_ms": r1(first_pass),
        "compose_ms": r1(compose),
        "decode_ms": r1(first_pass - compose),
        "to_color_image_ms": r1(convert),
        "cpu_per_frame_ms": r1((cpu1 - cpu0) * 1000.0 / n as f64),
        "cpu_compose_per_frame_ms": r1((cpu2 - cpu1) * 1000.0 / n as f64),
        "size": last.map(|i| format!("{}x{}", i.width, i.height)),
        "cache_hits": st.cache_hits,
        "seeks": st.seeks,
    })
}

/// Seeks and sequential frames through each decoding path (CPU, D3D11VA, CUDA): frames rendered
/// at the preview scale with a fresh pool per path.
fn decode_paths(snap: &Snapshot, scale: f32, cache: &std::path::Path, job: &cpu::Job) -> Value {
    use aiframes_media::HwAccel;
    let fps = snap.fps();
    let dur = snap.duration();
    let o = FrameOptions { scale, guides: false, draft: false };
    let mut res = serde_json::Map::new();
    for (name, hw) in [("cpu", HwAccel::Off), ("d3d11va", HwAccel::D3d11va), ("cuda", HwAccel::Cuda)] {
        let pool = MediaPool::with_cache_dir(512 << 20, cache.join(name));
        pool.set_hwaccel(hw);
        let r = Renderer::new(pool.clone());
        let mut seeks: Vec<f64> = Vec::new();
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let mut err: Option<String> = None;
        for _ in 0..6 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let t = frame_time(frame_index((rng % 10_000) as f64 / 10_000.0 * (dur - 4.0).max(0.0), fps), fps);
            let t0 = Instant::now();
            if let Err(e) = r.frame(&snap.dir, &snap.project, &snap.resolved, t, &o) {
                err = Some(e.to_string());
            }
            seeks.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        let start = frame_index(1.0f64.min(dur), fps);
        let _ = r.frame(&snap.dir, &snap.project, &snap.resolved, frame_time(start, fps), &o);
        let cpu0 = job.cpu_seconds();
        let t0 = Instant::now();
        let n = 90;
        for k in 1..=n {
            let _ = r.frame(&snap.dir, &snap.project, &snap.resolved, frame_time(start + k, fps), &o);
        }
        let wall = t0.elapsed().as_secs_f64();
        let cpu = job.cpu_seconds() - cpu0;
        res.insert(
            name.into(),
            json!({
                "seek_ms_median": r1(pct(&seeks, 0.5)),
                "seek_ms_max": r1(pct(&seeks, 1.0)),
                "seq_fps": r1(n as f64 / wall),
                "seq_cpu_ms_per_frame": r1(cpu * 1000.0 / n as f64),
                "error": err,
            }),
        );
    }
    Value::Object(res)
}

/// Renders every frame shown during play again with a fresh pool and renderer and compares.
fn verify(snap: &Snapshot, shown: &Shots, cache: &std::path::Path) -> Value {
    let pool = MediaPool::with_cache_dir(256 << 20, cache);
    let r = Renderer::new(pool);
    let fps = snap.fps();
    let mut exact = 0;
    let mut wrong: Vec<Value> = Vec::new();
    let mut black = 0;
    for ((i, width), (hash, thumb)) in shown {
        // The scale the frame was shown at (Auto quality may have lowered it).
        let scale = *width as f32 / snap.project.settings.width.max(1) as f32;
        let o = FrameOptions { scale, guides: false, draft: false };
        let Ok(img) = r.frame(&snap.dir, &snap.project, &snap.resolved, frame_time(*i, fps), &o) else { continue };
        let Some(ci) = crate::playback::to_color_image(&img) else { continue };
        let (h2, t2) = fingerprint(&ci);
        if h2 == *hash {
            exact += 1;
            continue;
        }
        let d = mad(thumb, &t2);
        let luma = |t: &[u8]| t.iter().map(|v| *v as f64).sum::<f64>() / t.len().max(1) as f64;
        if luma(thumb) < 4.0 && luma(&t2) > 16.0 {
            black += 1;
        }
        if d > 1.0 && wrong.len() < 20 {
            wrong.push(json!({"frame": i, "mad": r1(d)}));
        }
    }
    json!({"checked": shown.len(), "identical": exact, "black": black, "differs": wrong.len(), "examples": wrong})
}

/// CPU time of this process and its children, and an optional affinity limit for both.
mod cpu {
    #[cfg(windows)]
    pub struct Job(isize);

    #[cfg(windows)]
    #[repr(C)]
    #[derive(Default)]
    struct Accounting {
        total_user: i64,
        total_kernel: i64,
        period_user: i64,
        period_kernel: i64,
        page_faults: u32,
        total_processes: u32,
        active_processes: u32,
        terminated_processes: u32,
    }

    #[cfg(windows)]
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attrs: *mut std::ffi::c_void, name: *const u16) -> isize;
        fn AssignProcessToJobObject(job: isize, process: isize) -> i32;
        fn GetCurrentProcess() -> isize;
        fn QueryInformationJobObject(job: isize, class: i32, info: *mut std::ffi::c_void, len: u32, ret: *mut u32) -> i32;
        fn SetProcessAffinityMask(process: isize, mask: usize) -> i32;
        fn GetProcessTimes(process: isize, creation: *mut i64, exit: *mut i64, kernel: *mut i64, user: *mut i64) -> i32;
        fn SetInformationJobObject(job: isize, class: i32, info: *mut std::ffi::c_void, len: u32) -> i32;
    }

    #[cfg(windows)]
    #[repr(C)]
    #[derive(Default)]
    struct BasicLimits {
        per_process_user_time: i64,
        per_job_user_time: i64,
        limit_flags: u32,
        min_working_set: usize,
        max_working_set: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[cfg(windows)]
    impl Job {
        /// `priority`: "normal", "above" or "high" for this process and every child.
        pub fn new(mask: Option<u64>, priority: &str) -> Job {
            const LIMIT_AFFINITY: u32 = 0x10;
            const LIMIT_PRIORITY_CLASS: u32 = 0x20;
            let class = match priority {
                "high" => 0x80,
                "above" => 0x8000,
                _ => 0,
            };
            // SAFETY: documented Win32 calls on this process's pseudo handle; failures leave the
            // process unlimited / unaccounted (reported as the process's own CPU time).
            unsafe {
                if let Some(m) = mask {
                    SetProcessAffinityMask(GetCurrentProcess(), m as usize);
                }
                let j = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if j != 0 {
                    let mut l = BasicLimits::default();
                    if let Some(m) = mask {
                        l.limit_flags |= LIMIT_AFFINITY;
                        l.affinity = m as usize;
                    }
                    if class != 0 {
                        l.limit_flags |= LIMIT_PRIORITY_CLASS;
                        l.priority_class = class;
                    }
                    if l.limit_flags != 0 && SetInformationJobObject(j, 2, &mut l as *mut _ as *mut _, std::mem::size_of::<BasicLimits>() as u32) == 0 {
                        eprintln!("bench: could not limit the job (affinity/priority)");
                    }
                    if AssignProcessToJobObject(j, GetCurrentProcess()) != 0 {
                        return Job(j);
                    }
                }
            }
            Job(0)
        }

        /// User + kernel seconds of this process and every child it started so far.
        pub fn cpu_seconds(&self) -> f64 {
            // SAFETY: as above; `a`, `c`, `e`, `k`, `u` are ours.
            unsafe {
                if self.0 != 0 {
                    let mut a = Accounting::default();
                    if QueryInformationJobObject(self.0, 1, &mut a as *mut _ as *mut _, std::mem::size_of::<Accounting>() as u32, std::ptr::null_mut()) != 0 {
                        return (a.total_user + a.total_kernel) as f64 / 1e7;
                    }
                }
                let (mut c, mut e, mut k, mut u) = (0i64, 0i64, 0i64, 0i64);
                GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
                (k + u) as f64 / 1e7
            }
        }
    }

    #[cfg(not(windows))]
    pub struct Job;

    #[cfg(not(windows))]
    impl Job {
        pub fn new(_mask: Option<u64>, _priority: &str) -> Job {
            Job
        }
        pub fn cpu_seconds(&self) -> f64 {
            f64::NAN
        }
    }
}
