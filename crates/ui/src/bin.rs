//! The media bin: grid (poster frames at the tile's physical size, hover scrubbing, duration, kind,
//! transcript and "on timeline" badges) or list view; search and kind filter; import; drag out as
//! [`MediaDrag`]; double-click opens the file in the Source monitor; context menu (open, add, append,
//! transcribe, rename, notes, reveal, relink, remove).

use crate::style::{self, alpha, rgb};
use crate::util;
use crate::{edits, EditCtx, MediaDrag, UiAction};
use aiframes_core::{Id, MediaAsset, MediaKind, Op, Project, Source};
use egui::{pos2, vec2, Align2, Color32, CornerRadius, FontId, Rect, RichText, Sense, Stroke, StrokeKind, Ui};

/// The media bin: grid/list of assets with thumbnails, duration, kind badge, transcript badge,
/// notes; search; import button; drag to the timeline (`MediaDrag`); context menu (transcribe,
/// reveal, relink, remove, add to timeline).
pub struct MediaBin {
    /// (added)
    pub search: String,
    /// (added) Show only this kind.
    pub filter: Option<MediaKind>,
    /// (added)
    pub list_view: bool,
    /// (added) Grid tile width.
    pub tile: f32,
    edit: Option<BinEdit>,
}

impl Default for MediaBin {
    fn default() -> Self {
        MediaBin { search: String::new(), filter: None, list_view: false, tile: 150.0, edit: None }
    }
}

struct BinEdit {
    asset: Id,
    notes: bool,
    text: String,
    opened: bool,
}

pub(crate) fn kind_color(kind: MediaKind) -> Color32 {
    match kind {
        MediaKind::Video => style::clip_color(util::ClipKind::Video),
        MediaKind::Audio => style::clip_color(util::ClipKind::Audio),
        MediaKind::Image => style::clip_color(util::ClipKind::Image),
    }
}

fn kind_tag(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Video => "VID",
        MediaKind::Audio => "AUD",
        MediaKind::Image => "IMG",
    }
}

/// Clips using the asset.
pub(crate) fn usage(p: &Project, asset: &str) -> usize {
    p.clips.iter().filter(|c| matches!(&c.source, Source::Media { asset: a, .. } if a == asset)).count()
}

/// Assets matching the search (name, file, notes, tags) and kind filter, in project order.
pub(crate) fn filter_media<'a>(p: &'a Project, search: &str, kind: Option<MediaKind>) -> Vec<&'a MediaAsset> {
    let q = search.trim().to_lowercase();
    p.media
        .iter()
        .filter(|m| kind.is_none_or(|k| m.kind == k))
        .filter(|m| {
            q.is_empty()
                || util::media_name(m).to_lowercase().contains(&q)
                || m.path.to_lowercase().contains(&q)
                || m.notes.to_lowercase().contains(&q)
                || m.tags.iter().any(|t| t.to_lowercase().contains(&q))
                || m.id.to_lowercase() == q
        })
        .collect()
}

/// Frames prefetched across a video when the pointer enters its bin tile (hover scrubbing).
const SCRUB_STRIP: usize = 24;

/// The hover-scrub moment for a pointer at `frac` (0..1) across a tile `px` physical pixels wide:
/// quantized to the pixel (or the 0.1 s thumbnail grid if finer), so moving within one pixel asks
/// for the same frame.
pub(crate) fn scrub_time(frac: f64, duration: f64, px: f32) -> f64 {
    if duration.is_nan() || duration <= 0.0 || !frac.is_finite() {
        return 0.0;
    }
    let step = (duration / px.max(1.0) as f64).max(0.1);
    let t = frac.clamp(0.0, 1.0) * duration;
    ((t / step).floor() * step).clamp(0.0, (duration - 1e-3).max(0.0))
}

