//! The program viewer: letterboxed frame, zoom, safe-area guides, timecode overlay, click to select
//! the top clip, on-canvas move/scale/rotate handles (ops on release), drop media from the bin, and
//! the transport bar under it.

use crate::playback::{Quality, Shown};
use crate::shortcuts::{self, Command};
use crate::theme;
use crate::timecode;
use aiframes_core::{sample, Clip, Id, Op, Param, Project, Resolved};
use aiframes_render::LayerBox;
use aiframes_ui::{BrowserDrag, MediaDrag, UiAction};
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Shape, Stroke, Vec2};
use serde_json::json;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Zoom {
    #[default]
    Fit,
    P50,
    P100,
    P200,
}

impl Zoom {
    pub const ALL: [Zoom; 4] = [Zoom::Fit, Zoom::P50, Zoom::P100, Zoom::P200];
    pub fn label(self) -> &'static str {
        match self {
            Zoom::Fit => "Fit",
            Zoom::P50 => "50%",
            Zoom::P100 => "100%",
            Zoom::P200 => "200%",
        }
    }
    fn factor(self) -> Option<f32> {
        match self {
            Zoom::Fit => None,
            Zoom::P50 => Some(0.5),
            Zoom::P100 => Some(1.0),
            Zoom::P200 => Some(2.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum DragKind {
    Move,
    Scale,
    Rotate,
}

#[derive(Clone, Debug)]
struct HandleDrag {
    clip: Id,
    kind: DragKind,
    /// Mouse position at the start, project pixels.
    start: [f32; 2],
    pivot: [f32; 2],
    /// Values at the playhead when the drag started: x, y, scale, rotation.
    v0: [f64; 4],
    cur: [f64; 4],
    corners0: [[f32; 2]; 4],
}

/// Transform values being dragged (for a live preview): clip and x, y, scale, rotation.
#[derive(Clone, Debug, PartialEq)]
pub struct LivePreview {
    pub clip: Id,
    pub values: [f64; 4],
}

pub const PARAM_PATHS: [&str; 4] = ["transform.x", "transform.y", "transform.scale", "transform.rotation"];

#[derive(Default)]
pub struct Viewer {
    pub zoom: Zoom,
    pub pan: Vec2,
    pub safe: bool,
    pub quality: Quality,
    pub focused: bool,
    drag: Option<HandleDrag>,
    /// The playhead timecode being typed into.
    tc_edit: Option<String>,
    /// Physical pixel width of the displayed frame (drives the auto render scale).
    pub display_px: f32,
}

pub struct ViewerCx<'a> {
    pub project: &'a Project,
    pub resolved: &'a Resolved,
    pub playhead: f64,
    pub playing: bool,
    pub rate: f64,
    pub selection: &'a [Id],
    pub shown: Shown,
    pub layers: &'a [LayerBox],
    pub in_point: Option<f64>,
    pub out_point: Option<f64>,
    pub looping: bool,
    pub ai_flash: f32,
    pub render_scale: f32,
    pub buffering: bool,
    /// Render warnings for the frame on screen (offline media, missing fonts, unknown effects).
    pub warnings: &'a [String],
    pub volume: f32,
    pub muted: bool,
    /// Playback rate readout and Auto quality state (the dropped-frame indicator).
    pub perf: PlaybackPerf,
}

/// What the viewer says about playback performance.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlaybackPerf {
    /// Frames shown in the last second, and how many there should have been.
    pub fps: f32,
    pub target_fps: f32,
    /// Frames dropped in the last second.
    pub dropped_recent: u64,
    /// Auto quality lowered the preview from this scale to keep playback smooth.
    pub lowered_from: Option<f32>,
}

#[derive(Default)]
pub struct ViewerOut {
    pub actions: Vec<UiAction>,
    pub commands: Vec<Command>,
    pub preview: Option<LivePreview>,
    /// The drag ended: the live preview should be dropped.
    pub preview_done: bool,
    pub volume: Option<(f32, bool)>,
    /// The layers are needed (a clip is selected or the user clicked).
    pub want_layers: bool,
}

fn params(c: &Clip) -> [&Param; 4] {
    [&c.transform.x, &c.transform.y, &c.transform.scale, &c.transform.rotation]
}

/// Ops committing dragged transform values at clip-local time `local`: keyframes for animated
/// params (edit the key at the playhead), plain values otherwise.
pub fn commit_ops(c: &Clip, local: f64, before: [f64; 4], after: [f64; 4]) -> Vec<Op> {
    let mut ops = Vec::new();
    for (i, p) in params(c).iter().enumerate() {
        if (after[i] - before[i]).abs() < 1e-6 {
            continue;
        }
        let v = if p.is_animated() {
            json!({"op": "add_keyframe", "clip": c.id, "path": PARAM_PATHS[i], "t": local, "v": after[i]})
        } else {
            json!({"op": "set_param", "clip": c.id, "path": PARAM_PATHS[i], "value": after[i]})
        };
        if let Ok(o) = Op::from_json(&v) {
            ops.push(o);
        }
    }
    ops
}

/// Point-in-quad test (convex polygon, any winding).
pub fn inside_quad(q: &[[f32; 2]; 4], p: [f32; 2]) -> bool {
    let mut sign = 0.0f32;
    for i in 0..4 {
        let a = q[i];
        let b = q[(i + 1) % 4];
        let cross = (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0]);
        if cross.abs() < 1e-6 {
            continue;
        }
        if sign == 0.0 {
            sign = cross.signum();
        } else if cross.signum() != sign {
            return false;
        }
    }
    true
}

