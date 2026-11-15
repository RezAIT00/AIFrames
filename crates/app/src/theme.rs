//! Dark, dense, Resolve/Premiere-like look built on the suite's `suite-ui` spacing and fonts.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle};

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub bg_app: Color32,
    pub panel: Color32,
    pub panel_dark: Color32,
    pub field: Color32,
    pub viewer: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub text_faint: Color32,
    /// AIFrames violet (selection, focus, active tabs).
    pub accent: Color32,
    /// Everything the AI did (log, flashes).
    pub ai: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub error: Color32,
}

pub const fn palette() -> Palette {
    Palette {
        bg_app: Color32::from_rgb(0x16, 0x16, 0x18),
        panel: Color32::from_rgb(0x22, 0x22, 0x25),
        panel_dark: Color32::from_rgb(0x1b, 0x1b, 0x1e),
        field: Color32::from_rgb(0x15, 0x15, 0x17),
        viewer: Color32::from_rgb(0x0b, 0x0b, 0x0c),
        text: Color32::from_rgb(0xd6, 0xd6, 0xda),
        text_dim: Color32::from_rgb(0x8c, 0x8c, 0x94),
        text_faint: Color32::from_rgb(0x5c, 0x5c, 0x64),
        accent: Color32::from_rgb(0x8b, 0x6c, 0xff),
        ai: Color32::from_rgb(0xff, 0xb0, 0x2e),
        ok: Color32::from_rgb(0x5f, 0xcf, 0x80),
        warn: Color32::from_rgb(0xff, 0xc4, 0x4d),
        error: Color32::from_rgb(0xff, 0x62, 0x62),
    }
}

pub fn apply(ctx: &egui::Context) {
    let p = palette();
    // Suite spacing first, then the dark visuals on top.
    suite_ui::theme::apply(ctx, p.accent, Color32::from_rgb(0x3a, 0x2f, 0x6e));
    ctx.set_visuals(egui::Visuals::dark());
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(6.0, 3.0);
        style.spacing.button_padding = egui::vec2(6.0, 2.0);
        style.spacing.interact_size = egui::vec2(36.0, 20.0);
        style.spacing.menu_margin = egui::Margin::same(5);
        style.spacing.window_margin = egui::Margin::same(10);
        style.spacing.scroll.bar_width = 8.0;
        style.text_styles = [
            (TextStyle::Small, FontId::new(10.5, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(12.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(12.5, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(16.0, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(12.0, FontFamily::Monospace)),
        ]
        .into();
        let v = &mut style.visuals;
        v.dark_mode = true;
        v.panel_fill = p.panel;
        v.window_fill = Color32::from_rgb(0x26, 0x26, 0x2a);
        v.window_stroke = Stroke::new(1.0, Color32::from_rgb(0x3a, 0x3a, 0x40));
        v.extreme_bg_color = p.field;
        v.faint_bg_color = Color32::from_rgb(0x28, 0x28, 0x2c);
        v.code_bg_color = p.field;
        v.selection.bg_fill = Color32::from_rgb(0x4a, 0x3b, 0x8f);
        v.selection.stroke = Stroke::new(1.0, p.accent);
        v.hyperlink_color = Color32::from_rgb(0x9f, 0x8c, 0xff);
        v.window_corner_radius = CornerRadius::same(6);
        v.menu_corner_radius = CornerRadius::same(4);
        v.widgets.noninteractive.bg_fill = p.panel;
        v.widgets.noninteractive.weak_bg_fill = p.panel;
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x30, 0x30, 0x35));
        v.widgets.inactive.bg_fill = Color32::from_rgb(0x2e, 0x2e, 0x33);
        v.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x2c, 0x2c, 0x31);
        v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x3a, 0x3a, 0x40));
        v.widgets.hovered.bg_fill = Color32::from_rgb(0x3a, 0x3a, 0x42);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x3a, 0x3a, 0x42);
        v.widgets.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x55, 0x55, 0x60));
        v.widgets.active.bg_fill = Color32::from_rgb(0x4a, 0x3b, 0x8f);
        v.widgets.active.weak_bg_fill = Color32::from_rgb(0x4a, 0x3b, 0x8f);
        v.widgets.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
        v.widgets.open.weak_bg_fill = Color32::from_rgb(0x34, 0x34, 0x3a);
        for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.corner_radius = CornerRadius::same(3);
        }
    });
}

