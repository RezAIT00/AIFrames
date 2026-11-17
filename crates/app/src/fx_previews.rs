//! (Added) Pictures for the effects browser ([`FxPreview`]) and its live hover preview in the viewer.
//!
//! * **Thumbnails** show what a click would make ([`preview_plan`]) on the selected picture clip at the
//!   playhead, or on a sample scene when no picture clip is selected (or, for transitions, when the
//!   selected clip has no cut). They are rendered by the one renderer on a scratch copy of the project
//!   ([`aiframes_mcp::fx_preview`]); the document is never touched, so nothing enters the history.
//! * **Never in the way of playback or the UI.** One background thread renders, inside its own
//!   two-thread rayon pool (the renderer's inner loops would otherwise share the global pool with the
//!   playback workers). Nothing renders while the viewer plays or the playhead is scrubbed: the worker
//!   waits and resumes on pause (a lower OS priority was tried instead and caused priority inversion:
//!   a starved thumbnail thread holding a shared renderer/media lock stalled playback for seconds on
//!   4 cores). The UI thread only looks up textures.
//! * **Debounced base.** Thumbnails follow the document revision, the selection and the playhead once
//!   they have been still for a moment (not while playing or scrubbing); old pictures stay up until
//!   their replacements are ready, so nothing flickers. Only items the browser draws are rendered.
//! * **Hover previews** ([`HoverCtl`]): after a short dwell the hovered item's ops go to the app's
//!   transient op preview (the program viewer shows a scratch copy; nothing enters the history).

use crate::playback::Snapshot;
use aiframes_core::Id;
use aiframes_mcp::fx_preview::{self as fxp, Scratch};
use aiframes_render::Renderer;
use aiframes_ui::effects_browser::{anim_time, preview_plan, still_time, BrowserDrag, FxPreview, PreviewPlan, ANIM_FRAMES, PRESET_SETTLE};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Thumbnail width in pixels (tiles are about 100–150 points wide; this stays sharp at 150–200 % scaling).
pub const THUMB_W: u32 = 256;
/// How long the playhead/selection must rest before thumbnails follow it.
const SETTLE: f64 = 0.35;
/// Threads of the thumbnail render pool.
const POOL_THREADS: usize = 2;
/// Most queued thumbnail requests.
const MAX_QUEUE: usize = 512;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the thumbnails are rendered from.
struct Base {
    id: u64,
    /// The document as last saved into a snapshot (project, resolved spans, folder).
    doc: Arc<Snapshot>,
    selection: Vec<Id>,
    playhead: f64,
    /// True when a picture clip is selected (thumbnails show it); else the sample scene.
    on_clip: bool,
}

impl Base {
    fn key(&self) -> BaseKey {
        BaseKey::new(&self.doc, &self.selection, self.playhead, self.on_clip)
    }
}

/// The sample scene for one frame size.
struct Sample {
    size: (u32, u32, u64),
    scratch: Arc<Scratch>,
    dir: PathBuf,
}

#[derive(Clone, PartialEq)]
struct BaseKey {
    doc: (u64, u64, u64),
    selection: Vec<Id>,
    /// The playhead's frame (only when a clip is selected: the sample does not depend on it).
    frame: Option<i64>,
}

impl BaseKey {
    fn new(doc: &Snapshot, selection: &[Id], playhead: f64, on_clip: bool) -> BaseKey {
        BaseKey {
            doc: doc.key(),
            selection: if on_clip { selection.to_vec() } else { Vec::new() },
            frame: on_clip.then(|| crate::timecode::frame_index(playhead, doc.fps())),
        }
    }
}

type ThumbKey = (BrowserDrag, u32);

struct Tex {
    tex: TextureHandle,
    base: u64,
}

#[derive(Default)]
struct State {
    base: Option<Arc<Base>>,
    next_base: u64,
    thumbs: HashMap<ThumbKey, Tex>,
    queue: VecDeque<ThumbKey>,
    pending: HashSet<ThumbKey>,
    /// The viewer plays or the playhead is scrubbed: no thumbnail renders until it stops.
    busy: bool,
    stop: bool,
    /// Thumbnails rendered, their total render ms, and failures (perf log).
    rendered: u64,
    render_ms: f64,
    failed: u64,
    last_error: Option<String>,
}

