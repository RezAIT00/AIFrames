//! Painting: lanes, clips (filmstrips, waveforms, fades, labels, badges, keyframes, volume line,
//! anchors, errors, AI glow), transitions, ghosts of the pending edit, the ruler, the playhead and
//! the scrollbars.

use super::*;
use crate::style::{self, alpha, rgb, shade};
use crate::MediaPreview;
use aiframes_core::{eval, AnchorTarget, Clip, MediaAsset, MediaKind, ShapeKind, Source, Time};
use egui::epaint::{CubicBezierShape, Mesh};
use egui::{Align2, Color32, CornerRadius, FontId, Painter, Shape, Stroke, StrokeKind};
use std::sync::Arc;
use wave::WavePyramid;

const LANE_BG: Color32 = rgb(0x15, 0x16, 0x18);
const ROW_VIDEO: Color32 = rgb(0x1B, 0x1C, 0x20);
const ROW_AUDIO: Color32 = rgb(0x19, 0x1C, 0x1B);
const RULER_BG: Color32 = rgb(0x17, 0x18, 0x1B);

/// Waveform pyramids by asset and sound stream, rebuilt when the app hands out a different waveform.
#[derive(Default)]
pub(crate) struct WaveCache {
    map: HashMap<(Id, u32), (usize, Arc<WavePyramid>)>,
}

impl WaveCache {
    pub fn get(&mut self, preview: &dyn MediaPreview, asset: &MediaAsset, stream: u32) -> Option<Arc<WavePyramid>> {
        let w = preview.waveform_stream(asset, stream)?;
        let key = Arc::as_ptr(&w) as usize;
        if let Some((k, p)) = self.map.get(&(asset.id.clone(), stream)) {
            if *k == key {
                return Some(p.clone());
            }
        }
        let p = Arc::new(WavePyramid::new(&w));
        self.map.insert((asset.id.clone(), stream), (key, p.clone()));
        Some(p)
    }
}

/// How to draw one clip.
#[derive(Clone, Copy, Default)]
pub(crate) struct Look {
    pub selected: bool,
    pub hovered: Option<Zone>,
    /// The result of the pending edit.
    pub ghost: bool,
    /// The original of something the pending edit changes.
    pub dim: bool,
    pub error: bool,
    pub ai: f32,
    pub track_off: bool,
}

fn asset_aspect(a: &MediaAsset) -> f32 {
    let (w, h) = (a.info.width as f32, a.info.height as f32);
    if w <= 0.0 || h <= 0.0 {
        return 16.0 / 9.0;
    }
    if a.info.rotation.abs() % 180 == 90 {
        h / w
    } else {
        w / h
    }
}

/// Colour of track-matte links, badges and matte-source stripes.
const MATTE: Color32 = rgb(0x3F, 0xC8, 0xB8);

fn dashed_rect(p: &Painter, r: Rect, stroke: Stroke) {
    let pts = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
    for w in pts.windows(2) {
        p.extend(Shape::dashed_line(&[w[0], w[1]], stroke, 4.0, 3.0));
    }
}

impl Timeline {
    pub(super) fn paint(&mut self, ui: &mut Ui, cx: &EditCtx, lay: &Layout) {
        let duration = cx.resolved.duration.max(cx.resolved.content_end());
        self.corner(ui, cx, lay, duration);
        self.paint_lanes(ui, cx, lay);
        self.paint_ruler(ui, cx, lay);
        self.paint_scrollbars(ui, lay);
        // Insertion line while a track header is dragged.
        if let Some(y) = self.reorder_line {
            let p = ui.painter_at(Rect::from_min_max(pos2(lay.headers.left(), lay.lanes.top()), lay.lanes.max));
            p.hline(lay.headers.left()..=lay.lanes.right(), y, Stroke::new(2.0, style::ACCENT));
            p.circle_filled(pos2(lay.headers.left() + 4.0, y), 3.5, style::ACCENT);
        }
    }

    fn paint_scrollbars(&self, ui: &Ui, lay: &Layout) {
        let p = ui.painter();
        p.rect_filled(lay.hbar, CornerRadius::ZERO, LANE_BG);
        let th = self.hbar_thumb(lay);
        p.rect_filled(th, CornerRadius::same(3), rgb(0x3A, 0x3C, 0x42));
        p.rect_filled(lay.vbar, CornerRadius::ZERO, LANE_BG);
        if let Some(tv) = self.vbar_thumb(lay) {
            p.rect_filled(tv, CornerRadius::same(3), rgb(0x3A, 0x3C, 0x42));
        }
        let corner = Rect::from_min_max(pos2(lay.vbar.left(), lay.hbar.top()), pos2(lay.vbar.right(), lay.hbar.bottom()));
        p.rect_filled(corner, CornerRadius::ZERO, LANE_BG);
        // Under the headers, next to the scrollbar.
        let left = Rect::from_min_max(pos2(lay.headers.left(), lay.hbar.top()), pos2(lay.headers.right(), lay.hbar.bottom()));
        p.rect_filled(left, CornerRadius::ZERO, rgb(0x1F, 0x20, 0x23));
        let ruler_right = Rect::from_min_max(pos2(lay.ruler.right(), lay.ruler.top()), pos2(lay.vbar.right(), lay.ruler.bottom()));
        p.rect_filled(ruler_right, CornerRadius::ZERO, RULER_BG);
    }

    // ---------------------------------------------------------------------------------------------
    // Ruler
    // ---------------------------------------------------------------------------------------------

