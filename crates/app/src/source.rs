//! The Source monitor, like Premiere's and Resolve's: one media file with its own playhead, J/K/L
//! and Space, frame steps, in/out marks (I / O, remembered per file), and Insert (`,`) / Overwrite
//! (`.`) of the marked range into the timeline at the program playhead.
//!
//! It plays through its own playback [`Engine`] over a one-clip project of the file, so its frames
//! come from the same renderer as the program viewer, decode on worker threads (never on the UI
//! thread), and its sound plays. Only one of the two monitors plays at a time (the app pauses the
//! other). The edits are ordinary ops built by `aiframes_mcp::actions::source_edit_ops`, the same
//! function the AI's `source_monitor` tool uses, so both land as one undoable step.

use crate::playback::clock::Shuttle;
use crate::playback::{Engine, Quality, Snapshot};
use crate::shortcuts::{self, Command};
use crate::theme;
use crate::timecode::{self, frame_index, frame_time};
use aiframes_core::{resolve, Id, MediaAsset, MediaKind, Project};
use aiframes_mcp::actions::SourceEdit;
use aiframes_mcp::SourceView;
use aiframes_render::Renderer;
use aiframes_ui::MediaPreview;
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Rect, RichText, Sense, Stroke};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Length of the one-clip project for a still image (its marks can make any length up to it).
const STILL_SECS: f64 = 10.0;
/// Render workers for the Source monitor (the program viewer keeps the rest).
const WORKERS: usize = 2;

/// Marks and playhead of one file (seconds of the file).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Marks {
    pub in_point: Option<f64>,
    pub out_point: Option<f64>,
    pub playhead: f64,
}

/// What the Source monitor needs from the app this frame.
pub struct SourceCx<'a> {
    pub doc: u64,
    pub preview: &'a dyn MediaPreview,
}

/// What the Source monitor asks the app to do.
#[derive(Default)]
pub struct SourceOut {
    /// Insert / overwrite the marked range at the program playhead.
    pub edit: Option<SourceEdit>,
    pub close: bool,
}

/// The file shown and the media info the engine's project was built from.
struct Loaded {
    doc: u64,
    asset: MediaAsset,
    dir: PathBuf,
}

pub struct SourceMonitor {
    ctx: egui::Context,
    renderer: Arc<Renderer>,
    engine: Option<Engine>,
    loaded: Option<Loaded>,
    /// Marks and playhead per (document, asset).
    marks: HashMap<(u64, Id), Marks>,
    epoch: u64,
    /// Keyboard focus (the person clicked in the monitor).
    pub focused: bool,
    /// Screen rectangle of the monitor last frame (for focus).
    pub rect: Rect,
    scrubbing: bool,
    pub volume: f32,
    pub muted: bool,
}

/// The one-clip project the Source monitor plays: the file at its own size and frame rate.
pub fn source_project(m: &MediaAsset, base: &Project) -> Project {
    let picture = m.kind != MediaKind::Audio && m.info.width > 0 && m.info.height > 0;
    let (w, h) = if picture { (m.info.width.max(2) & !1, m.info.height.max(2) & !1) } else { (640, 360) };
    let fps = if m.kind == MediaKind::Video && m.info.fps > 0.0 && m.info.fps.is_finite() { m.info.fps } else { base.settings.fps };
    let duration = match m.kind {
        MediaKind::Image => STILL_SECS,
        _ if m.info.duration > 0.0 => m.info.duration,
        _ => STILL_SECS,
    };
    let track = if m.kind == MediaKind::Audio { "a1" } else { "v1" };
    let v = json!({
        "format": 1,
        "name": "Source",
        "settings": {"width": w, "height": h, "fps": fps, "sample_rate": base.settings.sample_rate},
        "media": [m],
        "tracks": [{"id": "v1", "kind": "video", "role": "main"}, {"id": "a1", "kind": "audio"}],
        "clips": [{"id": "src", "track": track, "start": 0.0, "duration": duration, "source": {"type": "media", "asset": m.id, "in_point": 0.0}}],
    });
    serde_json::from_value(v).unwrap_or_default()
}

fn same_media(a: &MediaAsset, b: &MediaAsset) -> bool {
    a.id == b.id && a.path == b.path && a.kind == b.kind && a.info == b.info
}

impl SourceMonitor {
    pub fn new(ctx: egui::Context, renderer: Arc<Renderer>) -> SourceMonitor {
        SourceMonitor { ctx, renderer, engine: None, loaded: None, marks: HashMap::new(), epoch: 0, focused: false, rect: Rect::NOTHING, scrubbing: false, volume: 1.0, muted: false }
    }

