//! A keyframe-aware parameter row: label · ◀ ◆ ▶ · value · curve toggle, plus the inline curve.
//!
//! Constant parameter: editing sets it (`set_param value`); ◆ adds a first key at the playhead.
//! Animated parameter: the field shows the value at the playhead and editing adds/replaces the key
//! at the playhead (`add_keyframe`); ◆ removes the key there (or adds one); ◀ ▶ jump between keys.

use super::*;
use crate::widgets::{key_button, tri_button, NumSpec};
use aiframes_core::{eval, Clip, Key, Param};
use egui::{pos2, vec2, Align2, CornerRadius, FontId, Rect, Sense, Shape, Stroke};

/// Where the playhead is relative to the clip.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Local {
    /// Timeline start of the clip.
    pub start: f64,
    /// Playhead in clip-local seconds (clamped to the clip).
    pub t: f64,
    /// The playhead is inside the clip.
    pub inside: bool,
    /// Half a frame (key matching tolerance).
    pub tol: f64,
    /// The project's exact frame rate and the clip's length.
    pub fps: f64,
    pub dur: f64,
}

impl Local {
    pub fn of(cx: &EditCtx, c: &Clip) -> Local {
        let fps = util::fps(cx.project);
        let start = cx.resolved.span(&c.id).map(|s| s.start).unwrap_or(0.0);
        let raw = cx.playhead - start;
        let dur = c.duration.max(0.0);
        Local { start, t: if raw.is_finite() { raw.clamp(0.0, dur) } else { 0.0 }, inside: raw >= -1e-9 && raw <= c.duration + 1e-9, tol: 0.5 / fps, fps, dur }
    }

    /// Where a key edited "at the playhead" goes: an existing key within half a frame (so the key the
    /// ◆ shows is the one edited), else the playhead snapped to the frame grid (timeline frames).
    pub fn key_time(&self, keys: &[Key]) -> f64 {
        if let Some(k) = keys.iter().find(|k| (k.t - self.t).abs() <= self.tol) {
            return k.t;
        }
        self.snap(self.t)
    }

    /// Clip-local `t` moved onto the timeline's frame grid (inside the clip).
    pub fn snap(&self, t: f64) -> f64 {
        let s = util::quantize(self.start + t, self.fps) - self.start;
        let s = (s * 1e6).round() / 1e6;
        if s.is_finite() {
            s.clamp(0.0, self.dur)
        } else {
            t
        }
    }
}

/// What editing a parameter row produces.
pub(crate) enum ParamEdit {
    /// The value was edited (model units).
    Value(f64),
    AddKey(f64, f64),
    RemoveKey(f64),
}

/// The op for an edit of the parameter at `path` (pure).
pub(crate) fn param_op(clip: &str, path: &str, param: &Param, local: &Local, edit: ParamEdit) -> Op {
    match edit {
        ParamEdit::Value(v) => {
            if param.is_animated() {
                // Keep the existing key's ease when editing it.
                let t = local.key_time(keys_of(param));
                let ease = keys_of(param).iter().find(|k| (k.t - t).abs() < 1e-9).map(|k| k.ease.clone());
                Op::AddKeyframe { clip: clip.into(), path: path.into(), t, v, ease }
            } else {
                Op::SetParam { clip: clip.into(), path: path.into(), value: Some(v), keys: None }
            }
        }
        ParamEdit::AddKey(t, v) => Op::AddKeyframe { clip: clip.into(), path: path.into(), t: local.snap(t), v, ease: None },
        ParamEdit::RemoveKey(t) => Op::RemoveKeyframe { clip: clip.into(), path: path.into(), t },
    }
}

fn keys_of(p: &Param) -> &[Key] {
    match p {
        Param::Animated { keys } => keys,
        Param::Value(_) => &[],
    }
}