/// Frame for a panel body.
pub fn panel_frame() -> egui::Frame {
    egui::Frame::NONE.fill(palette().panel).inner_margin(egui::Margin::same(6))
}

/// Frame for menu/status/tab bars.
pub fn bar_frame() -> egui::Frame {
    egui::Frame::NONE.fill(palette().panel_dark).inner_margin(egui::Margin::symmetric(6, 2))
}

/// A small header strip above a panel ("MEDIA", "PROGRAM").
pub fn panel_header(ui: &mut egui::Ui, title: &str) {
    ui.label(egui::RichText::new(title).small().strong().color(palette().text_dim));
}

/// The app icon (a violet film frame with a play mark), for the window and the taskbar.
pub fn icon() -> egui::IconData {
    let n = 64u32;
    let mut rgba = vec![0u8; (n * n * 4) as usize];
    let p = palette();
    for y in 0..n {
        for x in 0..n {
            let i = ((y * n + x) * 4) as usize;
            let (fx, fy) = (x as f32, y as f32);
            let inside = (4.0..60.0).contains(&fx) && (8.0..56.0).contains(&fy);
            if !inside {
                continue;
            }
            let mut c = p.accent;
            // Sprocket holes along the top and bottom edges.
            let hole = ((8.0..14.0).contains(&fy) || (50.0..56.0).contains(&fy)) && ((x / 6) % 2 == 1) && (8.0..56.0).contains(&fx);
            if hole {
                continue;
            }
            // Play triangle in the middle.
            let (cx, cy) = (fx - 26.0, fy - 32.0);
            if (0.0..=16.0).contains(&cx) && cy.abs() <= (16.0 - cx) * 0.62 {
                c = Color32::WHITE;
            }
            rgba[i..i + 4].copy_from_slice(&[c.r(), c.g(), c.b(), 255]);
        }
    }
    egui::IconData { rgba, width: n, height: n }
}

#[cfg(test)]
mod tests {
    use eframe::egui::{self, FontFamily, FontId};

    /// Symbols the app, the core's summaries and the AI's replies use. They must render (no tofu)
    /// in both the proportional and the monospace UI fonts.
    const SYMBOLS: &str = "✅❌⭐🎬📌→←↔⇒⇄⇥✓✔✗✕…—–×·•◆▸▶◀■●⚠⚙⟲⏭⏮⏴⏵⏸Σ›“”≥≤≠≈−±°µ";

    #[test]
    fn ui_fonts_cover_core_and_ai_symbols() {
        let windir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into());
        if !windir.join("Fonts").join("segoeui.ttf").exists() {
            return; // no Windows fonts on this machine
        }
        let ctx = egui::Context::default();
        let _fonts = suite_ui::fonts::FontManager::new(&ctx);
        super::apply(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
        let mut missing = Vec::new();
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            for c in SYMBOLS.chars() {
                if !ctx.fonts_mut(|f| f.has_glyph(&FontId::new(12.0, family.clone()), c)) {
                    missing.push(format!("{c} (U+{:04X}, {family:?})", c as u32));
                }
            }
        }
        ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
        assert!(missing.is_empty(), "missing glyphs: {}", missing.join(", "));
        // The check itself works: a private-use code point has no glyph.
        assert!(!ctx.fonts_mut(|f| f.has_glyph(&FontId::proportional(12.0), '\u{10FFFD}')));
    }
}