/// The top-most layer under a point (project pixels).
pub fn pick_layer(layers: &[LayerBox], p: [f32; 2]) -> Option<&LayerBox> {
    layers.iter().rev().find(|l| inside_quad(&l.corners, p))
}

/// New x, y, scale, rotation for a drag from `start` to `now` (project pixels).
fn drag_values(d: &HandleDrag, now: [f32; 2], shift: bool) -> [f64; 4] {
    let mut v = d.v0;
    let (dx, dy) = (now[0] - d.start[0], now[1] - d.start[1]);
    match d.kind {
        DragKind::Move => {
            let (mut dx, mut dy) = (dx, dy);
            if shift {
                if dx.abs() > dy.abs() {
                    dy = 0.0;
                } else {
                    dx = 0.0;
                }
            }
            v[0] = d.v0[0] + dx as f64;
            v[1] = d.v0[1] + dy as f64;
        }
        DragKind::Scale => {
            let d0 = ((d.start[0] - d.pivot[0]).powi(2) + (d.start[1] - d.pivot[1]).powi(2)).sqrt().max(1.0);
            let d1 = ((now[0] - d.pivot[0]).powi(2) + (now[1] - d.pivot[1]).powi(2)).sqrt();
            let mut s = d.v0[2] * (d1 / d0) as f64;
            if shift {
                s = (s * 20.0).round() / 20.0;
            }
            v[2] = s.clamp(0.01, 100.0);
        }
        DragKind::Rotate => {
            let a0 = (d.start[1] - d.pivot[1]).atan2(d.start[0] - d.pivot[0]);
            let a1 = (now[1] - d.pivot[1]).atan2(now[0] - d.pivot[0]);
            let mut r = d.v0[3] + (a1 - a0).to_degrees() as f64;
            if shift {
                r = (r / 15.0).round() * 15.0;
            }
            v[3] = r;
        }
    }
    v
}