    /// The asset shown (in document `doc`).
    pub fn asset(&self, doc: u64) -> Option<&MediaAsset> {
        self.loaded.as_ref().filter(|l| l.doc == doc).map(|l| &l.asset)
    }

    fn key(&self) -> Option<(u64, Id)> {
        self.loaded.as_ref().map(|l| (l.doc, l.asset.id.clone()))
    }

    /// Marks of the file shown.
    pub fn marks(&self) -> Marks {
        self.key().and_then(|k| self.marks.get(&k).copied()).unwrap_or_default()
    }

    fn marks_mut(&mut self) -> Option<&mut Marks> {
        let k = self.key()?;
        Some(self.marks.entry(k).or_default())
    }

    /// Seconds the file plays for in the monitor.
    pub fn duration(&self) -> f64 {
        self.engine.as_ref().map(|e| e.snapshot().duration()).unwrap_or(0.0)
    }

    fn fps(&self) -> f64 {
        self.engine.as_ref().map(|e| e.snapshot().fps()).unwrap_or(30.0)
    }

    pub fn playing(&self) -> bool {
        self.engine.as_ref().is_some_and(|e| e.playing())
    }

    pub fn pause(&mut self) {
        if let Some(e) = &mut self.engine {
            if e.playing() {
                e.pause();
            }
        }
    }

    /// Shows `asset` of document `doc` (its remembered marks and playhead come back).
    pub fn open(&mut self, doc: u64, p: &Project, dir: &Path, asset: &str) -> Result<(), String> {
        let m = p.media.iter().find(|m| m.id == asset).ok_or_else(|| format!("unknown media asset '{asset}'"))?.clone();
        let fresh = self.loaded.as_ref().is_none_or(|l| l.doc != doc || !same_media(&l.asset, &m) || l.dir != dir);
        if fresh {
            self.pause();
            self.epoch += 1;
            let project = source_project(&m, p);
            let resolved = resolve(&project);
            let snap = Arc::new(Snapshot { doc: u64::MAX - doc, revision: 0, epoch: self.epoch, project, resolved, dir: dir.to_path_buf() });
            match &mut self.engine {
                Some(e) => e.set_snapshot(snap),
                None => {
                    let mut e = Engine::new(self.ctx.clone(), self.renderer.clone(), WORKERS, snap);
                    e.set_volume(self.volume, self.muted);
                    self.engine = Some(e);
                }
            }
            self.loaded = Some(Loaded { doc, asset: m, dir: dir.to_path_buf() });
        }
        let t = self.marks().playhead;
        if let Some(e) = &mut self.engine {
            e.seek(t, false);
        }
        Ok(())
    }

    /// Applies the AI's request (`source_monitor` with asset/in/out/at).
    pub fn apply_request(&mut self, doc: u64, p: &Project, dir: &Path, v: &SourceView) -> Result<(), String> {
        let asset = v.asset.clone().ok_or("no asset")?;
        self.open(doc, p, dir, &asset)?;
        if let Some(m) = self.marks_mut() {
            m.in_point = v.in_point;
            m.out_point = v.out_point;
            m.playhead = v.playhead;
        }
        let t = v.playhead;
        if let Some(e) = &mut self.engine {
            e.seek(t, false);
        }
        Ok(())
    }

    /// What the monitor shows, for the AI (`get_state`, `source_monitor`).
    pub fn view(&self, doc: u64) -> SourceView {
        match self.asset(doc) {
            Some(a) => {
                let m = self.marks();
                SourceView { asset: Some(a.id.clone()), playhead: m.playhead, in_point: m.in_point, out_point: m.out_point }
            }
            None => SourceView::default(),
        }
    }

    /// Forgets the file when it left the project (or was relinked: reloaded with the new info).
    pub fn sync(&mut self, doc: u64, p: &Project, dir: &Path) {
        let Some(l) = &self.loaded else { return };
        if l.doc != doc {
            return;
        }
        match p.media.iter().find(|m| m.id == l.asset.id) {
            None => {
                self.pause();
                self.loaded = None;
            }
            Some(m) if !same_media(m, &l.asset) || l.dir != dir => {
                let id = m.id.clone();
                let _ = self.open(doc, p, dir, &id);
            }
            _ => {}
        }
    }

