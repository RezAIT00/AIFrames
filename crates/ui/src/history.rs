//! The history panel: every entry newest first with an author chip (you / AI / undo / redo),
//! label, relative time and op count; AI entries tinted; hover lists the op summaries; click
//! restores that revision; Undo/Redo buttons.

use crate::style::{self, alpha, rgb};
use crate::util;
use crate::{EditCtx, UiAction};
use aiframes_core::{HistoryEntry, HistoryKind};
use egui::{pos2, vec2, Align2, Color32, CornerRadius, FontId, Rect, RichText, Sense, Stroke, Ui};

/// Who made an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Author {
    Person,
    Ai,
    Undo,
    Redo,
    Other,
}

/// Classifies an entry's author ("user" → person, "ai"/"claude…" → AI, undo/redo entries).
pub fn author_of(e: &HistoryEntry) -> Author {
    match e.kind {
        HistoryKind::Undo => return Author::Undo,
        HistoryKind::Redo => return Author::Redo,
        HistoryKind::Edit => {}
    }
    let a = e.author.to_lowercase();
    if a == "ai" || a.starts_with("ai:") || a.starts_with("claude") || a.contains("agent") {
        Author::Ai
    } else if a == "user" || a == "you" || a.is_empty() {
        Author::Person
    } else {
        Author::Other
    }
}

/// Which entries to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HistoryFilter {
    #[default]
    All,
    Mine,
    Ai,
}

/// Every history entry with author (AI entries highlighted), label and time; click to restore.
#[derive(Default)]
pub struct HistoryPanel {
    /// (added)
    pub filter: HistoryFilter,
}

impl HistoryPanel {
    pub fn ui(&mut self, ui: &mut Ui, cx: &EditCtx) -> Vec<UiAction> {
        let mut out = Vec::new();
        ui.horizontal(|ui| {
            if ui.add_enabled(cx.can_undo, egui::Button::new("Undo")).on_hover_text("Ctrl+Z").clicked() {
                out.push(UiAction::Undo);
            }
            if ui.add_enabled(cx.can_redo, egui::Button::new("Redo")).on_hover_text("Ctrl+Y").clicked() {
                out.push(UiAction::Redo);
            }
            ui.separator();
            for (f, l) in [(HistoryFilter::All, "All"), (HistoryFilter::Mine, "You"), (HistoryFilter::Ai, "AI")] {
                if ui.selectable_label(self.filter == f, l).clicked() {
                    self.filter = f;
                }
            }
        });
        ui.separator();
        if cx.history.is_empty() {
            ui.label(RichText::new("No edits yet. Every change — yours and the AI's — shows up here.").weak());
            return out;
        }
        let now = util::now_unix();
        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("aiframes_history").show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for e in cx.history.iter().rev() {
                let author = author_of(e);
                let show = match self.filter {
                    HistoryFilter::All => true,
                    HistoryFilter::Mine => matches!(author, Author::Person),
                    HistoryFilter::Ai => matches!(author, Author::Ai),
                };
                if !show {
                    continue;
                }
                let r = entry_row(ui, e, author, e.revision == cx.revision, now);
                if r.clicked() && e.revision != cx.revision {
                    out.push(UiAction::RestoreRevision(e.revision));
                }
            }
        });
        out
    }
}

fn entry_row(ui: &mut Ui, e: &HistoryEntry, author: Author, current: bool, now: i64) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 36.0), Sense::click());
    let p = ui.painter_at(rect);
    let bg = match author {
        Author::Ai => alpha(style::AI, if resp.hovered() { 0.2 } else { 0.11 }),
        _ if resp.hovered() => alpha(Color32::WHITE, 0.05),
        _ => Color32::TRANSPARENT,
    };
    p.rect_filled(rect, CornerRadius::same(3), bg);
    if current {
        p.rect_filled(Rect::from_min_size(rect.min, vec2(3.0, rect.height())), CornerRadius::same(1), style::ACCENT);
    }
    let (chip, chip_col) = match author {
        Author::Person => ("You".to_string(), rgb(0x3A, 0x6E, 0xB0)),
        Author::Ai => ("AI".to_string(), style::AI),
        Author::Undo => ("Undo".to_string(), rgb(0x55, 0x58, 0x60)),
        Author::Redo => ("Redo".to_string(), rgb(0x55, 0x58, 0x60)),
        Author::Other => (util::ellipsize(&e.author, 10), rgb(0x60, 0x64, 0x6C)),
    };
    let x = rect.left() + 8.0;
    let cw = style::badge(&p, pos2(x, rect.top() + 5.0), &chip, chip_col, style::text_on(chip_col), false);
    let strong = ui.visuals().strong_text_color();
    let weak = ui.visuals().weak_text_color();
    let label = if e.label.is_empty() { e.summary.clone() } else { e.label.clone() };
    let right = format!("{}  #{}", util::relative_time(&e.time, now), e.revision);
    let rg = p.layout_no_wrap(right, FontId::proportional(9.5), weak);
    let rx = rect.right() - rg.size().x - 6.0;
    p.galley(pos2(rx, rect.top() + 5.0), rg, weak);
    p.with_clip_rect(Rect::from_min_max(pos2(x + cw + 6.0, rect.top()), pos2(rx - 6.0, rect.bottom()))).text(
        pos2(x + cw + 6.0, rect.top() + 4.0),
        Align2::LEFT_TOP,
        &label,
        FontId::proportional(11.5),
        if matches!(author, Author::Undo | Author::Redo) { weak } else { strong },
    );
    let sub = match e.kind {
        HistoryKind::Edit => {
            let mut names: Vec<&str> = e.ops.iter().map(|o| o.name()).collect();
            names.dedup();
            format!("{} op{} · {}", e.ops.len(), if e.ops.len() == 1 { "" } else { "s" }, util::ellipsize(&names.join(", "), 60))
        }
        _ => e.target.map(|t| format!("revision {t}")).unwrap_or_default(),
    };
    p.with_clip_rect(rect).text(pos2(x, rect.top() + 21.0), Align2::LEFT_TOP, sub, FontId::proportional(9.5), weak);
    p.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, alpha(Color32::BLACK, 0.3)));
    let tip = {
        let mut t = format!("{label}\nby {} · {}", e.author, e.time);
        if !e.summary.is_empty() {
            for s in e.summary.split("; ").take(12) {
                t.push_str(&format!("\n• {s}"));
            }
        }
        if !current {
            t.push_str("\n\nClick to go back to this point (undo/redo until here).");
        }
        t
    };
    resp.on_hover_text(tip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(author: &str, kind: HistoryKind) -> HistoryEntry {
        HistoryEntry { revision: 1, author: author.into(), label: "x".into(), time: "2026-11-08T21:41:00Z".into(), kind, target: None, ops: vec![], summary: String::new() }
    }

    #[test]
    fn authors() {
        assert_eq!(author_of(&entry("user", HistoryKind::Edit)), Author::Person);
        assert_eq!(author_of(&entry("ai", HistoryKind::Edit)), Author::Ai);
        assert_eq!(author_of(&entry("claude-code", HistoryKind::Edit)), Author::Ai);
        assert_eq!(author_of(&entry("ai", HistoryKind::Undo)), Author::Undo);
        assert_eq!(author_of(&entry("user", HistoryKind::Redo)), Author::Redo);
        assert_eq!(author_of(&entry("alice", HistoryKind::Edit)), Author::Other);
    }
}