/// An audio file's waveform filling `rect` (dB scale, peaks and RMS, one column per physical pixel).
pub(crate) fn paint_wave_tile(p: &egui::Painter, w: &aiframes_media::Waveform, duration: f64, rect: Rect, color: Color32) {
    let dur = w.duration.max(duration).max(1e-3);
    let ppp = p.ctx().pixels_per_point();
    let r = rect.shrink2(vec2(0.0, rect.height() * 0.08));
    let look = crate::timeline::wave::WaveLook { color, log: true, rectified: false, ppp };
    crate::timeline::wave::paint(p, r, rect.left(), rect.right(), look, |a, b| {
        let t0 = dur * ((a - rect.left()) / rect.width()) as f64;
        let t1 = dur * ((b - rect.left()) / rect.width()) as f64;
        let (mn, mx) = w.range(t0, t1);
        Some(crate::timeline::wave::Column { min: mn, max: mx, rms: w.rms_range(t0, t1) })
    });
}

/// The file's picture (or waveform for audio, or a placeholder) filling `rect`: its poster frame
/// (`t` is kept for existing callers, which all ask for the representative frame).
pub(crate) fn paint_thumb(painter: &egui::Painter, cx: &EditCtx, m: &MediaAsset, rect: Rect, t: f64) {
    let _ = t;
    paint_media(painter, cx, m, rect, None);
}

