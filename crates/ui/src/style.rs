//! The small accent palette shared by the widgets (everything else comes from `ui.visuals()`), plus
//! drawing helpers for the dense pro-NLE look: chips, badges, tiny painted icons.

use aiframes_core::{Color, MarkerKind, TrackKind};
use egui::{pos2, vec2, Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Stroke, StrokeKind, Ui};

use crate::util::ClipKind;

pub const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// The AI's colour (glows, history entries, "Ask AI").
pub const AI: Color32 = rgb(0xA7, 0x8B, 0xFA);
/// Primary accent (focus, links, the playhead timecode).
pub const ACCENT: Color32 = rgb(0x4C, 0xA3, 0xFF);
pub const PLAYHEAD: Color32 = rgb(0xFF, 0x45, 0x3A);
pub const SNAP: Color32 = rgb(0xFF, 0xD6, 0x0A);
pub const ERROR: Color32 = rgb(0xE5, 0x48, 0x4D);
pub const WARN: Color32 = rgb(0xFF, 0x9F, 0x0A);
pub const OK: Color32 = rgb(0x30, 0xD1, 0x58);
pub const IN_OUT: Color32 = rgb(0x4C, 0xA3, 0xFF);
pub const SELECT: Color32 = rgb(0xF2, 0xF2, 0xF2);
pub const KEYFRAME: Color32 = rgb(0xFF, 0xD6, 0x0A);
pub const MUTE_ON: Color32 = rgb(0xE8, 0xB3, 0x2E);
pub const SOLO_ON: Color32 = rgb(0x45, 0xC0, 0x6A);
pub const LOCK_ON: Color32 = rgb(0xD9, 0x5C, 0x5C);

/// Clip body colour by kind (muted, so thumbnails and waveforms read on top of it).
pub fn clip_color(kind: ClipKind) -> Color32 {
    match kind {
        ClipKind::Video => rgb(0x35, 0x5F, 0x91),
        ClipKind::Audio => rgb(0x2B, 0x6E, 0x4E),
        ClipKind::Image => rgb(0x5E, 0x50, 0x93),
        ClipKind::Text => rgb(0x93, 0x45, 0x72),
        ClipKind::Shape => rgb(0x9C, 0x60, 0x2A),
        ClipKind::Solid => rgb(0x5A, 0x5A, 0x5A),
        ClipKind::Adjustment => rgb(0x6E, 0x6E, 0x2C),
        ClipKind::Missing => rgb(0x6A, 0x2A, 0x2A),
    }
}

/// Waveform colour drawn over a clip of this kind.
pub fn wave_color(kind: ClipKind) -> Color32 {
    match kind {
        ClipKind::Audio => rgb(0x8E, 0xE6, 0xB4),
        _ => rgb(0xB5, 0xD3, 0xF5),
    }
}

pub fn track_kind_color(kind: TrackKind) -> Color32 {
    match kind {
        TrackKind::Video => rgb(0x4A, 0x7F, 0xC1),
        TrackKind::Audio => rgb(0x3C, 0x9A, 0x6A),
    }
}

pub fn marker_color(kind: MarkerKind, custom: Option<Color>) -> Color32 {
    if let Some(c) = custom {
        return color32(c);
    }
    match kind {
        MarkerKind::Marker => rgb(0x4F, 0xC3, 0xF7),
        MarkerKind::Chapter => rgb(0xFF, 0xB0, 0x20),
        MarkerKind::Beat => rgb(0xB3, 0x88, 0xFF),
        MarkerKind::Todo => rgb(0xFF, 0x52, 0x52),
    }
}

