//! Form fields that commit once: while a value is being dragged or typed it lives here (and is what
//! the field shows); when the pointer is released / focus leaves, the field returns the new value
//! once, so a slider drag becomes a single undoable op.

use aiframes_core::Color;
use egui::{color_picker::Alpha, Color32, DragValue, Key, Response, Sense, Slider, Stroke, TextEdit, Ui};
use std::collections::HashMap;
use std::ops::RangeInclusive;

use crate::style;
use crate::util;

#[derive(Default)]
pub(crate) struct Fields {
    nums: HashMap<String, f64>,
    colors: HashMap<String, Color32>,
    texts: HashMap<String, String>,
}

/// How a number is shown: `display = value * scale` (e.g. opacity 0..1 shown as 0..100 %), or as
/// decibels of a linear gain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Scale {
    Linear(f64),
    Db,
}

impl Scale {
    pub fn show(self, v: f64) -> f64 {
        match self {
            Scale::Linear(k) => v * k,
            Scale::Db => {
                if v <= 1e-6 {
                    -60.0
                } else {
                    (20.0 * v.log10()).max(-60.0)
                }
            }
        }
    }
    pub fn model(self, d: f64) -> f64 {
        match self {
            Scale::Linear(k) => d / k,
            Scale::Db => {
                if d <= -59.99 {
                    0.0
                } else {
                    10f64.powf(d / 20.0)
                }
            }
        }
    }
}

/// A number field's look.
#[derive(Clone, Debug)]
pub(crate) struct NumSpec {
    pub min: f64,
    pub max: f64,
    pub speed: f64,
    pub suffix: &'static str,
    pub decimals: usize,
    pub scale: Scale,
    pub slider: bool,
    pub width: f32,
    /// (added) A registry parameter: the field is a slider over its slider range and curve
    /// (`aiframes_core::effects::slider_value`) plus a value field that scrubs and accepts typed
    /// values over the whole valid range `min..max`.
    pub travel: Option<aiframes_core::ParamSpec>,
    /// (added) Whole numbers only.
    pub integer: bool,
}

impl NumSpec {
    pub fn drag(min: f64, max: f64, speed: f64, suffix: &'static str, decimals: usize) -> Self {
        NumSpec { min, max, speed, suffix, decimals, scale: Scale::Linear(1.0), slider: false, width: 70.0, travel: None, integer: false }
    }
    pub fn slider(min: f64, max: f64, suffix: &'static str, decimals: usize) -> Self {
        NumSpec { min, max, speed: (max - min) / 300.0, suffix, decimals, scale: Scale::Linear(1.0), slider: true, width: 150.0, travel: None, integer: false }
    }
    pub fn scaled(mut self, scale: Scale) -> Self {
        self.scale = scale;
        self
    }
}

/// What a number field did this frame (model units).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct NumOut {
    /// The edit finished (pointer released, focus left): apply it, once.
    pub commit: Option<f64>,
    /// The value while it is being dragged or typed (not committed yet): preview it.
    pub live: Option<f64>,
}

fn commit_num(map: &mut HashMap<String, f64>, key: &str, current: f64, v: f64, r: &Response) -> Option<f64> {
    commit_num_with(map, key, current, v, r.changed(), busy(r))
}

/// A widget is still being used: the pointer is down on it, it is dragged, or it is being typed in.
fn busy(r: &Response) -> bool {
    r.is_pointer_button_down_on() || r.dragged() || r.has_focus()
}

fn commit_num_with(map: &mut HashMap<String, f64>, key: &str, current: f64, v: f64, changed: bool, busy: bool) -> Option<f64> {
    if changed {
        map.insert(key.to_string(), v);
    }
    if !busy {
        if let Some(v) = map.remove(key) {
            if (v - current).abs() > 1e-12 {
                return Some(v);
            }
        }
    }
    None
}

