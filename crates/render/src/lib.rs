//! AIFrames renderer: compositor, text and shapes, effects, transitions, audio mixer, export,
//! snapshots and contact sheets (see ARCHITECTURE.md, "Render API").
//!
//! * [`Renderer::frame`] composites the frame at a timeline time (preview, snapshots and export
//!   all use it). Internally colours are premultiplied f32 in display (sRGB) space; output is
//!   straight RGBA8. Media pictures are requested from the [`MediaPool`] at a size suited to the
//!   output (a 4K source in a 0.5 preview decodes at 960×540).
//! * [`Renderer::mix`] mixes sound for any block (stateless, real-time capable).
//! * [`export`] renders, normalises loudness and encodes.
//! * [`Renderer::layers_at`] tells where each clip is drawn (viewer handles, safe-area checks).

// Pixel code: `!(x > 0.0)` deliberately treats NaN as "no"; per-channel index loops and small
// helper signatures read better than the alternatives here.
#![allow(clippy::neg_cmp_op_on_partial_ord, clippy::needless_range_loop, clippy::too_many_arguments, clippy::type_complexity)]

mod audio;
mod blur;
mod canvas;
mod compose;
mod effects;
mod export;
mod geom;
mod lut;
mod shape;
mod sheet;
mod text;
mod timeline;
mod transition;
mod util;

use aiframes_core::{ExportSettings, Id, Project, Resolved, TextSource};
use aiframes_media::{Loudness, MediaError, MediaPool};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use util::{frame_index, frame_time, snap_time, timecode};

/// Straight (non-premultiplied) RGBA8 image.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    pub fn new(width: u32, height: u32) -> Image {
        Image { width, height, rgba: vec![0; width as usize * height as usize * 4] }
    }

    /// The RGBA of pixel (x, y), or `None` outside the image.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y as usize * self.width as usize + x as usize) * 4;
        self.rgba.get(i..i + 4).map(|p| [p[0], p[1], p[2], p[3]])
    }
}

#[derive(Clone, Debug)]
pub struct FrameOptions {
    /// Output size relative to the project size (1 = full resolution, 0.5 = preview).
    pub scale: f32,
    /// Draw title/action-safe guides.
    pub guides: bool,
    /// Faster, lower-quality path for scrubbing (cheaper filters and resampling).
    pub draft: bool,
}

impl Default for FrameOptions {
    fn default() -> Self {
        FrameOptions { scale: 1.0, guides: false, draft: false }
    }
}

#[derive(Clone, Debug)]
pub enum RenderError {
    Media(MediaError),
    /// Something the project refers to does not exist (asset id, file, font).
    Missing(String),
    Encode(String),
    Cancelled,
    Other(String),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::Media(e) => write!(f, "{e}"),
            RenderError::Missing(m) => write!(f, "missing: {m}"),
            RenderError::Encode(m) => write!(f, "encoding failed: {m}"),
            RenderError::Cancelled => write!(f, "cancelled"),
            RenderError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for RenderError {}

impl From<MediaError> for RenderError {
    fn from(e: MediaError) -> Self {
        RenderError::Media(e)
    }
}

/// Renders frames and audio of a project. `Send + Sync`; caches fonts, glyphs, text layouts,
/// rendered titles/shapes, LUTs and ducking envelopes.
pub struct Renderer {
    pool: Arc<MediaPool>,
    layouts: Mutex<util::Lru<Arc<text::TextLayout>>>,
    vectors: Mutex<util::Lru<Arc<canvas::Layer>>>,
    /// Text and shapes rendered in their own box space (3-D cards, motion-blur samples).
    pictures: Mutex<util::Lru<canvas::Picture>>,
    luts: Mutex<HashMap<PathBuf, (std::time::SystemTime, u64, Arc<lut::Lut>)>>,
    duck: audio::DuckCache,
}

impl Renderer {
    pub fn new(pool: Arc<MediaPool>) -> Self {
        Renderer::with_vector_budget(pool, 384 << 20)
    }

    /// A renderer whose cache of rendered titles/shapes holds at most `bytes` (export uses a small
    /// one: it moves forward and only static titles repeat).
    pub(crate) fn with_vector_budget(pool: Arc<MediaPool>, bytes: usize) -> Self {
        Renderer {
            pool,
            layouts: Mutex::new(util::Lru::new(usize::MAX, 512)),
            vectors: Mutex::new(util::Lru::new(bytes, 256)),
            pictures: Mutex::new(util::Lru::new(bytes / 2, 64)),
            luts: Mutex::new(HashMap::new()),
            duck: audio::new_duck_cache(),
        }
    }

