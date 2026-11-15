//! Office-like light theme shared by the suite's apps; each app supplies its accent colour.

use egui::{Color32, CornerRadius, Stroke};

pub const RIBBON_BG: Color32 = Color32::from_rgb(0xF3, 0xF3, 0xF3);
pub const RIBBON_BORDER: Color32 = Color32::from_rgb(0xD6, 0xD6, 0xD6);
pub const PANEL_BG: Color32 = Color32::from_rgb(0xFA, 0xFA, 0xFA);
pub const TEXT: Color32 = Color32::from_rgb(0x20, 0x20, 0x20);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x60, 0x60, 0x60);
pub const WARN_BG: Color32 = Color32::from_rgb(0xFF, 0xF4, 0xCE);
pub const WARN_BORDER: Color32 = Color32::from_rgb(0xE8, 0xC4, 0x6A);
/// Outline of idle form widgets (check boxes, radio buttons, text fields, combo boxes).
pub const INACTIVE_BORDER: Color32 = Color32::from_rgb(0x9A, 0x9A, 0x9A);

/// Standard colour palette for colour pickers (Office theme rows + "Standard Colors").
pub const PALETTE: [[u32; 10]; 7] = [
    [0xFFFFFF, 0x000000, 0xE7E6E6, 0x44546A, 0x4472C4, 0xED7D31, 0xA5A5A5, 0xFFC000, 0x5B9BD5, 0x70AD47],
    [0xF2F2F2, 0x7F7F7F, 0xD0CECE, 0xD6DCE4, 0xD9E1F2, 0xFCE4D6, 0xEDEDED, 0xFFF2CC, 0xDDEBF7, 0xE2EFDA],
    [0xD9D9D9, 0x595959, 0xAEAAAA, 0xACB9CA, 0xB4C6E7, 0xF8CBAD, 0xDBDBDB, 0xFFE699, 0xBDD7EE, 0xC6E0B4],
    [0xBFBFBF, 0x404040, 0x757171, 0x8497B0, 0x8EA9DB, 0xF4B084, 0xC9C9C9, 0xFFD966, 0x9BC2E6, 0xA9D08E],
    [0xA6A6A6, 0x262626, 0x3A3838, 0x333F4F, 0x305496, 0xC65911, 0x7B7B7B, 0xBF8F00, 0x2F75B5, 0x548235],
    [0x808080, 0x0D0D0D, 0x161616, 0x222B35, 0x203764, 0x833C0C, 0x525252, 0x806000, 0x1F4E78, 0x375623],
    [0xC00000, 0xFF0000, 0xFFC000, 0xFFFF00, 0x92D050, 0x00B050, 0x00B0F0, 0x0070C0, 0x002060, 0x7030A0],
];

pub fn rgb(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// Apply the light Office-like style. `accent` is the app colour (Excel green, PowerPoint orange),
/// `selection_bg` a light tint of it.
pub fn apply(ctx: &egui::Context, accent: Color32, selection_bg: Color32) {
    ctx.set_visuals(egui::Visuals::light());
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(6.0, 3.0);
        style.spacing.interact_size.y = 22.0;
        style.spacing.menu_margin = egui::Margin::same(6);
        let v = &mut style.visuals;
        v.selection.bg_fill = selection_bg;
        v.selection.stroke = Stroke::new(1.0, accent);
        v.hyperlink_color = Color32::from_rgb(0x05, 0x63, 0xC1);
        v.window_corner_radius = CornerRadius::same(6);
        v.menu_corner_radius = CornerRadius::same(4);
        v.panel_fill = RIBBON_BG;
        v.window_fill = Color32::WHITE;
        v.extreme_bg_color = Color32::WHITE;
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
        v.widgets.inactive.bg_fill = Color32::from_rgb(0xFD, 0xFD, 0xFD);
        // Idle check boxes, radio buttons, text fields, combo boxes and dialog buttons need a
        // visible outline on white dialogs (ribbon buttons are custom-painted and unaffected).
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, INACTIVE_BORDER);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0xE1, 0xE1, 0xE1);
        v.widgets.hovered.bg_fill = Color32::from_rgb(0xE1, 0xE1, 0xE1);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0xC8, 0xC8, 0xC8));
        v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
        v.widgets.active.weak_bg_fill = Color32::from_rgb(0xC8, 0xC8, 0xC8);
        v.widgets.active.bg_fill = Color32::from_rgb(0xC8, 0xC8, 0xC8);
        v.widgets.active.fg_stroke = Stroke::new(1.0, TEXT);
        v.widgets.open.weak_bg_fill = Color32::from_rgb(0xD6, 0xD6, 0xD6);
        for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.corner_radius = CornerRadius::same(3);
        }
    });
}
