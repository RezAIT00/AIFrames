//! The effects browser: video effects by category (from the core registry), transitions, text
//! presets and shape presets, searchable. Click applies to the selection (effects to the selected
//! picture clips, transitions to their cuts, text/shapes at the playhead on a free overlay track);
//! drag an item onto the timeline ([`BrowserDrag`]).

use crate::edits::{self, ShapePreset, TextPreset};
use crate::style::{self, alpha, rgb};
use crate::util::{self, ClipKind};
use crate::{EditCtx, UiAction};
use aiframes_core::{effects::TRANSITIONS, EffectArg, Id, Op, EFFECTS};
use egui::{pos2, vec2, Align2, Color32, CornerRadius, FontId, Rect, RichText, Sense, Stroke, Ui};

/// (added) Drag-and-drop payload of the effects browser; the timeline accepts it on clips
/// (effects), near cuts (transitions) and at a time on a track (text/shape presets).
#[derive(Clone, Debug, PartialEq)]
pub enum BrowserDrag {
    Effect(String),
    Transition(String),
    Text(TextPreset),
    Shape(ShapePreset),
}

/// Searchable list of effects, transitions, text and shape presets from the core registries;
/// click or drag to apply to the selection.
#[derive(Default)]
pub struct EffectsBrowser {
    /// (added)
    pub search: String,
}

fn category_color(cat: &str) -> Color32 {
    match cat {
        "color" => rgb(0xD0, 0x7A, 0x3A),
        "blur" => rgb(0x4A, 0x8F, 0xC8),
        "stylize" => rgb(0xB0, 0x5A, 0xB8),
        "keying" => rgb(0x3F, 0xB0, 0x6A),
        "mask" => rgb(0x8A, 0x8A, 0x8A),
        "light" => rgb(0xE0, 0xC0, 0x40),
        "distort" => rgb(0xD0, 0x50, 0x60),
        "motion" => rgb(0x50, 0xB8, 0xB0),
        "transition" => rgb(0x6D, 0x5A, 0xC8),
        "text" => style::clip_color(ClipKind::Text),
        _ => style::clip_color(ClipKind::Shape),
    }
}

fn matches(q: &str, fields: &[&str]) -> bool {
    q.is_empty() || fields.iter().any(|f| f.to_lowercase().contains(q))
}

/// What clicking an item does with the current selection (pure).
pub(crate) fn click_actions(item: &BrowserDrag, cx: &EditCtx) -> Vec<UiAction> {
    let p = cx.project;
    let r = cx.resolved;
    let sel = cx.selected_clips();
    match item {
        BrowserDrag::Effect(kind) => {
            let ops: Vec<Op> = sel
                .iter()
                .filter(|id| p.clips.iter().any(|c| &c.id == *id && util::clip_kind(p, c) != ClipKind::Audio))
                .map(|id| Op::AddEffect { clip: id.clone(), effect: EffectArg::Kind(kind.clone()), index: None })
                .collect();
            if ops.is_empty() {
                vec![UiAction::Status("Select a picture clip first (or drag the effect onto one).".into())]
            } else {
                let label = aiframes_core::effect_spec(kind).map(|s| s.label).unwrap_or(kind);
                vec![UiAction::Apply { label: format!("Add {label}"), ops }]
            }
        }
        BrowserDrag::Transition(kind) => {
            let label = aiframes_core::transition_spec(kind).map(|s| s.label).unwrap_or(kind);
            // A selected transition changes kind.
            let trs: Vec<Op> = p
                .transitions
                .iter()
                .filter(|t| cx.is_selected(&t.id))
                .map(|t| Op::SetTransition { transition: t.id.clone(), kind: Some(kind.clone()), duration: None, params: Default::default() })
                .collect();
            if !trs.is_empty() {
                return vec![UiAction::Apply { label: format!("Transition: {label}"), ops: trs }];
            }
            let mut ops = Vec::new();
            let mut cuts: Vec<(Id, Id)> = Vec::new();
            for id in &sel {
                if let Some(op) = edits::add_transition_op(p, r, id, kind) {
                    let cut = match &op {
                        Op::AddTransition { from, to, .. } => (from.clone(), to.clone()),
                        Op::SetTransition { transition, .. } => (transition.clone(), String::new()),
                        _ => continue,
                    };
                    if !cuts.contains(&cut) {
                        cuts.push(cut);
                        ops.push(op);
                    }
                }
            }
            if ops.is_empty() {
                vec![UiAction::Status("Select a clip that touches another clip on its track (or drag the transition onto a cut).".into())]
            } else {
                vec![UiAction::Apply { label: format!("Add {label}"), ops }]
            }
        }
        BrowserDrag::Text(preset) => vec![UiAction::Apply { label: format!("Add {}", preset.label()), ops: edits::text_preset_ops(p, r, *preset, cx.playhead, None) }],
        BrowserDrag::Shape(preset) => vec![UiAction::Apply { label: format!("Add {}", preset.label()), ops: edits::shape_preset_ops(p, r, *preset, cx.playhead, None) }],
    }
}

