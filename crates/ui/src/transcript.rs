//! Text-based editing. The transcript of the selected clip (all its media's words; words the clip
//! does not play are struck through), or of every transcribed clip on the voice/main tracks in
//! timeline order (with the words cut between pieces struck through). Paragraphs break at long
//! pauses; the word under the playhead is highlighted; click = seek; drag or Shift+click selects;
//! Delete/Backspace = `delete_words` (ripple cut); search with next/previous; generate captions;
//! transcribe when there is no transcript. Low-confidence words (< 0.6) are underlined.

use crate::style::{self, alpha};
use crate::util;
use crate::{edits, EditCtx, UiAction};
use aiframes_core::{eval, Id, MediaKind, Op, Project, Resolved, Span};
use egui::{pos2, vec2, Align, Color32, CornerRadius, FontId, Key, Modifiers, RichText, Sense, Stroke, Ui};

/// A silence at least this long starts a new paragraph.
pub const PAUSE: f64 = 1.2;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WordView {
    pub index: usize,
    pub text: String,
    /// The clip plays this word.
    pub in_range: bool,
    /// Timeline time of the word's start (when played).
    pub time: Option<f64>,
    pub conf: f32,
    pub src: f64,
    pub src_end: f64,
}

#[derive(Clone, Debug)]
pub(crate) struct Seg {
    pub clip: Id,
    pub asset: Id,
    pub span: Span,
    pub paragraphs: Vec<Vec<WordView>>,
}

impl Seg {
    pub fn words(&self) -> impl Iterator<Item = &WordView> {
        self.paragraphs.iter().flatten()
    }
}

/// The display model. `full`: every word of each clip's media (single clip view); otherwise only the
/// words each clip plays, plus the words cut out between consecutive pieces of the same recording.
pub(crate) fn build_segments(p: &Project, r: &Resolved, clips: &[Id], full: bool) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::new();
    let mut last_shown: std::collections::HashMap<Id, usize> = Default::default();
    for id in clips {
        let Some(c) = p.clips.iter().find(|c| &c.id == id) else { continue };
        let Some(a) = util::asset_of(p, c) else { continue };
        let Some(tr) = p.transcripts.get(&a.id) else { continue };
        let Some(span) = r.span(id) else { continue };
        let Some((lo, hi)) = eval::source_span(c) else { continue };
        let view = |i: usize| -> WordView {
            let w = &tr.words[i];
            let mid = (w.start + w.end) * 0.5;
            let in_range = mid >= lo - 1e-6 && mid <= hi + 1e-6;
            let time = if in_range { eval::local_time_of_source(c, util::clamp_safe(w.start, lo, hi)).map(|l| span.start + l) } else { None };
            WordView { index: i, text: w.text.trim().to_string(), in_range, time, conf: w.conf, src: w.start, src_end: w.end }
        };
        let mut words: Vec<WordView> = Vec::new();
        if full {
            words = (0..tr.words.len()).map(view).collect();
        } else {
            let inside: Vec<usize> = (0..tr.words.len()).filter(|&i| view(i).in_range).collect();
            if let (Some(&first), Some(&prev)) = (inside.first(), last_shown.get(&a.id)) {
                if first > prev + 1 {
                    words.extend((prev + 1..first).map(view));
                }
            }
            words.extend(inside.iter().map(|&i| view(i)));
            if let Some(&last) = inside.last() {
                last_shown.insert(a.id.clone(), last);
            }
        }
        let mut paragraphs: Vec<Vec<WordView>> = Vec::new();
        for w in words {
            let new_para = match paragraphs.last().and_then(|p| p.last()) {
                None => true,
                Some(prev) => w.src - prev.src_end >= PAUSE,
            };
            if new_para {
                paragraphs.push(vec![w]);
            } else if let Some(p) = paragraphs.last_mut() {
                p.push(w);
            }
        }
        out.push(Seg { clip: id.clone(), asset: a.id.clone(), span, paragraphs });
    }
    out
}

