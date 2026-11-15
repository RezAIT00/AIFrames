//! Text: shaping with cosmic-text (system fonts + the project's `fonts/` folder, font fallback),
//! glyph outlines rasterised with tiny-skia directly in canvas space (crisp at any scale or
//! rotation), reveals, karaoke highlight, background pill, stroke and soft shadow.

use crate::blur;
use crate::canvas::{pixmap_layer, Layer};
use crate::geom::{Affine, IRect};
use crate::util::{lock, KeyHasher};
use aiframes_core::eval::ease;
use aiframes_core::{Align, Color, Ease, RevealKind, TextSource};
use cosmic_text::{fontdb, Attrs, Buffer, CacheKey, CacheKeyFlags, Command, Family, FontSystem, Metrics, Shaping, Style, SwashCache, Weight, Wrap};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tiny_skia::{FillRule, LineJoin, Paint, PathBuilder, Pixmap, Stroke};

/// Fonts shared by every renderer in the process (loading system fonts takes a moment).
pub(crate) struct Fonts {
    sys: FontSystem,
    swash: SwashCache,
    /// lower-case family name → family name as installed.
    families: HashMap<String, String>,
    glyphs: HashMap<(fontdb::ID, u16, u32, u16, u32), Option<Arc<tiny_skia::Path>>>,
    /// Bumped whenever fonts are added (invalidates layouts).
    pub generation: u64,
}

static FONTS: OnceLock<Mutex<Fonts>> = OnceLock::new();
/// Mirrors `Fonts::generation` so layout cache keys need no font lock.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Project font folders: when last checked and the signature of their files.
static LOADED_DIRS: Mutex<Option<HashMap<PathBuf, (std::time::Instant, u64)>>> = Mutex::new(None);

/// How often a project's fonts/ folder is checked for changes.
const FONT_DIR_RECHECK: std::time::Duration = std::time::Duration::from_secs(2);

/// Names, sizes and modification times of the font files under `dir` (0 when it does not exist).
fn dir_signature(dir: &Path) -> u64 {
    let mut h = KeyHasher::new("font-dir");
    let mut stack = vec![dir.to_path_buf()];
    let mut n = 0;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.path());
        for e in entries {
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                stack.push(e.path());
                continue;
            }
            let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0);
            h.str(&e.path().to_string_lossy()).u64(md.len()).u64(mtime);
            n += 1;
        }
    }
    if n == 0 {
        0
    } else {
        h.finish()
    }
}

/// Current font generation (bumped when a project's fonts/ folder is loaded).
pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Loads `<doc_dir>/fonts` into the shared font database once (cheap when already done).
pub(crate) fn ensure_project_fonts(doc_dir: &Path) {
    let dir = doc_dir.join("fonts");
    let now = std::time::Instant::now();
    let changed = {
        let mut g = lock(&LOADED_DIRS);
        let map = g.get_or_insert_with(HashMap::new);
        match map.get(&dir) {
            Some((at, _)) if now.duration_since(*at) < FONT_DIR_RECHECK => return,
            Some(&(_, sig)) => {
                let new = dir_signature(&dir);
                map.insert(dir.clone(), (now, new));
                new != sig
            }
            None => {
                let new = dir_signature(&dir);
                map.insert(dir.clone(), (now, new));
                new != 0
            }
        }
    };
    if changed {
        lock(fonts()).reload_project_fonts(&dir);
    }
}

pub(crate) fn fonts() -> &'static Mutex<Fonts> {
    FONTS.get_or_init(|| {
        let mut sys = FontSystem::new();
        {
            let db = sys.db_mut();
            db.set_sans_serif_family("Segoe UI");
            db.set_serif_family("Times New Roman");
            db.set_monospace_family("Consolas");
        }
        let mut f = Fonts { sys, swash: SwashCache::new(), families: HashMap::new(), glyphs: HashMap::new(), generation: 0 };
        f.index_families();
        Mutex::new(f)
    })
}

/// Fallback families tried in order when a font is not installed.
const FALLBACK_FAMILIES: &[&str] = &["Segoe UI", "Arial", "Helvetica", "DejaVu Sans", "Noto Sans", "Liberation Sans"];

impl Fonts {
    fn index_families(&mut self) {
        self.families.clear();
        for face in self.sys.db().faces() {
            for (name, _) in &face.families {
                self.families.entry(name.to_lowercase()).or_insert_with(|| name.clone());
            }
        }
    }