struct Inner {
    renderer: Arc<Renderer>,
    ctx: egui::Context,
    st: Mutex<State>,
    cv: Condvar,
    sample: Mutex<Option<Arc<Sample>>>,
}

pub struct FxPreviews {
    inner: Arc<Inner>,
    /// The base the app asked for and since when it has been the same (debounce).
    wanted: Option<(BaseKey, f64)>,
}

impl FxPreviews {
    pub fn new(ctx: egui::Context, renderer: Arc<Renderer>) -> FxPreviews {
        let inner = Arc::new(Inner { renderer, ctx, st: Mutex::new(State::default()), cv: Condvar::new(), sample: Mutex::new(None) });
        let i = inner.clone();
        let _ = std::thread::Builder::new().name("aiframes-fx-thumbs".into()).spawn(move || worker(i));
        FxPreviews { inner, wanted: None }
    }

    /// Per UI frame while the effects browser is visible: what the thumbnails should show. `doc` is
    /// the document's snapshot (not a live preview); `playing`/`scrubbing` pause rendering and hold the
    /// base still; `hold` only holds it (the hover preview moved the playhead).
    #[allow(clippy::too_many_arguments)]
    pub fn set_base(&mut self, doc: &Arc<Snapshot>, selection: &[Id], playhead: f64, playing: bool, scrubbing: bool, hold: bool, now: f64) {
        let on_clip = preview_plan(&BrowserDrag::Effect("color".into()), &doc.project, &doc.resolved, selection, playhead).is_some();
        let key = BaseKey::new(doc, selection, playhead, on_clip);
        if self.wanted.as_ref().map(|(k, _)| k) != Some(&key) {
            self.wanted = Some((key.clone(), now));
        }
        let since = self.wanted.as_ref().map(|(_, t)| *t).unwrap_or(now);
        let mut st = lock(&self.inner.st);
        let busy = playing || scrubbing;
        if st.busy != busy {
            st.busy = busy;
            if !busy {
                self.inner.cv.notify_all();
            }
        }
        if st.base.as_ref().map(|b| b.key()).as_ref() == Some(&key) {
            return;
        }
        // The first base at once; later ones once things rest (never while playing or scrubbing: the
        // pictures would chase the playhead).
        let settled = !busy && !hold && now - since >= SETTLE;
        if st.base.is_some() && !settled {
            if !busy && !hold {
                self.inner.ctx.request_repaint_after(Duration::from_secs_f64(SETTLE));
            }
            return;
        }
        st.next_base += 1;
        let id = st.next_base;
        st.base = Some(Arc::new(Base { id, doc: doc.clone(), selection: selection.to_vec(), playhead, on_clip }));
        // Requests for the old base are moot; the browser asks again for what it shows. Stills stay up
        // until their replacements arrive; old animation frames (asked for again on hover) go.
        st.queue.clear();
        st.pending.clear();
        st.thumbs.retain(|(_, frame), _| *frame == 0);
        drop(st);
        self.inner.cv.notify_all();
    }

    /// (thumbnails rendered, average ms, queued, failed) for the perf log.
    pub fn stats(&self) -> (u64, f64, usize, u64) {
        let st = lock(&self.inner.st);
        (st.rendered, if st.rendered > 0 { st.render_ms / st.rendered as f64 } else { 0.0 }, st.queue.len(), st.failed)
    }
}

impl Drop for FxPreviews {
    fn drop(&mut self) {
        lock(&self.inner.st).stop = true;
        self.inner.cv.notify_all();
    }
}