/// The word being spoken at timeline time `t`: (asset, word index).
pub(crate) fn playhead_word(p: &Project, r: &Resolved, clips: &[Id], t: f64) -> Option<(Id, usize)> {
    for id in clips {
        let Some(c) = p.clips.iter().find(|c| &c.id == id) else { continue };
        let Some(s) = r.span(id) else { continue };
        if !s.contains(t) {
            continue;
        }
        let a = util::asset_of(p, c)?;
        let words = &p.transcripts.get(&a.id)?.words;
        let src = eval::source_time(c, t - s.start)?;
        let i = words.partition_point(|w| w.start <= src + 1e-9);
        if i == 0 {
            return None;
        }
        let w = &words[i - 1];
        if src <= w.end + 0.25 {
            return Some((a.id.clone(), i - 1));
        }
        return None;
    }
    None
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric() || *c == '\'').collect::<String>().to_lowercase()
}

/// Matches of `query` (one or more words) in the segments: (segment, first word index).
pub(crate) fn find_matches(segs: &[Seg], query: &str) -> Vec<(usize, usize)> {
    let toks: Vec<String> = query.split_whitespace().map(norm).filter(|t| !t.is_empty()).collect();
    if toks.is_empty() {
        return vec![];
    }
    let mut out = Vec::new();
    for (si, s) in segs.iter().enumerate() {
        let words: Vec<&WordView> = s.words().collect();
        for i in 0..words.len() {
            if i + toks.len() > words.len() {
                break;
            }
            let ok = toks.iter().enumerate().all(|(k, t)| {
                let w = norm(&words[i + k].text);
                if toks.len() == 1 {
                    w.contains(t.as_str())
                } else if k + 1 == toks.len() {
                    w.starts_with(t.as_str())
                } else if k == 0 {
                    w.ends_with(t.as_str())
                } else {
                    w == *t
                }
            });
            if ok {
                out.push((si, words[i].index));
            }
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WordSel {
    pub clip: Id,
    pub asset: Id,
    pub anchor: usize,
    pub head: usize,
}

impl WordSel {
    pub fn range(&self) -> (usize, usize) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }
    pub fn contains(&self, asset: &str, i: usize) -> bool {
        let (a, b) = self.range();
        self.asset == asset && i >= a && i <= b
    }
    pub fn op(&self) -> Op {
        let (from, to) = self.range();
        Op::DeleteWords { clip: self.clip.clone(), from, to, pad: None, edges: None }
    }
    /// (added) [`Self::op`] with its cut edges moved into measured silence when the media's waveform is
    /// loaded (see `aiframes_media::cut_edges`); word times alone otherwise.
    pub fn op_snapped(&self, p: &aiframes_core::Project, preview: &dyn crate::MediaPreview) -> Op {
        let (from, to) = self.range();
        let found = p
            .media
            .iter()
            .find(|m| m.id == self.asset)
            .and_then(|m| preview.waveform_stream(m, p.transcripts.get(&self.asset).map(|t| t.stream).unwrap_or(0)))
            .zip(p.transcripts.get(&self.asset))
            .and_then(|(w, t)| aiframes_media::cut_edges(&w, &t.words, from, to, aiframes_core::ops::DEFAULT_WORD_PAD))
            .map(|e| [e.start.t, e.end.t]);
        let mut op = self.op();
        if let Op::DeleteWords { edges, .. } = &mut op {
            *edges = found;
        }
        op
    }
}

/// Text-based editing: the transcript of the selected (or main) voice/footage clips laid out as
/// words; click = seek, select + Delete = `delete_words`, search, the current word follows the playhead.
pub struct TranscriptPanel {
    /// (added)
    pub search: String,
    /// (added) Scroll to the spoken word while playing.
    pub follow: bool,
    /// (added) Words per caption for "Generate captions".
    pub max_words: usize,
    style: usize,
    sel: Option<WordSel>,
    dragging: bool,
    active: bool,
    match_idx: usize,
    scroll_to: Option<(Id, usize)>,
    last_spoken: Option<(Id, usize)>,
    /// "Replace all captions…" is waiting for confirmation.
    confirm_replace: bool,
}

impl Default for TranscriptPanel {
    fn default() -> Self {
        TranscriptPanel { search: String::new(), follow: true, max_words: 5, style: 0, sel: None, dragging: false, active: false, match_idx: 0, scroll_to: None, last_spoken: None, confirm_replace: false }
    }
}

impl TranscriptPanel {
    pub fn ui(&mut self, ui: &mut Ui, cx: &EditCtx) -> Vec<UiAction> {
        let mut out = Vec::new();
        let p = cx.project;
        let r = cx.resolved;
        let panel_rect = ui.available_rect_before_wrap();
        // Which clips.
        let selected_media: Vec<&aiframes_core::Clip> = p.clips.iter().filter(|c| cx.is_selected(&c.id) && edits::is_media(c)).collect();
        let single = selected_media.len() == 1 && util::asset_of(p, selected_media[0]).is_some_and(|a| p.transcripts.contains_key(&a.id));
        let clips = edits::transcript_clips(p, r, cx.selection);
        let segs = build_segments(p, r, &clips, single);
        let spoken = playhead_word(p, r, &clips, cx.playhead);
        if cx.playing && self.follow && spoken.is_some() && spoken != self.last_spoken {
            self.scroll_to = spoken.clone();
        }
        self.last_spoken = spoken.clone();
        let matches = find_matches(&segs, &self.search);
        if self.match_idx >= matches.len() {
            self.match_idx = 0;
        }

        // Toolbar.
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search words").desired_width(150.0));
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let mut jump = None;
            if crate::widgets::tri_button(ui, true, !matches.is_empty(), "Previous match").clicked() {
                self.match_idx = (self.match_idx + matches.len() - 1) % matches.len().max(1);
                jump = Some(self.match_idx);
            }
            if crate::widgets::tri_button(ui, false, !matches.is_empty(), "Next match (Enter)").clicked() || (enter && !matches.is_empty()) {
                if enter && !r.changed() {
                    self.match_idx = (self.match_idx + 1) % matches.len();
                }
                jump = Some(self.match_idx);
                if enter {
                    r.request_focus();
                }
            }
            if !self.search.trim().is_empty() {
                ui.label(RichText::new(if matches.is_empty() { "no match".to_string() } else { format!("{}/{}", self.match_idx + 1, matches.len()) }).small().weak());
            }
            if let Some(k) = jump {
                if let Some(&(si, wi)) = matches.get(k) {
                    self.scroll_to = Some((segs[si].asset.clone(), wi));
                    if let Some(t) = segs[si].words().find(|w| w.index == wi).and_then(|w| w.time) {
                        out.push(UiAction::Seek(t));
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.checkbox(&mut self.follow, "Follow").on_hover_text("Scroll to the spoken word while playing");
            });
        });
        // Captions: generate, or regenerate keeping what people changed, or replace everything.
        let styles = edits::caption_styles();
        let can_caption = !segs.is_empty();
        let (existing, edited) = edits::caption_counts(p);
        let caption_ids: Vec<Id> = segs.iter().map(|s| s.clip.clone()).collect();
        let caption_style = styles.get(self.style).and_then(|s| s.1.clone());
        let mut generate: Option<bool> = None;
        ui.horizontal_wrapped(|ui| {
            if existing == 0 {
                if ui.add_enabled(can_caption, egui::Button::new("Generate captions")).on_hover_text("Word-anchored karaoke captions on the captions track").clicked() {
                    generate = Some(false);
                }
            } else {
                let keep = format!(
                    "Regenerates the {existing} caption(s) from the transcript; keeps the {edited} a person edited and does not bring back captions a person deleted."
                );
                if ui.add_enabled(can_caption, egui::Button::new("Regenerate (keep my edits)")).on_hover_text(keep).clicked() {
                    generate = Some(false);
                }
                if ui.add_enabled(can_caption, egui::Button::new("Replace all captions…")).on_hover_text("Regenerates every caption: edited ones are replaced and deleted ones come back (asks first)").clicked() {
                    self.confirm_replace = true;
                }
            }
        });
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("caption_style").width(150.0).selected_text(styles.get(self.style).map(|s| s.0).unwrap_or("")).show_ui(ui, |ui| {
                for (i, (name, _)) in styles.iter().enumerate() {
                    ui.selectable_value(&mut self.style, i, *name);
                }
            });
            ui.add(egui::DragValue::new(&mut self.max_words).range(1..=12).suffix(" words")).on_hover_text("Most words per caption");
            if existing > 0 {
                ui.label(RichText::new(format!("{existing} captions · {edited} edited")).small().weak());
            }
        });
        if self.confirm_replace && existing > 0 {
            egui::Frame::group(ui.style()).stroke(egui::Stroke::new(1.0, crate::style::WARN)).show(ui, |ui| {
                ui.label(RichText::new(format!(
                    "Replace all {existing} captions? {} a person edited will be replaced, and captions a person deleted come back.",
                    if edited == 1 { "The one".to_string() } else { format!("The {edited}") }
                )).small());
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Replace all").color(crate::style::WARN)).clicked() {
                        generate = Some(true);
                        self.confirm_replace = false;
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_replace = false;
                    }
                });
            });
        } else {
            self.confirm_replace = false;
        }
        if let Some(replace) = generate {
            let ops = edits::caption_ops_with(p, &caption_ids, caption_style, Some(self.max_words), replace);
            let label = if replace { "Replace all captions" } else if existing > 0 { "Regenerate captions" } else { "Generate captions" };
            match edits::caption_receipt(p, &ops) {
                Ok(receipt) => {
                    out.push(UiAction::Apply { label: label.into(), ops });
                    out.push(UiAction::Status(receipt));
                }
                Err(e) => out.push(UiAction::Status(format!("{label}: {e}"))),
            }
        }
        // Transcribe offers.
        let missing: Vec<&aiframes_core::MediaAsset> = {
            let from_sel: Vec<&aiframes_core::MediaAsset> = selected_media.iter().filter_map(|c| util::asset_of(p, c)).filter(|a| !p.transcripts.contains_key(&a.id)).collect();
            if !from_sel.is_empty() {
                from_sel
            } else if segs.is_empty() {
                p.media.iter().filter(|m| (m.kind == MediaKind::Audio || m.info.has_audio) && !p.transcripts.contains_key(&m.id)).collect()
            } else {
                vec![]
            }
        };
        if !missing.is_empty() {
            ui.group(|ui| {
                ui.label(RichText::new(if segs.is_empty() { "No transcripts yet. Transcribe the voice-over to edit by text, anchor overlays to words and make captions." } else { "The selected clip's media has no transcript." }).weak());
                for m in missing.iter().take(6) {
                    ui.horizontal(|ui| {
                        if ui.button("Transcribe").clicked() {
                            out.push(UiAction::Transcribe(m.id.clone()));
                        }
                        ui.label(util::media_name(m));
                    });
                }
            });
        }
        if segs.is_empty() {
            return out;
        }
        let words_total: usize = segs.iter().map(|s| s.words().count()).sum();
        let hint = match &self.sel {
            Some(s) => {
                let (a, b) = s.range();
                format!("{} word(s) selected · Delete cuts them (ripple) · Esc clears", b - a + 1)
            }
            None => format!("{} clip(s) · {words_total} words · drag over words and press Delete to cut", segs.len()),
        };
        ui.label(RichText::new(hint).small().weak());
        ui.separator();

        // Words.
        let pointer_down = ui.input(|i| i.pointer.primary_down());
        if !pointer_down {
            self.dragging = false;
        }
        let shift = ui.input(|i| i.modifiers.shift);
        let current_match = matches.get(self.match_idx).map(|&(si, wi)| (segs[si].asset.clone(), wi));
        let match_set: std::collections::HashSet<(Id, usize)> = {
            let n = self.search.split_whitespace().count().max(1);
            matches.iter().flat_map(|&(si, wi)| (0..n).map(move |k| (si, wi + k))).map(|(si, wi)| (segs[si].asset.clone(), wi)).collect()
        };
        let fps = util::fps(p);
        let mut clicked_word = false;
        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("aiframes_transcript").show(ui, |ui| {
            for seg in &segs {
                let c = p.clips.iter().find(|c| c.id == seg.clip);
                let label = c.map(|c| util::clip_label(p, c)).unwrap_or_default();
                ui.horizontal(|ui| {
                    style::chip(ui, &seg.clip, style::clip_color(c.map(|c| util::clip_kind(p, c)).unwrap_or(util::ClipKind::Audio)));
                    ui.label(RichText::new(format!("{label} · {}–{}", util::timecode(seg.span.start, fps), util::timecode(seg.span.end, fps))).small().weak());
                });
                for para in &seg.paragraphs {
                    let first_t = para.iter().find_map(|w| w.time);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = vec2(0.0, 3.0);
                        let ts = RichText::new(first_t.map(|t| format!("{} ", util::short_time(t))).unwrap_or_else(|| "cut  ".into())).monospace().size(9.5).color(ui.visuals().weak_text_color());
                        if ui.add(egui::Label::new(ts).sense(Sense::click())).clicked() {
                            if let Some(t) = first_t {
                                out.push(UiAction::Seek(t));
                            }
                        }
                        for w in para {
                            let key = (seg.asset.clone(), w.index);
                            let selected = self.sel.as_ref().is_some_and(|s| s.contains(&seg.asset, w.index));
                            let is_spoken = spoken.as_ref() == Some(&key);
                            let font = FontId::proportional(13.5);
                            let col = if w.in_range { ui.visuals().text_color() } else { alpha(ui.visuals().weak_text_color(), 0.55) };
                            let g = ui.painter().layout_no_wrap(w.text.clone(), font, col);
                            let (rect, resp) = ui.allocate_exact_size(g.size() + vec2(5.0, 3.0), Sense::click_and_drag());
                            let pr = ui.painter();
                            let bg_rect = rect.shrink2(vec2(0.5, 0.0));
                            if selected {
                                pr.rect_filled(bg_rect, CornerRadius::same(2), alpha(style::ACCENT, 0.42));
                            } else if current_match.as_ref() == Some(&key) {
                                pr.rect_filled(bg_rect, CornerRadius::same(2), alpha(style::SNAP, 0.55));
                            } else if match_set.contains(&key) {
                                pr.rect_filled(bg_rect, CornerRadius::same(2), alpha(style::SNAP, 0.22));
                            } else if is_spoken {
                                pr.rect_filled(bg_rect, CornerRadius::same(2), alpha(style::PLAYHEAD, 0.3));
                            } else if resp.hovered() {
                                pr.rect_filled(bg_rect, CornerRadius::same(2), alpha(Color32::WHITE, 0.07));
                            }
                            let text_pos = rect.min + vec2(2.5, 1.5);
                            let gw = g.size().x;
                            let gh = g.size().y;
                            pr.galley(text_pos, g, col);
                            if !w.in_range {
                                pr.hline(text_pos.x..=text_pos.x + gw, text_pos.y + gh * 0.55, Stroke::new(1.0, col));
                            }
                            if w.conf < 0.6 {
                                let y = text_pos.y + gh - 0.5;
                                let mut x = text_pos.x;
                                while x < text_pos.x + gw {
                                    pr.line_segment([pos2(x, y), pos2((x + 2.0).min(text_pos.x + gw), y)], Stroke::new(1.0, style::WARN));
                                    x += 4.0;
                                }
                            }
                            if is_spoken {
                                pr.rect_stroke(bg_rect, CornerRadius::same(2), Stroke::new(1.0, style::PLAYHEAD), egui::StrokeKind::Inside);
                            }
                            if self.scroll_to.as_ref() == Some(&key) {
                                ui.scroll_to_rect(rect, Some(Align::Center));
                                self.scroll_to = None;
                            }
                            // Selection by press / drag / shift-click.
                            if resp.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed()) {
                                self.active = true;
                                clicked_word = true;
                                match (&mut self.sel, shift) {
                                    (Some(s), true) if s.asset == seg.asset => s.head = w.index,
                                    _ => {
                                        self.sel = Some(WordSel { clip: seg.clip.clone(), asset: seg.asset.clone(), anchor: w.index, head: w.index });
                                        self.dragging = true;
                                    }
                                }
                            } else if self.dragging && pointer_down && resp.contains_pointer() {
                                if let Some(s) = &mut self.sel {
                                    if s.asset == seg.asset {
                                        s.head = w.index;
                                    }
                                }
                            }
                            if resp.clicked() && !shift {
                                if let Some(t) = w.time {
                                    out.push(UiAction::Seek(t + 1e-4));
                                }
                                // A plain click places the playhead; it does not keep a one-word selection.
                                if self.sel.as_ref().is_some_and(|s| s.anchor == s.head) {
                                    self.sel = None;
                                }
                            }
                            let tip = format!(
                                "“{}” · word {}{}{}",
                                w.text,
                                w.index,
                                w.time.map(|t| format!(" · {}", util::timecode(t, fps))).unwrap_or_else(|| " · not played (cut)".into()),
                                if w.conf < 1.0 { format!(" · confidence {:.0} %", w.conf * 100.0) } else { String::new() }
                            );
                            resp.on_hover_text(tip);
                        }
                    });
                    ui.add_space(5.0);
                }
                ui.add_space(6.0);
            }
        });
        // Clicking elsewhere deactivates the panel's keys.
        if ui.input(|i| i.pointer.primary_pressed()) && !clicked_word {
            let inside = ui.input(|i| i.pointer.interact_pos()).is_some_and(|p| panel_rect.contains(p));
            if !inside {
                self.active = false;
            }
        }
        // Keys.
        let nothing_focused = ui.memory(|m| m.focused().is_none());
        if self.active && nothing_focused {
            let (del, esc) = ui.ctx().input_mut(|i| (i.consume_key(Modifiers::NONE, Key::Delete) || i.consume_key(Modifiers::NONE, Key::Backspace), i.consume_key(Modifiers::NONE, Key::Escape)));
            if esc {
                self.sel = None;
            }
            if del {
                if let Some(s) = self.sel.take() {
                    let (a, b) = s.range();
                    let mut idx: Vec<(usize, String)> = segs.iter().filter(|g| g.asset == s.asset).flat_map(|g| g.words()).filter(|w| w.index >= a && w.index <= b).map(|w| (w.index, w.text.clone())).collect();
                    idx.sort_by_key(|x| x.0);
                    idx.dedup_by_key(|x| x.0);
                    let text: Vec<String> = idx.into_iter().map(|x| x.1).collect();
                    out.push(UiAction::Apply { label: format!("Delete words “{}”", util::ellipsize(&text.join(" "), 40)), ops: vec![s.op_snapped(p, cx.preview)] });
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::tests::*;
    use aiframes_core::*;

    fn with_words() -> Project {
        let mut p = demo();
        // ma: word i at i*0.5 .. +0.4; a long pause before word 10 (paragraph break).
        let words = (0..30)
            .map(|i| {
                let s = i as f64 * 0.5 + if i >= 10 { 2.0 } else { 0.0 };
                Word { text: format!(" w{i}"), start: s, end: s + 0.4, conf: if i == 3 { 0.4 } else { 1.0 } }
            })
            .collect();
        p.transcripts.insert("ma".into(), Transcript { language: "en".into(), words, ..Default::default() });
        // c3 plays ma 0–20; make it play source 1–9 instead.
        p.clips[2].source = Source::Media { asset: "ma".into(), in_point: 1.0, stream: 0 };
        p.clips[2].duration = 8.0;
        p
    }

    #[test]
    fn segments_mark_unplayed_words_and_paragraphs() {
        let p = with_words();
        let r = resolve(&p);
        let segs = build_segments(&p, &r, &["c3".to_string()], true);
        assert_eq!(segs.len(), 1);
        let words: Vec<&WordView> = segs[0].words().collect();
        assert_eq!(words.len(), 30);
        assert!(!words[0].in_range, "word 0 (0.0 s) is before the in-point");
        assert!(words[2].in_range);
        assert_eq!(words[2].time, Some(0.0), "word 2 starts at source 1.0 = clip start");
        assert_eq!(segs[0].paragraphs.len(), 2, "the 2.1 s pause before word 10 breaks the paragraph");
        assert_eq!(words[3].conf, 0.4);
        // Only played words in the multi-clip view.
        let segs = build_segments(&p, &r, &["c3".to_string()], false);
        assert!(segs[0].words().all(|w| w.in_range));
    }

    #[test]
    fn cut_words_show_between_pieces_and_delete_works() {
        let p = with_words();
        let r = resolve(&p);
        // Delete words 4..=5 of c3 (text-based ripple cut).
        let sel = WordSel { clip: "c3".into(), asset: "ma".into(), anchor: 5, head: 4 };
        assert_eq!(sel.range(), (4, 5));
        let (q, rq) = run(&p, &[sel.op()]);
        let mut clips: Vec<Id> = q.clips.iter().filter(|c| c.track == "a1").map(|c| c.id.clone()).collect();
        clips.sort_by(|a, b| rq.span(a).unwrap().start.total_cmp(&rq.span(b).unwrap().start));
        assert_eq!(clips.len(), 2, "split into two pieces");
        let segs = build_segments(&q, &rq, &clips, false);
        let cut: Vec<usize> = segs.iter().flat_map(|s| s.words()).filter(|w| !w.in_range).map(|w| w.index).collect();
        assert_eq!(cut, vec![4, 5], "the deleted words show struck between the pieces");
        // Playhead word and search.
        let t = rq.span(&clips[0]).unwrap().start + 0.1;
        assert_eq!(playhead_word(&q, &rq, &clips, t), Some(("ma".into(), 2)));
        let m = find_matches(&segs, "w7");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].1, 7);
        assert_eq!(find_matches(&segs, "w6 w7").len(), 1);
        assert!(find_matches(&segs, "").is_empty());
        assert!(find_matches(&segs, "zzz").is_empty());
        let _ = r;
    }
}