    pub fn pool(&self) -> &Arc<MediaPool> {
        &self.pool
    }

    /// The composited frame at timeline time `t` (seconds; snapped to the frame grid). `doc_dir`
    /// resolves relative media paths. Offline media and other non-fatal problems render as a
    /// placeholder; use [`Renderer::frame_checked`] to get them as warnings.
    pub fn frame(&self, doc_dir: &Path, p: &Project, r: &Resolved, t: f64, o: &FrameOptions) -> Result<Image, RenderError> {
        self.frame_checked(doc_dir, p, r, t, o).map(|(img, _)| img)
    }

    /// (addition) Like [`Renderer::frame`], also returning warnings (offline media, missing fonts,
    /// unknown effects, unusable transitions).
    pub fn frame_checked(&self, doc_dir: &Path, p: &Project, r: &Resolved, t: f64, o: &FrameOptions) -> Result<(Image, Vec<String>), RenderError> {
        let tl = timeline::Timeline::new(p, r);
        compose::FrameJob { rd: self, doc_dir, p, tl: &tl, strict: false, caching: aiframes_media::FrameCaching::Normal }.render(t, o)
    }

    /// (addition) Media clips whose picture starts being needed in `(t0, t1]` (a clip's first
    /// frame, or the start of a transition into it) on tracks that show pictures: (time, clip id).
    pub fn media_starts(&self, p: &Project, r: &Resolved, t0: f64, t1: f64) -> Vec<(f64, Id)> {
        timeline::Timeline::new(p, r).media_starts(t0, t1).into_iter().map(|(t, id)| (t, id.to_string())).collect()
    }

    /// (addition) Decodes the picture clip `clip` shows at timeline time `t` into the pool's cache,
    /// at the size a frame rendered with `o` asks for, without compositing. It uses a decoder of
    /// its own (never one playback is reading from), which then continues from there: playback
    /// calls it for [`Renderer::media_starts`] a second ahead, so a cut does not wait for a seek.
    pub fn prefetch_clip(&self, doc_dir: &Path, p: &Project, r: &Resolved, clip: &str, t: f64, o: &FrameOptions) {
        let tl = timeline::Timeline::new(p, r);
        if let Ok(v) = compose::OutputView::scaled(&p.settings, o.scale) {
            compose::FrameJob { rd: self, doc_dir, p, tl: &tl, strict: false, caching: aiframes_media::FrameCaching::Ahead }.prefetch_clip(clip, t, &v, o.draft);
        }
    }

    /// (addition) Like [`Renderer::frame`] but any missing media, LUT or unknown effect is an error
    /// (what export uses).
    pub fn frame_strict(&self, doc_dir: &Path, p: &Project, r: &Resolved, t: f64, o: &FrameOptions) -> Result<Image, RenderError> {
        let tl = timeline::Timeline::new(p, r);
        compose::FrameJob { rd: self, doc_dir, p, tl: &tl, strict: true, caching: aiframes_media::FrameCaching::Normal }.render(t, o).map(|(img, _)| img)
    }

    /// Mixes the project's sound starting at timeline time `t0` into `out` (stereo interleaved f32 at
    /// `settings.sample_rate`; `out.len() / 2` frames). Stateless: any block size, any order.
    pub fn mix(&self, doc_dir: &Path, p: &Project, r: &Resolved, t0: f64, out: &mut [f32]) -> Result<(), RenderError> {
        audio::mix(self, doc_dir, p, r, t0, out, &audio::MixOpts { soft_clip: true, strict: false, only_track: None }).map(|_| ())
    }

    /// (addition) Like [`Renderer::mix`], returning warnings (offline media...).
    pub fn mix_checked(&self, doc_dir: &Path, p: &Project, r: &Resolved, t0: f64, out: &mut [f32]) -> Result<Vec<String>, RenderError> {
        audio::mix(self, doc_dir, p, r, t0, out, &audio::MixOpts { soft_clip: true, strict: false, only_track: None })
    }

    /// (addition) One track's sound (track volume, fades and its ducking applied; no soft clip),
    /// for per-track level analysis.
    pub fn mix_track(&self, doc_dir: &Path, p: &Project, r: &Resolved, track: &str, t0: f64, out: &mut [f32]) -> Result<(), RenderError> {
        audio::mix(self, doc_dir, p, r, t0, out, &audio::MixOpts { soft_clip: false, strict: false, only_track: Some(track) }).map(|_| ())
    }

