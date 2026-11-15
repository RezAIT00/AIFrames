//! (added) The review panel: edits the AI proposed instead of applying (`suggest`), oldest first, each
//! with what it does, why, and Play / Try / Apply / Skip. Try applies a suggestion and plays the spot,
//! then offers Undo, which brings the suggestion back: the way to hear an edit both ways.

use crate::util;
use crate::{EditCtx, UiAction};
use aiframes_core::{suggestion_ops, Op, Suggestion};
use egui::{Color32, CornerRadius, Frame, Margin, RichText, Stroke, Ui};

/// Seconds of lead-in before a suggestion's `at` when previewing it.
const PREVIEW_LEAD: f64 = 2.0;

/// A suggestion the person is trying: applied as revision `rev_after`, undoable back to the suggestion.
#[derive(Clone, Debug, PartialEq)]
struct Tried {
    label: String,
    at: Option<f64>,
    rev_after: u64,
}

#[derive(Default)]
pub struct ReviewPanel {
    tried: Option<Tried>,
}

fn apply(cx: &EditCtx, s: &Suggestion, verb: &str) -> Option<UiAction> {
    let ops = suggestion_ops(cx.project, &s.id).ok()?;
    Some(UiAction::Apply { label: format!("{verb} suggestion: {}", util::ellipsize(&s.label, 50)), ops })
}

fn skip(s: &Suggestion) -> UiAction {
    UiAction::Apply { label: format!("Skip suggestion: {}", util::ellipsize(&s.label, 50)), ops: vec![Op::RemoveSuggestion { suggestion: s.id.clone() }] }
}

fn play_from(out: &mut Vec<UiAction>, cx: &EditCtx, at: f64) {
    out.push(UiAction::Seek((at - PREVIEW_LEAD).max(0.0)));
    if !cx.playing {
        out.push(UiAction::TogglePlay);
    }
}