    /// (Re)loads a project's fonts folder: faces loaded from it before are dropped first, so
    /// replaced or removed files take effect.
    pub fn reload_project_fonts(&mut self, dir: &Path) {
        let stale: Vec<fontdb::ID> = self
            .sys
            .db()
            .faces()
            .filter(|f| match &f.source {
                fontdb::Source::File(p) | fontdb::Source::SharedFile(p, _) => p.starts_with(dir),
                _ => false,
            })
            .map(|f| f.id)
            .collect();
        let db = self.sys.db_mut();
        for id in stale {
            db.remove_face(id);
        }
        if dir.is_dir() {
            db.load_fonts_dir(dir);
        }
        self.index_families();
        self.glyphs.clear();
        self.generation += 1;
        GENERATION.store(self.generation, Ordering::Relaxed);
    }

    /// The installed family for `name`, and a warning when it had to fall back.
    pub fn family(&self, name: &str) -> (Option<String>, Option<String>) {
        if let Some(f) = self.families.get(&name.trim().to_lowercase()) {
            return (Some(f.clone()), None);
        }
        for fb in FALLBACK_FAMILIES {
            if let Some(f) = self.families.get(&fb.to_lowercase()) {
                return (Some(f.clone()), Some(format!("font \"{name}\" is not installed (or in the project's fonts/ folder); using {f}")));
            }
        }
        (None, Some(format!("font \"{name}\" is not installed; using the default font")))
    }

    pub fn has_family(&self, name: &str) -> bool {
        self.families.contains_key(&name.trim().to_lowercase())
    }

    /// The weight to shape `family` with: `want` when the family has that weight (or a variable face
    /// whose wght axis covers it), else the nearest weight it does have (CSS font matching: ties go
    /// lighter up to 500 and heavier above). The shaper only keeps a family whose face matches the
    /// weight exactly, so asking Anton (400 only) for 700 used to swap the whole text to Segoe UI.
    pub fn weight_for(&self, family: &str, want: u16) -> u16 {
        let db = self.sys.db();
        let faces: Vec<(fontdb::ID, u16)> =
            db.faces().filter(|f| f.families.iter().any(|(n, _)| n == family)).map(|f| (f.id, f.weight.0)).collect();
        if faces.is_empty() || faces.iter().any(|(_, w)| *w == want) {
            return want;
        }
        let w = want as f32;
        if faces.iter().any(|(id, _)| db.with_face_data(*id, wght_axis).flatten().is_some_and(|(lo, hi)| lo <= w && w <= hi)) {
            return want;
        }
        faces
            .iter()
            .map(|(_, w)| *w)
            .min_by_key(|w| (w.abs_diff(want), if want > 500 { u16::MAX - *w } else { *w }))
            .unwrap_or(want)
    }

    fn glyph_path(&mut self, g: &cosmic_text::LayoutGlyph) -> Option<Arc<tiny_skia::Path>> {
        let flags = g.cache_key_flags | CacheKeyFlags::DISABLE_HINTING;
        let key = (g.font_id, g.glyph_id, g.font_size.to_bits(), g.font_weight.0, flags.bits());
        if let Some(p) = self.glyphs.get(&key) {
            return p.clone();
        }
        let (ck, _, _) = CacheKey::new(g.font_id, g.glyph_id, g.font_size, (0.0, 0.0), g.font_weight, flags);
        let path = self.swash.get_outline_commands_uncached(&mut self.sys, ck).and_then(|cmds| outline_to_path(&cmds)).map(Arc::new);
        if self.glyphs.len() > 50_000 {
            self.glyphs.clear();
        }
        self.glyphs.insert(key, path.clone());
        path
    }
}

