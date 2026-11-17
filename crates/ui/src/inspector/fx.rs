//! Effect controls: the effects stack of a clip (one card per effect with a control per registry
//! parameter, keyframes, reset, enable, reorder, copy/paste), paste attributes between clips, the
//! transition controls, and the compact "Effect controls" panel docked under the effects browser.
//!
//! Every field shows the registry's units and uses its slider range and curve, previews live in the
//! viewer while it is dragged (`UiAction::Preview`), and commits one op when the drag ends.

use super::param::{reset_button, Local};
use super::*;
use crate::widgets::{fitw, narrow, NumSpec, Scale};
use aiframes_core::{effect_spec, eval, transition_spec, Effect, EffectArg, Param, ParamSpec, EFFECTS};
use egui::{ComboBox, Grid, RichText};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// What was copied with "Copy effect(s)" / "Copy attributes" (shared by every project in the window).
#[derive(Clone, Debug, Default)]
pub(crate) struct FxClipboard {
    pub effects: Vec<Effect>,
    /// The clip whose attributes were copied (transform, opacity, blend, volume, fades, effects).
    pub clip: Option<Clip>,
}

static CLIPBOARD: Mutex<Option<FxClipboard>> = Mutex::new(None);

pub(crate) fn clipboard() -> FxClipboard {
    CLIPBOARD.lock().map(|c| c.clone().unwrap_or_default()).unwrap_or_default()
}

fn set_clipboard(f: impl FnOnce(&mut FxClipboard)) {
    if let Ok(mut c) = CLIPBOARD.lock() {
        f(c.get_or_insert_with(FxClipboard::default));
    }
}

/// How a registry parameter's field looks: its units (fractions as %), its slider range and curve,
/// whole numbers for counts.
pub(crate) fn effect_num_spec(ps: &ParamSpec) -> NumSpec {
    let (suffix, scale, decimals): (&'static str, Scale, usize) = match ps.unit {
        "frac" => (" %", Scale::Linear(100.0), 1),
        "px" => (" px", Scale::Linear(1.0), 1),
        "deg" => ("°", Scale::Linear(1.0), 1),
        "x" => ("×", Scale::Linear(1.0), 2),
        "stops" => (" EV", Scale::Linear(1.0), 2),
        "hz" => (" Hz", Scale::Linear(1.0), 2),
        "s" => (" s", Scale::Linear(1.0), 3),
        "ratio" => (":1", Scale::Linear(1.0), 2),
        "count" => ("", Scale::Linear(1.0), 0),
        _ => ("", Scale::Linear(1.0), 2),
    };
    let [lo, hi] = [scale.show(ps.min), scale.show(ps.max)];
    NumSpec { min: lo, max: hi, speed: (hi - lo) / 300.0, suffix, decimals, scale, slider: true, width: 110.0, travel: Some(*ps), integer: ps.unit == "count" }
}

/// `set_effect` changing one parameter of effect `index` (`None` = back to the registry default).
pub(crate) fn effect_param_op(clip: &str, index: usize, name: &str, v: Option<Param>) -> Op {
    let mut params = BTreeMap::new();
    params.insert(name.to_string(), v);
    Op::SetEffect { clip: clip.into(), index, enabled: None, params, colors: Default::default(), strings: Default::default() }
}

/// `set_effect` resetting every parameter and colour of an effect to the registry defaults (string
/// settings such as a LUT file stay).
pub(crate) fn reset_effect_op(clip: &str, index: usize, e: &Effect) -> Op {
    let params = e.params.keys().map(|k| (k.clone(), None)).collect();
    let colors = e.colors.keys().map(|k| (k.clone(), None)).collect();
    Op::SetEffect { clip: clip.into(), index, enabled: None, params, colors, strings: Default::default() }
}

/// The ops pasting `effects` onto `clip`: appended, or replacing its stack.
pub(crate) fn paste_effects_ops(c: &Clip, effects: &[Effect], replace: bool) -> Vec<Op> {
    let mut ops = Vec::new();
    if replace {
        for i in (0..c.effects.len()).rev() {
            ops.push(Op::RemoveEffect { clip: c.id.clone(), index: i });
        }
    }
    for e in effects {
        ops.push(Op::AddEffect { clip: c.id.clone(), effect: EffectArg::Full(e.clone()), index: None });
    }
    ops
}