impl Viewer {
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &ViewerCx) -> ViewerOut {
        let mut out = ViewerOut::default();
        let pal = theme::palette();
        let s = &cx.project.settings;
        let fps = s.fps;
        // Header.
        ui.horizontal(|ui| {
            theme::panel_header(ui, "PROGRAM");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let lowered = cx.perf.lowered_from.filter(|_| self.quality == Quality::Auto);
                let label = match lowered {
                    Some(_) => format!("Auto {:.0}%", cx.render_scale * 100.0),
                    None => self.quality.label().to_string(),
                };
                let tip = match lowered {
                    Some(from) => format!(
                        "Preview resolution: rendering at {:.0}% (Auto lowered it from {:.0}% because playback was dropping frames; it goes back up after a smooth stretch). Pick Full or 1/2 to fix it.",
                        cx.render_scale * 100.0,
                        from * 100.0
                    ),
                    None => format!("Preview resolution (rendering at {:.0}%). Auto follows the viewer size and lowers it while playback drops frames.", cx.render_scale * 100.0),
                };
                egui::ComboBox::from_id_salt("viewer-quality").width(if lowered.is_some() { 74.0 } else { 58.0 }).selected_text(label).show_ui(ui, |ui| {
                    for q in Quality::ALL {
                        ui.selectable_value(&mut self.quality, q, q.label());
                    }
                }).response.on_hover_text(tip);
                egui::ComboBox::from_id_salt("viewer-zoom").width(54.0).selected_text(self.zoom.label()).show_ui(ui, |ui| {
                    for z in Zoom::ALL {
                        if ui.selectable_value(&mut self.zoom, z, z.label()).clicked() {
                            self.pan = Vec2::ZERO;
                        }
                    }
                });
                if ui.selectable_label(self.safe, "Safe").on_hover_text(format!("Title/action-safe guides ({})", shortcuts::label(Command::ToggleSafe))).clicked() {
                    self.safe = !self.safe;
                }
                ui.label(RichText::new(format!("{}×{} · {} fps", s.width, s.height, trim_fps(fps))).small().color(pal.text_dim));
            });
        });
        // Transport goes at the bottom; the picture takes the rest.
        let transport_h = 54.0;
        let avail = ui.available_rect_before_wrap();
        let area = Rect::from_min_max(avail.min, pos2(avail.max.x, (avail.max.y - transport_h).max(avail.min.y + 40.0)));
        let resp = ui.allocate_rect(area, Sense::click_and_drag());
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 0.0, pal.viewer);
        let ppp = ui.ctx().pixels_per_point();
        let (pw, ph) = (s.width.max(1) as f32, s.height.max(1) as f32);
        let size = match self.zoom.factor() {
            None => {
                let k = ((area.width() - 16.0) / pw).min((area.height() - 16.0) / ph).max(0.01);
                vec2(pw * k, ph * k)
            }
            Some(f) => vec2(pw * f / ppp, ph * f / ppp),
        };
        if self.zoom == Zoom::Fit {
            self.pan = Vec2::ZERO;
        }
        let img_rect = Rect::from_center_size(area.center() + self.pan, size);
        self.display_px = size.x * ppp;
        let k = size.x / pw; // screen points per project pixel
        let to_screen = |p: [f32; 2]| pos2(img_rect.min.x + p[0] * k, img_rect.min.y + p[1] * k);
        let to_project = |p: Pos2| [(p.x - img_rect.min.x) / k, (p.y - img_rect.min.y) / k];

        match cx.shown.texture {
            Some(tex) => {
                painter.image(tex, img_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
            None => {
                let c = s.background.to_rgba8();
                painter.rect_filled(img_rect, 0.0, Color32::from_rgb(c[0], c[1], c[2]));
            }
        }
        painter.rect_stroke(img_rect, 0.0, Stroke::new(1.0, Color32::from_gray(40)), egui::StrokeKind::Outside);
        if cx.ai_flash > 0.0 {
            let a = (cx.ai_flash * 200.0) as u8;
            painter.rect_stroke(img_rect.expand(2.0), 0.0, Stroke::new(2.0, Color32::from_rgba_unmultiplied(pal.ai.r(), pal.ai.g(), pal.ai.b(), a)), egui::StrokeKind::Outside);
        }
        if self.safe {
            for (frac, alpha) in [(s.action_safe, 90u8), (s.title_safe, 140u8)] {
                let f = frac.clamp(0.1, 1.0) as f32;
                let r = Rect::from_center_size(img_rect.center(), img_rect.size() * f);
                painter.rect_stroke(r, 0.0, Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, alpha)), egui::StrokeKind::Inside);
            }
            let c = img_rect.center();
            let st = Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 90));
            painter.line_segment([c - vec2(10.0, 0.0), c + vec2(10.0, 0.0)], st);
            painter.line_segment([c - vec2(0.0, 10.0), c + vec2(0.0, 10.0)], st);
        }
        // Overlays: timecode and status.
        // Timecode in the frame's bottom-left corner (clear of the selection handles at the top).
        let tc = timecode::timecode(cx.playhead, fps);
        let galley = painter.layout_no_wrap(tc, FontId::monospace(12.0), Color32::from_gray(235));
        let at = pos2(img_rect.left().max(area.left()) + 8.0, img_rect.bottom().min(area.bottom()) - 8.0 - galley.size().y);
        painter.rect_filled(Rect::from_min_size(at, galley.size()).expand(3.0), 3.0, Color32::from_black_alpha(150));
        painter.galley(at, galley, Color32::from_gray(235));
        if cx.shown.draft {
            painter.text(area.right_top() + vec2(-8.0, 6.0), Align2::RIGHT_TOP, "draft", FontId::proportional(11.0), pal.text_dim);
        }
        if cx.shown.loading {
            // (added) The frame at the playhead is still decoding (first look at a long file): say so
            // instead of showing black or an older frame without comment.
            let dots = ".".repeat(1 + (ui.input(|i| i.time) * 3.0) as usize % 3);
            let label = if cx.shown.texture.is_some() { format!("Loading frame{dots}") } else { format!("Loading{dots}") };
            let galley = painter.layout_no_wrap(label, FontId::proportional(13.0), Color32::from_gray(235));
            let at = pos2(img_rect.center().x - galley.size().x / 2.0, img_rect.top().max(area.top()) + 12.0);
            painter.rect_filled(Rect::from_min_size(at, galley.size()).expand2(vec2(10.0, 5.0)), 6.0, Color32::from_black_alpha(170));
            painter.galley(at, galley, Color32::from_gray(235));
        }
        if !cx.warnings.is_empty() {
            let text = format!("⚠ {}", cx.warnings.len());
            let galley = painter.layout_no_wrap(text.clone(), FontId::proportional(12.0), Color32::BLACK);
            let badge = Rect::from_min_size(pos2(img_rect.right() - galley.size().x - 18.0, img_rect.top() + 6.0), galley.size() + vec2(12.0, 4.0));
            painter.rect_filled(badge, 4.0, pal.warn);
            painter.galley(badge.min + vec2(6.0, 2.0), galley, Color32::BLACK);
            let tip = ui.interact(badge, ui.scope_id().with("render-warnings"), Sense::hover());
            tip.on_hover_ui(|ui| {
                ui.set_max_width(420.0);
                ui.label(RichText::new("This frame renders with problems:").strong());
                for w in cx.warnings.iter().take(12) {
                    ui.label(format!("• {w}"));
                }
                if cx.warnings.len() > 12 {
                    ui.label(format!("… and {} more", cx.warnings.len() - 12));
                }
            });
        }
        if cx.playing && cx.perf.target_fps > 0.0 {
            // Dropped-frame indicator (like Resolve's): green while every frame is shown, amber
            // with the real rate while frames are being dropped.
            let dropping = cx.perf.dropped_recent > 0;
            let text = if dropping {
                format!("{:.0} / {:.0} fps", cx.perf.fps.min(cx.perf.target_fps), cx.perf.target_fps)
            } else {
                format!("{:.0} fps", cx.perf.target_fps)
            };
            let galley = painter.layout_no_wrap(text, FontId::monospace(11.0), Color32::from_gray(225));
            let at = pos2(img_rect.left().max(area.left()) + 8.0, img_rect.top().max(area.top()) + 8.0);
            let bg = Rect::from_min_size(at, galley.size() + vec2(14.0, 0.0)).expand(3.0);
            painter.rect_filled(bg, 3.0, Color32::from_black_alpha(150));
            let dot = if dropping { pal.warn } else { Color32::from_rgb(80, 200, 120) };
            painter.circle_filled(pos2(at.x + 4.0, at.y + galley.size().y / 2.0), 3.5, dot);
            painter.galley(at + vec2(14.0, 0.0), galley, Color32::from_gray(225));
            ui.interact(bg, ui.scope_id().with("perf-badge"), Sense::hover()).on_hover_text(if dropping {
                "Frames are being dropped: this machine cannot render the preview in real time at this resolution. Auto quality lowers it; or pick 1/2 or 1/4 (top right)."
            } else {
                "Playback shows every frame."
            });
        }
        if cx.buffering {
            painter.text(area.center_bottom() + vec2(0.0, -12.0), Align2::CENTER_BOTTOM, "Buffering audio…", FontId::proportional(13.0), pal.text_dim);
        }
        if cx.rate != 0.0 && (cx.rate - 1.0).abs() > 1e-9 {
            let label = if cx.rate < 0.0 { format!("◀◀ {:.0}x", -cx.rate) } else { format!("{:.0}x ▶▶", cx.rate) };
            painter.text(area.center_top() + vec2(0.0, 8.0), Align2::CENTER_TOP, label, FontId::proportional(14.0), pal.warn);
        }

        // Selection handles.
        let sel_layer = cx.layers.iter().rev().find(|l| cx.selection.contains(&l.clip)).cloned();
        let drag_corners = self.drag.as_ref().map(transformed_corners);
        if let Some(l) = &sel_layer {
            let corners = drag_corners.unwrap_or(l.corners);
            let pts: Vec<Pos2> = corners.iter().map(|c| to_screen(*c)).collect();
            painter.add(Shape::closed_line(pts.clone(), Stroke::new(1.5, pal.accent)));
            for p in &pts {
                painter.rect_filled(Rect::from_center_size(*p, vec2(8.0, 8.0)), 1.0, Color32::WHITE);
                painter.rect_stroke(Rect::from_center_size(*p, vec2(8.0, 8.0)), 1.0, Stroke::new(1.0, pal.accent), egui::StrokeKind::Outside);
            }
            let (rot_handle, top_mid) = rotate_handle(&pts);
            painter.line_segment([top_mid, rot_handle], Stroke::new(1.0, pal.accent));
            painter.circle_filled(rot_handle, 5.0, Color32::WHITE);
            painter.circle_stroke(rot_handle, 5.0, Stroke::new(1.0, pal.accent));
        }
        if !cx.selection.is_empty() {
            out.want_layers = true;
        }

        // Interaction.
        let shift = ui.input(|i| i.modifiers.shift);
        if resp.clicked() || resp.drag_started() || resp.secondary_clicked() {
            self.focused = true;
        }
        let middle = ui.input(|i| i.pointer.middle_down());
        if middle && resp.hovered() && self.zoom != Zoom::Fit {
            self.pan += ui.input(|i| i.pointer.delta());
        }
        if resp.drag_started_by(egui::PointerButton::Primary) {
            // Where the button went down (the drag is only detected a few pixels later).
            let origin = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos());
            if let (Some(pos), Some(l)) = (origin, &sel_layer) {
                let pts: Vec<Pos2> = l.corners.iter().map(|c| to_screen(*c)).collect();
                let (rot_handle, _) = rotate_handle(&pts);
                let near_corner = pts.iter().any(|p| p.distance(pos) < 9.0);
                let kind = if rot_handle.distance(pos) < 9.0 {
                    Some(DragKind::Rotate)
                } else if near_corner {
                    Some(DragKind::Scale)
                } else if inside_quad(&l.corners, to_project(pos)) {
                    Some(DragKind::Move)
                } else {
                    None
                };
                let clip = cx.project.clips.iter().find(|c| c.id == l.clip);
                if let (Some(kind), Some(clip), Some(span)) = (kind, clip, cx.resolved.span(&l.clip)) {
                    let local = (cx.playhead - span.start).max(0.0);
                    let v0 = params(clip).map(|p| sample(p, local));
                    self.drag = Some(HandleDrag { clip: l.clip.clone(), kind, start: to_project(pos), pivot: l.pivot, v0, cur: v0, corners0: l.corners });
                }
            }
        }
        if let Some(d) = &mut self.drag {
            if let Some(pos) = ui.input(|i| i.pointer.latest_pos()) {
                let v = drag_values(d, to_project(pos), shift);
                if v != d.cur {
                    d.cur = v;
                }
                out.preview = Some(LivePreview { clip: d.clip.clone(), values: d.cur });
            }
            ui.ctx().set_cursor_icon(match d.kind {
                DragKind::Move => egui::CursorIcon::Grabbing,
                DragKind::Scale => egui::CursorIcon::ResizeNwSe,
                DragKind::Rotate => egui::CursorIcon::Alias,
            });
            if !ui.input(|i| i.pointer.primary_down()) {
                let d = self.drag.take().expect("drag");
                out.preview_done = true;
                if let (Some(c), Some(span)) = (cx.project.clips.iter().find(|c| c.id == d.clip), cx.resolved.span(&d.clip)) {
                    let local = (cx.playhead - span.start).clamp(0.0, c.duration);
                    let ops = commit_ops(c, local, d.v0, d.cur);
                    if !ops.is_empty() {
                        let label = match d.kind {
                            DragKind::Move => "Move in frame",
                            DragKind::Scale => "Scale in frame",
                            DragKind::Rotate => "Rotate in frame",
                        };
                        out.actions.push(UiAction::Apply { label: label.into(), ops });
                    }
                }
            }
        } else if let Some(pos) = resp.hover_pos() {
            if let Some(l) = &sel_layer {
                let pts: Vec<Pos2> = l.corners.iter().map(|c| to_screen(*c)).collect();
                let (rot_handle, _) = rotate_handle(&pts);
                if rot_handle.distance(pos) < 9.0 {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Alias);
                } else if pts.iter().any(|p| p.distance(pos) < 9.0) {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);
                } else if inside_quad(&l.corners, to_project(pos)) {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Move);
                }
            }
        }
        if resp.clicked() {
            out.want_layers = true;
            if let Some(pos) = resp.interact_pointer_pos() {
                let hit = pick_layer(cx.layers, to_project(pos)).map(|l| l.clip.clone());
                out.actions.push(UiAction::Select(hit.into_iter().collect()));
            }
        }
        if resp.double_clicked() {
            out.commands.push(Command::PlayPause);
        }
        // Media dragged from the bin.
        let dragging_media = egui::DragAndDrop::has_payload_of_type::<MediaDrag>(ui.ctx());
        if dragging_media && resp.contains_pointer() {
            painter.rect_stroke(area.shrink(2.0), 0.0, Stroke::new(2.0, pal.accent), egui::StrokeKind::Inside);
            painter.text(area.center(), Align2::CENTER_CENTER, "Drop to add at the playhead on a new track", FontId::proportional(15.0), Color32::WHITE);
        }
        if let Some(m) = resp.dnd_release_payload::<MediaDrag>() {
            let ops = crate::edits::drop_on_viewer_ops(cx.project, &m.asset, cx.playhead);
            if !ops.is_empty() {
                out.actions.push(UiAction::Apply { label: "Add to new track".into(), ops });
            }
        }
        // Effects, text and shapes dragged from the effects browser.
        if let Some(b) = egui::DragAndDrop::payload::<BrowserDrag>(ui.ctx()).filter(|_| resp.contains_pointer()) {
            let hint = match &*b {
                BrowserDrag::Effect(_) => "Drop to apply the effect to the clip under the pointer",
                BrowserDrag::Transition(_) => "Transitions go on a cut in the timeline",
                BrowserDrag::Text(_) | BrowserDrag::Shape(_) => "Drop to add at the playhead",
            };
            painter.rect_stroke(area.shrink(2.0), 0.0, Stroke::new(2.0, pal.accent), egui::StrokeKind::Inside);
            painter.text(area.center(), Align2::CENTER_CENTER, hint, FontId::proportional(15.0), Color32::WHITE);
            want_layers_now(&mut out);
        }
        if let Some(b) = resp.dnd_release_payload::<BrowserDrag>() {
            match &*b {
                BrowserDrag::Effect(kind) => {
                    let under = resp.interact_pointer_pos().or(resp.hover_pos()).and_then(|p| pick_layer(cx.layers, to_project(p))).map(|l| l.clip.clone());
                    let target = under.or_else(|| cx.selection.iter().find(|i| cx.project.clips.iter().any(|c| &c.id == *i)).cloned());
                    match target.and_then(|clip| Op::from_json(&json!({"op": "add_effect", "clip": clip, "effect": kind})).ok()) {
                        Some(op) => out.actions.push(UiAction::Apply { label: format!("Add {kind}"), ops: vec![op] }),
                        None => out.actions.push(UiAction::Status("Drop the effect on a clip in the frame (or select one first)".into())),
                    }
                }
                BrowserDrag::Text(p) => {
                    let ops = aiframes_ui::edits::text_preset_ops(cx.project, cx.resolved, *p, cx.playhead, None);
                    if !ops.is_empty() {
                        out.actions.push(UiAction::Apply { label: format!("Add {}", p.label()), ops });
                    }
                }
                BrowserDrag::Shape(p) => {
                    let ops = aiframes_ui::edits::shape_preset_ops(cx.project, cx.resolved, *p, cx.playhead, None);
                    if !ops.is_empty() {
                        out.actions.push(UiAction::Apply { label: format!("Add {}", p.label()), ops });
                    }
                }
                BrowserDrag::Transition(_) => out.actions.push(UiAction::Status("Drop transitions on a cut between two clips in the timeline".into())),
            }
        }
        resp.context_menu(|ui| {
            for z in Zoom::ALL {
                if ui.radio(self.zoom == z, z.label()).clicked() {
                    self.zoom = z;
                    self.pan = Vec2::ZERO;
                    ui.close();
                }
            }
            ui.separator();
            if ui.checkbox(&mut self.safe, "Safe-area guides").clicked() {
                ui.close();
            }
        });

        // Transport.
        let trect = Rect::from_min_max(pos2(avail.min.x, area.max.y), avail.max);
        ui.scope_builder(egui::UiBuilder::new().max_rect(trect.shrink2(vec2(4.0, 4.0))), |ui| {
            self.transport(ui, cx, &mut out);
        });
        out
    }

    fn transport(&mut self, ui: &mut egui::Ui, cx: &ViewerCx, out: &mut ViewerOut) {
        let pal = theme::palette();
        let fps = cx.project.settings.fps;
        ui.horizontal(|ui| {
            let f = |v: Option<f64>| v.map(|v| timecode::timecode(v, fps)).unwrap_or_else(|| "--:--:--:--".into());
            ui.label(RichText::new(format!("In {}", f(cx.in_point))).small().monospace().color(pal.text_dim));
            ui.label(RichText::new(format!("Out {}", f(cx.out_point))).small().monospace().color(pal.text_dim));
            if let (Some(a), Some(b)) = (cx.in_point, cx.out_point) {
                ui.label(RichText::new(format!("({})", timecode::short_duration(b - a))).small().color(pal.text_dim));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("/ {}", timecode::timecode(cx.resolved.duration, fps))).small().monospace().color(pal.text_dim));
                let tc = timecode::timecode(cx.playhead, fps);
                match &mut self.tc_edit {
                    Some(buf) => {
                        let r = ui.add(egui::TextEdit::singleline(buf).desired_width(96.0).font(egui::TextStyle::Monospace));
                        if r.lost_focus() {
                            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                if let Some(t) = timecode::parse_time(buf, fps) {
                                    out.actions.push(UiAction::Seek(t));
                                }
                            }
                            self.tc_edit = None;
                        } else {
                            r.request_focus();
                        }
                    }
                    None => {
                        let r = ui.add(egui::Label::new(RichText::new(&tc).monospace().strong().color(pal.accent)).sense(Sense::click()));
                        if r.on_hover_text("Click to go to a time (timecode, seconds or frames like 45f)").clicked() {
                            self.tc_edit = Some(tc);
                        }
                    }
                }
            });
        });
        ui.horizontal(|ui| {
            let btn = |ui: &mut egui::Ui, text: &str, tip: &str, cmd: Command, on: bool| -> bool {
                let b = egui::Button::new(RichText::new(text).size(14.0)).selected(on).min_size(vec2(28.0, 22.0));
                let sc = shortcuts::label(cmd);
                let tip = if sc.is_empty() { tip.to_string() } else { format!("{tip} ({sc})") };
                ui.add(b).on_hover_text(tip).clicked()
            };
            let w = 7.0 * 32.0 + 3.0 * 32.0 + 20.0;
            let pad = ((ui.available_width() - w - 150.0) / 2.0).max(0.0);
            if btn(ui, "{", "Set in", Command::SetIn, false) {
                out.commands.push(Command::SetIn);
            }
            if btn(ui, "}", "Set out", Command::SetOut, false) {
                out.commands.push(Command::SetOut);
            }
            ui.add_space(pad);
            if btn(ui, "⏮", "Go to start", Command::GoStart, false) {
                out.commands.push(Command::GoStart);
            }
            if btn(ui, "⏴|", "Previous frame", Command::FrameBack, false) {
                out.commands.push(Command::FrameBack);
            }
            if btn(ui, "◀", "Play backward", Command::ShuttleReverse, cx.rate < 0.0) {
                out.commands.push(Command::ShuttleReverse);
            }
            let play = if cx.playing { "⏸" } else { "▶" };
            if btn(ui, play, "Play / pause", Command::PlayPause, cx.playing && cx.rate > 0.0) {
                out.commands.push(Command::PlayPause);
            }
            if btn(ui, "|⏵", "Next frame", Command::FrameForward, false) {
                out.commands.push(Command::FrameForward);
            }
            if btn(ui, "⏭", "Go to end", Command::GoEnd, false) {
                out.commands.push(Command::GoEnd);
            }
            if btn(ui, "⟲", "Loop playback", Command::ToggleLoop, cx.looping) {
                out.commands.push(Command::ToggleLoop);
            }
            if btn(ui, "⇥", "Play in to out", Command::PlayFromIn, false) {
                out.commands.push(Command::PlayFromIn);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut v = cx.volume;
                let mut muted = cx.muted;
                let r = ui.add(egui::Slider::new(&mut v, 0.0..=1.5).show_value(false)).on_hover_text(format!("Volume {:.0}%", v * 100.0));
                if ui.selectable_label(muted, if muted { "🔇" } else { "🔊" }).on_hover_text("Mute").clicked() {
                    muted = !muted;
                    out.volume = Some((v, muted));
                }
                if r.changed() {
                    out.volume = Some((v, muted));
                }
            });
        });
    }
}