/// Min and max of a font's `wght` variation axis (None for a static font). Reads the sfnt table
/// directory and `fvar` directly (collections included).
fn wght_axis(data: &[u8], index: u32) -> Option<(f32, f32)> {
    let be16 = |o: usize| data.get(o..o + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    let be32 = |o: usize| data.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let mut off = 0usize;
    if data.get(0..4)? == b"ttcf" {
        off = be32(12 + 4 * index as usize)? as usize;
    }
    let tables = be16(off + 4)? as usize;
    for i in 0..tables {
        let rec = off + 12 + 16 * i;
        if data.get(rec..rec + 4)? != b"fvar" {
            continue;
        }
        let t = be32(rec + 8)? as usize;
        let axes = t + be16(t + 4)? as usize;
        let (count, size) = (be16(t + 8)? as usize, be16(t + 10)? as usize);
        for a in 0..count {
            let ar = axes + a * size;
            if data.get(ar..ar + 4)? == b"wght" {
                let fixed = |o: usize| be32(o).map(|v| v as i32 as f32 / 65536.0);
                return Some((fixed(ar + 4)?, fixed(ar + 12)?));
            }
        }
        return None;
    }
    None
}

/// Swash outline commands (y up, origin on the baseline) → tiny-skia path (y down).
fn outline_to_path(cmds: &[Command]) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    for c in cmds {
        match c {
            Command::MoveTo(p) => pb.move_to(p.x, -p.y),
            Command::LineTo(p) => pb.line_to(p.x, -p.y),
            Command::CurveTo(a, b, p) => pb.cubic_to(a.x, -a.y, b.x, -b.y, p.x, -p.y),
            Command::QuadTo(a, p) => pb.quad_to(a.x, -a.y, p.x, -p.y),
            Command::Close => pb.close(),
        }
    }
    pb.finish()
}

// ---------------------------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------------------------

pub(crate) struct PGlyph {
    pub path: Option<Arc<tiny_skia::Path>>,
    /// Origin (pen position on the baseline) in text-box coordinates.
    pub x: f32,
    pub y: f32,
    /// Advance width.
    pub w: f32,
    pub line: usize,
    /// Global byte offset of the glyph's cluster in the (normalised) text.
    pub byte: usize,
    pub word: Option<usize>,
    /// Logical order (typewriter).
    pub order: usize,
}

pub(crate) struct LineBox {
    pub top: f32,
    pub height: f32,
}

pub(crate) struct TextLayout {
    pub glyphs: Vec<PGlyph>,
    pub lines: Vec<LineBox>,
    pub width: f32,
    pub height: f32,
    pub size: f32,
    pub words: usize,
    pub warning: Option<String>,
}

/// Seconds a word takes to pop in with the `spoken` reveal.
const SPOKEN_POP: f64 = 0.07;