/// A JSON merge patch that makes `dst` equal `src` (keys `src` lacks are removed, i.e. take their
/// defaults; objects recurse).
fn replace_patch(src: &Value, dst: &Value) -> Value {
    match (src, dst) {
        (Value::Object(s), Value::Object(d)) => {
            let mut out = serde_json::Map::new();
            for (k, sv) in s {
                let p = match d.get(k) {
                    Some(dv) => replace_patch(sv, dv),
                    None => sv.clone(),
                };
                out.insert(k.clone(), p);
            }
            for k in d.keys().filter(|k| !s.contains_key(*k)) {
                out.insert(k.clone(), Value::Null);
            }
            Value::Object(out)
        }
        _ => src.clone(),
    }
}

/// Groups of clip attributes "Paste attributes" can copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Attr {
    Transform,
    OpacityBlend,
    VolumeFades,
    EffectsAdd,
    EffectsReplace,
}

impl Attr {
    pub const ALL: [Attr; 5] = [Attr::Transform, Attr::OpacityBlend, Attr::VolumeFades, Attr::EffectsAdd, Attr::EffectsReplace];
    pub fn label(self) -> &'static str {
        match self {
            Attr::Transform => "Transform (position, scale, rotation, crop, fit)",
            Attr::OpacityBlend => "Opacity and blend mode",
            Attr::VolumeFades => "Volume and fades",
            Attr::EffectsAdd => "Effects (add to the clip's own)",
            Attr::EffectsReplace => "Effects (replace the clip's own)",
        }
    }
}

/// The ops pasting `attrs` of `src` onto `dst` (keyframes included; they are clip-local).
pub(crate) fn paste_attributes_ops(src: &Clip, dst: &Clip, attrs: &[Attr]) -> Vec<Op> {
    let (sj, dj) = (serde_json::to_value(src).unwrap_or(Value::Null), serde_json::to_value(dst).unwrap_or(Value::Null));
    let mut patch = serde_json::Map::new();
    let mut take = |keys: &[&str]| {
        for k in keys {
            let s = sj.get(*k).cloned().unwrap_or(Value::Null);
            let p = match dj.get(*k) {
                Some(d) if !s.is_null() => replace_patch(&s, d),
                _ => s,
            };
            patch.insert(k.to_string(), p);
        }
    };
    for a in attrs {
        match a {
            Attr::Transform => take(&["transform"]),
            Attr::OpacityBlend => take(&["opacity", "blend"]),
            Attr::VolumeFades => take(&["volume", "fade_in", "fade_out"]),
            _ => {}
        }
    }
    let mut ops = Vec::new();
    if !patch.is_empty() {
        ops.push(Op::SetClip { clip: dst.id.clone(), patch: Value::Object(patch) });
    }
    if attrs.contains(&Attr::EffectsReplace) {
        ops.extend(paste_effects_ops(dst, &src.effects, true));
    } else if attrs.contains(&Attr::EffectsAdd) {
        ops.extend(paste_effects_ops(dst, &src.effects, false));
    }
    ops
}

/// A seed's "new pattern" value (deterministic, so undo/redo and the AI see plain numbers).
fn next_seed(v: f64) -> f64 {
    ((v.max(0.0) as u64).wrapping_mul(1_103_515_245).wrapping_add(12_345) % 1_000_000) as f64
}

/// Drag payload for reordering an effect stack.
#[derive(Clone, Debug)]
struct EffectSlot {
    clip: Id,
    index: usize,
}

impl Inspector {
    /// The "+ Add effect" menu (by category) and the copy/paste buttons of a clip's effects.
    fn effects_toolbar(&mut self, ui: &mut Ui, c: &Clip, ops: &mut Vec<(String, Op)>) {
        let clip_fx = clipboard().effects;
        ui.horizontal_wrapped(|ui| {
            ui.menu_button(RichText::new("+ Add effect").strong(), |ui| {
                let mut cats: Vec<&str> = Vec::new();
                for e in EFFECTS {
                    if !cats.contains(&e.category) {
                        cats.push(e.category);
                    }
                }
                for cat in cats {
                    ui.menu_button(util::capitalize(cat), |ui| {
                        for e in EFFECTS.iter().filter(|e| e.category == cat) {
                            if ui.button(e.label).on_hover_text(e.help).clicked() {
                                ops.push((format!("Add {}", e.label), Op::AddEffect { clip: c.id.clone(), effect: EffectArg::Kind(e.kind.into()), index: None }));
                                ui.close();
                            }
                        }
                    });
                }
            });
            if !c.effects.is_empty() && ui.small_button("Copy all").on_hover_text("Copy this clip's effects (with their settings and keyframes) to paste on other clips").clicked() {
                set_clipboard(|b| b.effects = c.effects.clone());
                ui.ctx().copy_text(serde_json::to_string(&c.effects).unwrap_or_default());
            }
            if !clip_fx.is_empty() {
                let n = clip_fx.len();
                if ui.small_button(format!("Paste {n}")).on_hover_text(format!("Add the {n} copied effect(s) after this clip's own")).clicked() {
                    for op in paste_effects_ops(c, &clip_fx, false) {
                        ops.push(("Paste effects".into(), op));
                    }
                }
            }
        });
    }