/// Core colour → egui colour.
pub fn color32(c: Color) -> Color32 {
    let [r, g, b, a] = c.to_rgba8();
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// egui colour → core colour.
pub fn core_color(c: Color32) -> Color {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    Color::rgba8(r, g, b, a)
}

/// Lighter (`k > 0`) or darker (`k < 0`) version of a colour.
pub fn shade(c: Color32, k: f32) -> Color32 {
    let f = |v: u8| -> u8 {
        let v = v as f32;
        let out = if k >= 0.0 { v + (255.0 - v) * k } else { v * (1.0 + k) };
        out.clamp(0.0, 255.0) as u8
    };
    Color32::from_rgba_unmultiplied(f(c.r()), f(c.g()), f(c.b()), c.a())
}

/// The same colour at alpha `a` (0..1).
pub fn alpha(c: Color32, a: f32) -> Color32 {
    let [r, g, b, _] = c.to_srgba_unmultiplied();
    Color32::from_rgba_unmultiplied(r, g, b, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

/// Grey version (for disabled clips).
pub fn desaturate(c: Color32, k: f32) -> Color32 {
    let l = (c.r() as f32 * 0.3 + c.g() as f32 * 0.59 + c.b() as f32 * 0.11) as u8;
    let mix = |v: u8| (v as f32 * (1.0 - k) + l as f32 * k) as u8;
    Color32::from_rgb(mix(c.r()), mix(c.g()), mix(c.b()))
}

pub fn small() -> FontId {
    FontId::proportional(10.5)
}

pub fn tiny() -> FontId {
    FontId::proportional(9.5)
}

pub fn body() -> FontId {
    FontId::proportional(12.0)
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

/// Text colour that reads on `bg`.
pub fn text_on(bg: Color32) -> Color32 {
    let l = bg.r() as f32 * 0.3 + bg.g() as f32 * 0.59 + bg.b() as f32 * 0.11;
    if l > 150.0 {
        rgb(0x10, 0x10, 0x10)
    } else {
        rgb(0xF0, 0xF0, 0xF0)
    }
}

/// A small rounded badge with text; returns its width. `right_aligned`: `pos` is the right edge.
pub fn badge(p: &Painter, pos: Pos2, text: &str, bg: Color32, fg: Color32, right_aligned: bool) -> f32 {
    let galley = p.layout_no_wrap(text.to_string(), tiny(), fg);
    let w = galley.size().x + 6.0;
    let h = 12.0;
    let x = if right_aligned { pos.x - w } else { pos.x };
    let r = Rect::from_min_size(pos2(x, pos.y), vec2(w, h));
    p.rect_filled(r, CornerRadius::same(3), bg);
    p.galley(pos2(x + 3.0, pos.y + (h - galley.size().y) * 0.5), galley, fg);
    w
}

/// A coloured chip with text for panels (author chips, kind badges). Returns its response.
pub fn chip(ui: &mut Ui, text: &str, bg: Color32) -> egui::Response {
    let fg = text_on(bg);
    let galley = ui.painter().layout_no_wrap(text.to_string(), small(), fg);
    let size = vec2(galley.size().x + 10.0, 16.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(8), bg);
    ui.painter().galley(rect.center() - galley.size() * 0.5, galley, fg);
    resp
}

/// A diamond (keyframe) shape.
pub fn diamond(p: &Painter, c: Pos2, r: f32, fill: Color32, stroke: Stroke) {
    let pts = vec![pos2(c.x, c.y - r), pos2(c.x + r, c.y), pos2(c.x, c.y + r), pos2(c.x - r, c.y)];
    p.add(egui::Shape::convex_polygon(pts, fill, stroke));
}

/// A tiny chain-link glyph (anchored clips).
pub fn link_glyph(p: &Painter, c: Pos2, color: Color32) {
    let s = Stroke::new(1.2, color);
    let a = Rect::from_center_size(pos2(c.x - 2.2, c.y + 1.2), vec2(5.5, 3.6));
    let b = Rect::from_center_size(pos2(c.x + 2.2, c.y - 1.2), vec2(5.5, 3.6));
    p.rect_stroke(a, CornerRadius::same(2), s, StrokeKind::Middle);
    p.rect_stroke(b, CornerRadius::same(2), s, StrokeKind::Middle);
}

/// (round 4) The colour of a validation severity (errors red, warnings orange, notes grey).
pub fn severity_color(s: aiframes_core::Severity) -> Color32 {
    match s {
        aiframes_core::Severity::Error => ERROR,
        aiframes_core::Severity::Warning => WARN,
        aiframes_core::Severity::Info => rgb(0x9A, 0x9A, 0x9A),
    }
}

/// (round 4) The end-anchor glyph: a link followed by a bar ("ends with …"), centred at `c`.
pub fn end_link_glyph(p: &Painter, c: Pos2, color: Color32) {
    link_glyph(p, pos2(c.x - 2.0, c.y), color);
    p.vline(c.x + 5.0, (c.y - 4.5)..=(c.y + 4.5), Stroke::new(1.6, color));
}

/// A tiny eye glyph (track visibility); `off` draws it struck through.
pub fn eye_glyph(p: &Painter, c: Pos2, color: Color32, off: bool) {
    let s = Stroke::new(1.2, color);
    let pts: Vec<Pos2> = (0..=16)
        .map(|i| {
            let a = std::f32::consts::TAU * i as f32 / 16.0;
            pos2(c.x + a.cos() * 5.0, c.y + a.sin() * 2.8)
        })
        .collect();
    p.add(egui::Shape::closed_line(pts, s));
    p.circle_filled(c, 1.6, color);
    if off {
        p.line_segment([pos2(c.x - 5.0, c.y + 4.0), pos2(c.x + 5.0, c.y - 4.0)], Stroke::new(1.4, color));
    }
}

/// A tiny padlock glyph.
pub fn lock_glyph(p: &Painter, c: Pos2, color: Color32, locked: bool) {
    let body = Rect::from_center_size(pos2(c.x, c.y + 1.5), vec2(7.0, 5.0));
    p.rect_filled(body, CornerRadius::same(1), color);
    let s = Stroke::new(1.2, color);
    let top = c.y - 1.0;
    let (l, r) = (c.x - 2.2, c.x + 2.2);
    let r_x = if locked { r } else { r + 2.5 };
    let pts = vec![pos2(l, top), pos2(l, top - 2.0), pos2(l + 1.0, top - 3.6), pos2(r_x - 1.0, top - 3.6), pos2(r_x, top - 2.0), pos2(r_x, if locked { top } else { top - 1.0 })];
    p.add(egui::Shape::line(pts, s));
}

/// A speaker glyph (audio track).
pub fn speaker_glyph(p: &Painter, c: Pos2, color: Color32) {
    let pts = vec![pos2(c.x - 4.0, c.y - 1.5), pos2(c.x - 2.0, c.y - 1.5), pos2(c.x + 1.0, c.y - 4.0), pos2(c.x + 1.0, c.y + 4.0), pos2(c.x - 2.0, c.y + 1.5), pos2(c.x - 4.0, c.y + 1.5)];
    p.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
    p.line_segment([pos2(c.x + 3.0, c.y - 2.0), pos2(c.x + 3.0, c.y + 2.0)], Stroke::new(1.0, color));
}

/// A film-frame glyph (video track).
pub fn film_glyph(p: &Painter, c: Pos2, color: Color32) {
    let r = Rect::from_center_size(c, vec2(9.0, 7.0));
    p.rect_stroke(r, CornerRadius::same(1), Stroke::new(1.1, color), StrokeKind::Middle);
    p.line_segment([pos2(r.left() + 2.5, r.top()), pos2(r.left() + 2.5, r.bottom())], Stroke::new(1.0, color));
    p.line_segment([pos2(r.right() - 2.5, r.top()), pos2(r.right() - 2.5, r.bottom())], Stroke::new(1.0, color));
}

/// A soft outer glow around `rect` (AI highlight).
pub fn glow(p: &Painter, rect: Rect, color: Color32, strength: f32) {
    let s = strength.clamp(0.0, 1.0);
    for i in 1..=4 {
        let k = i as f32;
        let a = (0.5 - k * 0.1).max(0.06) * (0.4 + 0.6 * s);
        p.rect_stroke(rect.expand(k * 1.3), CornerRadius::same((3.0 + k) as u8), Stroke::new(1.5, alpha(color, a)), StrokeKind::Outside);
    }
    p.rect_stroke(rect, CornerRadius::same(3), Stroke::new(1.5, alpha(color, 0.7 + 0.3 * s)), StrokeKind::Inside);
}

/// Text centred in a rect, clipped to it.
pub fn centered_text(p: &Painter, rect: Rect, text: &str, font: FontId, color: Color32) {
    p.with_clip_rect(rect.intersect(p.clip_rect())).text(rect.center(), Align2::CENTER_CENTER, text, font, color);
}

/// A section heading in panels.
pub fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(text).size(11.0).strong().color(ui.visuals().weak_text_color()));
}

/// A panel with a dense dark look (the gallery and app can use it for consistency).
pub fn apply_dark(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = vec2(6.0, 4.0);
        s.spacing.button_padding = vec2(6.0, 2.0);
        s.spacing.interact_size.y = 20.0;
        s.visuals.panel_fill = rgb(0x1E, 0x1F, 0x22);
        s.visuals.window_fill = rgb(0x24, 0x25, 0x29);
        s.visuals.extreme_bg_color = rgb(0x14, 0x15, 0x17);
        s.visuals.faint_bg_color = rgb(0x26, 0x27, 0x2B);
        s.visuals.selection.bg_fill = alpha(ACCENT, 0.45);
        s.visuals.hyperlink_color = ACCENT;
    });
}