impl FxPreview for FxPreviews {
    fn thumb(&self, item: &BrowserDrag, frame: u32) -> Option<egui::TextureId> {
        let frame = frame.min(ANIM_FRAMES);
        let mut st = lock(&self.inner.st);
        let base = st.base.as_ref()?.id;
        let key = (item.clone(), frame);
        let (have, stale) = match st.thumbs.get(&key) {
            Some(t) => (Some(t.tex.id()), t.base != base),
            None => (None, true),
        };
        if stale && !st.pending.contains(&key) && st.queue.len() < MAX_QUEUE {
            st.pending.insert(key.clone());
            // Animation frames are wanted now (the item is hovered); stills in reading order.
            if frame > 0 {
                st.queue.push_front(key);
            } else {
                st.queue.push_back(key);
            }
            drop(st);
            self.inner.cv.notify_one();
        }
        have
    }

    fn source(&self) -> Option<String> {
        let st = lock(&self.inner.st);
        let b = st.base.as_ref()?;
        if !b.on_clip {
            return Some("a sample picture — select a clip to preview on it".into());
        }
        let names: Vec<&str> = b.doc.project.clips.iter().filter(|c| b.selection.contains(&c.id)).map(|c| if c.name.is_empty() { c.id.as_str() } else { c.name.as_str() }).take(2).collect();
        let more = if b.selection.len() > 2 { " …" } else { "" };
        Some(format!("{}{more} at {}", names.join(", "), aiframes_ui::util::timecode(b.playhead, b.doc.fps())))
    }
}

impl Inner {
    /// The sample scene for the document's frame size (its pictures are written to the cache once).
    fn sample(&self, doc: &Snapshot) -> Result<Arc<Sample>, String> {
        let s = &doc.project.settings;
        let size = (s.width, s.height, (s.fps * 1000.0).round() as u64);
        if let Some(x) = lock(&self.sample).as_ref().filter(|x| x.size == size) {
            return Ok(x.clone());
        }
        let cache = self.renderer.pool().cache_dir().to_path_buf();
        let p = fxp::sample_project(&cache, s.width, s.height, s.fps)?;
        let x = Arc::new(Sample { size, scratch: Arc::new(fxp::scratch(&p, &[])?), dir: cache });
        *lock(&self.sample) = Some(x.clone());
        Ok(x)
    }

    /// The picture of `item` (animation `frame`) on `base`: on the document when a click there would
    /// do something, else on the sample scene.
    fn render_thumb(&self, base: &Base, item: &BrowserDrag, frame: u32) -> Result<ColorImage, String> {
        let on_doc = if base.on_clip { preview_plan(item, &base.doc.project, &base.doc.resolved, &base.selection, base.playhead) } else { None };
        let img = match on_doc {
            Some(plan) => fxp::render(&self.renderer, &base.doc.dir, &fxp::scratch(&base.doc.project, &plan.ops)?, at_of(&plan, frame), THUMB_W)?,
            None => {
                let s = self.sample(&base.doc)?;
                let sel = vec![fxp::SAMPLE_A.to_string()];
                let plan = preview_plan(item, &s.scratch.project, &s.scratch.resolved, &sel, fxp::SAMPLE_AT).ok_or_else(|| format!("{} cannot be shown on the sample", item.label()))?;
                fxp::render(&self.renderer, &s.dir, &fxp::scratch(&s.scratch.project, &plan.ops)?, at_of(&plan, frame), THUMB_W)?
            }
        };
        Ok(ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.rgba))
    }
}

/// When to render: the plan's still, or a frame of a transition's animation.
fn at_of(plan: &PreviewPlan, frame: u32) -> f64 {
    match (frame, plan.span) {
        (1.., Some(span)) => anim_time(span, frame),
        _ => still_time(plan),
    }
}