impl Inspector {
    /// One row of the grid. Returns the op to apply (the caller labels it).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn param_row(&mut self, ui: &mut Ui, _cx: &EditCtx, c: &Clip, local: &Local, path: &str, label: &str, help: &str, param: &Param, spec: &NumSpec, out: &mut Vec<UiAction>) -> Option<Op> {
        let keys = keys_of(param);
        let animated = param.is_animated();
        let cur = eval::sample(param, local.t);
        let at = keys.iter().position(|k| (k.t - local.t).abs() <= local.tol);
        let lr = ui.add(egui::Label::new(egui::RichText::new(label).color(ui.visuals().weak_text_color())).extend());
        if !help.is_empty() {
            lr.on_hover_text(help);
        }
        let mut edit: Option<ParamEdit> = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let prev = keys.iter().rev().find(|k| k.t < local.t - local.tol).map(|k| k.t);
            let next = keys.iter().find(|k| k.t > local.t + local.tol).map(|k| k.t);
            if tri_button(ui, true, prev.is_some(), "Previous keyframe").clicked() {
                if let Some(t) = prev {
                    out.push(UiAction::Seek(local.start + t));
                }
            }
            let kb = key_button(ui, at.is_some(), animated, local.inside).on_hover_text(if !local.inside {
                "Move the playhead into the clip to set keyframes"
            } else if at.is_some() {
                "Remove the keyframe at the playhead"
            } else {
                "Add a keyframe at the playhead"
            });
            if kb.clicked() {
                crate::timeline::show_keyframe_lane(ui.ctx(), &c.id, path);
                edit = Some(match at {
                    Some(i) => ParamEdit::RemoveKey(keys[i].t),
                    None => ParamEdit::AddKey(local.t, cur),
                });
            }
            if tri_button(ui, false, next.is_some(), "Next keyframe").clicked() {
                if let Some(t) = next {
                    out.push(UiAction::Seek(local.start + t));
                }
            }
            ui.add_space(4.0);
            let key = format!("{}.{path}", c.id);
            let enabled = !animated || local.inside;
            let r = ui.add_enabled_ui(enabled, |ui| self.fields.num(ui, &key, cur, spec)).inner;
            if let Some(v) = r {
                edit = Some(ParamEdit::Value(v));
            }
            if animated {
                let open = self.curves.contains(&key);
                if ui.selectable_label(open, egui::RichText::new("~").size(12.0)).on_hover_text("Show the curve").clicked() {
                    if open {
                        self.curves.remove(&key);
                    } else {
                        self.curves.insert(key.clone());
                        crate::timeline::show_keyframe_lane(ui.ctx(), &c.id, path);
                    }
                }
            }
        });
        ui.end_row();
        let key = format!("{}.{path}", c.id);
        if animated && self.curves.contains(&key) {
            ui.label("");
            if let Some(t) = curve(ui, param, c.duration, local, spec) {
                out.push(UiAction::Seek(local.start + t));
            }
            ui.end_row();
        }
        edit.map(|e| param_op(&c.id, path, param, local, e))
    }
}