/// A thumbnail (or waveform for audio, or a placeholder) filling `rect`: the poster frame, or the
/// frame at `t` (hover scrubbing). Decoded at the rect's physical size.
pub(crate) fn paint_media(painter: &egui::Painter, cx: &EditCtx, m: &MediaAsset, rect: Rect, t: Option<f64>) {
    let p = painter.with_clip_rect(rect.intersect(painter.clip_rect()));
    p.rect_filled(rect, CornerRadius::same(3), rgb(0x0F, 0x10, 0x12));
    let px = rect.height() * p.ctx().pixels_per_point();
    match m.kind {
        MediaKind::Audio => {
            p.rect_filled(rect, CornerRadius::same(3), alpha(kind_color(m.kind), 0.35));
            if let Some(w) = cx.preview.waveform(m) {
                paint_wave_tile(&p, &w, m.info.duration, rect, style::wave_color(util::ClipKind::Audio));
            } else {
                style::speaker_glyph(&p, rect.center(), alpha(Color32::WHITE, 0.5));
            }
        }
        _ => {
            let tex = match t {
                Some(t) if m.kind == MediaKind::Video => cx.preview.thumbnail_px(m, t.clamp(0.0, (m.info.duration - 1e-3).max(0.0)), px, f64::INFINITY),
                _ => cx.preview.poster(m, px),
            };
            match tex {
                Some(tex) => {
                    // Fit the picture's aspect inside the rect (info is the display size: rotation applied).
                    let aspect = if m.info.width > 0 && m.info.height > 0 { m.info.width as f32 / m.info.height as f32 } else { 16.0 / 9.0 };
                    let r = if rect.width() / rect.height() > aspect {
                        Rect::from_center_size(rect.center(), vec2(rect.height() * aspect, rect.height()))
                    } else {
                        Rect::from_center_size(rect.center(), vec2(rect.width(), rect.width() / aspect))
                    };
                    p.image(tex, r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                }
                None if cx.preview.failed(m) => {
                    p.rect_filled(rect.shrink(1.0), CornerRadius::same(3), alpha(style::ERROR, 0.18));
                    style::film_glyph(&p, rect.center() - vec2(0.0, 7.0), alpha(style::ERROR, 0.8));
                    if rect.height() >= 40.0 {
                        p.text(rect.center() + vec2(0.0, 6.0), Align2::CENTER_CENTER, "no preview", FontId::proportional(10.0), alpha(Color32::WHITE, 0.7));
                    }
                }
                None => {
                    p.rect_filled(rect.shrink(1.0), CornerRadius::same(3), alpha(kind_color(m.kind), 0.25));
                    style::film_glyph(&p, rect.center(), alpha(Color32::WHITE, 0.4));
                }
            }
        }
    }
}

impl MediaBin {
    pub fn ui(&mut self, ui: &mut Ui, cx: &EditCtx) -> Vec<UiAction> {
        let mut out = Vec::new();
        let p = cx.project;
        // Toolbar.
        ui.horizontal(|ui| {
            if ui.button("+ Import").on_hover_text("Import media files (or drop files from Explorer on the window)").clicked() {
                out.push(UiAction::Import(vec![]));
            }
            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search media").desired_width(ui.available_width().min(160.0) - 60.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.selectable_label(self.list_view, "List").on_hover_text("List view").clicked() {
                    self.list_view = true;
                }
                if ui.selectable_label(!self.list_view, "Grid").on_hover_text("Grid view").clicked() {
                    self.list_view = false;
                }
            });
        });
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (k, l) in [(None, "All"), (Some(MediaKind::Video), "Video"), (Some(MediaKind::Audio), "Audio"), (Some(MediaKind::Image), "Images")] {
                let n = p.media.iter().filter(|m| k.is_none_or(|k| m.kind == k)).count();
                if ui.selectable_label(self.filter == k, RichText::new(format!("{l} {n}")).size(11.0)).clicked() {
                    self.filter = k;
                }
            }
            if !self.list_view {
                ui.add_space(6.0);
                ui.add(egui::Slider::new(&mut self.tile, 90.0..=260.0).show_value(false)).on_hover_text("Thumbnail size");
            }
        });
        ui.separator();
        let items = filter_media(p, &self.search, self.filter);
        if p.media.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No media yet").strong());
                ui.label(RichText::new("Import video, audio and images, or drop files on the window.").weak());
                if ui.button("Import media…").clicked() {
                    out.push(UiAction::Import(vec![]));
                }
            });
            return out;
        }
        if items.is_empty() {
            ui.label(RichText::new("Nothing matches the search.").weak());
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("aiframes_bin").show(ui, |ui| {
            if self.list_view {
                for m in &items {
                    let r = self.list_row(ui, cx, m);
                    self.item_interaction(ui, cx, m, &r, &mut out);
                }
            } else {
                let gap = 8.0;
                let w = ui.available_width();
                let cols = ((w + gap) / (self.tile + gap)).floor().max(1.0) as usize;
                let tile = ((w + gap) / cols as f32 - gap).max(60.0);
                for row in items.chunks(cols) {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        for m in row {
                            let r = self.tile_ui(ui, cx, m, tile);
                            self.item_interaction(ui, cx, m, &r, &mut out);
                        }
                    });
                    ui.add_space(gap * 0.5);
                }
            }
        });
        self.edit_popup(ui, cx, &mut out);
        out
    }

    fn tile_ui(&self, ui: &mut Ui, cx: &EditCtx, m: &MediaAsset, w: f32) -> egui::Response {
        let thumb_h = (w * 9.0 / 16.0).round();
        let (rect, resp) = ui.allocate_exact_size(vec2(w, thumb_h + 32.0), Sense::click_and_drag());
        let selected = cx.is_selected(&m.id);
        let p = ui.painter_at(rect.expand(4.0));
        if selected || resp.hovered() {
            p.rect_filled(rect.expand(2.0), CornerRadius::same(5), if selected { alpha(style::ACCENT, 0.18) } else { alpha(Color32::WHITE, 0.04) });
        }
        let thumb = Rect::from_min_size(rect.min, vec2(w, thumb_h));
        // Hover scrubbing: the frame under the pointer (quantized to a pixel), with a strip across
        // the file prefetched so moving around is instant.
        let ppp = ui.ctx().pixels_per_point();
        let scrub = match resp.hover_pos() {
            Some(pos) if m.kind == MediaKind::Video && thumb.contains(pos) && m.info.duration > 0.0 => {
                let px = thumb.height() * ppp;
                let strip: Vec<f64> = (0..SCRUB_STRIP).map(|i| scrub_time((i as f64 + 0.5) / SCRUB_STRIP as f64, m.info.duration, thumb.width() * ppp)).collect();
                cx.preview.prefetch(m, &strip, px);
                Some(scrub_time(((pos.x - thumb.left()) / thumb.width()) as f64, m.info.duration, thumb.width() * ppp))
            }
            _ => None,
        };
        let t = scrub.unwrap_or(0.0);
        paint_media(&ui.painter_at(thumb), cx, m, thumb, scrub);
        if resp.hovered() && m.kind == MediaKind::Video {
            if let Some(pos) = resp.hover_pos().filter(|p| thumb.contains(*p)) {
                p.vline(pos.x, thumb.y_range(), Stroke::new(1.0, alpha(style::PLAYHEAD, 0.8)));
                p.text(thumb.left_top() + vec2(4.0, thumb.height() - 4.0), Align2::LEFT_BOTTOM, util::timecode(t, util::fps(cx.project)), FontId::monospace(9.5), Color32::WHITE);
            }
        }
        // Badges.
        style::badge(&p, thumb.left_top() + vec2(4.0, 4.0), kind_tag(m.kind), alpha(kind_color(m.kind), 0.95), Color32::WHITE, false);
        if cx.project.transcripts.contains_key(&m.id) {
            style::badge(&p, pos2(thumb.right() - 4.0, thumb.top() + 4.0), "T", alpha(rgb(0x2C, 0x7A, 0x5A), 0.95), Color32::WHITE, true);
        }
        if m.kind != MediaKind::Image {
            style::badge(&p, pos2(thumb.right() - 4.0, thumb.bottom() - 16.0), &util::duration_label(m.info.duration), alpha(Color32::BLACK, 0.75), Color32::WHITE, true);
        }
        let n = usage(cx.project, &m.id);
        if n > 0 && !resp.hovered() {
            style::badge(&p, pos2(thumb.left() + 4.0, thumb.bottom() - 16.0), &format!("on timeline ×{n}"), alpha(style::ACCENT, 0.85), Color32::WHITE, false);
        }
        p.rect_stroke(thumb, CornerRadius::same(3), if selected { Stroke::new(1.5, style::ACCENT) } else { Stroke::new(1.0, rgb(0x2A, 0x2B, 0x30)) }, StrokeKind::Inside);
        if cx.is_ai(&m.id) {
            style::glow(&p, thumb, style::AI, 0.35 + 0.65 * cx.ai_flash);
        }
        // Name and info.
        let strong = ui.visuals().strong_text_color();
        let weak = ui.visuals().weak_text_color();
        let tp = ui.painter_at(Rect::from_min_size(pos2(rect.left(), thumb.bottom()), vec2(w, 32.0)));
        tp.text(pos2(rect.left() + 2.0, thumb.bottom() + 4.0), Align2::LEFT_TOP, util::media_name(m), FontId::proportional(11.5), strong);
        let mut info = match m.kind {
            MediaKind::Audio => format!("{} Hz · {} ch", m.info.audio_rate, m.info.audio_channels),
            _ if m.info.width > 0 => format!("{}×{}{}", m.info.width, m.info.height, if m.info.fps > 0.0 { format!(" · {} fps", trim(m.info.fps)) } else { String::new() }),
            _ => String::new(),
        };
        if m.info.audio_streams.len() > 1 {
            info.push_str(&format!(" · {} audio", m.info.audio_streams.len()));
        }
        tp.text(pos2(rect.left() + 2.0, thumb.bottom() + 18.0), Align2::LEFT_TOP, info, FontId::proportional(10.0), weak);
        resp
    }

    fn list_row(&self, ui: &mut Ui, cx: &EditCtx, m: &MediaAsset) -> egui::Response {
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 28.0), Sense::click_and_drag());
        let selected = cx.is_selected(&m.id);
        let p = ui.painter_at(rect);
        if selected {
            p.rect_filled(rect, CornerRadius::same(3), alpha(style::ACCENT, 0.2));
        } else if resp.hovered() {
            p.rect_filled(rect, CornerRadius::same(3), alpha(Color32::WHITE, 0.04));
        }
        let thumb = Rect::from_min_size(rect.min + vec2(2.0, 2.0), vec2(42.0, 24.0));
        paint_media(&ui.painter_at(thumb), cx, m, thumb, None);
        let strong = ui.visuals().strong_text_color();
        let weak = ui.visuals().weak_text_color();
        let x = thumb.right() + 8.0;
        style::badge(&p, pos2(x, rect.top() + 8.0), kind_tag(m.kind), alpha(kind_color(m.kind), 0.95), Color32::WHITE, false);
        let name_w = (w - 300.0).max(60.0);
        p.with_clip_rect(Rect::from_min_size(pos2(x + 30.0, rect.top()), vec2(name_w, rect.height()))).text(pos2(x + 30.0, rect.center().y), Align2::LEFT_CENTER, util::media_name(m), FontId::proportional(11.5), strong);
        let mut cx_ = x + 34.0 + name_w;
        let cols = [
            if m.kind == MediaKind::Image { "still".to_string() } else { util::duration_label(m.info.duration) },
            match (m.info.width > 0, m.info.audio_streams.len()) {
                (true, n) if n > 1 => format!("{}×{} · {n} audio", m.info.width, m.info.height),
                (true, _) => format!("{}×{}", m.info.width, m.info.height),
                (false, n) if n > 1 => format!("{n} audio streams"),
                (false, _) => format!("{} Hz", m.info.audio_rate),
            },
            if cx.project.transcripts.contains_key(&m.id) { "transcript".into() } else { String::new() },
            match usage(cx.project, &m.id) {
                0 => String::new(),
                n => format!("×{n}"),
            },
        ];
        for (i, c) in cols.iter().enumerate() {
            p.text(pos2(cx_, rect.center().y), Align2::LEFT_CENTER, c, FontId::proportional(10.5), if i == 3 { style::ACCENT } else { weak });
            cx_ += [56.0, 76.0, 70.0, 30.0][i];
        }
        if cx.is_ai(&m.id) {
            style::glow(&p, rect.shrink(1.0), style::AI, 0.35 + 0.65 * cx.ai_flash);
        }
        resp
    }

    fn item_interaction(&mut self, ui: &Ui, cx: &EditCtx, m: &MediaAsset, resp: &egui::Response, out: &mut Vec<UiAction>) {
        let p = cx.project;
        resp.dnd_set_drag_payload(MediaDrag { asset: m.id.clone() });
        if resp.dragged() {
            if let Some(pos) = ui.ctx().pointer_interact_pos() {
                let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique("aiframes_bin_drag")));
                // Above-left of the pointer: the timeline shows its drop info below-right.
                let r = Rect::from_min_size(pos + vec2(-108.0, -76.0), vec2(96.0, 54.0));
                painter.rect_filled(r.expand(2.0), CornerRadius::same(4), alpha(Color32::BLACK, 0.8));
                paint_media(&painter, cx, m, r, None);
                painter.text(r.left_bottom() + vec2(0.0, 4.0), Align2::LEFT_TOP, util::media_name(m), FontId::proportional(10.5), Color32::WHITE);
            }
        }
        if resp.clicked() {
            let mods = ui.input(|i| i.modifiers);
            let mut sel: Vec<Id> = if mods.command || mods.shift { cx.selection.to_vec() } else { vec![] };
            if mods.command && sel.contains(&m.id) {
                sel.retain(|s| s != &m.id);
            } else if !sel.contains(&m.id) {
                sel.push(m.id.clone());
            }
            out.push(UiAction::Select(sel));
        }
        if resp.double_clicked() {
            // Like Premiere and Resolve: double-click opens the file in the Source monitor.
            out.push(UiAction::OpenSource(m.id.clone()));
        }
        let notes = if m.notes.is_empty() { String::new() } else { format!("\n“{}”", util::ellipsize(&m.notes, 120)) };
        let tags = if m.tags.is_empty() { String::new() } else { format!("\ntags: {}", m.tags.join(", ")) };
        let resp = resp.clone().on_hover_text(format!("{} ({})\n{}{notes}{tags}\nDouble-click opens it in the Source monitor · drag to the timeline", util::media_name(m), m.id, m.path));
        resp.context_menu(|ui| {
            ui.set_min_width(200.0);
            ui.label(RichText::new(util::media_name(m)).small().weak());
            if ui.button("Open in Source monitor").clicked() {
                out.push(UiAction::OpenSource(m.id.clone()));
                ui.close();
            }
            if ui.button("Add to timeline at playhead").clicked() {
                out.push(UiAction::Apply { label: format!("Add {}", util::media_name(m)), ops: edits::add_media_at_ops(p, cx.resolved, &m.id, cx.playhead) });
                ui.close();
            }
            if ui.button("Append to the end").clicked() {
                out.push(UiAction::Apply { label: format!("Append {}", util::media_name(m)), ops: edits::append_media_ops(p, &m.id) });
                ui.close();
            }
            let has_sound = m.kind == MediaKind::Audio || m.info.has_audio;
            if has_sound {
                let label = if p.transcripts.contains_key(&m.id) { "Re-transcribe" } else { "Transcribe" };
                if m.info.audio_streams.len() > 1 {
                    // Pick the stream with the speech (the mic of an OBS recording).
                    ui.menu_button(label, |ui| {
                        let speech = m.info.speech_stream();
                        for (k, s) in m.info.audio_streams.iter().enumerate() {
                            let text = format!("Stream {}: {}{}", k + 1, s.describe(), if k as u32 == speech { "  (speech)" } else { "" });
                            if ui.button(text).clicked() {
                                out.push(UiAction::TranscribeStream(m.id.clone(), k as u32));
                                ui.close();
                            }
                        }
                    });
                } else if ui.button(label).clicked() {
                    out.push(UiAction::Transcribe(m.id.clone()));
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("Rename…").clicked() {
                self.edit = Some(BinEdit { asset: m.id.clone(), notes: false, text: m.name.clone(), opened: false });
                ui.close();
            }
            if ui.button("Edit notes…").clicked() {
                self.edit = Some(BinEdit { asset: m.id.clone(), notes: true, text: m.notes.clone(), opened: false });
                ui.close();
            }
            if ui.button("Reveal in Explorer").clicked() {
                out.push(UiAction::RevealMedia(m.id.clone()));
                ui.close();
            }
            if ui.button("Relink…").clicked() {
                out.push(UiAction::RelinkMedia(m.id.clone()));
                ui.close();
            }
            if ui.button("Copy id").clicked() {
                ui.ctx().copy_text(m.id.clone());
                ui.close();
            }
            ui.separator();
            let n = usage(p, &m.id);
            if n == 0 {
                if ui.button("Remove from project").clicked() {
                    out.push(UiAction::Apply { label: format!("Remove {}", util::media_name(m)), ops: vec![Op::RemoveMedia { asset: m.id.clone(), force: false }] });
                    ui.close();
                }
            } else {
                ui.add_enabled(false, egui::Button::new(format!("Used by {n} clip(s)")));
                if ui.button(RichText::new(format!("Remove media and its {n} clip(s)")).color(style::ERROR)).clicked() {
                    out.push(UiAction::Apply { label: format!("Remove {} and its clips", util::media_name(m)), ops: vec![Op::RemoveMedia { asset: m.id.clone(), force: true }] });
                    ui.close();
                }
            }
            if ui.button("Ask AI about this media…").clicked() {
                out.push(UiAction::AskAi { prompt: format!("About media {} (\"{}\"): ", m.id, util::media_name(m)), ids: vec![m.id.clone()] });
                ui.close();
            }
        });
    }

    fn edit_popup(&mut self, ui: &mut Ui, cx: &EditCtx, out: &mut Vec<UiAction>) {
        let Some(e) = &mut self.edit else { return };
        let mut done = None;
        let title = if e.notes { "Notes" } else { "Rename" };
        egui::Window::new(title).id(egui::Id::unique(("aiframes_bin_edit", &e.asset))).collapsible(false).resizable(false).default_width(260.0).show(ui.ctx(), |ui| {
            let r = if e.notes { ui.add(egui::TextEdit::multiline(&mut e.text).desired_rows(4).desired_width(250.0)) } else { ui.add(egui::TextEdit::singleline(&mut e.text).desired_width(250.0)) };
            if !e.opened {
                r.request_focus();
                e.opened = true;
            }
            if !e.notes && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                done = Some(true);
            }
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    done = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    done = Some(false);
                }
            });
        });
        match done {
            Some(true) => {
                let op = if e.notes {
                    Op::SetMedia { asset: e.asset.clone(), name: None, notes: Some(e.text.clone()), tags: None }
                } else {
                    Op::SetMedia { asset: e.asset.clone(), name: Some(e.text.trim().to_string()), notes: None, tags: None }
                };
                out.push(UiAction::Apply { label: title.into(), ops: vec![op] });
                self.edit = None;
            }
            Some(false) => self.edit = None,
            None => {}
        }
        let _ = cx;
    }
}