/// Normalised text: CRLF/CR → LF.
fn normalise(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Whitespace that separates words (no-break spaces join: a caption word like "New York" written
/// with U+00A0 stays one word for its highlight window).
fn is_word_break(ch: char) -> bool {
    ch.is_whitespace() && !matches!(ch, '\u{00A0}' | '\u{2007}' | '\u{202F}')
}

/// Byte ranges of whitespace-separated words.
fn word_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, ch) in text.char_indices() {
        if is_word_break(ch) {
            if let Some(s) = start.take() {
                out.push((s, i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push((s, text.len()));
    }
    out
}

pub(crate) fn layout_key(ts: &TextSource, generation: u64) -> u64 {
    let mut h = KeyHasher::new("text-layout");
    h.str(&ts.text)
        .str(&ts.font)
        .f64(ts.size)
        .u64(ts.weight as u64)
        .u64(ts.italic as u64)
        .f64(ts.line_height)
        .f64(ts.letter_spacing)
        .f64(ts.max_width.unwrap_or(-1.0))
        .u64(ts.align as u64)
        .u64(generation);
    h.finish()
}

/// Shapes the text (at project resolution; outlines scale losslessly).
pub(crate) fn layout(fonts: &mut Fonts, ts: &TextSource) -> TextLayout {
    let text = normalise(&ts.text);
    let size = (ts.size as f32).clamp(1.0, 2000.0);
    let lh = size * (ts.line_height as f32).clamp(0.1, 10.0);
    let (family, warning) = fonts.family(&ts.font);
    let want = ts.weight.clamp(1, 1000);
    let weight = family.as_deref().map(|f| fonts.weight_for(f, want)).unwrap_or(want);
    let mut attrs = Attrs::new()
        .weight(Weight(weight))
        .style(if ts.italic { Style::Italic } else { Style::Normal })
        .cache_key_flags(CacheKeyFlags::DISABLE_HINTING);
    attrs = match &family {
        Some(f) => attrs.family(Family::Name(f)),
        None => attrs.family(Family::SansSerif),
    };
    if ts.letter_spacing != 0.0 {
        attrs = attrs.letter_spacing(ts.letter_spacing as f32 / size);
    }
    let mut buf = Buffer::new_empty(Metrics::new(size, lh));
    let max_w = ts.max_width.filter(|w| w.is_finite() && *w > 0.0).map(|w| w as f32);
    buf.set_wrap(if max_w.is_some() { Wrap::WordOrGlyph } else { Wrap::None });
    buf.set_size(max_w, None);
    buf.set_text(&text, &attrs, Shaping::Advanced, Some(cosmic_text::Align::Left));
    buf.shape_until_scroll(&mut fonts.sys, false);

    // Global byte offset of each paragraph.
    let mut para_start = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            para_start.push(i + 1);
        }
    }
    let words = word_ranges(&text);
    let word_of = |byte: usize| -> Option<usize> {
        let i = words.partition_point(|w| w.1 <= byte);
        words.get(i).filter(|w| w.0 <= byte && byte < w.1).map(|_| i)
    };

    struct RawRun {
        top: f32,
        height: f32,
        width: f32,
        glyphs: Vec<(cosmic_text::LayoutGlyph, f32)>,
        line_i: usize,
    }
    let mut runs: Vec<RawRun> = Vec::new();
    for run in buf.layout_runs() {
        runs.push(RawRun {
            top: run.line_top,
            height: run.line_height,
            width: run.line_w,
            glyphs: run.glyphs.iter().map(|g| (g.clone(), run.line_y)).collect(),
            line_i: run.line_i,
        });
    }
    let width = runs.iter().map(|r| r.width).fold(0.0f32, f32::max);
    let height = runs.iter().map(|r| r.top + r.height).fold(0.0f32, f32::max);
    let mut lines = Vec::new();
    let mut glyphs = Vec::new();
    for (li, r) in runs.iter().enumerate() {
        let x = match ts.align {
            Align::Left => 0.0,
            Align::Center => (width - r.width) / 2.0,
            Align::Right => width - r.width,
        };
        lines.push(LineBox { top: r.top, height: r.height });
        for (g, line_y) in &r.glyphs {
            let path = fonts.glyph_path(g);
            let byte = para_start.get(r.line_i).copied().unwrap_or(0) + g.start;
            glyphs.push(PGlyph {
                path,
                x: x + g.x + g.font_size * g.x_offset,
                y: line_y + g.y - g.font_size * g.y_offset,
                w: g.w,
                line: li,
                byte,
                word: word_of(byte),
                order: 0,
            });
        }
    }
    let mut idx: Vec<usize> = (0..glyphs.len()).collect();
    idx.sort_by(|&a, &b| glyphs[a].byte.cmp(&glyphs[b].byte).then(glyphs[a].x.total_cmp(&glyphs[b].x)));
    for (o, i) in idx.into_iter().enumerate() {
        glyphs[i].order = o;
    }
    TextLayout { glyphs, lines, width, height, size, words: words.len(), warning }
}

/// The text box plus the background padding: the clip's content box.
pub(crate) fn content_size(ts: &TextSource, l: &TextLayout) -> (f64, f64, f64) {
    let pad = ts.background.as_ref().map(|b| b.padding.max(0.0)).unwrap_or(0.0);
    (l.width as f64 + 2.0 * pad, l.height as f64 + 2.0 * pad, pad)
}

// ---------------------------------------------------------------------------------------------
// Animation state
// ---------------------------------------------------------------------------------------------

/// Per-glyph drawing state at one instant.
#[derive(Clone, Copy, PartialEq)]
struct GState {
    alpha: f32,
    scale: f32,
    dy: f32,
    highlight: bool,
}

pub(crate) struct TextState {
    glyphs: Vec<GState>,
    /// Alpha of the background pill.
    pill: f32,
    active_word: Option<usize>,
}

impl TextState {
    pub fn hash_into(&self, h: &mut KeyHasher) {
        for g in &self.glyphs {
            h.f32(g.alpha).f32(g.scale).f32(g.dy).u64(g.highlight as u64);
        }
        h.f32(self.pill).u64(self.active_word.map(|w| w as u64 + 1).unwrap_or(0));
    }
    /// True when nothing is visible.
    pub fn invisible(&self) -> bool {
        self.pill <= 0.0 && self.glyphs.iter().all(|g| g.alpha <= 0.0)
    }
}

/// Stagger window `k` of `n` over progress `u`: 0..1.
fn stagger(u: f64, k: usize, n: usize) -> f64 {
    if n <= 1 {
        return u.clamp(0.0, 1.0);
    }
    let f = (2.0 / (n as f64 + 1.0)).clamp(0.15, 1.0);
    let start = k as f64 * (1.0 - f) / (n as f64 - 1.0);
    ((u - start) / f).clamp(0.0, 1.0)
}

pub(crate) fn state(ts: &TextSource, l: &TextLayout, t_local: f64) -> TextState {
    let d = ts.reveal.duration;
    let u = if d > 0.0 { (t_local / d).clamp(0.0, 1.0) } else if t_local >= 0.0 { 1.0 } else { 0.0 };
    let n = l.glyphs.len();
    let mut gs = vec![GState { alpha: 1.0, scale: 1.0, dy: 0.0, highlight: false }; n];
    let mut pill = 1.0f32;
    match ts.reveal.kind {
        RevealKind::None => {}
        RevealKind::Typewriter => {
            let total = l.glyphs.len();
            let shown = if u >= 1.0 { total } else { (u * total as f64).floor() as usize };
            for (g, s) in l.glyphs.iter().zip(gs.iter_mut()) {
                if g.order >= shown {
                    s.alpha = 0.0;
                }
            }
        }
        RevealKind::FadeWords | RevealKind::PopWords => {
            let nw = l.words.max(1);
            for (g, s) in l.glyphs.iter().zip(gs.iter_mut()) {
                let k = g.word.unwrap_or(0);
                let w = stagger(u, k, nw);
                if ts.reveal.kind == RevealKind::FadeWords {
                    s.alpha = ease(&Ease::Out, w) as f32;
                } else {
                    s.alpha = (w * 3.0).clamp(0.0, 1.0) as f32;
                    s.scale = (0.35 + 0.65 * ease(&Ease::BackOut, w)) as f32;
                }
            }
        }
        RevealKind::SlideUp => {
            let nl = l.lines.len().max(1);
            for (g, s) in l.glyphs.iter().zip(gs.iter_mut()) {
                let w = ease(&Ease::Out, stagger(u, g.line, nl));
                s.alpha = w as f32;
                let lh = l.lines.get(g.line).map(|b| b.height).unwrap_or(l.size);
                s.dy = ((1.0 - w) * 0.6) as f32 * lh;
            }
            pill = ease(&Ease::Out, u) as f32;
        }
        RevealKind::Fade => {
            let a = u as f32;
            gs.iter_mut().for_each(|s| s.alpha = a);
            pill = a;
        }
        RevealKind::Spoken => {
            // Without word windows there is nothing to follow: the text just shows.
            if let Some(hl) = ts.highlight.as_ref().filter(|h| !h.words.is_empty()) {
                for (g, s) in l.glyphs.iter().zip(gs.iter_mut()) {
                    let Some(w) = g.word.and_then(|k| hl.words.get(k)) else { continue };
                    let a = ((t_local - w[0]) / SPOKEN_POP).clamp(0.0, 1.0);
                    s.alpha = a as f32;
                    s.scale = (0.8 + 0.2 * ease(&Ease::BackOut, a)) as f32;
                }
            }
        }
    }
    let mut active_word = None;
    if let Some(hl) = &ts.highlight {
        active_word = hl.words.iter().position(|w| t_local >= w[0] && t_local < w[1]);
        if let Some(k) = active_word {
            for (g, s) in l.glyphs.iter().zip(gs.iter_mut()) {
                if g.word == Some(k) {
                    s.highlight = true;
                }
            }
        }
    }
    TextState { glyphs: gs, pill, active_word }
}

// ---------------------------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------------------------

fn skia_color(c: Color, alpha: f32) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba(c.r.clamp(0.0, 1.0), c.g.clamp(0.0, 1.0), c.b.clamp(0.0, 1.0), (c.a * alpha).clamp(0.0, 1.0))
        .unwrap_or(tiny_skia::Color::TRANSPARENT)
}

/// A rounded rectangle path starting at the middle of the top edge, clockwise.
pub(crate) fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    if !(w > 0.0 && h > 0.0) {
        return None;
    }
    let r = r.clamp(0.0, w.min(h) / 2.0);
    let k = 0.552_284_8 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + w / 2.0, y);
    pb.line_to(x + w - r, y);
    if r > 0.0 {
        pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    }
    pb.line_to(x + w, y + h - r);
    if r > 0.0 {
        pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    }
    pb.line_to(x + r, y + h);
    if r > 0.0 {
        pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    }
    pb.line_to(x, y + r);
    if r > 0.0 {
        pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    }
    pb.close();
    pb.finish()
}