    fn paint_ruler(&self, ui: &Ui, cx: &EditCtx, lay: &Layout) {
        let r = lay.ruler;
        let p = ui.painter_at(r);
        p.rect_filled(r, CornerRadius::ZERO, RULER_BG);
        let weak = ui.visuals().weak_text_color();
        let tk = geom::ticks(self.zoom, lay.fps, 84.0);
        let (t0, t1) = (lay.tmap.t(r.left()), lay.tmap.t(r.right()));
        // In/out.
        if cx.in_point.is_some() || cx.out_point.is_some() {
            let a = cx.in_point.unwrap_or(0.0);
            let b = cx.out_point.unwrap_or(cx.resolved.duration.max(a));
            let band = Rect::from_min_max(pos2(lay.tmap.x(a), r.top()), pos2(lay.tmap.x(b), r.bottom()));
            p.rect_filled(band, CornerRadius::ZERO, alpha(style::IN_OUT, 0.18));
            for (t, is_in) in [(cx.in_point, true), (cx.out_point, false)] {
                if let Some(t) = t {
                    let x = lay.tmap.x(t);
                    p.vline(x, r.y_range(), Stroke::new(1.5, style::IN_OUT));
                    let dx = if is_in { 5.0 } else { -5.0 };
                    p.line_segment([pos2(x, r.top() + 1.0), pos2(x + dx, r.top() + 1.0)], Stroke::new(1.5, style::IN_OUT));
                    p.line_segment([pos2(x, r.bottom() - 1.0), pos2(x + dx, r.bottom() - 1.0)], Stroke::new(1.5, style::IN_OUT));
                }
            }
        }
        for t in geom::tick_times(t0, t1, tk.minor) {
            let x = lay.tmap.x(t).round() + 0.5;
            p.vline(x, (r.bottom() - 4.0)..=r.bottom(), Stroke::new(1.0, alpha(weak, 0.45)));
        }
        for t in geom::tick_times(t0, t1, tk.major) {
            let x = lay.tmap.x(t).round() + 0.5;
            p.vline(x, (r.bottom() - 10.0)..=r.bottom(), Stroke::new(1.0, alpha(weak, 0.9)));
            p.text(pos2(x + 3.0, r.top() + 3.0), Align2::LEFT_TOP, geom::tick_label(t, &tk, lay.fps), FontId::monospace(9.5), weak);
        }
        // Markers.
        for (id, x) in &lay.markers {
            let Some(m) = cx.project.markers.iter().find(|m| &m.id == id) else { continue };
            let col = style::marker_color(m.kind, m.color);
            let y = r.bottom() - 12.0;
            let pts = vec![pos2(x - 4.5, y), pos2(x + 4.5, y), pos2(x + 4.5, y + 7.0), pos2(*x, y + 11.0), pos2(x - 4.5, y + 7.0)];
            let sel = cx.is_selected(id);
            p.add(Shape::convex_polygon(pts, col, if sel { Stroke::new(1.5, style::SELECT) } else { Stroke::new(1.0, shade(col, -0.5)) }));
            if cx.is_ai(id) {
                p.circle_stroke(pos2(*x, y + 5.0), 8.0, Stroke::new(1.5, alpha(style::AI, 0.4 + 0.6 * cx.ai_flash)));
            }
            if (m.kind == aiframes_core::MarkerKind::Chapter || sel) && !m.name.is_empty() {
                let g = p.layout_no_wrap(m.name.clone(), FontId::proportional(10.0), col);
                let bg = Rect::from_min_size(pos2(x + 6.0, y - 1.0), g.size() + vec2(6.0, 0.0));
                p.rect_filled(bg, CornerRadius::same(2), alpha(RULER_BG, 0.9));
                p.galley(pos2(x + 9.0, y - 1.0), g, col);
            }
        }
        // Snap line.
        if let Some(s) = self.snap_line {
            p.vline(lay.tmap.x(s), r.y_range(), Stroke::new(1.0, style::SNAP));
        }
        // Playhead head.
        let x = lay.tmap.x(cx.playhead);
        if x >= r.left() - 8.0 && x <= r.right() + 8.0 {
            let top = r.top();
            let pts = vec![pos2(x - 6.0, top), pos2(x + 6.0, top), pos2(x + 6.0, top + 8.0), pos2(x, top + 14.0), pos2(x - 6.0, top + 8.0)];
            p.add(Shape::convex_polygon(pts, style::PLAYHEAD, Stroke::NONE));
            p.vline(x, (top + 12.0)..=r.bottom(), Stroke::new(1.0, style::PLAYHEAD));
        }
        p.hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0, rgb(0x0E, 0x0F, 0x11)));
    }

    // ---------------------------------------------------------------------------------------------
    // Lanes
    // ---------------------------------------------------------------------------------------------

    fn paint_lanes(&mut self, ui: &Ui, cx: &EditCtx, lay: &Layout) {
        let p = cx.project;
        let r = cx.resolved;
        let lanes = lay.lanes;
        let pl = ui.painter_at(lanes);
        pl.rect_filled(lanes, CornerRadius::ZERO, LANE_BG);
        // Rows.
        for (i, row) in lay.rows.iter().enumerate() {
            let rr = lay.row_rect(i);
            if rr.bottom() < lanes.top() || rr.top() > lanes.bottom() {
                continue;
            }
            let Some(track) = p.tracks.get(row.index) else { continue };
            let mut bg = if row.kind == TrackKind::Video { ROW_VIDEO } else { ROW_AUDIO };
            if cx.is_selected(&track.id) {
                bg = shade(bg, 0.06);
            }
            pl.rect_filled(rr.shrink2(vec2(0.0, 0.5)), CornerRadius::ZERO, bg);
            pl.hline(rr.x_range(), rr.bottom() - 0.5, Stroke::new(1.0, rgb(0x12, 0x13, 0x15)));
            if track.locked {
                let mut x = rr.left() - rr.height() - ((rr.left() - lanes.left()) % 10.0);
                while x < rr.right() {
                    pl.line_segment([pos2(x, rr.bottom()), pos2(x + rr.height(), rr.top())], Stroke::new(1.0, alpha(style::LOCK_ON, 0.05)));
                    x += 10.0;
                }
            }
        }
        // Video/audio divider.
        if let Some(i) = lay.rows.iter().position(|r| r.kind == TrackKind::Audio) {
            if i > 0 {
                let y = lay.y0 + lay.rows[i].y - geom::DIVIDER * 0.5;
                pl.hline(lanes.x_range(), y, Stroke::new(geom::DIVIDER - 1.0, rgb(0x0F, 0x10, 0x12)));
            }
        }
        // Grid at major ticks.
        let tk = geom::ticks(self.zoom, lay.fps, 84.0);
        let bottom = lay.rows_bottom().min(lanes.bottom());
        for t in geom::tick_times(lay.tmap.t(lanes.left()), lay.tmap.t(lanes.right()), tk.major) {
            let x = lay.tmap.x(t).round() + 0.5;
            pl.vline(x, lanes.top()..=bottom, Stroke::new(1.0, alpha(Color32::WHITE, 0.025)));
        }
        // In/out tint.
        if let (Some(a), b) = (cx.in_point.or(cx.out_point.map(|_| 0.0)), cx.out_point) {
            let b = b.unwrap_or(r.duration.max(a));
            let band = Rect::from_min_max(pos2(lay.tmap.x(a), lanes.top()), pos2(lay.tmap.x(b), bottom));
            pl.rect_filled(band, CornerRadius::ZERO, alpha(style::IN_OUT, 0.05));
        }
        // Chapter (and selected) marker lines.
        for (id, x) in &lay.markers {
            let Some(m) = p.markers.iter().find(|m| &m.id == id) else { continue };
            if m.kind == aiframes_core::MarkerKind::Chapter || cx.is_selected(id) {
                let col = style::marker_color(m.kind, m.color);
                pl.extend(Shape::dashed_line(&[pos2(*x, lanes.top()), pos2(*x, bottom)], Stroke::new(1.0, alpha(col, 0.55)), 4.0, 4.0));
            }
        }

        // Clips (dimmed when the pending edit changes them).
        let (has_sim, dimmed, refused): (bool, HashSet<Id>, Option<String>) = match &self.preview {
            Some(pv) if !pv.ops.is_empty() && pv.ghost => match &pv.result {
                Ok(s) => (true, s.changed.union(&s.removed).cloned().collect(), None),
                Err(e) => (false, HashSet::new(), Some(e.clone())),
            },
            _ => (false, HashSet::new(), None),
        };
        let hover_clip = match &self.hover {
            Some(Hit::Clip { id, zone }) => Some((id.clone(), *zone)),
            _ => None,
        };
        let boxes = lay.boxes.clone();
        for b in &boxes {
            let Some(c) = p.clips.iter().find(|c| c.id == b.id) else { continue };
            let span = util::span_or_fallback(p, r, c).0;
            let track = p.tracks.iter().find(|t| t.id == c.track);
            let look = Look {
                selected: cx.is_selected(&c.id),
                hovered: hover_clip.as_ref().filter(|(id, _)| *id == c.id).map(|(_, z)| *z),
                ghost: false,
                dim: dimmed.contains(&c.id),
                error: !b.resolved,
                ai: if cx.is_ai(&c.id) { 0.35 + 0.65 * cx.ai_flash } else { 0.0 },
                track_off: track.is_some_and(|t| t.hidden || (t.kind == TrackKind::Audio && t.muted)),
            };
            self.paint_clip(&pl, cx.preview, p, r, c, span, b.rect, lay, &look);
        }
        // Transitions (from the pending result when there is one).
        match &self.preview {
            Some(Preview { result: Ok(s), .. }) if has_sim => paint_transitions(&pl, &s.project, &s.resolved, lay, cx, true),
            _ => paint_transitions(&pl, p, r, lay, cx, false),
        }
        // Anchor lines for the selection (the magnetic chain made visible).
        self.paint_anchors(&pl, cx, lay);
        // Keyframe lanes and volume lines of the selected clips.
        self.paint_overlays(&pl, cx, lay);
        // Ghosts of the pending edit.
        if let Some(Preview { result: Ok(_), ghost: true, .. }) = &self.preview {
            let pv = self.preview.take();
            if let Some(pv) = &pv {
                if let Ok(s) = &pv.result {
                    let mut ids: Vec<&Id> = s.changed.iter().collect();
                    ids.sort();
                    let new_row_y = lay.rows_bottom() + 4.0;
                    for id in ids {
                        let Some(c) = s.project.clips.iter().find(|c| &c.id == id) else { continue };
                        let Some(span) = s.resolved.span(id) else { continue };
                        let rect = match lay.row_of_track(&c.track) {
                            Some(row) => lay.clip_rect(row, span),
                            None => Rect::from_min_max(pos2(lay.tmap.x(span.start), new_row_y), pos2(lay.tmap.x(span.end).max(lay.tmap.x(span.start) + 1.0), new_row_y + 40.0)),
                        };
                        let look = Look { ghost: true, selected: false, ..Default::default() };
                        self.paint_clip(&pl, cx.preview, &s.project, &s.resolved, c, span, rect, lay, &look);
                    }
                    if s.project.tracks.len() > p.tracks.len() {
                        let band = Rect::from_min_max(pos2(lanes.left(), new_row_y - 2.0), pos2(lanes.right(), new_row_y + 42.0));
                        pl.rect_stroke(band, CornerRadius::same(2), Stroke::new(1.0, alpha(style::ACCENT, 0.6)), StrokeKind::Inside);
                        pl.text(pos2(lanes.left() + 6.0, new_row_y - 2.0), Align2::LEFT_BOTTOM, "new track", FontId::proportional(10.0), style::ACCENT);
                    }
                }
            }
            self.preview = pv;
        }
        if let (Some(err), Some(pv)) = (&refused, &self.preview) {
            for (track, span) in &pv.hints {
                if let Some(row) = lay.row_of_track(track) {
                    let rect = lay.clip_rect(row, *span);
                    pl.rect_filled(rect, CornerRadius::same(3), alpha(style::ERROR, 0.25));
                    dashed_rect(&pl, rect, Stroke::new(1.2, style::ERROR));
                }
            }
            let _ = err;
        }
        // Razor line.
        if self.tool == Tool::Razor && self.drag.is_none() {
            if let (Some(pos), Some(Hit::Clip { id, .. })) = (self.hover_pos, &self.hover) {
                let mods = ui.input(|i| i.modifiers);
                let t = self.razor_time(cx, lay, lay.tmap.t(pos.x), mods);
                let x = lay.tmap.x(t);
                let range = if mods.shift {
                    lanes.top()..=bottom
                } else {
                    match lay.boxes.iter().find(|b| &b.id == id) {
                        Some(b) => b.rect.top()..=b.rect.bottom(),
                        None => lanes.top()..=bottom,
                    }
                };
                pl.vline(x, range, Stroke::new(1.5, style::SELECT));
                let g = pl.layout_no_wrap(util::timecode(t, lay.fps), FontId::monospace(10.0), style::SELECT);
                let at = pos2(x + 6.0, pos.y + 14.0);
                pl.rect_filled(Rect::from_min_size(at - vec2(3.0, 1.0), g.size() + vec2(6.0, 2.0)), CornerRadius::same(3), alpha(rgb(0x0D, 0x0E, 0x10), 0.9));
                pl.galley(at, g, style::SELECT);
            }
        }
        // Box selection.
        if let Some(DragState { kind: DragKind::Box { .. }, origin, .. }) = &self.drag {
            if let Some(pos) = ui.input(|i| i.pointer.hover_pos()) {
                let rect = Rect::from_two_pos(*origin, pos);
                pl.rect_filled(rect, CornerRadius::ZERO, alpha(style::ACCENT, 0.08));
                pl.rect_stroke(rect, CornerRadius::ZERO, Stroke::new(1.0, alpha(style::ACCENT, 0.8)), StrokeKind::Inside);
            }
        }
        // Snap line.
        if let Some(s) = self.snap_line {
            pl.vline(lay.tmap.x(s), lanes.top()..=lanes.bottom(), Stroke::new(1.0, style::SNAP));
        }
        // Playhead.
        let x = lay.tmap.x(cx.playhead);
        pl.vline(x, lanes.top()..=lanes.bottom(), Stroke::new(1.0, style::PLAYHEAD));
        // Empty state.
        if p.clips.is_empty() {
            let msg = if p.tracks.is_empty() { "No tracks yet — drop media here or right-click to add a track" } else { "Drag media here from the bin" };
            pl.text(pos2(lanes.center().x, lay.y0 + geom::rows_height(&lay.rows).max(40.0) * 0.5), Align2::CENTER_CENTER, msg, FontId::proportional(12.0), ui.visuals().weak_text_color());
        }
        // Drag info label.
        if let (Some(pv), Some(pos)) = (&self.preview, ui.input(|i| i.pointer.hover_pos())) {
            if self.drag.is_some() || pv.owner == Owner::Drop {
                let (text, col) = match &pv.result {
                    Err(e) => (util::plain(&format!("{}\n{}", pv.info, util::ellipsize(e, 140))), style::ERROR),
                    Ok(_) => (util::plain(&pv.info), rgb(0xF0, 0xF0, 0xF0)),
                };
                let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique("aiframes_tl_info")));
                let g = painter.layout(text, FontId::proportional(11.0), col, 360.0);
                let at = pos + vec2(16.0, 18.0);
                let bg = Rect::from_min_size(at, g.size() + vec2(12.0, 8.0));
                painter.rect_filled(bg, CornerRadius::same(4), alpha(rgb(0x0D, 0x0E, 0x10), 0.94));
                painter.rect_stroke(bg, CornerRadius::same(4), Stroke::new(1.0, alpha(col, 0.35)), StrokeKind::Inside);
                painter.galley(at + vec2(6.0, 4.0), g, col);
            }
        }
    }

    fn paint_overlays(&self, pl: &Painter, cx: &EditCtx, lay: &Layout) {
        // While a keyframe/volume drag is previewed, draw the dragged values.
        let sim = match &self.preview {
            Some(Preview { result: Ok(s), ghost: false, ops, .. }) if !ops.is_empty() => Some(&s.project),
            _ => None,
        };
        for ov in &lay.overlays {
            let param = sim
                .and_then(|q| q.clips.iter().find(|c| c.id == ov.clip))
                .and_then(|c| if ov.volume { Some(c.volume.clone()) } else { util::param_at(c, &ov.path).cloned() })
                .unwrap_or_else(|| ov.param.clone());
            let ov = Overlay { param, ..ov.clone() };
            let pc = pl.with_clip_rect(ov.rect.expand(5.0).intersect(lay.lanes));
            let col = if ov.volume { rgb(0xFF, 0xD6, 0x5A) } else { rgb(0x7F, 0xD4, 0xFF) };
            let hovered_line = matches!(&self.hover, Some(Hit::Line { clip, path }) if *clip == ov.clip && *path == ov.path);
            if !ov.volume {
                pc.rect_filled(ov.rect, CornerRadius::ZERO, alpha(Color32::BLACK, 0.22));
                // Lane label.
                let g = pc.layout_no_wrap(ov.label.clone(), FontId::proportional(9.5), col);
                let at = pos2(ov.rect.left().max(lay.lanes.left()) + 4.0, ov.rect.top() + 2.0);
                pc.rect_filled(Rect::from_min_size(at - vec2(2.0, 0.0), g.size() + vec2(4.0, 0.0)), CornerRadius::same(2), alpha(rgb(0x0D, 0x0E, 0x10), 0.8));
                pc.galley(at, g, col);
            }
            let x0 = ov.rect.left().max(lay.lanes.left());
            let x1 = ov.rect.right().min(lay.lanes.right());
            let mut pts = Vec::new();
            let mut x = x0;
            while x <= x1 {
                pts.push(pos2(x, ov.line_y(&lay.tmap, x)));
                x += 2.0;
            }
            pts.push(pos2(x1, ov.line_y(&lay.tmap, x1)));
            if pts.len() >= 2 {
                pc.add(Shape::line(pts, Stroke::new(if hovered_line { 2.4 } else { 1.5 }, col)));
            }
            for (i, k) in ov.keys().iter().enumerate() {
                let kp = ov.key_pos(&lay.tmap, k);
                let selected = self.key_sel.as_ref().is_some_and(|s| s.clip == ov.clip && s.path == ov.path && (s.t - k.t).abs() < 1e-4);
                let hovered = matches!(&self.hover, Some(Hit::Key { clip, path, index }) if *clip == ov.clip && *path == ov.path && *index == i);
                let r = if selected || hovered { 5.5 } else { 4.2 };
                let fill = if selected { Color32::WHITE } else { col };
                style::diamond(&pc, kp, r, fill, Stroke::new(1.0, if selected { style::ACCENT } else { Color32::BLACK }));
                if k.ease == aiframes_core::Ease::Hold {
                    pc.hline(kp.x..=(kp.x + 6.0), kp.y, Stroke::new(1.0, col));
                }
            }
        }
        let _ = cx;
    }

    fn paint_anchors(&self, pl: &Painter, cx: &EditCtx, lay: &Layout) {
        let p = cx.project;
        let r = cx.resolved;
        let center_y = |id: &str| -> Option<(Rect, f32)> {
            let c = p.clips.iter().find(|c| c.id == id)?;
            let row = lay.row_of_track(&c.track)?;
            let s = r.span(id)?;
            let rect = lay.clip_rect(row, s);
            Some((rect, rect.top() + LABEL_H * 0.5 + 1.0))
        };
        let draw_link = |c: &Clip, strong: bool, faint: bool| {
            let Time::Anchored(a) = &c.start else { return };
            let Some((rect, y)) = center_y(&c.id) else { return };
            let to = pos2(rect.left(), y);
            let from = match &a.to {
                AnchorTarget::After(t) => center_y(t).map(|(tr, ty)| pos2(tr.right(), ty)),
                AnchorTarget::With(t) | AnchorTarget::Before(t) => center_y(t).map(|(tr, ty)| pos2(tr.left(), ty)).or_else(|| r.marker(t).map(|m| pos2(lay.tmap.x(m), lay.lanes.top()))),
                AnchorTarget::Marker(m) => r.marker(m).map(|t| pos2(lay.tmap.x(t), lay.lanes.top())),
                AnchorTarget::Word(w) => center_y(&w.clip).and_then(|(_, ty)| r.span(&c.id).map(|s| pos2(lay.tmap.x(s.start - a.offset), ty))),
            };
            let Some(from) = from else { return };
            let col = alpha(style::ACCENT, if strong { 0.95 } else if faint { 0.22 } else { 0.55 });
            let stroke = Stroke::new(if strong { 1.5 } else { 1.0 }, col);
            let dx = ((to.x - from.x).abs() * 0.4).clamp(12.0, 80.0);
            let curve = CubicBezierShape::from_points_stroke([from, pos2(from.x + dx, from.y), pos2(to.x - dx, to.y), to], false, Color32::TRANSPARENT, stroke);
            pl.add(curve);
            if !faint {
                pl.circle_filled(from, 2.5, col);
                pl.circle_filled(to, 2.5, col);
            }
            if strong {
                let label = aiframes_core::resolve::describe_anchor(a);
                let g = pl.layout_no_wrap(label, FontId::proportional(9.5), col);
                let mid = pos2((from.x + to.x) * 0.5 - g.size().x * 0.5, from.y.min(to.y) - 13.0);
                pl.rect_filled(Rect::from_min_size(mid - vec2(3.0, 0.0), g.size() + vec2(6.0, 1.0)), CornerRadius::same(2), alpha(rgb(0x0D, 0x0E, 0x10), 0.85));
                pl.galley(mid, g, col);
            }
        };
        // End anchors (`until`): from the target's point to the clip's end, dashed.
        let draw_end_link = |c: &Clip, strong: bool| {
            let Some(Time::Anchored(a)) = &c.until else { return };
            let Some((rect, y)) = center_y(&c.id) else { return };
            let to = pos2(rect.right(), y);
            let from = match &a.to {
                AnchorTarget::After(t) => center_y(t).map(|(tr, ty)| pos2(tr.right(), ty)),
                AnchorTarget::With(t) | AnchorTarget::Before(t) => center_y(t).map(|(tr, ty)| pos2(tr.left(), ty)).or_else(|| r.marker(t).map(|m| pos2(lay.tmap.x(m), lay.lanes.top()))),
                AnchorTarget::Marker(m) => r.marker(m).map(|t| pos2(lay.tmap.x(t), lay.lanes.top())),
                AnchorTarget::Word(w) => center_y(&w.clip).and_then(|(_, ty)| r.span(&c.id).map(|s| pos2(lay.tmap.x(s.end - a.offset), ty))),
            };
            let Some(from) = from else { return };
            let col = alpha(style::ACCENT, if strong { 0.95 } else { 0.5 });
            let stroke = Stroke::new(if strong { 1.4 } else { 1.0 }, col);
            let dx = ((to.x - from.x).abs() * 0.4).clamp(10.0, 60.0);
            let bz = CubicBezierShape::from_points_stroke([from, pos2(from.x + dx, from.y), pos2(to.x - dx, to.y), to], false, Color32::TRANSPARENT, stroke);
            let pts: Vec<Pos2> = (0..=16).map(|i| bz.sample(i as f32 / 16.0)).collect();
            pl.extend(Shape::dashed_line(&pts, stroke, 4.0, 3.0));
            pl.circle_filled(from, 2.5, col);
            pl.vline(to.x - 1.0, (to.y - 5.0)..=(to.y + 5.0), Stroke::new(2.0, col));
            if strong {
                let label = format!("until {}", aiframes_core::until_text(&Time::Anchored(a.clone())));
                let g = pl.layout_no_wrap(label, FontId::proportional(9.5), col);
                let mid = pos2((from.x + to.x) * 0.5 - g.size().x * 0.5, from.y.max(to.y) + 4.0);
                pl.rect_filled(Rect::from_min_size(mid - vec2(3.0, 0.0), g.size() + vec2(6.0, 1.0)), CornerRadius::same(2), alpha(rgb(0x0D, 0x0E, 0x10), 0.85));
                pl.galley(mid, g, col);
            }
        };
        // Track mattes: a dashed link between a clip and its matte source (either selected).
        for c in &p.clips {
            let Some(m) = &c.matte else { continue };
            let strong = cx.is_selected(&c.id);
            if !strong && !cx.is_selected(&m.clip) {
                continue;
            }
            let (Some((ra, _)), Some((rb, _))) = (center_y(&c.id), center_y(&m.clip)) else { continue };
            let (from, to) = (rb.center(), ra.center());
            let col = alpha(MATTE, if strong { 0.95 } else { 0.6 });
            pl.extend(Shape::dashed_line(&[from, to], Stroke::new(if strong { 1.6 } else { 1.0 }, col), 5.0, 3.0));
            pl.circle_filled(from, 3.0, col);
            pl.circle_stroke(to, 3.5, Stroke::new(1.2, col));
            if strong {
                let label = format!("matte {} · {}", m.clip, crate::inspector::matte_mode_label(m.mode).to_lowercase());
                let g = pl.layout_no_wrap(label, FontId::proportional(9.5), col);
                let mid = pos2((from.x + to.x) * 0.5 + 6.0, (from.y + to.y) * 0.5 - g.size().y * 0.5);
                pl.rect_filled(Rect::from_min_size(mid - vec2(3.0, 0.0), g.size() + vec2(6.0, 1.0)), CornerRadius::same(2), alpha(rgb(0x0D, 0x0E, 0x10), 0.85));
                pl.galley(mid, g, col);
            }
        }
        // Dependants of the selection (thin lines; many — e.g. word-anchored captions — are drawn fainter).
        let deps: Vec<&Clip> = p.clips.iter().filter(|c| !cx.is_selected(&c.id) && util::anchor_parent(&c.start).is_some_and(|par| cx.is_selected(par))).collect();
        let faint = deps.len() > 8;
        for c in &p.clips {
            if cx.is_selected(&c.id) {
                draw_link(c, true, false);
                draw_end_link(c, true);
            } else if c.until.as_ref().and_then(util::anchor_parent).is_some_and(|t| cx.is_selected(t)) {
                draw_end_link(c, false);
            }
        }
        for c in deps {
            draw_link(c, false, faint);
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Clips
    // ---------------------------------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_clip(&mut self, painter: &Painter, preview: &dyn MediaPreview, p: &Project, r: &Resolved, c: &Clip, span: Span, rect: Rect, lay: &Layout, look: &Look) {
        let vis = rect.intersect(lay.lanes);
        if vis.width() <= 0.0 || vis.height() <= 0.0 {
            return;
        }
        let pc = painter.with_clip_rect(rect.expand(2.0).intersect(lay.lanes));
        let kind = util::clip_kind_on_track(p, c);
        let mut base = style::clip_color(kind);
        if look.error {
            base = rgb(0x8A, 0x26, 0x26);
        }
        if !c.enabled {
            base = shade(style::desaturate(base, 0.75), -0.25);
        }
        if look.track_off {
            base = shade(base, -0.35);
        }
        let a = if look.dim { 0.32 } else if look.ghost { 0.82 } else { 1.0 };
        let fill = if look.selected { shade(base, 0.12) } else { base };
        pc.rect_filled(rect, CornerRadius::same(3), alpha(fill, a));
        let has_label = rect.height() >= 30.0;
        let content = super::content_of(rect);
        let cpc = pc.with_clip_rect(content.intersect(lay.lanes));
        let tint = alpha(Color32::WHITE, a * if c.enabled { 1.0 } else { 0.45 });
        let asset = util::asset_of(p, c);
        match (kind, asset) {
            (ClipKind::Video | ClipKind::Image, Some(asset)) => {
                let sound = kind == ClipKind::Video && util::has_sound(p, c) && self.show_waveforms && content.height() >= 40.0;
                let (film, wave_area) = if sound {
                    let split = content.top() + content.height() * 0.62;
                    (Rect::from_min_max(content.min, pos2(content.right(), split)), Some(Rect::from_min_max(pos2(content.left(), split), content.max)))
                } else {
                    (content, None)
                };
                if self.show_filmstrips && film.height() >= 12.0 {
                    self.filmstrip(&cpc, preview, c, asset, span, film, lay, tint, kind == ClipKind::Image);
                }
                if let Some(w) = wave_area {
                    cpc.rect_filled(w, CornerRadius::ZERO, alpha(rgb(0x10, 0x18, 0x24), 0.55 * a));
                    self.waveform(&cpc, preview, c, asset, span, w, lay, alpha(style::wave_color(kind), 0.85 * a), c.muted || look.track_off);
                }
            }
            (ClipKind::Audio, Some(asset)) => {
                if self.show_waveforms {
                    self.waveform(&cpc, preview, c, asset, span, content, lay, alpha(style::wave_color(kind), 0.9 * a), c.muted || look.track_off);
                }
            }
            (ClipKind::Text, _) => {
                if let Source::Text(t) = &c.source {
                    if content.height() >= 12.0 && !has_label {
                        // Label bar is skipped on short rows; the name already shows the text.
                    } else if content.height() >= 12.0 {
                        let first = t.text.lines().next().unwrap_or("");
                        let col = style::color32(t.color);
                        let x = content.left().max(lay.lanes.left()) + 6.0;
                        cpc.text(pos2(x, content.center().y), Align2::LEFT_CENTER, "T", FontId::proportional(13.0), alpha(shade(col, 0.0), a));
                        cpc.text(pos2(x + 14.0, content.center().y), Align2::LEFT_CENTER, first, FontId::proportional(11.0), alpha(rgb(0xF4, 0xE6, 0xEE), a * 0.9));
                    }
                }
            }
            (ClipKind::Shape, _) => {
                if let Source::Shape(s) = &c.source {
                    let x = content.left().max(lay.lanes.left()) + 12.0;
                    let ic = Rect::from_center_size(pos2(x, content.center().y), vec2(14.0, 10.0));
                    let col = s.stroke.as_ref().map(|k| style::color32(k.color)).or(s.fill.map(style::color32)).unwrap_or(Color32::WHITE);
                    let st = Stroke::new(1.5, alpha(col, a));
                    match s.kind {
                        ShapeKind::Rect => {
                            cpc.rect_stroke(ic, CornerRadius::same(2), st, StrokeKind::Middle);
                        }
                        ShapeKind::Ellipse => {
                            cpc.circle_stroke(ic.center(), 5.0, st);
                        }
                        ShapeKind::Line => {
                            cpc.line_segment([ic.left_center(), ic.right_center()], st);
                        }
                        ShapeKind::Arrow => cpc.arrow(ic.left_bottom(), ic.right_top() - ic.left_bottom(), st),
                    }
                    if content.height() >= 14.0 && !has_label {
                        cpc.text(pos2(x + 12.0, content.center().y), Align2::LEFT_CENTER, format!("{:?}", s.kind), FontId::proportional(10.5), alpha(Color32::WHITE, a));
                    }
                }
            }
            (ClipKind::Solid, _) => {
                if let Source::Solid { color } = &c.source {
                    let sw = content.shrink(3.0);
                    let mut col = style::color32(*color);
                    col = Color32::from_rgb(col.r(), col.g(), col.b());
                    cpc.rect_filled(sw, CornerRadius::same(2), alpha(col, a));
                    cpc.rect_stroke(sw, CornerRadius::same(2), Stroke::new(1.0, alpha(Color32::WHITE, 0.15 * a)), StrokeKind::Inside);
                }
            }
            (ClipKind::Adjustment, _) => {
                let mut x = content.left() - content.height() - ((content.left() - lay.lanes.left()).rem_euclid(9.0));
                while x < content.right() {
                    cpc.line_segment([pos2(x, content.bottom()), pos2(x + content.height(), content.top())], Stroke::new(1.0, alpha(Color32::WHITE, 0.08 * a)));
                    x += 9.0;
                }
                cpc.text(pos2(content.left().max(lay.lanes.left()) + 6.0, content.center().y), Align2::LEFT_CENTER, "fx", FontId::proportional(12.0), alpha(rgb(0xE8, 0xE8, 0xB0), a));
            }
            _ => {
                cpc.text(pos2(content.left().max(lay.lanes.left()) + 6.0, content.center().y), Align2::LEFT_CENTER, "media missing", FontId::proportional(10.5), alpha(rgb(0xFF, 0xC0, 0xC0), a));
            }
        }

        // Fades.
        let z = self.zoom as f32;
        let shade_col = alpha(Color32::BLACK, 0.38 * a);
        let ramp = Stroke::new(1.0, alpha(Color32::WHITE, 0.75 * a));
        if c.fade_in > 0.0 {
            let fx = rect.left() + c.fade_in as f32 * z;
            pc.add(Shape::convex_polygon(vec![rect.left_top(), pos2(fx, rect.top()), rect.left_bottom()], shade_col, Stroke::NONE));
            pc.line_segment([rect.left_bottom(), pos2(fx, rect.top())], ramp);
        }
        if c.fade_out > 0.0 {
            let fx = rect.right() - c.fade_out as f32 * z;
            pc.add(Shape::convex_polygon(vec![pos2(fx, rect.top()), rect.right_top(), rect.right_bottom()], shade_col, Stroke::NONE));
            pc.line_segment([pos2(fx, rect.top()), rect.right_bottom()], ramp);
        }

        // Volume line of unselected sound clips not at unity (selected clips get the editable line).
        if self.show_waveforms && util::has_sound(p, c) && !look.selected && (c.volume.is_animated() || c.volume != aiframes_core::Param::Value(1.0)) && content.height() >= 16.0 && !look.dim {
            let x0 = rect.left().max(lay.lanes.left());
            let x1 = rect.right().min(lay.lanes.right());
            let mut pts = Vec::new();
            let mut x = x0;
            while x <= x1 {
                let tl = util::clamp_safe(lay.tmap.t(x) - span.start, 0.0, c.duration);
                let g = eval::sample(&c.volume, tl);
                let f = super::headers::gain_to_frac(g);
                pts.push(pos2(x, content.bottom() - 2.0 - (content.height() - 4.0) * f));
                x += 3.0;
            }
            if pts.len() >= 2 {
                pc.add(Shape::line(pts, Stroke::new(1.2, alpha(rgb(0xFF, 0xD6, 0x5A), 0.9 * a))));
            }
        }

        // Label bar: link glyph, name, badges.
        if has_label {
            let bar = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + LABEL_H));
            pc.rect_filled(bar, CornerRadius { nw: 3, ne: 3, sw: 0, se: 0 }, alpha(Color32::BLACK, 0.28 * a));
            let lp = pc.with_clip_rect(bar.intersect(lay.lanes));
            let mut x = rect.left().max(lay.lanes.left()) + 4.0;
            if matches!(c.start, Time::Anchored(_)) {
                style::link_glyph(&lp, pos2(x + 4.0, bar.center().y), alpha(if look.selected { style::ACCENT } else { rgb(0xC8, 0xD8, 0xF0) }, a));
                x += 12.0;
            }
            // Badges from the right (after the end-anchor glyph of clips with `until`).
            let mut bx = rect.right().min(lay.lanes.right()) - 3.0;
            if c.until.is_some() && bx - x > 40.0 {
                style::end_link_glyph(&lp, pos2(bx - 6.0, bar.center().y), alpha(if look.selected { style::ACCENT } else { rgb(0xC8, 0xD8, 0xF0) }, a));
                bx -= 15.0;
            }
            let mut badges: Vec<(String, Color32)> = Vec::new();
            if look.error {
                badges.push(("!".into(), style::ERROR));
            } else if !look.ghost {
                if let Some(sev) = self.flags.get(&c.id) {
                    badges.push(("!".into(), if *sev == aiframes_core::Severity::Error { style::ERROR } else { style::WARN }));
                }
            }
            if !c.effects.is_empty() {
                badges.push((if c.effects.len() == 1 { "fx".into() } else { format!("fx{}", c.effects.len()) }, rgb(0x6B, 0x4F, 0xA8)));
            }
            // Round 6: motion blur, frame blend, 3-D, track matte.
            if c.motion_blur.is_some() {
                badges.push(("MB".into(), rgb(0x2F, 0x6F, 0xA8)));
            }
            if c.frame_blend {
                badges.push(("FB".into(), rgb(0x2F, 0x6F, 0xA8)));
            }
            if crate::edits::is_3d(c) {
                badges.push(("3D".into(), rgb(0x9A, 0x62, 0x2A)));
            }
            if c.matte.is_some() {
                badges.push(("matte".into(), MATTE));
            }
            match &c.speed {
                aiframes_core::Param::Value(s) if (*s - 1.0).abs() > 1e-9 => {
                    let txt = if *s == 0.0 { "freeze".to_string() } else { format!("{}×", trim_num(*s)) };
                    badges.push((txt, rgb(0x2F, 0x7F, 0x8F)));
                }
                aiframes_core::Param::Animated { .. } => badges.push(("ramp".into(), rgb(0x2F, 0x7F, 0x8F))),
                _ => {}
            }
            if c.reverse {
                badges.push(("rev".into(), rgb(0x2F, 0x7F, 0x8F)));
            }
            if c.muted && util::has_sound(p, c) {
                badges.push(("muted".into(), rgb(0x80, 0x60, 0x20)));
            }
            if !c.enabled {
                badges.push(("off".into(), rgb(0x55, 0x55, 0x55)));
            }
            let name_room = (bx - x).max(0.0);
            if name_room > 30.0 {
                for (t, col) in &badges {
                    let w = lp.layout_no_wrap(t.clone(), style::tiny(), Color32::WHITE).size().x + 6.0;
                    if bx - w < x + 24.0 {
                        break;
                    }
                    style::badge(&lp, pos2(bx, bar.top() + 1.0), t, alpha(*col, a), alpha(Color32::WHITE, a), true);
                    bx -= w + 2.0;
                }
            }
            let name = util::clip_label(p, c);
            let name_clip = lp.with_clip_rect(Rect::from_min_max(pos2(x, bar.top()), pos2(bx, bar.bottom())).intersect(lay.lanes));
            name_clip.text(pos2(x, bar.center().y), Align2::LEFT_CENTER, name, FontId::proportional(10.5), alpha(rgb(0xF2, 0xF2, 0xF2), a));
        } else {
            let name = util::clip_label(p, c);
            pc.text(pos2(rect.left().max(lay.lanes.left()) + 4.0, rect.center().y), Align2::LEFT_CENTER, name, FontId::proportional(10.0), alpha(rgb(0xF2, 0xF2, 0xF2), a));
        }

        // Keyframe ticks.
        let keys = util::keyframe_times(c);
        if !keys.is_empty() && rect.height() >= 20.0 {
            let y = rect.bottom() - 4.5;
            for k in keys {
                let x = rect.left() + (k * self.zoom) as f32;
                if x < rect.left() - 1.0 || x > rect.right() + 1.0 {
                    continue;
                }
                style::diamond(&pc, pos2(x, y), if look.selected { 3.5 } else { 2.6 }, alpha(style::KEYFRAME, 0.95 * a), Stroke::new(0.8, alpha(Color32::BLACK, 0.6 * a)));
            }
        }

        // Matte sources are not drawn themselves: dimmed, with diagonal stripes.
        if self.matte_src.contains(&c.id) && !look.ghost {
            pc.rect_filled(rect, CornerRadius::same(3), alpha(Color32::BLACK, 0.38 * a));
            let mut x = rect.left() - rect.height();
            while x < rect.right() {
                pc.line_segment([pos2(x, rect.bottom()), pos2(x + rect.height(), rect.top())], Stroke::new(1.2, alpha(MATTE, 0.45 * a)));
                x += 6.0;
            }
        }

        // Disabled stripes.
        if !c.enabled {
            let mut x = rect.left() - rect.height();
            while x < rect.right() {
                pc.line_segment([pos2(x, rect.bottom()), pos2(x + rect.height(), rect.top())], Stroke::new(1.0, alpha(Color32::BLACK, 0.25)));
                x += 7.0;
            }
        }

        // Outline and hover feedback.
        if look.ghost {
            dashed_rect(&pc, rect.shrink(0.5), Stroke::new(1.2, alpha(Color32::WHITE, 0.9)));
        } else if look.error {
            pc.rect_stroke(rect, CornerRadius::same(3), Stroke::new(1.5, style::ERROR), StrokeKind::Inside);
        } else if look.selected {
            pc.rect_stroke(rect, CornerRadius::same(3), Stroke::new(1.5, style::SELECT), StrokeKind::Inside);
        } else {
            pc.rect_stroke(rect, CornerRadius::same(3), Stroke::new(1.0, alpha(Color32::BLACK, 0.55 * a)), StrokeKind::Inside);
        }
        if let Some(z) = look.hovered {
            let hl = Stroke::new(2.0, style::SELECT);
            match z {
                Zone::Start => {
                    pc.vline(rect.left() + 1.0, rect.y_range(), hl);
                }
                Zone::End => {
                    pc.vline(rect.right() - 1.0, rect.y_range(), hl);
                }
                _ => {}
            }
        }
        // Fade handles (hover/selected).
        if (look.hovered.is_some() || look.selected) && rect.width() >= 28.0 && !look.ghost {
            for (x, active) in [
                ((rect.left() + c.fade_in as f32 * z).max(rect.left() + geom::FADE_HANDLE * 0.5), look.hovered == Some(Zone::FadeIn)),
                ((rect.right() - c.fade_out as f32 * z).min(rect.right() - geom::FADE_HANDLE * 0.5), look.hovered == Some(Zone::FadeOut)),
            ] {
                let hr = Rect::from_center_size(pos2(x, rect.top() + geom::FADE_HANDLE * 0.5), vec2(7.0, 7.0));
                pc.rect_filled(hr, CornerRadius::same(1), if active { style::SNAP } else { alpha(Color32::WHITE, 0.85) });
                pc.rect_stroke(hr, CornerRadius::same(1), Stroke::new(1.0, Color32::BLACK), StrokeKind::Outside);
            }
        }
        if look.ai > 0.0 {
            style::glow(&pc, rect, style::AI, look.ai);
        }
        let _ = r;
    }

    #[allow(clippy::too_many_arguments)]
    fn filmstrip(&mut self, pc: &Painter, preview: &dyn MediaPreview, c: &Clip, asset: &MediaAsset, span: Span, area: Rect, lay: &Layout, tint: Color32, still: bool) {
        let tile_w = (area.height() * asset_aspect(asset)).max(8.0);
        let vis_l = area.left().max(lay.lanes.left());
        let vis_r = area.right().min(lay.lanes.right());
        if vis_r <= vis_l {
            return;
        }
        let first = ((vis_l - area.left()) / tile_w).floor().max(0.0) as i64;
        let last = ((vis_r - area.left()) / tile_w).ceil() as i64;
        let tile_secs = tile_w as f64 / self.zoom;
        for k in first..last.min(first + 240) {
            let x = area.left() + k as f32 * tile_w;
            let tile = Rect::from_min_size(pos2(x, area.top()), vec2(tile_w, area.height()));
            let t_local = ((k as f64 + 0.5) * tile_secs).clamp(0.0, (c.duration - 1e-3).max(0.0));
            let src = if still || asset.kind == MediaKind::Image { 0.0 } else { eval::source_time(c, t_local).unwrap_or(0.0).max(0.0) };
            match preview.thumbnail(asset, src) {
                Some(tex) => {
                    pc.image(tex, tile, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), tint);
                }
                None => {
                    pc.rect_filled(tile.shrink(1.0), CornerRadius::ZERO, alpha(Color32::BLACK, 0.12));
                }
            }
            pc.vline(tile.right(), tile.y_range(), Stroke::new(1.0, alpha(Color32::BLACK, 0.35)));
        }
        let _ = span;
    }

    #[allow(clippy::too_many_arguments)]
    fn waveform(&mut self, pc: &Painter, preview: &dyn MediaPreview, c: &Clip, asset: &MediaAsset, span: Span, area: Rect, lay: &Layout, color: Color32, muted: bool) {
        let mid = area.center().y;
        let half = (area.height() * 0.5 - 1.0).max(1.0);
        let stream = match &c.source {
            Source::Media { stream, .. } => *stream,
            _ => 0,
        };
        let Some(pyr) = self.waves.get(preview, asset, stream) else {
            pc.hline(area.x_range(), mid, Stroke::new(1.0, alpha(color, 0.4)));
            return;
        };
        let color = if muted { alpha(rgb(0x90, 0x90, 0x90), 0.5) } else { color };
        let x0 = area.left().max(lay.lanes.left()).floor();
        let x1 = area.right().min(lay.lanes.right()).ceil();
        let px = 1.0 / self.zoom;
        let mut mesh = Mesh::default();
        let mut x = x0;
        while x < x1 {
            let tl0 = lay.tmap.t(x) - span.start;
            let tl1 = tl0 + px;
            if tl1 >= 0.0 && tl0 <= c.duration {
                let (a, b) = (tl0.max(0.0), tl1.min(c.duration));
                if let (Some(s0), Some(s1)) = (eval::source_time(c, a), eval::source_time(c, b)) {
                    let (lo, hi) = if s0 <= s1 { (s0, s1) } else { (s1, s0) };
                    let (mn, mx) = pyr.range(lo, hi.max(lo + 1e-4));
                    let g = (eval::sample(&c.volume, a) * eval::fade_gain(c, a)).clamp(0.0, 4.0) as f32;
                    let top = mid - (mx * g).min(1.0) * half;
                    let bot = mid - (mn * g).max(-1.0) * half;
                    mesh.add_colored_rect(Rect::from_min_max(pos2(x, top.min(mid - 0.5)), pos2(x + 1.0, bot.max(mid + 0.5))), color);
                }
            }
            x += 1.0;
        }
        pc.add(Shape::mesh(mesh));
    }
}