/// A slider over a registry parameter's slider range and curve, then its value (drag to scrub,
/// double-click to type anything in the valid range). `v` is in display units. Returns (changed,
/// still in use).
fn travel_field(ui: &mut Ui, v: &mut f64, ps: &aiframes_core::ParamSpec, spec: &NumSpec) -> (bool, bool) {
    use aiframes_core::effects::{slider_pos, slider_value};
    let value_w = 64.0f32;
    let mut pos = slider_pos(ps, spec.scale.model(*v));
    let slider_w = (fitw(ui, spec.width + value_w + 4.0) - value_w - 4.0).max(36.0);
    ui.spacing_mut().slider_width = slider_w;
    let rs = ui.add(Slider::new(&mut pos, 0.0..=1.0).show_value(false).trailing_fill(true));
    let mut changed = false;
    if rs.changed() {
        let x = slider_value(ps, pos);
        *v = spec.scale.show(if spec.integer { x.round() } else { x });
        changed = true;
    }
    let lo = spec.scale.show(ps.min).min(spec.scale.show(ps.max));
    let hi = spec.scale.show(ps.min).max(spec.scale.show(ps.max));
    let [s0, s1] = ps.slider_range();
    let speed = ((spec.scale.show(s1) - spec.scale.show(s0)).abs() / 300.0).max(if spec.integer { 0.05 } else { 1e-4 });
    let mut d = DragValue::new(v).speed(speed).range(lo..=hi).suffix(spec.suffix).max_decimals(spec.decimals).update_while_editing(false);
    if spec.integer {
        d = d.fixed_decimals(0);
    }
    let rd = ui.add_sized([value_w, 18.0], d);
    (changed || rd.changed(), busy(&rs) || busy(&rd))
}

/// (round 5) `want` points, or less when the row has less room (never below 36): fixed widths in
/// the inspector shrink in narrow side panels instead of pushing them wider.
pub(crate) fn fitw(ui: &Ui, want: f32) -> f32 {
    want.min((ui.available_width() - 2.0).max(36.0))
}

/// (round 5) Runs `add` (e.g. a truncating ComboBox) with at most `want` points of width (less in a
/// narrow row): truncated text then stops at that width instead of filling the row.
pub(crate) fn narrow<R>(ui: &mut Ui, want: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let w = fitw(ui, want);
    ui.scope(|ui| {
        ui.set_max_width(w);
        add(ui)
    })
    .inner
}

impl Fields {
    /// A number field (drag value or slider) in display units; returns the new *model* value once.
    pub fn num(&mut self, ui: &mut Ui, key: &str, current: f64, spec: &NumSpec) -> Option<f64> {
        self.num_ex(ui, key, current, spec).commit
    }

    /// (added) A number field that also reports the value while it is dragged or typed, for live
    /// previews; the commit still happens once, when the edit ends (one undo step per drag).
    pub fn num_ex(&mut self, ui: &mut Ui, key: &str, current: f64, spec: &NumSpec) -> NumOut {
        let cur_d = spec.scale.show(current);
        let mut v = self.nums.get(key).copied().unwrap_or(cur_d);
        if let Some(ps) = spec.travel {
            let (changed, busy) = travel_field(ui, &mut v, &ps, spec);
            if spec.integer {
                v = v.round();
            }
            let commit = commit_num_with(&mut self.nums, key, cur_d, v, changed, busy).map(|d| spec.scale.model(d));
            let live = self.nums.get(key).copied().filter(|d| (d - cur_d).abs() > 1e-12).map(|d| spec.scale.model(d));
            return NumOut { commit, live };
        }
        let r = if spec.slider {
            ui.spacing_mut().slider_width = (fitw(ui, spec.width + 60.0) - 60.0).max(40.0);
            let mut s = Slider::new(&mut v, spec.min..=spec.max).suffix(spec.suffix).max_decimals(spec.decimals).clamping(egui::SliderClamping::Edits);
            if spec.integer {
                s = s.integer();
            }
            ui.add(s)
        } else {
            let mut d = DragValue::new(&mut v).speed(spec.speed).range(spec.min..=spec.max).suffix(spec.suffix).max_decimals(spec.decimals).update_while_editing(false);
            if spec.integer {
                d = d.fixed_decimals(0);
            }
            ui.add_sized([fitw(ui, spec.width), 18.0], d)
        };
        if spec.integer {
            v = v.round();
        }
        let commit = commit_num(&mut self.nums, key, cur_d, v, &r).map(|d| spec.scale.model(d));
        let live = self.nums.get(key).copied().filter(|d| (d - cur_d).abs() > 1e-12).map(|d| spec.scale.model(d));
        NumOut { commit, live }
    }