pub(crate) struct TextDraw<'a> {
    pub ts: &'a TextSource,
    pub layout: &'a TextLayout,
    pub state: &'a TextState,
    /// Content-box coordinates → canvas pixels.
    pub to_out: Affine,
    /// Canvas pixels per project pixel along the text (for shadow sizes).
    pub px_scale: f64,
    pub canvas: (u32, u32),
}

/// Renders the text clip's content into a canvas-space layer.
pub(crate) fn render(d: &TextDraw) -> Layer {
    let (cw, ch) = d.canvas;
    let ts = d.ts;
    let l = d.layout;
    let (bw, bh, pad) = content_size(ts, l);
    let stroke_w = ts.stroke.as_ref().map(|s| s.width.clamp(0.0, 2000.0)).unwrap_or(0.0);
    let margin = stroke_w + 0.35 * l.size as f64 + 2.0;
    let bounds = d.to_out.bounds_of(-margin, -margin, bw + margin, bh + margin);
    let canvas = IRect::of_size(cw, ch);
    let rect = IRect::from_bounds(bounds).expand(1);
    let shadow = ts.shadow.as_ref().filter(|s| s.color.a > 0.0);
    let lim = 4.0 * cw.max(ch).max(1) as f64;
    let (sdx, sdy, sigma) = match shadow {
        Some(s) => (
            (s.x * d.px_scale).clamp(-lim, lim).round() as i32,
            (s.y * d.px_scale).clamp(-lim, lim).round() as i32,
            (s.blur * d.px_scale / 2.0).clamp(0.0, lim) as f32,
        ),
        None => (0, 0, 0.0),
    };
    let main_rect = rect.intersect(&canvas);
    if main_rect.is_empty() || d.state.invisible() {
        return Layer::empty(cw, ch);
    }
    let Some(mut pm) = Pixmap::new(main_rect.w as u32, main_rect.h as u32) else { return Layer::empty(cw, ch) };
    let base = d.to_out.then(&Affine::translate(-main_rect.x as f64, -main_rect.y as f64));
    let mut paint = Paint { anti_alias: true, ..Paint::default() };

    // Background pill.
    if let Some(bg) = &ts.background {
        if d.state.pill > 0.0 {
            paint.set_color(skia_color(bg.color, d.state.pill));
            if ts.reveal.kind == RevealKind::Spoken {
                // The pill grows with the line: per line, around the words spoken so far (an empty pill
                // waiting for words reads as a glitch).
                let p = pad as f32;
                for (li, lb) in l.lines.iter().enumerate() {
                    let (mut x0, mut x1) = (f32::INFINITY, f32::NEG_INFINITY);
                    for (g, s) in l.glyphs.iter().zip(&d.state.glyphs) {
                        if g.line == li && g.word.is_some() && s.alpha > 0.0 {
                            x0 = x0.min(g.x);
                            x1 = x1.max(g.x + g.w);
                        }
                    }
                    if x0.is_finite() {
                        if let Some(path) = rounded_rect(x0, lb.top, x1 - x0 + 2.0 * p, lb.height + 2.0 * p, bg.radius as f32) {
                            pm.fill_path(&path, &paint, FillRule::Winding, base.to_skia(), None);
                        }
                    }
                }
            } else if let Some(path) = rounded_rect(0.0, 0.0, bw as f32, bh as f32, bg.radius as f32) {
                pm.fill_path(&path, &paint, FillRule::Winding, base.to_skia(), None);
            }
        }
    }
    let pad = pad as f32;
    // Per-word boxes (for the highlight box and pop scaling).
    let word_box = |k: usize| -> Option<[f32; 4]> {
        let mut b = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
        for g in l.glyphs.iter().filter(|g| g.word == Some(k)) {
            let lb = &l.lines[g.line];
            b[0] = b[0].min(g.x);
            b[2] = b[2].max(g.x + g.w);
            b[1] = b[1].min(lb.top);
            b[3] = b[3].max(lb.top + lb.height);
        }
        b[0].is_finite().then_some(b)
    };
    let mut boxes: HashMap<usize, [f32; 4]> = HashMap::new();
    let mut glyph_xf = |i: usize| -> Affine {
        let g = &l.glyphs[i];
        let s = &d.state.glyphs[i];
        let mut m = Affine::translate((g.x + pad) as f64, (g.y + pad + s.dy) as f64);
        if (s.scale - 1.0).abs() > 1e-4 {
            let (cx, cy) = match g.word {
                Some(k) => {
                    let b = *boxes.entry(k).or_insert_with(|| word_box(k).unwrap_or([g.x, g.y, g.x + g.w, g.y]));
                    (((b[0] + b[2]) / 2.0 + pad) as f64, ((b[1] + b[3]) / 2.0 + pad + s.dy) as f64)
                }
                None => ((g.x + g.w / 2.0 + pad) as f64, (g.y + pad) as f64),
            };
            m = m.then(&Affine::scale_about(s.scale as f64, cx, cy));
        }
        m.then(&base)
    };
    // Highlight box behind the active word.
    if let (Some(hl), Some(k)) = (&ts.highlight, d.state.active_word) {
        if let (Some(bgc), Some(b)) = (hl.background, word_box(k)) {
            let a = l.glyphs.iter().zip(&d.state.glyphs).filter(|(g, _)| g.word == Some(k)).map(|(_, s)| s.alpha).fold(0.0f32, f32::max);
            let ex = 0.12 * l.size;
            if let Some(path) = rounded_rect(b[0] - ex + pad, b[1] + pad, b[2] - b[0] + 2.0 * ex, b[3] - b[1], 0.22 * l.size) {
                paint.set_color(skia_color(bgc, a));
                pm.fill_path(&path, &paint, FillRule::Winding, base.to_skia(), None);
            }
        }
    }
    let xfs: Vec<Option<Affine>> =
        (0..l.glyphs.len()).map(|i| (d.state.glyphs[i].alpha > 0.0 && l.glyphs[i].path.is_some()).then(|| glyph_xf(i))).collect();
    // A slide-up reveal slides the lines up inside the content box (masked), like a lower third.
    let sliding = ts.reveal.kind == RevealKind::SlideUp && d.state.glyphs.iter().any(|g| g.dy > 0.01);
    let clip_mask = if sliding {
        let radius = ts.background.as_ref().map(|b| b.radius as f32).unwrap_or(0.0);
        rounded_rect(0.0, 0.0, bw as f32, bh as f32, radius).and_then(|path| {
            let mut m = tiny_skia::Mask::new(main_rect.w as u32, main_rect.h as u32)?;
            m.fill_path(&path, FillRule::Winding, true, base.to_skia());
            Some(m)
        })
    } else {
        None
    };
    let mask = clip_mask.as_ref();
    // Outline behind the fill, following the letters' outer silhouette only. Glyphs built from
    // overlapping contours (variable fonts such as Bahnschrift) have contour edges inside the
    // letter; their strokes are cut away wherever the fill covers (the coverage eroded by a pixel,
    // so the anti-aliased edge keeps the outline under it), which keeps them out of sight under a
    // translucent or fading fill too.
    if let Some(st) = ts.stroke.as_ref().filter(|s| s.width > 0.0 && s.color.a > 0.0) {
        let stroke = Stroke { width: (2.0 * stroke_w) as f32, line_join: LineJoin::Round, ..Stroke::default() };
        let (w, h) = (pm.width(), pm.height());
        if let (Some(mut outline), Some(mut cover)) = (Pixmap::new(w, h), tiny_skia::Mask::new(w, h)) {
            for (i, g) in l.glyphs.iter().enumerate() {
                if let (Some(m), Some(path)) = (xfs[i], &g.path) {
                    paint.set_color(skia_color(st.color, d.state.glyphs[i].alpha));
                    outline.stroke_path(path, &paint, &stroke, m.to_skia(), mask);
                    cover.fill_path(path, FillRule::Winding, true, m.to_skia());
                }
            }
            let inside = erode3(cover.data(), w as usize, h as usize);
            for (q, &e) in outline.data_mut().as_chunks_mut::<4>().0.iter_mut().zip(inside.iter()) {
                if e > 0 {
                    let k = 255 - e as u32;
                    for v in q.iter_mut() {
                        *v = ((*v as u32 * k + 127) / 255) as u8;
                    }
                }
            }
            pm.draw_pixmap(0, 0, outline.as_ref(), &tiny_skia::PixmapPaint::default(), tiny_skia::Transform::identity(), None);
        }
    }
    for (i, g) in l.glyphs.iter().enumerate() {
        if let (Some(m), Some(path)) = (xfs[i], &g.path) {
            let s = &d.state.glyphs[i];
            let c = match (&ts.highlight, s.highlight) {
                (Some(hl), true) => hl.color,
                _ => ts.color,
            };
            paint.set_color(skia_color(c, s.alpha));
            pm.fill_path(path, &paint, FillRule::Winding, m.to_skia(), mask);
        }
    }
    let mut main = pixmap_layer(cw, ch, &pm, main_rect.x, main_rect.y);
    if let Some(sh) = shadow {
        let c = sh.color;
        blur::shadow_under(&mut main, sdx, sdy, sigma, [c.r * c.a, c.g * c.a, c.b * c.a, c.a], 3);
    }
    main
}