fn trim_num(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn paint_transitions(pl: &Painter, p: &Project, r: &Resolved, lay: &Layout, cx: &EditCtx, ghost: bool) {
    for t in &p.transitions {
        let (Some(sf), Some(fc)) = (r.span(&t.from), p.clips.iter().find(|c| c.id == t.from)) else { continue };
        let Some(row) = lay.row_of_track(&fc.track) else { continue };
        let rect = lay.trans_rect(row, sf.end, t.duration);
        if rect.right() < lay.lanes.left() || rect.left() > lay.lanes.right() {
            continue;
        }
        let sel = cx.is_selected(&t.id);
        let col = rgb(0x6D, 0x5A, 0xC8);
        pl.rect_filled(rect, CornerRadius::same(3), alpha(col, if ghost { 0.6 } else { 0.85 }));
        pl.line_segment([rect.left_bottom(), rect.right_top()], Stroke::new(1.0, alpha(Color32::WHITE, 0.55)));
        pl.line_segment([rect.left_top(), rect.right_bottom()], Stroke::new(1.0, alpha(Color32::WHITE, 0.25)));
        let label = aiframes_core::transition_spec(&t.kind).map(|s| s.label).unwrap_or(t.kind.as_str());
        if rect.width() > 46.0 {
            let g = pl.layout_no_wrap(label.to_string(), FontId::proportional(9.5), Color32::WHITE);
            if g.size().x + 6.0 < rect.width() {
                let at = rect.center() - g.size() * 0.5;
                pl.rect_filled(Rect::from_min_size(at - vec2(2.0, 0.0), g.size() + vec2(4.0, 0.0)), CornerRadius::same(2), alpha(col, 0.95));
                pl.galley(at, g, Color32::WHITE);
            }
        }
        pl.rect_stroke(rect, CornerRadius::same(3), if sel { Stroke::new(1.5, style::SELECT) } else { Stroke::new(1.0, alpha(Color32::BLACK, 0.5)) }, StrokeKind::Inside);
        if cx.is_ai(&t.id) {
            style::glow(pl, rect, style::AI, 0.35 + 0.65 * cx.ai_flash);
        }
    }
}