/// The inline curve of an animated parameter; returns a clip-local time when clicked.
fn curve(ui: &mut Ui, param: &Param, dur: f64, local: &Local, spec: &NumSpec) -> Option<f64> {
    let w = ui.available_width().clamp(120.0, 260.0);
    let (rect, r) = ui.allocate_exact_size(vec2(w, 46.0), Sense::click());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(3), style::rgb(0x14, 0x15, 0x17));
    let keys = keys_of(param);
    let n = 120;
    let vals: Vec<f64> = (0..=n).map(|i| spec.scale.show(eval::sample(param, dur * i as f64 / n as f64))).collect();
    let (mut lo, mut hi) = vals.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), v| (a.min(*v), b.max(*v)));
    if (hi - lo).abs() < 1e-9 {
        lo -= 1.0;
        hi += 1.0;
    }
    let pad = (hi - lo) * 0.12;
    let (lo, hi) = (lo - pad, hi + pad);
    let x = |t: f64| rect.left() + 4.0 + ((t / dur.max(1e-6)) as f32) * (rect.width() - 8.0);
    let y = |v: f64| rect.bottom() - 3.0 - (((v - lo) / (hi - lo)) as f32) * (rect.height() - 6.0);
    let pts: Vec<egui::Pos2> = vals.iter().enumerate().map(|(i, v)| pos2(x(dur * i as f64 / n as f64), y(*v))).collect();
    p.add(Shape::line(pts, Stroke::new(1.3, style::ACCENT)));
    for k in keys {
        style::diamond(&p, pos2(x(k.t), y(spec.scale.show(k.v))), 3.5, style::KEYFRAME, Stroke::new(0.8, egui::Color32::BLACK));
    }
    p.vline(x(local.t), rect.y_range(), Stroke::new(1.0, style::PLAYHEAD));
    p.text(rect.left_top() + vec2(4.0, 2.0), Align2::LEFT_TOP, format!("{:.2}{}", hi - pad, spec.suffix), FontId::proportional(9.0), ui.visuals().weak_text_color());
    p.text(rect.left_bottom() + vec2(4.0, -2.0), Align2::LEFT_BOTTOM, format!("{:.2}{}", lo + pad, spec.suffix), FontId::proportional(9.0), ui.visuals().weak_text_color());
    if r.clicked() {
        if let Some(pos) = r.interact_pointer_pos() {
            let f = ((pos.x - rect.left() - 4.0) / (rect.width() - 8.0)).clamp(0.0, 1.0) as f64;
            return Some(f * dur);
        }
    }
    let _ = Rect::NOTHING;
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::tests::*;
    use aiframes_core::*;

    #[test]
    fn param_edits_become_the_right_ops() {
        let p = demo();
        let local = Local { start: 0.0, t: 2.0, inside: true, tol: 1.0 / 60.0, fps: 30.0, dur: 10.0 };
        // Constant: set the value.
        let op = param_op("c1", "opacity", &Param::Value(1.0), &local, ParamEdit::Value(0.5));
        let (q, _) = run(&p, &[op]);
        assert_eq!(q.clips[0].opacity, Param::Value(0.5));
        // ◆ on a constant: first key at the playhead.
        let op = param_op("c1", "transform.scale", &Param::Value(1.0), &local, ParamEdit::AddKey(2.0, 1.0));
        let (q, _) = run(&p, &[op]);
        assert!(q.clips[0].transform.scale.is_animated());
        // Editing an animated param adds/replaces the key at the playhead.
        let scale = q.clips[0].transform.scale.clone();
        let op = param_op("c1", "transform.scale", &scale, &Local { t: 4.0, ..local }, ParamEdit::Value(1.5));
        let (q2, _) = run(&q, &[op]);
        assert_eq!(eval::sample(&q2.clips[0].transform.scale, 4.0), 1.5);
        assert_eq!(eval::sample(&q2.clips[0].transform.scale, 2.0), 1.0);
        assert_eq!(eval::sample(&q2.clips[0].transform.scale, 3.0), 1.25);
        // ◆ on a key removes it.
        let scale = q2.clips[0].transform.scale.clone();
        let (q3, _) = run(&q2, &[param_op("c1", "transform.scale", &scale, &local, ParamEdit::RemoveKey(2.0))]);
        assert_eq!(eval::sample(&q3.clips[0].transform.scale, 0.0), 1.5, "one key left → constant");
        // Within half a frame of a key, editing changes that key (not a new key next to it); new keys
        // land on the frame grid.
        let anim = Param::Animated { keys: vec![Key { t: 2.0, v: 1.0, ease: Ease::InOut }, Key { t: 4.0, v: 2.0, ease: Ease::Linear }] };
        let near = Local { t: 2.01, ..local };
        assert!(matches!(param_op("c1", "opacity", &anim, &near, ParamEdit::Value(0.5)), Op::AddKeyframe { t, ease: Some(Ease::InOut), .. } if t == 2.0));
        let off = Local { t: 3.01, start: 0.5, ..local };
        let Op::AddKeyframe { t, .. } = param_op("c1", "opacity", &anim, &off, ParamEdit::AddKey(3.01, 1.0)) else { panic!() };
        assert!(((0.5 + t) * 30.0 - ((0.5 + t) * 30.0).round()).abs() < 1e-6, "on a timeline frame: {t}");
        // Effect params work through the same path.
        let (q4, _) = run(&p, &[Op::AddEffect { clip: "c1".into(), effect: EffectArg::Kind("blur".into()), index: None }]);
        let (q5, _) = run(&q4, &[param_op("c1", "effects.0.radius", &Param::Value(10.0), &local, ParamEdit::AddKey(2.0, 30.0))]);
        assert!(q5.clips[0].effects[0].params["radius"].is_animated());
    }
}