/// 3x3 minimum of an 8-bit coverage mask (edges count as uncovered).
fn erode3(src: &[u8], w: usize, h: usize) -> Vec<u8> {
    if w < 3 || h < 3 {
        return vec![0; w * h];
    }
    let mut rows = vec![0u8; w * h];
    for y in 0..h {
        let r = &src[y * w..(y + 1) * w];
        let o = &mut rows[y * w..(y + 1) * w];
        for x in 1..w - 1 {
            o[x] = r[x - 1].min(r[x]).min(r[x + 1]);
        }
    }
    let mut out = vec![0u8; w * h];
    for y in 1..h - 1 {
        for x in 0..w {
            out[y * w + x] = rows[(y - 1) * w + x].min(rows[y * w + x]).min(rows[(y + 1) * w + x]);
        }
    }
    out
}

/// A quick single-line label (contact sheets, offline placeholders): returns the layout.
pub(crate) fn simple_layout(text: &str, size: f64, weight: u16) -> Arc<TextLayout> {
    let ts = TextSource { text: text.to_string(), font: "Segoe UI".into(), size, weight, ..TextSource::default() };
    let mut f = lock(fonts());
    Arc::new(layout(&mut f, &ts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_and_stagger() {
        assert_eq!(word_ranges("  ab cd\nef "), vec![(2, 4), (5, 7), (8, 10)]);
        assert_eq!(stagger(0.0, 0, 3), 0.0);
        assert_eq!(stagger(1.0, 2, 3), 1.0);
        assert!(stagger(0.5, 0, 3) > stagger(0.5, 2, 3));
    }

    #[test]
    fn missing_weight_keeps_the_family() {
        // Impact ships with Windows in one weight (400): asking for 700 must still shape Impact (the
        // shaper used to fall back to Segoe UI Bold), so both widths are equal.
        let f = lock(fonts());
        if !f.has_family("Impact") {
            return;
        }
        assert_eq!(f.weight_for("Impact", 700), 400);
        assert_eq!(f.weight_for("Impact", 400), 400);
        drop(f);
        let ts = TextSource { text: "RAGE BEYOND".into(), font: "Impact".into(), size: 80.0, weight: 400, ..TextSource::default() };
        let mut f = lock(fonts());
        let regular = layout(&mut f, &ts).width;
        let bold = layout(&mut f, &TextSource { weight: 700, ..ts }).width;
        assert!((regular - bold).abs() < 0.5, "{regular} vs {bold}");
    }

    #[test]
    fn wght_axis_of_static_and_broken_data() {
        assert_eq!(wght_axis(b"", 0), None);
        assert_eq!(wght_axis(&[0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 0), None);
    }

    #[test]
    fn layout_measures_text() {
        let ts = TextSource { text: "Hello world".into(), size: 50.0, ..TextSource::default() };
        let mut f = lock(fonts());
        let l = layout(&mut f, &ts);
        assert!(l.width > 100.0 && l.width < 600.0, "{}", l.width);
        assert!((l.height - 57.5).abs() < 1.0, "{}", l.height);
        assert_eq!(l.words, 2);
        assert!(l.glyphs.iter().filter(|g| g.path.is_some()).count() >= 10);
        let wrapped = TextSource { max_width: Some(l.width as f64 * 0.6), ..ts };
        let l2 = layout(&mut f, &wrapped);
        assert_eq!(l2.lines.len(), 2);
    }
}