    /// The effects of a picture clip: a card per effect. `local` is where the playhead is in it.
    pub(crate) fn effects_stack(&mut self, ui: &mut Ui, cx: &EditCtx, c: &Clip, local: &Local, ops: &mut Vec<(String, Op)>, out: &mut Vec<UiAction>) {
        self.track_new_effects(c);
        if c.effects.is_empty() {
            ui.add(egui::Label::new(RichText::new("No effects yet. Add one here, click one in the Effects tab, or drag one onto the clip.").small().weak()).wrap());
        }
        let n = c.effects.len();
        let reveal = self.reveal.as_ref().filter(|(id, _)| *id == c.id).map(|(_, i)| *i);
        for (i, e) in c.effects.iter().enumerate() {
            let open_key = format!("{}.fx{i}", c.id);
            if reveal == Some(i) {
                self.collapsed.remove(&open_key);
            }
            let flash = self.flash_of(ui, &c.id, i);
            let stroke = if flash > 0.0 { egui::Stroke::new(1.0 + flash, style::ACCENT) } else { egui::Stroke::new(1.0, style::rgb(0x2E, 0x30, 0x35)) };
            let frame = egui::Frame::new().fill(style::rgb(0x23, 0x24, 0x28)).corner_radius(4).inner_margin(egui::Margin::symmetric(6, 4)).stroke(stroke);
            let resp = frame.show(ui, |ui| self.effect_card(ui, cx, c, local, i, e, n, ops, out)).response;
            let resp = resp.interact(egui::Sense::click());
            resp.context_menu(|ui| self.effect_menu(ui, c, i, e, ops));
            if reveal == Some(i) {
                ui.scroll_to_rect(resp.rect, Some(egui::Align::TOP));
                self.reveal = None;
            }
            if let Some(slot) = resp.dnd_release_payload::<EffectSlot>() {
                if slot.clip == c.id && slot.index != i {
                    ops.push(("Reorder effects".into(), Op::MoveEffect { clip: c.id.clone(), from: slot.index, to: i }));
                }
            } else if resp.dnd_hover_payload::<EffectSlot>().is_some_and(|s| s.clip == c.id && s.index != i) {
                ui.painter().hline(resp.rect.x_range(), resp.rect.top() - 1.0, egui::Stroke::new(2.0, style::ACCENT));
            }
            ui.add_space(3.0);
        }
        self.effects_toolbar(ui, c, ops);
    }