    /// A timecode field (seconds in the model, `HH:MM:SS:FF` on screen; typing accepts seconds,
    /// `m:ss.s` or timecode). Frame-quantised.
    pub fn time(&mut self, ui: &mut Ui, key: &str, current: f64, fps: f64, range: RangeInclusive<f64>) -> Option<f64> {
        let mut v = self.nums.get(key).copied().unwrap_or(current);
        let r = ui.add_sized(
            [fitw(ui, 92.0), 18.0],
            DragValue::new(&mut v)
                .speed(1.0 / fps)
                .range(range)
                .custom_formatter(move |x, _| util::timecode(x, fps))
                .custom_parser(move |s| util::parse_time(s, fps))
                .update_while_editing(false),
        );
        commit_num(&mut self.nums, key, current, util::quantize(v, fps), &r).map(|t| util::quantize(t, fps))
    }

    /// A colour button; returns the new colour once the pointer is released.
    pub fn color(&mut self, ui: &mut Ui, key: &str, current: Color) -> Option<Color> {
        self.color_ex(ui, key, current).0
    }

    /// (added) A colour button: `(committed, live)` — the colour once the pointer is released, and the
    /// colour being picked while the pointer is down (for previews).
    pub fn color_ex(&mut self, ui: &mut Ui, key: &str, current: Color) -> (Option<Color>, Option<Color>) {
        let cur = style::color32(current);
        let mut c = self.colors.get(key).copied().unwrap_or(cur);
        let r = egui::color_picker::color_edit_button_srgba(ui, &mut c, Alpha::OnlyBlend);
        if r.changed() {
            self.colors.insert(key.to_string(), c);
        }
        if !ui.input(|i| i.pointer.any_down()) {
            if let Some(c) = self.colors.remove(key) {
                if c != cur {
                    return (Some(style::core_color(c)), None);
                }
            }
            return (None, None);
        }
        (None, self.colors.get(key).filter(|c| **c != cur).map(|c| style::core_color(*c)))
    }

    /// A text field that commits when focus leaves (Escape cancels).
    pub fn text(&mut self, ui: &mut Ui, key: &str, current: &str, multiline: bool, width: f32, hint: &str) -> Option<String> {
        let buf = self.texts.entry(key.to_string()).or_insert_with(|| current.to_string());
        let r = if multiline {
            ui.add(TextEdit::multiline(buf).desired_rows(3).desired_width(fitw(ui, width)).hint_text(hint))
        } else {
            ui.add(TextEdit::singleline(buf).desired_width(fitw(ui, width)).hint_text(hint))
        };
        if r.has_focus() {
            return None;
        }
        let v = self.texts.remove(key).unwrap_or_default();
        if r.lost_focus() && !ui.input(|i| i.key_pressed(Key::Escape)) && v != current {
            return Some(v);
        }
        None
    }

    /// Forgets every in-progress edit (e.g. when the selection changes).
    pub fn clear(&mut self) {
        self.nums.clear();
        self.colors.clear();
        self.texts.clear();
    }
}