impl ReviewPanel {
    /// The banner for a suggestion being tried, while its edit is the latest step in the history.
    fn tried_banner(&mut self, ui: &mut Ui, cx: &EditCtx, out: &mut Vec<UiAction>) {
        let Some(t) = self.tried.clone() else { return };
        if cx.revision != t.rev_after || !cx.can_undo {
            // Something else changed the project since (or it was undone): nothing to offer any more.
            if cx.revision != t.rev_after.saturating_sub(1) {
                self.tried = None;
            }
            return;
        }
        let ok = Color32::from_rgb(0x4c, 0xc9, 0x8a);
        Frame::new().fill(ui.visuals().faint_bg_color).stroke(Stroke::new(1.0, ok.gamma_multiply(0.6))).corner_radius(CornerRadius::same(6)).inner_margin(Margin::same(8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(format!("Trying: {}", t.label)).strong());
            ui.label(RichText::new("It is applied. Keep it, or undo to get the suggestion back.").small().color(ui.visuals().weak_text_color()));
            ui.horizontal(|ui| {
                if let Some(at) = t.at {
                    if ui.button("▶ Play again").clicked() {
                        play_from(out, cx, at);
                    }
                }
                if ui.button("Undo").on_hover_text("Back to how it was, with the suggestion waiting again (Ctrl+Z)").clicked() {
                    out.push(UiAction::Undo);
                    self.tried = None;
                }
                if ui.button(RichText::new("Keep").strong()).clicked() {
                    self.tried = None;
                }
            });
        });
        ui.add_space(6.0);
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &EditCtx) -> Vec<UiAction> {
        let mut out = Vec::new();
        self.tried_banner(ui, cx, &mut out);
        let list = &cx.project.suggestions;
        if list.is_empty() {
            if self.tried.is_none() {
                ui.add_space(8.0);
                ui.label(RichText::new("No suggestions.").strong());
                ui.add_space(4.0);
                ui.label(
                    RichText::new("When the AI is unsure about an edit — which take to keep, whether a pause is a beat — it proposes it here instead of applying it, for you to play, try, apply or skip.")
                        .color(ui.visuals().weak_text_color()),
                );
            }
            return out;
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} waiting for you", list.len())).strong());
            if list.len() > 1 {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Skip all").on_hover_text("Dismiss every suggestion (one undoable step)").clicked() {
                        let ops = list.iter().map(|s| Op::RemoveSuggestion { suggestion: s.id.clone() }).collect();
                        out.push(UiAction::Apply { label: format!("Skip {} suggestions", list.len()), ops });
                    }
                    if ui.button("Apply all").on_hover_text("Apply every suggestion in order (one undoable step)").clicked() {
                        let ops: Option<Vec<Op>> = list.iter().map(|s| suggestion_ops(cx.project, &s.id).ok()).collect::<Option<Vec<_>>>().map(|v| v.concat());
                        if let Some(ops) = ops {
                            out.push(UiAction::Apply { label: format!("Apply {} suggestions", list.len()), ops });
                        }
                    }
                });
            }
        });
        ui.add_space(4.0);
        let accent = Color32::from_rgb(0x8a, 0x6c, 0xff);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (k, s) in list.iter().enumerate() {
                Frame::new()
                    .fill(ui.visuals().faint_bg_color)
                    .stroke(Stroke::new(1.0, accent.gamma_multiply(0.5)))
                    .corner_radius(CornerRadius::same(6))
                    .inner_margin(Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(format!("{}.", k + 1)).color(accent).strong());
                            ui.label(RichText::new(&s.label).strong());
                        });
                        if !s.reason.is_empty() {
                            ui.label(RichText::new(&s.reason).color(ui.visuals().weak_text_color()));
                        }
                        let mut meta = Vec::new();
                        if let Some(t) = s.at {
                            meta.push(format!("at {}", util::short_time(t)));
                        }
                        meta.push(if s.ops.len() == 1 { "1 change".to_string() } else { format!("{} changes", s.ops.len()) });
                        if !s.author.is_empty() {
                            meta.push(format!("by {}", if aiframes_core::is_ai_author(&s.author) { "AI" } else { s.author.as_str() }));
                        }
                        ui.label(RichText::new(meta.join(" · ")).small().color(ui.visuals().weak_text_color()));
                        ui.add_space(2.0);
                        ui.horizontal_wrapped(|ui| {
                            if let Some(t) = s.at {
                                if ui.button("▶ Play").on_hover_text("Play from just before this spot, as it is now").clicked() {
                                    play_from(&mut out, cx, t);
                                }
                            }
                            let applicable = suggestion_ops(cx.project, &s.id).is_ok();
                            if ui.add_enabled(applicable, egui::Button::new("Try")).on_hover_text("Apply it and play the spot; then keep it or undo").clicked() {
                                if let Some(a) = apply(cx, s, "Try") {
                                    out.push(a);
                                    if let Some(t) = s.at {
                                        play_from(&mut out, cx, t);
                                    }
                                    self.tried = Some(Tried { label: s.label.clone(), at: s.at, rev_after: cx.revision + 1 });
                                }
                            }
                            if ui.add_enabled(applicable, egui::Button::new(RichText::new("Apply").strong())).on_hover_text("Make this edit (Ctrl+Z undoes it)").clicked() {
                                out.extend(apply(cx, s, "Apply"));
                            }
                            if ui.button("Skip").on_hover_text("Dismiss it without changing anything").clicked() {
                                out.push(skip(s));
                            }
                        });
                    });
                ui.add_space(6.0);
            }
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_offers_undo_only_while_its_edit_is_the_latest() {
        let mut panel = ReviewPanel { tried: Some(Tried { label: "x".into(), at: Some(3.0), rev_after: 5 }) };
        let p = aiframes_core::Project::default();
        let r = aiframes_core::resolve(&p);
        let ctx = egui::Context::default();
        let run = |panel: &mut ReviewPanel, revision: u64, can_undo: bool| {
            let cx = EditCtx { project: &p, resolved: &r, playhead: 0.0, playing: false, selection: &[], preview: &crate::NoPreview, ai_highlight: &[], ai_flash: 0.0, history: &[], revision, can_undo, can_redo: false, in_point: None, out_point: None };
            let mut out = Vec::new();
            let mut frame = ctx.run_ui(Default::default(), |ui| {
                out = panel.ui(ui, &cx);
            });
            // The font atlas upload is ours to handle (egui asserts it is not dropped unapplied).
            frame.textures_delta.clear();
            out
        };
        // Revision 4: the apply has not landed yet; the banner waits.
        run(&mut panel, 4, true);
        assert!(panel.tried.is_some());
        // Revision 5: the tried edit is the latest step; the banner shows.
        run(&mut panel, 5, true);
        assert!(panel.tried.is_some());
        // Something else edited the project: the offer goes away.
        run(&mut panel, 6, true);
        assert!(panel.tried.is_none());
    }
}