    /// Right-click / "…" menu of an effect card.
    fn effect_menu(&mut self, ui: &mut Ui, c: &Clip, i: usize, e: &Effect, ops: &mut Vec<(String, Op)>) {
        if ui.button("Copy effect").clicked() {
            set_clipboard(|b| b.effects = vec![e.clone()]);
            ui.ctx().copy_text(serde_json::to_string(e).unwrap_or_default());
            ui.close();
        }
        let clip_fx = clipboard().effects;
        if ui.add_enabled(!clip_fx.is_empty(), egui::Button::new(format!("Paste {} effect(s) after this", clip_fx.len()))).clicked() {
            for (k, fx) in clip_fx.iter().enumerate() {
                ops.push(("Paste effect".into(), Op::AddEffect { clip: c.id.clone(), effect: EffectArg::Full(fx.clone()), index: Some(i + 1 + k) }));
            }
            ui.close();
        }
        if ui.button("Duplicate").clicked() {
            ops.push(("Duplicate effect".into(), Op::AddEffect { clip: c.id.clone(), effect: EffectArg::Full(e.clone()), index: Some(i + 1) }));
            ui.close();
        }
        if ui.add_enabled(!e.params.is_empty() || !e.colors.is_empty(), egui::Button::new("Reset to defaults")).clicked() {
            ops.push(("Reset effect".into(), reset_effect_op(&c.id, i, e)));
            ui.close();
        }
        ui.separator();
        if ui.button("Remove").clicked() {
            ops.push(("Remove effect".into(), Op::RemoveEffect { clip: c.id.clone(), index: i }));
            ui.close();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn effect_card(&mut self, ui: &mut Ui, cx: &EditCtx, c: &Clip, local: &Local, i: usize, e: &Effect, n: usize, ops: &mut Vec<(String, Op)>, out: &mut Vec<UiAction>) {
        let spec = effect_spec(&e.kind);
        let open_key = format!("{}.fx{i}", c.id);
        let open = !self.collapsed.contains(&open_key);
        ui.horizontal(|ui| {
            let handle = crate::widgets::grip(ui).on_hover_text("Drag to reorder");
            handle.dnd_set_drag_payload(EffectSlot { clip: c.id.clone(), index: i });
            let mut en = e.enabled;
            if ui.checkbox(&mut en, "").on_hover_text(if en { "On: click to bypass this effect" } else { "Bypassed: click to turn it on" }).changed() {
                ops.push((if en { "Turn effect on" } else { "Bypass effect" }.into(), Op::SetEffect { clip: c.id.clone(), index: i, enabled: Some(en), params: Default::default(), colors: Default::default(), strings: Default::default() }));
            }
            let name = spec.map(|s| s.label).unwrap_or(&e.kind);
            let title = ui.selectable_label(open, RichText::new(format!("{} {name}", if open { "▾" } else { "▸" })).strong());
            let title = match spec {
                Some(s) => title.on_hover_text(format!("{}\nClick to fold; right-click for more.", s.help)),
                None => title,
            };
            if title.clicked() {
                if open {
                    self.collapsed.insert(open_key.clone());
                } else {
                    self.collapsed.remove(&open_key);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if ui.small_button("×").on_hover_text("Remove").clicked() {
                    ops.push(("Remove effect".into(), Op::RemoveEffect { clip: c.id.clone(), index: i }));
                }
                if crate::widgets::dir_button(ui, crate::widgets::Dir::Down, i + 1 < n, "Move down (applied later)").clicked() {
                    ops.push(("Move effect".into(), Op::MoveEffect { clip: c.id.clone(), from: i, to: i + 1 }));
                }
                if crate::widgets::dir_button(ui, crate::widgets::Dir::Up, i > 0, "Move up (applied earlier)").clicked() {
                    ops.push(("Move effect".into(), Op::MoveEffect { clip: c.id.clone(), from: i, to: i - 1 }));
                }
                if (!e.params.is_empty() || !e.colors.is_empty()) && reset_button(ui, "Reset every parameter of this effect to its default").clicked() {
                    ops.push(("Reset effect".into(), reset_effect_op(&c.id, i, e)));
                }
                ui.menu_button("…", |ui| self.effect_menu(ui, c, i, e, ops)).response.on_hover_text("Copy, paste, duplicate, reset");
            });
        });
        if !open {
            return;
        }
        let Some(spec) = spec else {
            ui.colored_label(style::ERROR, format!("Unknown effect {:?}: remove it or ask the AI what it was meant to be.", e.kind));
            return;
        };
        Grid::new(("fx", &c.id, i)).num_columns(2).spacing([8.0, 3.0]).show(ui, |ui| {
            for ps in spec.params {
                let path = format!("effects.{i}.{}", ps.name);
                let cur = e.params.get(ps.name).cloned().unwrap_or(Param::Value(ps.default));
                let now = eval::sample(&cur, local.t);
                if ps.unit == "toggle" {
                    lbl(ui, ps.label).on_hover_text(ps.help);
                    let mut on = now >= 0.5;
                    if ui.checkbox(&mut on, "").changed() {
                        ops.push((ps.label.into(), effect_param_op(&c.id, i, ps.name, Some(Param::Value(if on { 1.0 } else { 0.0 })))));
                    }
                    ui.end_row();
                    continue;
                }
                if let Some(choices) = ps.choices() {
                    lbl(ui, ps.label).on_hover_text(ps.help);
                    let mut v = now.round();
                    let shown = choices.iter().find(|(x, _)| *x == v).map(|(_, l)| util::capitalize(l)).unwrap_or_else(|| format!("{v}"));
                    narrow(ui, 150.0, |ui| {
                        ComboBox::from_id_salt(("fxchoice", &c.id, i, ps.name)).width(fitw(ui, 150.0)).truncate().selected_text(shown).show_ui(ui, |ui| {
                            for (x, l) in &choices {
                                ui.selectable_value(&mut v, *x, util::capitalize(l));
                            }
                        });
                    });
                    if (v - now).abs() > 1e-9 {
                        ops.push((ps.label.into(), effect_param_op(&c.id, i, ps.name, Some(Param::Value(v)))));
                    }
                    ui.end_row();
                    continue;
                }
                if ps.name == "seed" {
                    lbl(ui, ps.label).on_hover_text(ps.help);
                    ui.horizontal(|ui| {
                        let key = format!("{}.{path}", c.id);
                        if let Some(v) = self.fields.num(ui, &key, now, &NumSpec { width: 70.0, integer: true, ..NumSpec::drag(ps.min, ps.max, 1.0, "", 0) }) {
                            ops.push((ps.label.into(), effect_param_op(&c.id, i, ps.name, Some(Param::Value(v.round())))));
                        }
                        if ui.small_button("New").on_hover_text("Another pattern (a new seed)").clicked() {
                            ops.push(("New seed".into(), effect_param_op(&c.id, i, ps.name, Some(Param::Value(next_seed(now))))));
                        }
                    });
                    ui.end_row();
                    continue;
                }
                let spec_n = effect_num_spec(ps);
                if let Some(op) = self.param_row(ui, cx, c, local, &path, ps.label, ps.help, &cur, &spec_n, Some(ps.default), out) {
                    ops.push((ps.label.into(), op));
                }
            }
            for (name, def) in spec.colors {
                lbl(ui, &util::capitalize(name)).on_hover_text(format!("Default {def}"));
                let cur = aiframes_core::effects::effect_color(e, name);
                ui.horizontal(|ui| {
                    let (commit, live) = self.fields.color_ex(ui, &format!("{}.fx{i}.{name}", c.id), cur);
                    let set = |col: Option<aiframes_core::Color>| {
                        let mut m = BTreeMap::new();
                        m.insert(name.to_string(), col);
                        Op::SetEffect { clip: c.id.clone(), index: i, enabled: None, params: Default::default(), colors: m, strings: Default::default() }
                    };
                    if let Some(nc) = commit {
                        ops.push(("Effect colour".into(), set(Some(nc))));
                    } else if let Some(lc) = live {
                        out.push(UiAction::Preview(vec![set(Some(lc))]));
                    }
                    if e.colors.contains_key(*name) && reset_button(ui, &format!("Back to {def}")).clicked() {
                        ops.push(("Reset colour".into(), set(None)));
                    }
                });
                ui.end_row();
            }
            for name in spec.strings {
                lbl(ui, if *name == "path" { "File" } else { name });
                let cur = e.strings.get(*name).cloned().unwrap_or_default();
                ui.horizontal(|ui| {
                    if let Some(v) = self.fields.text(ui, &format!("{}.fx{i}.s.{name}", c.id), &cur, false, 120.0, if e.kind == "lut" { "a .cube file" } else { "path" }) {
                        let mut m = BTreeMap::new();
                        m.insert(name.to_string(), if v.is_empty() { None } else { Some(v) });
                        ops.push(("Effect setting".into(), Op::SetEffect { clip: c.id.clone(), index: i, enabled: None, params: Default::default(), colors: Default::default(), strings: m }));
                    }
                    if ui.small_button("Browse…").on_hover_text("Pick the file").clicked() {
                        out.push(UiAction::PickEffectFile { clip: c.id.clone(), index: i, name: name.to_string() });
                    }
                });
                ui.end_row();
                if e.kind == "lut" && cur.is_empty() {
                    ui.label("");
                    ui.add(egui::Label::new(RichText::new("Pick a .cube LUT: until then the clip is unchanged.").small().color(style::WARN)).wrap());
                    ui.end_row();
                }
            }
        });
    }

    /// The effect card to reveal (expanded, scrolled into view, outlined) the next time `clip` is shown:
    /// after adding an effect, so its controls show up without hunting.
    pub fn reveal_effect(&mut self, clip: &str, index: usize) {
        self.collapsed.remove(&format!("{clip}.fx{index}"));
        self.reveal = Some((clip.to_string(), index));
        self.flash = Some((clip.to_string(), index, None));
    }

    /// Notices effects added to the shown clip by anyone (the AI, another window, undo) and reveals
    /// the first new one.
    fn track_new_effects(&mut self, c: &Clip) {
        let kinds: Vec<String> = c.effects.iter().map(|e| e.kind.clone()).collect();
        if let Some((id, before)) = &self.fx_seen {
            if *id == c.id && kinds.len() > before.len() && self.reveal.is_none() {
                let first_new = kinds.iter().zip(before).position(|(a, b)| a != b).unwrap_or(before.len());
                self.reveal_effect(&c.id, first_new);
            }
        }
        self.fx_seen = Some((c.id.clone(), kinds));
    }

    /// 0..1 strength of the outline of a just-revealed effect card (fades over 1.5 s).
    fn flash_of(&mut self, ui: &Ui, clip: &str, i: usize) -> f32 {
        let now = ui.input(|inp| inp.time);
        let Some((id, idx, start)) = &mut self.flash else { return 0.0 };
        if id != clip || *idx != i {
            return 0.0;
        }
        let t0 = *start.get_or_insert(now);
        let k = 1.0 - ((now - t0) / 1.5) as f32;
        if k <= 0.0 {
            self.flash = None;
            return 0.0;
        }
        ui.ctx().request_repaint();
        k
    }

    /// The controls of a transition's registry parameters (shared by the transition inspector and
    /// the docked effect controls): drop-downs for named values, checkboxes for toggles, sliders in
    /// their units with live preview and reset.
    pub(crate) fn transition_params(&mut self, ui: &mut Ui, t: &Transition, ops: &mut Vec<(String, Op)>, out: &mut Vec<UiAction>) {
        let Some(spec) = transition_spec(&t.kind) else { return };
        let set = |name: &str, v: Option<f64>| {
            let mut m = BTreeMap::new();
            m.insert(name.to_string(), v);
            Op::SetTransition { transition: t.id.clone(), kind: None, duration: None, params: m }
        };
        for ps in spec.params {
            lbl(ui, ps.label).on_hover_text(ps.help);
            let cur = transition_param(t, ps.name);
            if let Some(choices) = ps.choices() {
                let mut v = cur.round();
                let shown = choices.iter().find(|(x, _)| *x == v).map(|(_, l)| util::capitalize(l)).unwrap_or_else(|| format!("{v}"));
                narrow(ui, 150.0, |ui| {
                    ComboBox::from_id_salt(("tparam", &t.id, ps.name)).width(fitw(ui, 150.0)).truncate().selected_text(shown).show_ui(ui, |ui| {
                        for (x, l) in &choices {
                            ui.selectable_value(&mut v, *x, util::capitalize(l));
                        }
                    });
                });
                if (v - cur).abs() > 1e-9 {
                    ops.push((ps.label.into(), set(ps.name, Some(v))));
                }
            } else if ps.unit == "toggle" {
                let mut on = cur >= 0.5;
                if ui.checkbox(&mut on, "").changed() {
                    ops.push((ps.label.into(), set(ps.name, Some(if on { 1.0 } else { 0.0 }))));
                }
            } else if ps.name == "seed" {
                ui.horizontal(|ui| {
                    if let Some(v) = self.fields.num(ui, &format!("{}.{}", t.id, ps.name), cur, &NumSpec { width: 70.0, integer: true, ..NumSpec::drag(ps.min, ps.max, 1.0, "", 0) }) {
                        ops.push((ps.label.into(), set(ps.name, Some(v.round()))));
                    }
                    if ui.small_button("New").on_hover_text("Another pattern (a new seed)").clicked() {
                        ops.push(("New seed".into(), set(ps.name, Some(next_seed(cur)))));
                    }
                });
            } else {
                ui.horizontal(|ui| {
                    let ns = effect_num_spec(ps);
                    let r = self.fields.num_ex(ui, &format!("{}.{}", t.id, ps.name), cur, &ns);
                    if let Some(v) = r.commit {
                        ops.push((ps.label.into(), set(ps.name, Some(v))));
                    } else if let Some(v) = r.live {
                        out.push(UiAction::Preview(vec![set(ps.name, Some(v))]));
                    }
                    if t.params.contains_key(ps.name) && (cur - ps.default).abs() > 1e-9 && reset_button(ui, &format!("Reset to {}", super::param::spec_text(&ns, ps.default))).clicked() {
                        ops.push((format!("Reset {}", ps.label), set(ps.name, None)));
                    }
                });
            }
            ui.end_row();
        }
        // Colour parameters (a film burn's tint).
        for (name, def) in spec.colors {
            lbl(ui, &util::capitalize(name)).on_hover_text(format!("Default {def}"));
            ui.horizontal(|ui| {
                let cur = aiframes_core::transition_color(t, name);
                let (commit, live) = self.fields.color_ex(ui, &format!("{}.color.{name}", t.id), cur);
                if let Some(nc) = commit {
                    ops.push(("Transition colour".into(), crate::edits::transition_color_op(&t.id, name, Some(nc))));
                } else if let Some(lc) = live {
                    out.push(UiAction::Preview(vec![crate::edits::transition_color_op(&t.id, name, Some(lc))]));
                }
                if t.colors.contains_key(*name) && reset_button(ui, &format!("Back to {def}")).clicked() {
                    ops.push(("Reset transition colour".into(), crate::edits::transition_color_op(&t.id, name, None)));
                }
            });
            ui.end_row();
        }
    }

    /// "Copy attributes / Paste attributes" for the clip panel header (Premiere's Paste Attributes).
    pub(crate) fn attributes_menu(&mut self, ui: &mut Ui, targets: &[&Clip], source: Option<&Clip>, out: &mut Vec<UiAction>) {
        ui.menu_button("Attributes", |ui| {
            if let Some(src) = source {
                if ui.button("Copy attributes").on_hover_text("Transform, opacity, blend, volume, fades and effects of this clip").clicked() {
                    let src = src.clone();
                    set_clipboard(|b| {
                        b.effects = src.effects.clone();
                        b.clip = Some(src);
                    });
                    ui.close();
                }
            }
            let copied = clipboard().clip;
            let targets: Vec<&Clip> = targets.iter().copied().filter(|c| copied.as_ref().is_none_or(|s| s.id != c.id)).collect();
            ui.add_enabled_ui(copied.is_some() && !targets.is_empty(), |ui| {
                ui.label(RichText::new(match &copied {
                    Some(s) => format!("Paste from {} onto {} clip(s):", s.id, targets.len()),
                    None => "Copy a clip's attributes first".into(),
                }).small().weak());
                let mut pick: Vec<Attr> = Vec::new();
                if ui.button("All (transform, opacity, volume, effects added)").clicked() {
                    pick = vec![Attr::Transform, Attr::OpacityBlend, Attr::VolumeFades, Attr::EffectsAdd];
                }
                for a in Attr::ALL {
                    if ui.button(a.label()).clicked() {
                        pick = vec![a];
                    }
                }
                if let (false, Some(src)) = (pick.is_empty(), &copied) {
                    let ops: Vec<Op> = targets.iter().flat_map(|d| paste_attributes_ops(src, d, &pick)).collect();
                    if !ops.is_empty() {
                        out.push(UiAction::Apply { label: format!("Paste attributes from {} onto {} clip(s)", src.id, targets.len()), ops });
                    }
                    ui.close();
                }
            });
        });
    }

    /// The compact "Effect controls" panel docked under the effects browser: the selected clip's
    /// effects (or the selected transition's settings), so browsing and adjusting happen side by side
    /// like Premiere's Effect Controls and Resolve's Inspector. Call it before drawing the browser.
    pub fn effect_controls(&mut self, ui: &mut Ui, cx: &EditCtx) -> Vec<UiAction> {
        let mut out = Vec::new();
        let mut ops: Vec<(String, Op)> = Vec::new();
        egui::Panel::bottom("aiframes_fx_controls").resizable(true).default_size(280.0).size_range(90.0..=900.0).show(ui, |ui| {
            ui.add_space(4.0);
            match selected(cx.project, cx.selection) {
                Selected::Clip(c) if util::clip_kind(cx.project, c) != ClipKind::Audio => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("EFFECT CONTROLS").small().strong());
                        ui.label(RichText::new(format!("{} · {}", c.id, util::ellipsize(&util::clip_label(cx.project, c), 26))).small().weak());
                    });
                    let local = Local::of(cx, c);
                    egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("aiframes_fx_controls_scroll").show(ui, |ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                        self.effects_stack(ui, cx, c, &local, &mut ops, &mut out);
                    });
                    for (l, op) in ops.drain(..) {
                        out.push(UiAction::Apply { label: format!("{l} ({})", c.id), ops: vec![op] });
                    }
                }
                Selected::Transition(t) => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("TRANSITION").small().strong());
                        ui.label(RichText::new(format!("{} · {}", t.id, transition_spec(&t.kind).map(|s| s.label).unwrap_or(&t.kind))).small().weak());
                    });
                    egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("aiframes_tr_controls_scroll").show(ui, |ui| {
                        Grid::new(("trdock", &t.id)).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                            self.transition_duration_row(ui, cx, t, &mut ops, &mut out);
                            self.transition_params(ui, t, &mut ops, &mut out);
                        });
                    });
                    for (l, op) in ops.drain(..) {
                        out.push(UiAction::Apply { label: format!("{l} ({})", t.id), ops: vec![op] });
                    }
                }
                _ => {
                    ui.label(RichText::new("EFFECT CONTROLS").small().strong());
                    ui.add(egui::Label::new(RichText::new("Select a clip (or a transition) on the timeline to see and adjust its effects here.").small().weak()).wrap());
                }
            }
        });
        out
    }

    /// The duration row of a transition (live preview while dragging).
    pub(crate) fn transition_duration_row(&mut self, ui: &mut Ui, cx: &EditCtx, t: &Transition, ops: &mut Vec<(String, Op)>, out: &mut Vec<UiAction>) {
        let fps = util::fps(cx.project);
        lbl(ui, "Duration").on_hover_text("Length of the transition, centred on the cut.");
        let max = crate::timeline::drag::transition_max(cx.resolved, &t.from, &t.to).max(1.0 / fps);
        let r = self.fields.num_ex(ui, &format!("{}.dur", t.id), t.duration, &NumSpec::drag(1.0 / fps, max, 0.01, " s", 2));
        let set = |d: f64| Op::SetTransition { transition: t.id.clone(), kind: None, duration: Some(d), params: Default::default() };
        if let Some(d) = r.commit {
            ops.push(("Transition length".into(), set(d)));
        } else if let Some(d) = r.live {
            out.push(UiAction::Preview(vec![set(d)]));
        }
        ui.end_row();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::tests::*;
    use aiframes_core::*;

    #[test]
    fn every_registry_parameter_gets_a_field_in_its_units() {
        for (kind, ps) in EFFECTS.iter().flat_map(|e| e.params.iter().map(move |p| (e.kind, p))).chain(TRANSITIONS.iter().flat_map(|t| t.params.iter().map(move |p| (t.kind, p)))) {
            let ns = effect_num_spec(ps);
            // The field covers exactly the registry's valid range (the same one list_effects gives the AI).
            assert!((ns.scale.model(ns.min) - ps.min).abs() < 1e-9 && (ns.scale.model(ns.max) - ps.max).abs() < 1e-9, "{kind}.{}", ps.name);
            assert_eq!(ns.travel.unwrap(), *ps);
            let known = ["", "x", "stops", "px", "deg", "frac", "toggle", "count", "ratio", "hz", "s"];
            assert!(known.contains(&ps.unit), "{kind}.{}: unit {:?} has no display", ps.name, ps.unit);
            if ps.unit == "frac" {
                assert_eq!(ns.suffix, " %");
            }
        }
    }

    #[test]
    fn paste_and_reset_ops_apply() {
        let p = demo();
        let (q, _) = run(&p, &[
            Op::AddEffect { clip: "c1".into(), effect: EffectArg::Kind("blur".into()), index: None },
            Op::AddEffect { clip: "c1".into(), effect: EffectArg::Kind("vignette".into()), index: None },
            Op::SetEffect { clip: "c1".into(), index: 0, enabled: None, params: [("radius".to_string(), Some(Param::Value(40.0)))].into_iter().collect(), colors: Default::default(), strings: Default::default() },
            Op::SetParam { clip: "c1".into(), path: "transform.scale".into(), value: Some(0.5), keys: None },
            Op::SetParam { clip: "c2".into(), path: "transform.x".into(), value: Some(300.0), keys: None },
        ]);
        let c1 = q.clips.iter().find(|c| c.id == "c1").unwrap().clone();
        let c2 = q.clips.iter().find(|c| c.id == "c2").unwrap().clone();
        // Reset one parameter, then a whole effect.
        let (r1, _) = run(&q, &[super::super::param::param_op("c1", "effects.0.radius", &Param::Value(40.0), &Local { start: 0.0, t: 0.0, inside: true, tol: 0.01, fps: 30.0, dur: 10.0 }, super::super::param::ParamEdit::Reset(10.0))]);
        assert!(r1.clips[0].effects[0].params.is_empty());
        let (r2, _) = run(&q, &[reset_effect_op("c1", 0, &c1.effects[0])]);
        assert!(r2.clips[0].effects[0].params.is_empty());
        // Paste effects: appended or replacing.
        let (r3, _) = run(&q, &paste_effects_ops(&c2, &c1.effects, false));
        assert_eq!(r3.clips.iter().find(|c| c.id == "c2").unwrap().effects, c1.effects);
        // Paste attributes: the transform becomes c1's exactly (c2's own x = 300 goes back to 0).
        let (r4, _) = run(&q, &paste_attributes_ops(&c1, &c2, &[Attr::Transform, Attr::EffectsReplace]));
        let d = r4.clips.iter().find(|c| c.id == "c2").unwrap();
        assert_eq!(d.transform, c1.transform);
        assert_eq!(d.effects, c1.effects);
        // Opacity/volume from a clip at defaults resets them.
        let (r5, _) = run(&r4, &paste_attributes_ops(&c2, &c1, &[Attr::OpacityBlend, Attr::VolumeFades]));
        assert_eq!(r5.clips[0].opacity, Param::Value(1.0));
        assert_eq!(next_seed(0.0), 12_345.0);
        assert!(next_seed(999_999.0) < 1_000_000.0);
    }
}