    /// (addition) The raw mix of `[t0, t1)` rendered in parallel (no soft clip, no loudness
    /// normalisation): stereo interleaved at `settings.sample_rate`.
    pub fn mixdown(&self, doc_dir: &Path, p: &Project, r: &Resolved, t0: f64, t1: f64) -> Result<Vec<f32>, RenderError> {
        export::mixdown(self, doc_dir, p, r, t0, t1)
    }

    /// Where each visible clip's content sits at time `t`, in project pixels, top-most last. The viewer
    /// uses it for click-to-select and on-canvas move/scale/rotate handles; validation for safe areas.
    pub fn layers_at(&self, doc_dir: &Path, p: &Project, r: &Resolved, t: f64) -> Vec<LayerBox> {
        compose::layers_at(self, doc_dir, p, r, t)
    }

    /// (addition) The size of a text clip's content box (text plus background padding) in project
    /// pixels.
    pub fn text_size(&self, doc_dir: &Path, ts: &TextSource) -> (f64, f64) {
        let (l, _) = self.text_layout(doc_dir, ts);
        let (w, h, _) = text::content_size(ts, &l);
        (w, h)
    }

    /// (addition) Whether a font family is available (system fonts or `<doc_dir>/fonts`).
    pub fn has_font(&self, doc_dir: &Path, family: &str) -> bool {
        text::ensure_project_fonts(doc_dir);
        util::lock(text::fonts()).has_family(family)
    }

    /// (addition) Loads the system font database now (the first text render otherwise pays for it,
    /// up to about a second). Call once from a worker thread at startup.
    pub fn warm_up(&self) {
        let _ = text::fonts();
    }

    /// (addition) Drops cached layouts, rendered titles, LUTs, ducking envelopes and pooled buffers.
    pub fn clear_caches(&self) {
        util::lock(&self.layouts).clear();
        util::lock(&self.vectors).clear();
        util::lock(&self.pictures).clear();
        util::lock(&self.luts).clear();
        util::lock(&self.duck).clear();
        canvas::clear_pool();
    }

    // -- internal caches ----------------------------------------------------------------------

    pub(crate) fn text_layout(&self, doc_dir: &Path, ts: &TextSource) -> (Arc<text::TextLayout>, Option<String>) {
        text::ensure_project_fonts(doc_dir);
        let key = text::layout_key(ts, text::generation());
        if let Some(l) = util::lock(&self.layouts).get(key) {
            let w = l.warning.clone();
            return (l, w);
        }
        let l = {
            let mut f = util::lock(text::fonts());
            Arc::new(text::layout(&mut f, ts))
        };
        util::lock(&self.layouts).insert(key, l.clone(), 1);
        let w = l.warning.clone();
        (l, w)
    }

    pub(crate) fn vector_get(&self, key: u64) -> Option<Arc<canvas::Layer>> {
        util::lock(&self.vectors).get(key)
    }

    pub(crate) fn picture_get(&self, key: u64) -> Option<canvas::Picture> {
        util::lock(&self.pictures).get(key)
    }

    pub(crate) fn picture_put(&self, key: u64, p: canvas::Picture) {
        let cost = p.data.len() + 64;
        util::lock(&self.pictures).insert(key, p, cost);
    }

    /// Caches a rendered title/shape layer (compact it first: `Layer::compact`).
    pub(crate) fn vector_put(&self, key: u64, l: Arc<canvas::Layer>) {
        let cost = l.px.capacity() * 16 + 64;
        util::lock(&self.vectors).insert(key, l, cost);
    }