fn worker(inner: Arc<Inner>) {
    let pool = rayon::ThreadPoolBuilder::new().num_threads(POOL_THREADS).thread_name(|n| format!("aiframes-fx-render-{n}")).build().ok();
    loop {
        let (base, key) = {
            let mut st = lock(&inner.st);
            loop {
                if st.stop {
                    return;
                }
                if !st.busy {
                    if let (Some(base), Some(k)) = (st.base.clone(), st.queue.pop_front()) {
                        break (base, k);
                    }
                }
                st = inner.cv.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        };
        let t0 = Instant::now();
        let job = || inner.render_thumb(&base, &key.0, key.1);
        let result = match &pool {
            Some(p) => p.install(job),
            None => job(),
        };
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mut st = lock(&inner.st);
        st.pending.remove(&key);
        match result {
            Ok(img) => {
                st.rendered += 1;
                st.render_ms += ms;
                let name = format!("fx-{:?}-{}", key.0, key.1);
                let tex = inner.ctx.load_texture(name, img, TextureOptions::LINEAR);
                st.thumbs.insert(key, Tex { tex, base: base.id });
            }
            Err(e) => {
                // Loud in the log; the item keeps its icon. It stays marked as pending for this base
                // (no retry loop); a new base retries it.
                st.failed += 1;
                if st.last_error.as_deref() != Some(e.as_str()) {
                    eprintln!("aiframes: effect thumbnail {:?}: {e}", key.0);
                    st.last_error = Some(e);
                }
                st.pending.insert(key);
            }
        }
        drop(st);
        inner.ctx.request_repaint();
    }
}

// ---------------------------------------------------------------------------------------------
// Hover preview in the program viewer
// ---------------------------------------------------------------------------------------------

/// Pointer rest on an item before the viewer previews it (moving across the list does not thrash).
const DWELL: f64 = 0.15;
/// Steps per second of a transition looping in the viewer while hovered, and steps held at its end.
const LOOP_RATE: f64 = 12.0;
const LOOP_HOLD: u64 = 5;
/// Most steps one pass of the loop takes (long transitions are sampled more coarsely).
const LOOP_STEPS: u64 = 18;

/// Drives the live preview of the hovered browser item in the program viewer. After a short dwell it
/// returns, every frame, the ops of what a click would do; the app shows them through its transient
/// op preview (a scratch copy of the document on the playback engine; nothing enters the history) and
/// restores the document's picture when they stop. When the result is not visible at the playhead (a
/// transition on another cut, an effect on a clip elsewhere, a title still drawing on) the playhead
/// moves there for the hover and comes back afterwards. A clicked item is not previewed again until
/// the pointer leaves it (it is in the document now; previewing it would show it twice).
#[derive(Default)]
pub struct HoverCtl {
    item: Option<BrowserDrag>,
    since: f64,
    /// The plan for (item, document snapshot key, selection), kept while they stay the same.
    plan: Option<(BrowserDrag, (u64, u64, u64), Vec<Id>, Option<PreviewPlan>)>,
    /// (document, playhead before the preview moved it, where it moved it).
    restore: Option<(u64, f64, f64)>,
    /// The playhead was checked for this plan (and since when a transition loops in the viewer).
    placed: bool,
    loop_start: f64,
    loop_step: Option<u64>,
    suppressed: Option<BrowserDrag>,
    /// Text for the viewer overlay while previewing (or why nothing previews).
    pub note: Option<String>,
    /// True while ops are being previewed.
    pub previewing: bool,
}

impl HoverCtl {
    /// True while the hover preview has moved the playhead (thumbnails do not follow it then).
    pub fn holds_playhead(&self) -> bool {
        self.restore.is_some()
    }

    /// Per UI frame. `hovered` is the browser's hovered item (`None` when the browser is not shown),
    /// `clicked` whether the browser emitted an action this frame. Returns the ops to preview now.
    #[allow(clippy::too_many_arguments)]
    pub fn step(&mut self, ctx: &egui::Context, engine: &mut crate::playback::Engine, doc: &Arc<Snapshot>, selection: &[Id], hovered: Option<BrowserDrag>, clicked: bool, now: f64) -> Option<Vec<aiframes_core::Op>> {
        if clicked {
            self.suppressed = hovered.clone();
        }
        if self.suppressed.is_some() && self.suppressed != hovered {
            self.suppressed = None;
        }
        let want = hovered.filter(|h| Some(h) != self.suppressed.as_ref());
        if want != self.item || clicked {
            self.end(engine, doc);
            self.item = want;
            self.since = now;
        }
        let item = self.item.clone()?;
        if now - self.since < DWELL {
            ctx.request_repaint_after(Duration::from_secs_f64(DWELL));
            return None;
        }
        let fresh = !matches!(&self.plan, Some((i, k, s, _)) if *i == item && *k == doc.key() && s.as_slice() == selection);
        if fresh {
            // A title or shape is placed so that it has finished drawing itself on at the playhead.
            let t = engine.time();
            let playhead = match item {
                BrowserDrag::Text(_) | BrowserDrag::Shape(_) => (t - PRESET_SETTLE).max(0.0),
                _ => t,
            };
            let plan = preview_plan(&item, &doc.project, &doc.resolved, selection, playhead);
            self.plan = Some((item.clone(), doc.key(), selection.to_vec(), plan));
            self.placed = false;
        }
        let Some(plan) = self.plan.as_ref().and_then(|(.., p)| p.clone()) else {
            self.previewing = false;
            self.note = Some(match &item {
                BrowserDrag::Effect(_) => format!("Select a picture clip to preview {} on it", item.label()),
                BrowserDrag::Transition(_) => format!("Select a clip next to a cut (or a transition) to preview {}", item.label()),
                _ => format!("{} cannot be previewed here", item.label()),
            });
            return None;
        };
        // Show it where it can be seen: inside the clip, after a title's reveal; a transition loops
        // through its frames (paused only: playing, the person watches it go by).
        if !engine.playing() {
            let t = engine.time();
            let back = self.restore.map(|(_, b, _)| b).unwrap_or(t);
            match plan.span {
                Some((a, b)) => {
                    if !self.placed {
                        self.placed = true;
                        self.loop_start = now;
                        self.loop_step = None;
                    }
                    let frames = (((b - a) * doc.fps()).round() as u64).clamp(2, LOOP_STEPS);
                    let k = ((now - self.loop_start).max(0.0) * LOOP_RATE) as u64 % (frames + LOOP_HOLD);
                    let k = k.min(frames - 1);
                    if self.loop_step != Some(k) {
                        self.loop_step = Some(k);
                        engine.seek(a + (b - a) * (k as f64 + 0.5) / frames as f64, false);
                        self.restore = Some((doc.doc, back, engine.time()));
                    }
                    ctx.request_repaint_after(Duration::from_secs_f64(0.5 / LOOP_RATE));
                }
                None if !self.placed => {
                    self.placed = true;
                    if (t - plan.at).abs() >= 0.5 / doc.fps() {
                        engine.seek(plan.at, false);
                        self.restore = Some((doc.doc, back, engine.time()));
                    }
                }
                None => {}
            }
        }
        self.previewing = true;
        self.note = Some(format!("Preview: {} — click to apply", item.label()));
        Some(plan.ops)
    }

    /// Back to the document's playhead (the app restores the picture when no preview ops come).
    fn end(&mut self, engine: &mut crate::playback::Engine, doc: &Arc<Snapshot>) {
        if let Some((d, before, moved)) = self.restore.take() {
            // Only when the person has not moved the playhead since.
            if d == doc.doc && !engine.playing() && (engine.time() - moved).abs() < 1e-6 {
                engine.seek(before, false);
            }
        }
        self.plan = None;
        self.placed = false;
        self.loop_step = None;
        self.previewing = false;
        self.note = None;
    }
}

/// The hover preview's note over the top-left of the viewer ("Preview: Glow — click to apply").
pub fn draw_note(ctx: &egui::Context, viewer: egui::Rect, note: &str, previewing: bool) {
    if !viewer.is_positive() {
        return;
    }
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("aiframes_fx_hover_note")));
    let font = egui::FontId::proportional(12.5);
    let g = painter.layout_no_wrap(note.to_string(), font, egui::Color32::WHITE);
    let r = egui::Rect::from_min_size(viewer.left_top() + egui::vec2(14.0, 30.0), g.size() + egui::vec2(16.0, 8.0));
    let fill = if previewing { egui::Color32::from_rgba_unmultiplied(0x4C, 0xA3, 0xFF, 230) } else { egui::Color32::from_rgba_unmultiplied(0x30, 0x30, 0x36, 235) };
    painter.rect_filled(r, egui::CornerRadius::same(4), fill);
    painter.galley(r.min + egui::vec2(8.0, 4.0), g, egui::Color32::WHITE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiframes_core::{resolve, Project};

    fn wait_for(mut done: impl FnMut() -> bool, secs: u64) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(secs) {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        done()
    }

    #[test]
    fn thumbnails_render_in_the_background_and_wait_while_playing() {
        let dir = std::env::temp_dir().join(format!("aiframes-fxthumbs-{}", std::process::id()));
        let rd = Arc::new(Renderer::new(aiframes_media::MediaPool::with_cache_dir(64 << 20, &dir)));
        let ctx = egui::Context::default();
        let mut fx = FxPreviews::new(ctx, rd);
        let project: Project = serde_json::from_value(serde_json::json!({
            "format": 1, "settings": {"width": 1280, "height": 720, "fps": 30},
            "tracks": [{"id": "v1", "kind": "video"}, {"id": "v2", "kind": "video"}],
            "clips": [
                {"id": "c1", "track": "v1", "start": 0, "duration": 4, "source": {"type": "solid", "color": "#2060A0"}},
                {"id": "c2", "track": "v1", "start": 4, "duration": 4, "source": {"type": "solid", "color": "#C04020"}},
                {"id": "t", "track": "v2", "start": 0, "duration": 8, "source": {"type": "text", "text": "Hi", "size": 120}}
            ]
        }))
        .unwrap();
        let resolved = resolve(&project);
        let snap = Arc::new(Snapshot { doc: 1, revision: 1, epoch: 0, project, resolved, dir: dir.clone() });
        let items = BrowserDrag::all();
        let sel = vec!["c1".to_string()];

        // Playing: the browser's requests queue up, nothing renders.
        fx.set_base(&snap, &sel, 1.0, true, false, false, 0.0);
        assert!(items.iter().all(|it| fx.thumb(it, 0).is_none()));
        std::thread::sleep(Duration::from_millis(400));
        let (n, _, queued, _) = fx.stats();
        assert_eq!(n, 0, "nothing renders while the viewer plays");
        assert_eq!(queued, items.len());

        // Paused: every item renders in the background, from the selected clip (on the document).
        fx.set_base(&snap, &sel, 1.0, false, false, false, 1.0);
        assert!(wait_for(|| fx.stats().0 + fx.stats().3 >= items.len() as u64, 120), "{:?}", fx.stats());
        let (n, avg_ms, _, failed) = fx.stats();
        assert_eq!((n, failed), (items.len() as u64, 0));
        assert!(fx.source().unwrap().starts_with("c1 at "), "{:?}", fx.source());

        // What the UI thread pays per frame: lookups only.
        let t0 = Instant::now();
        for _ in 0..10 {
            assert!(items.iter().all(|it| fx.thumb(it, 0).is_some()));
        }
        let per_frame_us = t0.elapsed().as_secs_f64() * 1e6 / 10.0;
        assert!(per_frame_us < 5000.0, "{per_frame_us} µs per frame for {} lookups", items.len());
        eprintln!("{} thumbnails, {avg_ms:.1} ms each in the background; UI lookups {per_frame_us:.0} µs per frame", items.len());

        // Nothing selected: after the selection has rested a moment, the sample scene (written once to
        // the cache folder); the old pictures stay up meanwhile.
        fx.set_base(&snap, &[], 1.0, false, false, false, 10.0);
        assert!(fx.source().unwrap().starts_with("c1"), "not yet: the base is debounced");
        fx.set_base(&snap, &[], 1.0, false, false, false, 10.0 + SETTLE + 0.01);
        assert!(fx.source().unwrap().contains("sample"));
        assert!(items.iter().all(|it| fx.thumb(it, 0).is_some()), "stale pictures stay up while the new ones render");
        assert!(wait_for(|| fx.stats().0 + fx.stats().3 >= 2 * items.len() as u64, 120), "{:?}", fx.stats());
        assert_eq!(fx.stats().3, 0, "no failures on the sample");
        assert!(dir.join("fx-sample").is_dir());
        drop(fx);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