fn trim_fps(fps: f64) -> String {
    if (fps - fps.round()).abs() < 1e-6 {
        format!("{}", fps.round() as i64)
    } else {
        format!("{fps:.3}").trim_end_matches('0').to_string()
    }
}

fn want_layers_now(out: &mut ViewerOut) {
    out.want_layers = true;
}

/// Rotation handle position (above the top edge's midpoint, outward) and the top midpoint.
fn rotate_handle(pts: &[Pos2]) -> (Pos2, Pos2) {
    let top_mid = pos2((pts[0].x + pts[1].x) / 2.0, (pts[0].y + pts[1].y) / 2.0);
    let center = pos2(pts.iter().map(|p| p.x).sum::<f32>() / 4.0, pts.iter().map(|p| p.y).sum::<f32>() / 4.0);
    let dir = (top_mid - center).normalized();
    let dir = if dir.x.is_finite() && dir.y.is_finite() && dir.length() > 0.5 { dir } else { vec2(0.0, -1.0) };
    (top_mid + dir * 24.0, top_mid)
}

/// Corners of the dragged box (moved/scaled/rotated around the pivot) for the outline.
fn transformed_corners(d: &HandleDrag) -> [[f32; 2]; 4] {
    let (dx, dy) = ((d.cur[0] - d.v0[0]) as f32, (d.cur[1] - d.v0[1]) as f32);
    let s = if d.v0[2].abs() > 1e-9 { (d.cur[2] / d.v0[2]) as f32 } else { 1.0 };
    let r = ((d.cur[3] - d.v0[3]) as f32).to_radians();
    let (sn, cs) = r.sin_cos();
    d.corners0.map(|c| {
        let (x, y) = ((c[0] - d.pivot[0]) * s, (c[1] - d.pivot[1]) * s);
        [d.pivot[0] + x * cs - y * sn + dx, d.pivot[1] + x * sn + y * cs + dy]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(json: serde_json::Value) -> Clip {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn quads() {
        let q = [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        assert!(inside_quad(&q, [5.0, 5.0]));
        assert!(!inside_quad(&q, [11.0, 5.0]));
        let rot = [[5.0, -2.0], [12.0, 5.0], [5.0, 12.0], [-2.0, 5.0]];
        assert!(inside_quad(&rot, [5.0, 5.0]));
        assert!(!inside_quad(&rot, [0.0, 0.0]));
        let layers = vec![
            LayerBox { clip: "bottom".into(), corners: q, bounds: [0.0, 0.0, 10.0, 10.0], pivot: [5.0, 5.0] },
            LayerBox { clip: "top".into(), corners: [[4.0, 4.0], [6.0, 4.0], [6.0, 6.0], [4.0, 6.0]], bounds: [4.0, 4.0, 2.0, 2.0], pivot: [5.0, 5.0] },
        ];
        assert_eq!(pick_layer(&layers, [5.0, 5.0]).unwrap().clip, "top");
        assert_eq!(pick_layer(&layers, [1.0, 1.0]).unwrap().clip, "bottom");
        assert!(pick_layer(&layers, [20.0, 1.0]).is_none());
    }

    #[test]
    fn drags() {
        let d = HandleDrag { clip: "c1".into(), kind: DragKind::Move, start: [100.0, 100.0], pivot: [50.0, 50.0], v0: [0.0, 0.0, 1.0, 0.0], cur: [0.0, 0.0, 1.0, 0.0], corners0: [[0.0; 2]; 4] };
        assert_eq!(drag_values(&d, [130.0, 90.0], false), [30.0, -10.0, 1.0, 0.0]);
        assert_eq!(drag_values(&d, [130.0, 90.0], true), [30.0, 0.0, 1.0, 0.0], "shift constrains to an axis");
        let s = HandleDrag { kind: DragKind::Scale, ..d.clone() };
        let v = drag_values(&s, [150.0, 150.0], false);
        assert!((v[2] - 2.0).abs() < 1e-6, "{v:?}");
        let r = HandleDrag { kind: DragKind::Rotate, start: [100.0, 50.0], ..d.clone() };
        let v = drag_values(&r, [50.0, 100.0], false);
        assert!((v[3] - 90.0).abs() < 1e-4, "clockwise on screen is positive: {v:?}");
        let v = drag_values(&r, [99.0, 60.0], true);
        assert_eq!(v[3] % 15.0, 0.0);
    }

    #[test]
    fn commits_values_or_keys() {
        let c = clip(json!({"id": "c1", "track": "v1", "start": 0.0, "duration": 5.0, "source": {"type": "solid", "color": "#FF0000"},
            "transform": {"scale": {"keys": [{"t": 0.0, "v": 1.0}, {"t": 4.0, "v": 2.0}]}}}));
        let ops = commit_ops(&c, 2.0, [0.0, 0.0, 1.5, 0.0], [12.0, 0.0, 1.8, 0.0]);
        assert_eq!(ops.len(), 2);
        let j: Vec<serde_json::Value> = ops.iter().map(|o| serde_json::to_value(o).unwrap()).collect();
        assert_eq!(j[0]["op"], "set_param");
        assert_eq!(j[0]["path"], "transform.x");
        assert_eq!(j[0]["value"], 12.0);
        assert_eq!(j[1]["op"], "add_keyframe");
        assert_eq!(j[1]["path"], "transform.scale");
        assert_eq!(j[1]["t"], 2.0);
        assert!(commit_ops(&c, 2.0, [1.0, 2.0, 3.0, 4.0], [1.0, 2.0, 3.0, 4.0]).is_empty());
    }

    #[test]
    fn outline_follows_drag() {
        let d = HandleDrag {
            clip: "c".into(),
            kind: DragKind::Scale,
            start: [0.0, 0.0],
            pivot: [5.0, 5.0],
            v0: [0.0, 0.0, 1.0, 0.0],
            cur: [0.0, 0.0, 2.0, 0.0],
            corners0: [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]],
        };
        assert_eq!(transformed_corners(&d)[0], [-5.0, -5.0]);
        assert_eq!(trim_fps(29.97), "29.97");
        assert_eq!(trim_fps(30.0), "30");
    }
}
