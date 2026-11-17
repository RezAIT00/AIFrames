//! Toolbar (tools, snapping, view toggles, zoom), the timecode corner and the track headers
//! (kind tag, name, role, mute/solo/lock/hide, volume, resize).

use super::*;
use crate::style::{self, alpha, rgb};
use egui::{Align, Color32, CornerRadius, FontId, Layout as ELayout, RichText, Stroke, StrokeKind, UiBuilder};

const HEADER_BG: Color32 = rgb(0x1F, 0x20, 0x23);
const TOOLBAR_BG: Color32 = rgb(0x19, 0x1A, 0x1D);

/// Bottom of the fader scale (dB); anything at or below is silence.
pub(crate) const FADER_MIN_DB: f64 = -60.0;
/// Top of the fader scale: +24.1 dB covers the core's maximum gain of 16 (+24.08 dB).
pub(crate) const FADER_MAX_DB: f64 = 24.1;

/// Fader fraction ↔ linear gain (−60 … +24.1 dB) for track volume bars, clip volume lines and
/// volume keyframe lanes.
pub(crate) fn gain_to_frac(g: f64) -> f32 {
    if g.is_nan() || g <= 1e-6 {
        return 0.0;
    }
    let db = 20.0 * g.log10();
    (((db - FADER_MIN_DB) / (FADER_MAX_DB - FADER_MIN_DB)).clamp(0.0, 1.0)) as f32
}

pub(crate) fn frac_to_gain(f: f32) -> f64 {
    if f.is_nan() || f <= 0.005 {
        return 0.0;
    }
    let db = FADER_MIN_DB + (FADER_MAX_DB - FADER_MIN_DB) * f.min(1.0) as f64;
    10f64.powf(db / 20.0)
}

pub(crate) fn db_label(g: f64) -> String {
    if g <= 1e-4 {
        "-inf dB".into()
    } else {
        let db = 20.0 * g.log10();
        if db.abs() < 0.05 {
            "0.0 dB".into()
        } else {
            format!("{:+.1} dB", db)
        }
    }
}