/// A small painted triangle button (◀ / ▶ keyframe navigation).
pub(crate) fn tri_button(ui: &mut Ui, left: bool, enabled: bool, tip: &str) -> Response {
    let (rect, r) = ui.allocate_exact_size(egui::vec2(12.0, 16.0), if enabled { Sense::click() } else { Sense::hover() });
    let col = if !enabled {
        ui.visuals().weak_text_color().gamma_multiply(0.5)
    } else if r.hovered() {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().text_color()
    };
    let c = rect.center();
    let pts = if left {
        vec![egui::pos2(c.x - 3.5, c.y), egui::pos2(c.x + 2.5, c.y - 4.0), egui::pos2(c.x + 2.5, c.y + 4.0)]
    } else {
        vec![egui::pos2(c.x + 3.5, c.y), egui::pos2(c.x - 2.5, c.y - 4.0), egui::pos2(c.x - 2.5, c.y + 4.0)]
    };
    ui.painter().add(egui::Shape::convex_polygon(pts, col, Stroke::NONE));
    r.on_hover_text(tip)
}

/// Arrow direction of [`dir_button`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dir {
    Up,
    Down,
}

/// A small painted up/down arrow button (the default fonts have no triangles).
pub(crate) fn dir_button(ui: &mut Ui, dir: Dir, enabled: bool, tip: &str) -> Response {
    let (rect, r) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), if enabled { Sense::click() } else { Sense::hover() });
    if r.hovered() && enabled {
        ui.painter().rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let col = if enabled { ui.visuals().text_color() } else { ui.visuals().weak_text_color().gamma_multiply(0.5) };
    let c = rect.center();
    let pts = match dir {
        Dir::Up => vec![egui::pos2(c.x, c.y - 3.5), egui::pos2(c.x + 4.0, c.y + 2.5), egui::pos2(c.x - 4.0, c.y + 2.5)],
        Dir::Down => vec![egui::pos2(c.x, c.y + 3.5), egui::pos2(c.x - 4.0, c.y - 2.5), egui::pos2(c.x + 4.0, c.y - 2.5)],
    };
    ui.painter().add(egui::Shape::convex_polygon(pts, col, Stroke::NONE));
    r.on_hover_text(tip)
}

/// A drag handle (three short lines).
pub(crate) fn grip(ui: &mut Ui) -> Response {
    let (rect, r) = ui.allocate_exact_size(egui::vec2(12.0, 16.0), Sense::drag());
    let col = if r.hovered() || r.dragged() { ui.visuals().strong_text_color() } else { ui.visuals().weak_text_color() };
    for k in [-3.0, 0.0, 3.0] {
        ui.painter().hline((rect.left() + 2.0)..=(rect.right() - 2.0), rect.center().y + k, Stroke::new(1.2, col));
    }
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }
    r
}

/// The keyframe toggle: a filled diamond when there is a key at the playhead.
pub(crate) fn key_button(ui: &mut Ui, on_key: bool, animated: bool, enabled: bool) -> Response {
    let (rect, r) = ui.allocate_exact_size(egui::vec2(14.0, 16.0), if enabled { Sense::click() } else { Sense::hover() });
    let col = if !enabled {
        ui.visuals().weak_text_color().gamma_multiply(0.5)
    } else if animated {
        style::KEYFRAME
    } else {
        ui.visuals().weak_text_color()
    };
    let col = if r.hovered() { style::shade(col, 0.25) } else { col };
    let fill = if on_key { col } else { Color32::TRANSPARENT };
    style::diamond(ui.painter(), rect.center(), 4.5, fill, Stroke::new(1.2, col));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_round_trip() {
        let s = Scale::Linear(100.0);
        assert_eq!(s.show(0.5), 50.0);
        assert_eq!(s.model(50.0), 0.5);
        let d = Scale::Db;
        assert!((d.show(1.0)).abs() < 1e-12);
        assert!((d.model(-6.0) - 0.501187).abs() < 1e-5);
        assert_eq!(d.model(-60.0), 0.0);
        assert_eq!(d.show(0.0), -60.0);
    }
}