impl EffectsBrowser {
    pub fn ui(&mut self, ui: &mut Ui, cx: &EditCtx) -> Vec<UiAction> {
        let mut out = Vec::new();
        ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search effects, transitions, titles…").desired_width(f32::INFINITY));
        let n = cx.selected_clips().len();
        ui.label(
            RichText::new(if n > 0 { format!("Click: apply to {n} selected clip(s) · drag onto the timeline") } else { "Click a title/shape to add it at the playhead · drag anything onto the timeline".into() })
                .small()
                .weak(),
        );
        let q = self.search.trim().to_lowercase();
        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("aiframes_fx").show(ui, |ui| {
            let mut cats: Vec<&str> = Vec::new();
            for e in EFFECTS {
                if !cats.contains(&e.category) {
                    cats.push(e.category);
                }
            }
            let mut any = false;
            for cat in cats {
                let items: Vec<_> = EFFECTS.iter().filter(|e| e.category == cat && matches(&q, &[e.label, e.kind, e.help, e.category])).collect();
                if items.is_empty() {
                    continue;
                }
                any = true;
                group(ui, &format!("{} · {}", util::capitalize(cat), items.len()), cat, !q.is_empty() || cat == "color", |ui| {
                    for e in items {
                        item(ui, cx, e.label, e.help, category_color(e.category), BrowserDrag::Effect(e.kind.to_string()), &mut out);
                    }
                });
            }
            let trans: Vec<_> = TRANSITIONS.iter().filter(|t| matches(&q, &[t.label, t.kind, t.help, "transition"])).collect();
            if !trans.is_empty() {
                any = true;
                group(ui, &format!("Transitions · {}", trans.len()), "transitions", true, |ui| {
                    for t in trans {
                        item(ui, cx, t.label, t.help, category_color("transition"), BrowserDrag::Transition(t.kind.to_string()), &mut out);
                    }
                });
            }
            let texts: Vec<_> = TextPreset::ALL.iter().filter(|t| matches(&q, &[t.label(), t.help(), "text title"])).collect();
            if !texts.is_empty() {
                any = true;
                group(ui, &format!("Titles & text · {}", texts.len()), "text", true, |ui| {
                    for t in texts {
                        item(ui, cx, t.label(), t.help(), category_color("text"), BrowserDrag::Text(*t), &mut out);
                    }
                });
            }
            let shapes: Vec<_> = ShapePreset::ALL.iter().filter(|s| matches(&q, &[s.label(), s.help(), "shape"])).collect();
            if !shapes.is_empty() {
                any = true;
                group(ui, &format!("Shapes · {}", shapes.len()), "shapes", true, |ui| {
                    for s in shapes {
                        item(ui, cx, s.label(), s.help(), category_color("shape"), BrowserDrag::Shape(*s), &mut out);
                    }
                });
            }
            if !any {
                ui.label(RichText::new("Nothing matches.").weak());
            }
        });
        out
    }
}

fn group(ui: &mut Ui, title: &str, id: &str, open: bool, add: impl FnOnce(&mut Ui)) {
    egui::CollapsingHeader::new(RichText::new(title).strong().size(12.0)).id_salt(("fxgroup", id)).default_open(open).show(ui, add);
}