    pub(crate) fn lut(&self, doc_dir: &Path, path: &str) -> Result<Arc<lut::Lut>, String> {
        let full = doc_dir.join(path);
        let meta = std::fs::metadata(&full).map_err(|_| format!("LUT file {} not found", full.display()))?;
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if let Some((m, len, l)) = util::lock(&self.luts).get(&full) {
            if *m == mtime && *len == meta.len() {
                return Ok(l.clone());
            }
        }
        let l = Arc::new(lut::Lut::load(&full)?);
        let mut g = util::lock(&self.luts);
        if g.len() > 64 {
            g.clear();
        }
        g.insert(full, (mtime, meta.len(), l.clone()));
        Ok(l)
    }
}

/// A visible clip's placement at one instant (project pixels, +y down).
#[derive(Clone, Debug, PartialEq)]
pub struct LayerBox {
    pub clip: Id,
    /// Corners of the transformed content box: top-left, top-right, bottom-right, bottom-left.
    pub corners: [[f32; 2]; 4],
    /// Axis-aligned bounds `[x, y, w, h]` of `corners`.
    pub bounds: [f32; 4],
    /// Pivot point (transform anchor) in project pixels.
    pub pivot: [f32; 2],
}

#[derive(Clone, Debug)]
pub struct ExportProgress {
    /// "prepare" | "audio" | "video" | "loudness" | "mux" | "done".
    pub stage: &'static str,
    pub frame: u64,
    pub frames: u64,
    /// Seconds since the export started.
    pub elapsed: f64,
    /// Estimated seconds remaining.
    pub eta: f64,
}

#[derive(Clone, Debug)]
pub struct ExportReport {
    pub path: PathBuf,
    pub duration: f64,
    pub frames: u64,
    pub width: u32,
    pub height: u32,
    /// ffmpeg encoder used ("h264_nvenc", "libx264", ...).
    pub encoder: String,
    /// Loudness of the final mix (after normalisation).
    pub loudness: Option<Loudness>,
    /// Wall-clock seconds the export took.
    pub seconds: f64,
    /// (addition) Peak private (committed) memory of the whole process while exporting, in bytes,
    /// sampled at every progress step; `None` where the platform does not report it.
    pub peak_memory_bytes: Option<u64>,
    /// (addition) Things the person should know: loudness target not reached, hardware encoder
    /// fell back to software.
    pub warnings: Vec<String>,
}

/// Renders the project (or `s.range`) to `out`. `progress` returning false cancels.
///
/// Stages: "prepare", "audio" (mix chunks), "loudness" (normalisation passes), "video" (frames),
/// "mux", "done". The codec `wav` writes only the normalised mix (24-bit) to `out`. Missing media,
/// LUTs or unknown effects fail the export (frames use [`Renderer::frame_strict`]).
pub fn export(
    doc_dir: &Path,
    p: &Project,
    pool: Arc<MediaPool>,
    s: &ExportSettings,
    out: &Path,
    progress: &mut dyn FnMut(ExportProgress) -> bool,
) -> Result<ExportReport, RenderError> {
    export::export(doc_dir, p, pool, s, out, progress)
}

/// A labelled grid of frames at `times` (timeline seconds), `cols` per row, each `thumb_w` wide.
/// Each frame carries its time as `mm:ss.ff` (ff = frame within the second).
pub fn contact_sheet(r: &Renderer, doc_dir: &Path, p: &Project, res: &Resolved, times: &[f64], cols: u32, thumb_w: u32) -> Result<Image, RenderError> {
    sheet::contact_sheet(r, doc_dir, p, res, times, cols, thumb_w)
}

/// PNG-encodes an image.
pub fn encode_png(img: &Image) -> Vec<u8> {
    sheet::encode_png(img)
}

#[cfg(test)]
mod send_sync {
    #[test]
    fn renderer_is_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<super::Renderer>();
    }
}

#[cfg(test)]
mod memory {
    use super::*;

    /// Regression (dogfood export grew to 4.5 GB): animated titles filled the title cache with
    /// small layers that kept whole pooled canvases alive while being costed by their length.
    #[test]
    fn title_cache_holds_what_it_counts() {
        let _g = canvas::tests::POOL_TEST.lock().unwrap_or_else(|p| p.into_inner());
        let p: Project = serde_json::from_value(serde_json::json!({
            "format": 1, "settings": {"width": 640, "height": 360, "fps": 30},
            "tracks": [{"id": "v1", "kind": "video"}, {"id": "v2", "kind": "video"}],
            "clips": [
                {"id": "bg", "track": "v1", "start": 0, "duration": 4, "source": {"type": "solid", "color": "#203040"},
                 "effects": [{"kind": "vignette"}]},
                {"id": "t", "track": "v2", "start": 0, "duration": 4, "source": {"type": "text", "text": "moving", "size": 40},
                 "transform": {"x": {"keys": [{"t": 0, "v": -200}, {"t": 4, "v": 200}]}}}
            ]
        }))
        .unwrap();
        let r = aiframes_core::resolve(&p);
        let rd = Renderer::with_vector_budget(MediaPool::new(1 << 20), 8 << 20);
        for i in 0..90 {
            rd.frame(Path::new("."), &p, &r, i as f64 / 30.0, &FrameOptions::default()).unwrap();
        }
        let v = util::lock(&rd.vectors);
        let held: usize = v.values().map(|l| l.px.capacity() * 16).sum();
        assert!(held <= 8 << 20, "title cache holds {held} bytes over its 8 MB budget");
        assert!(v.values().all(|l| l.px.capacity() <= l.px.len() * 5 / 4 + 4096), "a cached title owns an oversized buffer");
        assert!(v.used() >= held);
    }
}