fn trim(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::tests::*;

    #[test]
    fn hover_scrub_times() {
        // 20 s across a 300 px tile: one moment per pixel (1/15 s, below the 0.1 s grid -> 0.1 s).
        assert_eq!(scrub_time(0.0, 20.0, 300.0), 0.0);
        assert!((scrub_time(0.5, 20.0, 300.0) - 10.0).abs() < 1e-9);
        assert!(scrub_time(1.0, 20.0, 300.0) < 20.0, "never past the last frame");
        // A 12-minute file: 2.4 s per pixel, and nearby pointer positions share a frame.
        let a = scrub_time(0.5001, 720.0, 300.0);
        let b = scrub_time(0.5002, 720.0, 300.0);
        assert_eq!(a, b);
        assert!((a % 2.4).abs() < 1e-6 || (2.4 - a % 2.4).abs() < 1e-6);
        assert_eq!(scrub_time(0.5, 0.0, 300.0), 0.0, "no duration");
        assert_eq!(scrub_time(f64::NAN, 10.0, 300.0), 0.0);
    }

    #[test]
    fn search_filter_and_usage() {
        let mut p = demo();
        p.media[1].notes = "voice-over part 2".into();
        p.media[2].tags = vec!["logo".into()];
        assert_eq!(filter_media(&p, "", None).len(), 3);
        assert_eq!(filter_media(&p, "", Some(MediaKind::Audio)).len(), 1);
        assert_eq!(filter_media(&p, "VOICE", None)[0].id, "ma");
        assert_eq!(filter_media(&p, "logo", None)[0].id, "mi");
        assert_eq!(filter_media(&p, "mv.mp4", None)[0].id, "mv");
        assert!(filter_media(&p, "zzz", None).is_empty());
        assert_eq!(usage(&p, "mv"), 2);
        assert_eq!(usage(&p, "mi"), 0);
        // Removing unused media works; used media needs force.
        run(&p, &[Op::RemoveMedia { asset: "mi".into(), force: false }]);
        let mut q = p.clone();
        assert!(aiframes_core::apply(&mut q, &Op::RemoveMedia { asset: "mv".into(), force: false }).is_err());
        let (q, _) = run(&p, &[Op::RemoveMedia { asset: "mv".into(), force: true }]);
        assert_eq!(q.clips.len(), 1);
    }
}