impl Timeline {
    pub(super) fn toolbar(&mut self, ui: &mut Ui, cx: &EditCtx, lay: &Layout, duration: f64, out: &mut Vec<UiAction>) {
        ui.painter().rect_filled(lay.toolbar, CornerRadius::ZERO, TOOLBAR_BG);
        ui.painter().hline(lay.toolbar.x_range(), lay.toolbar.bottom() - 0.5, Stroke::new(1.0, rgb(0x0E, 0x0F, 0x11)));
        let inner = lay.toolbar.shrink2(vec2(6.0, 2.0));
        let mut child = ui.new_child(UiBuilder::new().max_rect(inner).layout(ELayout::left_to_right(Align::Center)));
        let ui = &mut child;
        ui.spacing_mut().item_spacing.x = 2.0;
        ui.spacing_mut().button_padding = vec2(6.0, 1.0);
        let small = |s: &str| RichText::new(s).size(11.5);
        for t in Tool::ALL {
            let r = ui.selectable_label(self.tool == t, small(t.label())).on_hover_text(format!("{} tool ({})\n{}", t.label(), t.key(), t.help()));
            if r.clicked() {
                self.tool = t;
            }
        }
        ui.add_space(8.0);
        ui.separator();
        if ui.selectable_label(self.snapping, small("Snap")).on_hover_text("Snapping to the playhead, clip edges, markers and in/out (N). Hold Alt while dragging to bypass.").clicked() {
            self.snapping = !self.snapping;
        }
        if ui.selectable_label(self.linked_selection, small("Linked")).on_hover_text("Clicking a clip also selects its linked (detached) audio.").clicked() {
            self.linked_selection = !self.linked_selection;
        }
        ui.separator();
        if ui.selectable_label(self.show_filmstrips, small("Thumbs")).on_hover_text("Filmstrip thumbnails on video clips").clicked() {
            self.show_filmstrips = !self.show_filmstrips;
        }
        let waves = ui.selectable_label(self.show_waveforms, small("Waves")).on_hover_text(format!("Waveforms on clips with sound ({} scale; right-click to change)", if self.wave_log { "dB" } else { "linear" }));
        if waves.clicked() {
            self.show_waveforms = !self.show_waveforms;
        }
        waves.context_menu(|ui| {
            if ui.radio(self.wave_log, "dB scale (quiet sound stays visible)").clicked() {
                self.wave_log = true;
                ui.close();
            }
            if ui.radio(!self.wave_log, "Linear amplitude").clicked() {
                self.wave_log = false;
                ui.close();
            }
        });
        ui.separator();
        if ui.button(small("-")).on_hover_text("Zoom out (-)").clicked() {
            self.zoom_by(lay, 1.0 / 1.5, cx.playhead);
        }
        let mut z = self.zoom;
        let max = geom::max_zoom(lay.fps);
        let r = ui.add_sized([90.0, 16.0], egui::Slider::new(&mut z, geom::MIN_ZOOM..=max).logarithmic(true).show_value(false)).on_hover_text("Zoom (Ctrl+wheel)");
        if r.changed() {
            self.zoom_by(lay, z / self.zoom, cx.playhead);
        }
        if ui.button(small("+")).on_hover_text("Zoom in (+)").clicked() {
            self.zoom_by(lay, 1.5, cx.playhead);
        }
        if ui.button(small("Fit")).on_hover_text("Fit the whole edit (\\)").clicked() {
            let (s, z) = geom::fit(duration, lay.lanes.width(), geom::MIN_ZOOM, max);
            self.scroll = s;
            self.zoom = z;
        }
        ui.with_layout(ELayout::right_to_left(Align::Center), |ui| {
            let n = cx.selected_clips().len();
            let errors = cx.resolved.errors.len();
            if errors > 0 {
                let r = ui.label(RichText::new(format!("{errors} unresolved")).size(11.0).color(style::ERROR));
                r.on_hover_text(cx.resolved.errors.iter().take(8).map(|e| e.message.as_str()).collect::<Vec<_>>().join("\n"));
            }
            if n > 0 {
                ui.add_space(10.0);
                ui.label(RichText::new(format!("{n} selected")).size(11.0).color(ui.visuals().weak_text_color()));
            }
            let _ = out;
        });
    }

    pub(super) fn corner(&self, ui: &Ui, cx: &EditCtx, lay: &Layout, duration: f64) {
        let p = ui.painter_at(lay.corner);
        p.rect_filled(lay.corner, CornerRadius::ZERO, HEADER_BG);
        p.text(pos2(lay.corner.left() + 10.0, lay.corner.center().y - 1.0), egui::Align2::LEFT_CENTER, util::timecode(cx.playhead, lay.fps), FontId::monospace(15.0), style::ACCENT);
        p.text(
            pos2(lay.corner.right() - 8.0, lay.corner.center().y),
            egui::Align2::RIGHT_CENTER,
            util::short_time(duration),
            FontId::proportional(10.0),
            ui.visuals().weak_text_color(),
        );
        p.vline(lay.corner.right() - 0.5, lay.corner.y_range(), Stroke::new(1.0, rgb(0x0E, 0x0F, 0x11)));
        p.hline(lay.corner.x_range(), lay.corner.bottom() - 0.5, Stroke::new(1.0, rgb(0x0E, 0x0F, 0x11)));
    }