    /// Per UI frame: advances its playback and uploads the frame. Returns the source playhead.
    pub fn update(&mut self, display_px: f32) -> f64 {
        if self.loaded.is_none() {
            return 0.0;
        }
        let Some(e) = &mut self.engine else { return 0.0 };
        if display_px > 0.0 {
            let w = e.snapshot().project.settings.width;
            e.set_scale(Quality::Auto.scale(display_px, w));
        }
        let t = e.update();
        if let Some(m) = self.marks_mut() {
            m.playhead = t;
        }
        t
    }

    /// Commands that act on the Source monitor while it has the keyboard.
    pub fn handles(c: Command) -> bool {
        matches!(
            c,
            Command::PlayPause
                | Command::PlayFromIn
                | Command::ShuttleForward
                | Command::ShuttleReverse
                | Command::ShuttleStop
                | Command::FrameBack
                | Command::FrameForward
                | Command::FrameBack10
                | Command::FrameForward10
                | Command::GoStart
                | Command::GoEnd
                | Command::SetIn
                | Command::SetOut
                | Command::ClearInOut
                | Command::ToggleLoop
        )
    }

    /// Runs a transport / mark command on the monitor. `k_down`: K is held (J/L step a frame).
    pub fn command(&mut self, c: Command, k_down: bool) {
        if self.loaded.is_none() {
            return;
        }
        let fps = self.fps();
        let dur = self.duration();
        let marks = self.marks();
        let Some(e) = &mut self.engine else { return };
        let t = if e.playing() { e.time() } else { marks.playhead };
        let range = match (marks.in_point, marks.out_point) {
            (Some(a), Some(b)) if b > a => Some((a, b)),
            _ => None,
        };
        match c {
            Command::PlayPause => e.shuttle(t, Shuttle::Toggle, range.filter(|_| e.looping)),
            Command::PlayFromIn => {
                let a = marks.in_point.unwrap_or(0.0);
                let b = marks.out_point.filter(|b| *b > a).unwrap_or(dur);
                e.play(a, 1.0, Some((a, b)));
            }
            Command::ShuttleForward | Command::ShuttleReverse => {
                if k_down {
                    let step = if c == Command::ShuttleForward { 1.0 } else { -1.0 };
                    e.pause();
                    e.seek(timecode::snap(t, fps) + step / fps, false);
                } else {
                    e.shuttle(t, if c == Command::ShuttleForward { Shuttle::Forward } else { Shuttle::Reverse }, None);
                }
            }
            Command::ShuttleStop => e.shuttle(t, Shuttle::Stop, None),
            Command::FrameBack | Command::FrameForward | Command::FrameBack10 | Command::FrameForward10 => {
                let n = match c {
                    Command::FrameBack => -1.0,
                    Command::FrameForward => 1.0,
                    Command::FrameBack10 => -10.0,
                    _ => 10.0,
                };
                if e.playing() {
                    e.pause();
                }
                let base = frame_time(frame_index(t, fps), fps);
                e.seek(base + n / fps + 1e-9, false);
            }
            Command::GoStart => e.seek(0.0, false),
            Command::GoEnd => e.seek((dur - 1.0 / fps).max(0.0), false),
            Command::ToggleLoop => e.looping = !e.looping,
            Command::SetIn | Command::SetOut | Command::ClearInOut => {
                let t = timecode::snap(t, fps);
                if let Some(m) = self.marks_mut() {
                    match c {
                        Command::SetIn => {
                            m.in_point = Some(t);
                            if m.out_point.is_some_and(|o| o <= t) {
                                m.out_point = None;
                            }
                        }
                        Command::SetOut => {
                            // The out mark includes the frame under the playhead.
                            let o = (t + 1.0 / fps).min(dur);
                            m.out_point = Some(o);
                            if m.in_point.is_some_and(|i| i >= o) {
                                m.in_point = None;
                            }
                        }
                        _ => {
                            m.in_point = None;
                            m.out_point = None;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn seek(&mut self, t: f64, scrubbing: bool) {
        if let Some(e) = &mut self.engine {
            e.seek(t, scrubbing);
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &SourceCx) -> SourceOut {
        let mut out = SourceOut::default();
        let pal = theme::palette();
        self.rect = ui.max_rect();
        if let Some(l) = &self.loaded {
            if l.doc != cx.doc {
                self.pause();
            }
        }
        let shown = self.asset(cx.doc).cloned();
        let fps = self.fps();
        let dur = self.duration();
        let marks = self.marks();
        // Header.
        ui.horizontal(|ui| {
            theme::panel_header(ui, "SOURCE");
            if let Some(m) = &shown {
                ui.label(RichText::new(aiframes_ui::util::media_name(m)).small().color(pal.text));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✕").on_hover_text("Close the Source monitor (View ▸ Source Monitor brings it back)").clicked() {
                    out.close = true;
                }
                if let Some(m) = &shown {
                    let info = match m.kind {
                        MediaKind::Audio => format!("{} Hz · {} ch", m.info.audio_rate, m.info.audio_channels),
                        _ if m.info.width > 0 => format!("{}×{}{}", m.info.width, m.info.height, if m.kind == MediaKind::Video { format!(" · {} fps", crate::viewer::trim_fps(m.info.fps)) } else { String::new() }),
                        _ => String::new(),
                    };
                    ui.label(RichText::new(info).small().color(pal.text_dim));
                }
            });
        });
        let transport_h = 78.0;
        let avail = ui.available_rect_before_wrap();
        let area = Rect::from_min_max(avail.min, pos2(avail.max.x, (avail.max.y - transport_h).max(avail.min.y + 40.0)));
        let resp = ui.allocate_rect(area, Sense::click_and_drag());
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 0.0, pal.viewer);
        let ppp = ui.ctx().pixels_per_point();
        let Some(m) = shown else {
            painter.text(area.center() - vec2(0.0, 8.0), Align2::CENTER_CENTER, "No source loaded", FontId::proportional(14.0), pal.text_dim);
            painter.text(area.center() + vec2(0.0, 12.0), Align2::CENTER_CENTER, "Double-click media in the bin (or right-click ▸ Open in Source monitor)", FontId::proportional(11.5), pal.text_faint);
            self.update(0.0);
            return out;
        };
        // The picture (or the waveform of a sound file).
        let (sw, sh) = if m.kind != MediaKind::Audio && m.info.width > 0 && m.info.height > 0 { (m.info.width as f32, m.info.height as f32) } else { (16.0, 9.0) };
        let k = ((area.width() - 12.0) / sw).min((area.height() - 12.0) / sh).max(0.01);
        let img_rect = Rect::from_center_size(area.center(), vec2(sw * k, sh * k));
        let t = self.update(img_rect.width() * ppp);
        if m.kind == MediaKind::Audio {
            painter.rect_filled(img_rect, 2.0, Color32::from_rgb(0x12, 0x1a, 0x16));
            match cx.preview.waveform(&m) {
                Some(w) => {
                    let wdur = w.duration.max(dur).max(1e-3);
                    let look = aiframes_ui::timeline::wave::WaveLook { color: aiframes_ui::style::wave_color(aiframes_ui::util::ClipKind::Audio), log: true, rectified: false, ppp };
                    let r = img_rect.shrink2(vec2(0.0, img_rect.height() * 0.15));
                    aiframes_ui::timeline::wave::paint(&painter, r, r.left(), r.right(), look, |a, b| {
                        let t0 = wdur * ((a - r.left()) / r.width()) as f64;
                        let t1 = wdur * ((b - r.left()) / r.width()) as f64;
                        let (mn, mx) = w.range(t0, t1);
                        Some(aiframes_ui::timeline::wave::Column { min: mn, max: mx, rms: w.rms_range(t0, t1) })
                    });
                    let x_of = |s: f64| img_rect.left() + img_rect.width() * (s / wdur).clamp(0.0, 1.0) as f32;
                    // Outside the marked range is dimmed.
                    if marks.in_point.is_some() || marks.out_point.is_some() {
                        let (a, b) = (x_of(marks.in_point.unwrap_or(0.0)), x_of(marks.out_point.unwrap_or(wdur)));
                        for r in [Rect::from_x_y_ranges(img_rect.left()..=a, img_rect.y_range()), Rect::from_x_y_ranges(b..=img_rect.right(), img_rect.y_range())] {
                            if r.width() > 0.0 {
                                painter.rect_filled(r, 0.0, Color32::from_black_alpha(140));
                            }
                        }
                    }
                    painter.vline(x_of(t), img_rect.y_range(), Stroke::new(1.5, aiframes_ui::style::PLAYHEAD));
                }
                None => {
                    painter.text(img_rect.center(), Align2::CENTER_CENTER, "Loading waveform…", FontId::proportional(12.0), pal.text_dim);
                }
            }
        } else {
            let shown = self.engine.as_ref().map(|e| e.shown());
            match shown.as_ref().and_then(|s| s.texture) {
                Some(tex) => {
                    painter.image(tex, img_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                }
                None => {
                    painter.rect_filled(img_rect, 0.0, Color32::BLACK);
                }
            }
            if shown.as_ref().is_some_and(|s| s.loading) {
                painter.text(img_rect.center_top() + vec2(0.0, 14.0), Align2::CENTER_TOP, "Loading frame…", FontId::proportional(12.0), Color32::from_gray(220));
            }
        }
        painter.rect_stroke(img_rect, 0.0, Stroke::new(1.0, Color32::from_gray(40)), egui::StrokeKind::Outside);
        if self.focused {
            painter.rect_stroke(area.shrink(1.0), 0.0, Stroke::new(1.0, pal.accent.gamma_multiply(0.6)), egui::StrokeKind::Inside);
        }
        // Timecode in the corner.
        let tc = timecode::timecode(t, fps);
        let galley = painter.layout_no_wrap(tc, FontId::monospace(12.0), Color32::from_gray(235));
        let at = pos2(img_rect.left().max(area.left()) + 8.0, img_rect.bottom().min(area.bottom()) - 8.0 - galley.size().y);
        painter.rect_filled(Rect::from_min_size(at, galley.size()).expand(3.0), 3.0, Color32::from_black_alpha(150));
        painter.galley(at, galley, Color32::from_gray(235));
        if resp.clicked() {
            self.command(Command::PlayPause, false);
        }
        resp.on_hover_text("Click: play / pause · I / O mark in / out · , insert · . overwrite (at the timeline playhead)");

        // Scrub bar with the marked range.
        let (bar, bresp) = ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::click_and_drag());
        let bp = ui.painter_at(bar);
        bp.rect_filled(bar, 3.0, pal.field);
        let x_of = |s: f64| bar.left() + bar.width() * (s / dur.max(1e-6)).clamp(0.0, 1.0) as f32;
        let (mi, mo) = (marks.in_point, marks.out_point);
        if mi.is_some() || mo.is_some() {
            let r = Rect::from_x_y_ranges(x_of(mi.unwrap_or(0.0))..=x_of(mo.unwrap_or(dur)), bar.y_range());
            bp.rect_filled(r, 2.0, pal.accent.gamma_multiply(0.45));
            for (v, left) in [(mi, true), (mo, false)] {
                if let Some(v) = v {
                    let x = x_of(v);
                    bp.vline(x, bar.y_range(), Stroke::new(2.0, pal.accent));
                    let tip = if left { [pos2(x, bar.top()), pos2(x + 5.0, bar.top()), pos2(x, bar.top() + 5.0)] } else { [pos2(x, bar.top()), pos2(x - 5.0, bar.top()), pos2(x, bar.top() + 5.0)] };
                    bp.add(egui::Shape::convex_polygon(tip.to_vec(), pal.accent, Stroke::NONE));
                }
            }
        }
        let px = x_of(t);
        bp.vline(px, bar.y_range(), Stroke::new(2.0, aiframes_ui::style::PLAYHEAD));
        if let Some(pos) = bresp.interact_pointer_pos().filter(|_| bresp.dragged() || bresp.clicked() || bresp.is_pointer_button_down_on()) {
            let s = (((pos.x - bar.left()) / bar.width()) as f64 * dur).clamp(0.0, (dur - 1.0 / fps).max(0.0));
            self.scrubbing = true;
            self.seek(timecode::snap(s, fps), true);
        } else if self.scrubbing {
            self.scrubbing = false;
            if let Some(e) = &mut self.engine {
                e.set_scrubbing(false);
            }
        }
        // Marks and times.
        ui.horizontal(|ui| {
            let f = |v: Option<f64>| v.map(|v| timecode::timecode(v, fps)).unwrap_or_else(|| "--:--:--:--".into());
            ui.label(RichText::new(format!("In {}", f(mi))).small().monospace().color(pal.text_dim));
            ui.label(RichText::new(format!("Out {}", f(mo))).small().monospace().color(pal.text_dim));
            let (a, b) = (mi.unwrap_or(0.0), mo.unwrap_or(dur));
            if b > a && (mi.is_some() || mo.is_some()) {
                ui.label(RichText::new(format!("({})", timecode::short_duration(b - a))).small().color(pal.accent));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("/ {}", timecode::timecode(dur, fps))).small().monospace().color(pal.text_dim));
                ui.label(RichText::new(timecode::timecode(t, fps)).monospace().strong().color(pal.accent));
            });
        });
        // Transport and edits.
        ui.horizontal(|ui| {
            let btn = |ui: &mut egui::Ui, text: &str, tip: &str, cmd: Option<Command>, on: bool| -> bool {
                let sc = cmd.map(shortcuts::label).unwrap_or_default();
                let tip = if sc.is_empty() { tip.to_string() } else { format!("{tip} ({sc})") };
                ui.add(egui::Button::new(RichText::new(text).size(13.0)).selected(on).min_size(vec2(26.0, 22.0))).on_hover_text(tip).clicked()
            };
            let playing = self.playing();
            let rate = self.engine.as_ref().map(|e| e.rate()).unwrap_or(0.0);
            let mut cmd = None;
            if btn(ui, "{", "Mark in", Some(Command::SetIn), false) {
                cmd = Some(Command::SetIn);
            }
            if btn(ui, "}", "Mark out", Some(Command::SetOut), false) {
                cmd = Some(Command::SetOut);
            }
            if btn(ui, "⌫", "Clear in and out", Some(Command::ClearInOut), false) {
                cmd = Some(Command::ClearInOut);
            }
            ui.add_space(6.0);
            if btn(ui, "⏴|", "Previous frame", Some(Command::FrameBack), false) {
                cmd = Some(Command::FrameBack);
            }
            if btn(ui, if playing && rate > 0.0 { "⏸" } else { "▶" }, "Play / pause", Some(Command::PlayPause), playing) {
                cmd = Some(Command::PlayPause);
            }
            if btn(ui, "|⏵", "Next frame", Some(Command::FrameForward), false) {
                cmd = Some(Command::FrameForward);
            }
            if btn(ui, "⇥", "Play in to out", Some(Command::PlayFromIn), false) {
                cmd = Some(Command::PlayFromIn);
            }
            if let Some(c) = cmd {
                self.focused = true;
                self.command(c, false);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(RichText::new("Overwrite").size(12.0)).on_hover_text(format!("Put the marked range at the timeline playhead, replacing what is there ({})", shortcuts::label(Command::OverwriteSource))).clicked() {
                    out.edit = Some(SourceEdit::Overwrite);
                }
                if ui.button(RichText::new("Insert").size(12.0)).on_hover_text(format!("Put the marked range at the timeline playhead, moving later clips right ({})", shortcuts::label(Command::InsertSource))).clicked() {
                    out.edit = Some(SourceEdit::Insert);
                }
            });
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(kind: &str, info: serde_json::Value) -> MediaAsset {
        serde_json::from_value(json!({"id": "m1", "path": "x", "kind": kind, "info": info})).unwrap()
    }

    #[test]
    fn source_projects_play_the_whole_file_at_its_own_size() {
        let base: Project = serde_json::from_value(json!({"format": 1, "settings": {"width": 1920, "height": 1080, "fps": 25.0}})).unwrap();
        // A rotated phone clip (display size from the probe), odd sizes rounded to even.
        let p = source_project(&asset("video", json!({"duration": 12.5, "width": 1081, "height": 1920, "fps": 59.94, "has_video": true, "has_audio": true})), &base);
        assert_eq!((p.settings.width, p.settings.height), (1080, 1920));
        assert!((p.settings.fps - 59.94).abs() < 1e-9);
        let r = resolve(&p);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!((r.duration - 12.5).abs() < 0.05, "{}", r.duration);
        assert_eq!(p.clips[0].track, "v1");
        // Sound: on the audio track, project frame rate.
        let p = source_project(&asset("audio", json!({"duration": 30.0, "has_audio": true})), &base);
        assert_eq!(p.clips[0].track, "a1");
        assert!((p.settings.fps - 25.0).abs() < 1e-9);
        assert!((resolve(&p).duration - 30.0).abs() < 1e-6);
        // Stills: a fixed length to mark a range in.
        let p = source_project(&asset("image", json!({"width": 900, "height": 1600})), &base);
        assert!((resolve(&p).duration - STILL_SECS).abs() < 1e-6);
        assert_eq!((p.settings.width, p.settings.height), (900, 1600));
    }

    #[test]
    fn transport_commands_act_on_the_monitor() {
        for c in [Command::PlayPause, Command::SetIn, Command::SetOut, Command::FrameForward, Command::ShuttleReverse] {
            assert!(SourceMonitor::handles(c), "{c:?}");
        }
        for c in [Command::Split, Command::Undo, Command::ZoomIn, Command::InsertSource, Command::Delete] {
            assert!(!SourceMonitor::handles(c), "{c:?}");
        }
    }
}