fn item(ui: &mut Ui, cx: &EditCtx, label: &str, help: &str, color: Color32, payload: BrowserDrag, out: &mut Vec<UiAction>) {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 22.0), Sense::click_and_drag());
    let p = ui.painter_at(rect);
    if resp.hovered() || resp.dragged() {
        p.rect_filled(rect, CornerRadius::same(3), alpha(Color32::WHITE, 0.06));
    }
    let sw = Rect::from_min_size(pos2(rect.left() + 4.0, rect.center().y - 6.0), vec2(16.0, 12.0));
    p.rect_filled(sw, CornerRadius::same(2), alpha(color, 0.9));
    match &payload {
        BrowserDrag::Transition(_) => {
            p.line_segment([sw.left_bottom(), sw.right_top()], Stroke::new(1.0, Color32::WHITE));
        }
        BrowserDrag::Text(_) => {
            p.text(sw.center(), Align2::CENTER_CENTER, "T", FontId::proportional(10.0), Color32::WHITE);
        }
        BrowserDrag::Shape(s) => {
            let st = Stroke::new(1.2, Color32::WHITE);
            match s {
                ShapePreset::Circle => {
                    p.circle_stroke(sw.center(), 4.0, st);
                }
                ShapePreset::Arrow => p.arrow(sw.left_bottom() + vec2(3.0, -2.0), vec2(9.0, -7.0), st),
                ShapePreset::Line => {
                    p.hline(sw.x_range().shrink(3.0), sw.center().y, st);
                }
                _ => {
                    p.rect_stroke(sw.shrink(2.5), CornerRadius::same(1), st, egui::StrokeKind::Middle);
                }
            }
        }
        BrowserDrag::Effect(_) => {
            p.text(sw.center(), Align2::CENTER_CENTER, "fx", FontId::proportional(8.5), Color32::WHITE);
        }
    }
    p.text(pos2(sw.right() + 8.0, rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(12.0), ui.visuals().text_color());
    resp.dnd_set_drag_payload(payload.clone());
    if resp.dragged() {
        if let Some(pos) = ui.ctx().pointer_interact_pos() {
            let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique("aiframes_fx_drag")));
            let g = painter.layout_no_wrap(label.to_string(), FontId::proportional(11.5), Color32::WHITE);
            let r = Rect::from_min_size(pos + vec2(-g.size().x - 26.0, -28.0), g.size() + vec2(12.0, 6.0));
            painter.rect_filled(r, CornerRadius::same(4), alpha(color, 0.95));
            painter.galley(r.min + vec2(6.0, 3.0), g, Color32::WHITE);
        }
    }
    let resp = resp.on_hover_text(help);
    if resp.clicked() {
        out.extend(click_actions(&payload, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::tests::*;
    use crate::NoPreview;
    use aiframes_core::*;

    fn cx<'a>(p: &'a Project, r: &'a Resolved, sel: &'a [Id]) -> EditCtx<'a> {
        EditCtx { project: p, resolved: r, playhead: 3.0, playing: false, selection: sel, preview: &NoPreview, ai_highlight: &[], ai_flash: 0.0, history: &[], revision: 0, can_undo: false, can_redo: false, in_point: None, out_point: None }
    }

    fn ops_of(a: &[UiAction]) -> Vec<Op> {
        a.iter().flat_map(|x| if let UiAction::Apply { ops, .. } = x { ops.clone() } else { vec![] }).collect()
    }

    #[test]
    fn clicks_apply_to_the_selection() {
        let p = demo();
        let r = resolve(&p);
        let sel = vec!["c1".to_string(), "c2".to_string(), "c3".to_string()];
        // Effects go on picture clips only (c3 is audio).
        let a = click_actions(&BrowserDrag::Effect("blur".into()), &cx(&p, &r, &sel));
        let ops = ops_of(&a);
        assert_eq!(ops.len(), 2);
        let (q, _) = run(&p, &ops);
        assert_eq!(q.clips[0].effects[0].kind, "blur");
        // One transition for the c1|c2 cut even though both are selected.
        let a = click_actions(&BrowserDrag::Transition("crossfade".into()), &cx(&p, &r, &sel));
        let ops = ops_of(&a);
        assert_eq!(ops.len(), 1, "{ops:?}");
        let (q, rq) = run(&p, &ops);
        // With the transition selected, clicking another kind changes it.
        let tsel = vec![q.transitions[0].id.clone()];
        let a = click_actions(&BrowserDrag::Transition("whip".into()), &cx(&q, &rq, &tsel));
        let (q2, _) = run(&q, &ops_of(&a));
        assert_eq!(q2.transitions[0].kind, "whip");
        // Nothing selected: a status message, no ops.
        assert!(matches!(click_actions(&BrowserDrag::Effect("blur".into()), &cx(&p, &r, &[]))[0], UiAction::Status(_)));
        // Titles go at the playhead.
        let a = click_actions(&BrowserDrag::Text(TextPreset::LowerThird), &cx(&p, &r, &[]));
        let (q, rq) = run(&p, &ops_of(&a));
        let t = q.clips.iter().find(|c| matches!(c.source, Source::Text(_))).unwrap();
        assert_eq!(rq.span(&t.id).unwrap().start, 3.0);
        let a = click_actions(&BrowserDrag::Shape(ShapePreset::Circle), &cx(&p, &r, &[]));
        run(&p, &ops_of(&a));
    }
}