    pub(super) fn headers_ui(&mut self, ui: &mut Ui, cx: &EditCtx, lay: &Layout, base: egui::Id, out: &mut Vec<UiAction>) {
        let p = cx.project;
        let painter = ui.painter_at(lay.headers);
        painter.rect_filled(lay.headers, CornerRadius::ZERO, HEADER_BG);
        let weak = ui.visuals().weak_text_color();
        let strong = ui.visuals().strong_text_color();
        for (i, row) in lay.rows.iter().enumerate() {
            let rect = lay.header_rect(i);
            if rect.bottom() < lay.headers.top() || rect.top() > lay.headers.bottom() {
                continue;
            }
            let Some(track) = p.tracks.get(row.index) else { continue };
            let vis = rect.intersect(lay.headers);
            let resp = ui.interact(vis, base.with(("hdr", &track.id)), Sense::click_and_drag());
            if resp.drag_started_by(egui::PointerButton::Primary) {
                let origin = resp.interact_pointer_pos().unwrap_or(rect.center());
                self.drag = Some(DragState { kind: DragKind::TrackReorder { track: track.id.clone() }, origin, origin_t: 0.0 });
            }
            if resp.dragged() {
                ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
            }
            let selected = cx.is_selected(&track.id);
            let kc = style::track_kind_color(track.kind);
            let bg = if selected { rgb(0x2C, 0x2F, 0x36) } else if resp.hovered() { rgb(0x25, 0x27, 0x2B) } else { HEADER_BG };
            painter.rect_filled(rect, CornerRadius::ZERO, bg);
            painter.rect_filled(Rect::from_min_size(rect.min, vec2(3.0, rect.height())), CornerRadius::ZERO, if selected { style::ACCENT } else { alpha(kc, 0.8) });
            painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, rgb(0x14, 0x15, 0x17)));
            if cx.is_ai(&track.id) {
                style::glow(&painter, rect.shrink(1.5), style::AI, cx.ai_flash.max(0.35));
            }
            // Kind tag and name.
            // Kind icon + id tag.
            let tag_txt: String = track.id.to_uppercase().chars().take(4).collect();
            let tg = painter.layout_no_wrap(tag_txt, FontId::proportional(9.5), rgb(0xE8, 0xE8, 0xE8));
            let tag = Rect::from_min_size(pos2(rect.left() + 8.0, rect.top() + 5.0), vec2(tg.size().x + 20.0, 14.0));
            painter.rect_filled(tag, CornerRadius::same(3), style::shade(kc, -0.35));
            let icon = pos2(tag.left() + 8.0, tag.center().y);
            match track.kind {
                TrackKind::Video => style::film_glyph(&painter, icon, rgb(0xE8, 0xE8, 0xE8)),
                TrackKind::Audio => style::speaker_glyph(&painter, icon + vec2(0.5, 0.0), rgb(0xE8, 0xE8, 0xE8)),
            }
            painter.galley(pos2(tag.left() + 15.0, tag.center().y - tg.size().y * 0.5), tg, rgb(0xE8, 0xE8, 0xE8));
            let name = if !track.name.is_empty() {
                track.name.clone()
            } else if !track.role.is_empty() {
                let mut c = track.role.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            } else if track.kind == TrackKind::Video {
                "Video".into()
            } else {
                "Audio".into()
            };
            // Toggles, right-aligned on the first line.
            let toggles: Vec<(&str, bool, Color32)> = match track.kind {
                TrackKind::Video => vec![("M", track.muted, style::MUTE_ON), ("eye", !track.hidden, rgb(0x6F, 0x9F, 0xD8)), ("lock", track.locked, style::LOCK_ON)],
                TrackKind::Audio => vec![("M", track.muted, style::MUTE_ON), ("S", track.solo, style::SOLO_ON), ("lock", track.locked, style::LOCK_ON)],
            };
            let tw = 18.0;
            let mut x = rect.right() - 6.0 - tw * toggles.len() as f32 - 2.0 * (toggles.len() as f32 - 1.0);
            let name_right = x - 4.0;
            painter.with_clip_rect(Rect::from_min_max(pos2(tag.right() + 6.0, rect.top()), pos2(name_right, rect.bottom()))).text(
                pos2(tag.right() + 6.0, tag.center().y),
                egui::Align2::LEFT_CENTER,
                &name,
                FontId::proportional(11.5),
                if track.hidden || track.muted { weak } else { strong },
            );
            for (k, on, col) in toggles {
                let r = Rect::from_min_size(pos2(x, rect.top() + 4.0), vec2(tw, 16.0));
                x += tw + 2.0;
                if !vis.contains(r.center()) {
                    continue;
                }
                let tr = ui.interact(r, base.with(("tog", &track.id, k)), Sense::click());
                let active_bg = match k {
                    "eye" => alpha(Color32::WHITE, 0.06),
                    _ if on => col,
                    _ => alpha(Color32::WHITE, 0.06),
                };
                painter.rect_filled(r, CornerRadius::same(3), if tr.hovered() { style::shade(active_bg, 0.15) } else { active_bg });
                let fg = if k == "eye" { if on { col } else { weak } } else if on { rgb(0x10, 0x10, 0x10) } else { weak };
                match k {
                    "eye" => style::eye_glyph(&painter, r.center(), fg, !on),
                    "lock" => style::lock_glyph(&painter, r.center(), fg, on),
                    _ => {
                        painter.text(r.center(), egui::Align2::CENTER_CENTER, k, FontId::proportional(10.0), fg);
                    }
                }
                let (tip, op) = match k {
                    "M" => (if track.kind == TrackKind::Video { "Mute this track's sound" } else { "Mute" }, Op::SetTrack { track: track.id.clone(), name: None, role: None, muted: Some(!track.muted), hidden: None, locked: None, solo: None, volume: None }),
                    "S" => ("Solo", Op::SetTrack { track: track.id.clone(), name: None, role: None, muted: None, hidden: None, locked: None, solo: Some(!track.solo), volume: None }),
                    "eye" => ("Show/hide the picture of this track", Op::SetTrack { track: track.id.clone(), name: None, role: None, muted: None, hidden: Some(!track.hidden), locked: None, solo: None, volume: None }),
                    _ => ("Lock: protect the track from edits", Op::SetTrack { track: track.id.clone(), name: None, role: None, muted: None, hidden: None, locked: Some(!track.locked), solo: None, volume: None }),
                };
                let tr = tr.on_hover_text(tip);
                if tr.clicked() {
                    out.push(UiAction::Apply { label: format!("{} {}", tip.split(':').next().unwrap_or(tip), track.id), ops: vec![op] });
                }
            }
            // Second line: role, volume.
            if rect.height() >= 40.0 {
                let y = rect.top() + 28.0;
                let left = rect.left() + 10.0;
                let sound = track.kind == TrackKind::Audio || p.clips.iter().any(|c| c.track == track.id && util::has_sound(p, c));
                if sound {
                    let bar = Rect::from_min_size(pos2(left, y), vec2((rect.width() - 76.0).max(30.0), 8.0));
                    let br = ui.interact(bar.expand2(vec2(0.0, 3.0)), base.with(("vol", &track.id)), Sense::click_and_drag());
                    let live = self.volume_live.as_ref().filter(|(t, _)| *t == track.id).map(|(_, v)| *v);
                    let g = live.unwrap_or(track.volume);
                    if br.dragged() || br.drag_started() || br.clicked() {
                        if let Some(pos) = br.interact_pointer_pos() {
                            let f = ((pos.x - bar.left()) / bar.width()).clamp(0.0, 1.0);
                            self.volume_live = Some((track.id.clone(), (frac_to_gain(f) * 1000.0).round() / 1000.0));
                        }
                    }
                    if br.double_clicked() {
                        self.volume_live = Some((track.id.clone(), 1.0));
                    }
                    if !br.is_pointer_button_down_on() && !br.dragged() {
                        if let Some((t, v)) = self.volume_live.take() {
                            if t == track.id {
                                if (v - track.volume).abs() > 1e-6 {
                                    out.push(UiAction::Apply { label: format!("Volume {}", track.id), ops: vec![Op::SetTrack { track: t, name: None, role: None, muted: None, hidden: None, locked: None, solo: None, volume: Some(v) }] });
                                }
                            } else {
                                self.volume_live = Some((t, v));
                            }
                        }
                    }
                    let f = gain_to_frac(g);
                    painter.rect_filled(bar, CornerRadius::same(2), rgb(0x13, 0x14, 0x16));
                    painter.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * f, bar.height())), CornerRadius::same(2), if track.muted { weak } else { alpha(kc, 0.9) });
                    let unity = bar.left() + bar.width() * gain_to_frac(1.0);
                    painter.vline(unity, bar.y_range(), Stroke::new(1.0, alpha(Color32::WHITE, 0.35)));
                    painter.text(pos2(rect.right() - 8.0, bar.center().y), egui::Align2::RIGHT_CENTER, db_label(g), FontId::proportional(9.5), weak);
                    br.on_hover_text("Track volume: drag; double-click = 0 dB");
                } else if !track.role.is_empty() && !track.name.is_empty() {
                    painter.text(pos2(left, y + 4.0), egui::Align2::LEFT_CENTER, &track.role, FontId::proportional(10.0), weak);
                }
            }
            // Resize handle.
            let handle = Rect::from_min_max(pos2(rect.left(), rect.bottom() - 4.0), rect.max);
            let hr = ui.interact(handle.intersect(lay.headers), base.with(("resize", &track.id)), Sense::drag());
            if hr.hovered() || hr.dragged() {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
            }
            if hr.drag_started() {
                let origin = hr.interact_pointer_pos().unwrap_or(rect.max);
                self.drag = Some(DragState { kind: DragKind::RowResize { track: track.id.clone(), h0: row.h }, origin, origin_t: 0.0 });
            }
            if resp.clicked() {
                out.push(UiAction::Select(vec![track.id.clone()]));
            }
            if resp.double_clicked() {
                self.rename = Some(Rename { target: RenameTarget::Track(track.id.clone()), text: track.name.clone(), pos: pos2(rect.left() + 36.0, rect.top() + 2.0), opened: false });
            }
            let tid = track.id.clone();
            let hp = pos2(rect.left() + 36.0, rect.top() + 2.0);
            resp.context_menu(|ui| super::menus::track_menu(self, ui, cx, &tid, hp, out));
            if track.locked {
                let stripes = painter.with_clip_rect(rect);
                let mut xx = rect.left() - rect.height();
                while xx < rect.right() {
                    stripes.line_segment([pos2(xx, rect.bottom()), pos2(xx + rect.height(), rect.top())], Stroke::new(1.0, alpha(style::LOCK_ON, 0.08)));
                    xx += 8.0;
                }
            }
            let _ = StrokeKind::Inside;
        }
        // Drop zone hint below the last track.
        let bottom = lay.rows_bottom();
        if bottom < lay.headers.bottom() {
            painter.text(pos2(lay.headers.left() + 10.0, bottom + 14.0), egui::Align2::LEFT_CENTER, if lay.rows.is_empty() { "No tracks — right-click the lanes" } else { "" }, FontId::proportional(10.0), weak);
        }
        painter.vline(lay.headers.right() - 0.5, lay.headers.y_range(), Stroke::new(1.0, rgb(0x0E, 0x0F, 0x11)));
        // Header resize (drag the divider between headers and lanes).
        let div = Rect::from_center_size(pos2(lay.headers.right(), lay.headers.center().y), vec2(5.0, lay.headers.height()));
        let dr = ui.interact(div, base.with("hdr_width"), Sense::drag());
        if dr.hovered() || dr.dragged() {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
        if dr.dragged() {
            self.header_width = (self.header_width + dr.drag_delta().x).clamp(120.0, 360.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_bar_mapping() {
        let unity = gain_to_frac(1.0);
        assert!((unity as f64 - 60.0 / 84.1).abs() < 1e-6);
        assert!((frac_to_gain(unity) - 1.0).abs() < 1e-5);
        // The core's maximum gain (16 = +24.08 dB) fits on the scale.
        assert!(gain_to_frac(16.0) < 1.0);
        assert!((frac_to_gain(gain_to_frac(8.0)) - 8.0).abs() < 1e-3);
        assert_eq!(frac_to_gain(0.0), 0.0);
        assert_eq!(gain_to_frac(0.0), 0.0);
        let g = frac_to_gain(0.5);
        assert!((gain_to_frac(g) - 0.5).abs() < 1e-6);
        assert_eq!(db_label(1.0), "0.0 dB");
        assert_eq!(db_label(0.0), "-inf dB");
        assert_eq!(db_label(0.5), "-6.0 dB");
    }
}
