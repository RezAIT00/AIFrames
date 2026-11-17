//! The effects browser: video effects by category (from the core registry), transitions, text
//! presets and shape presets, searchable, as a grid of thumbnails or a list. Click applies to the
//! selection (effects to the selected picture clips, transitions to their cuts, text/shapes at the
//! playhead on a free overlay track); drag an item onto the timeline ([`BrowserDrag`]).
//!
//! (added) Previews: every item shows a thumbnail rendered by the app's renderer ([`FxPreview`]) from
//! what a click would make ([`preview_plan`]); transitions animate while hovered. The hovered item
//! is published in [`EffectsBrowser::hovered`] so the app can preview it live in the program viewer.
//! Each item's one-line help shows in its tooltip (and under its name in the list view).

use crate::edits::{self, ShapePreset, TextPreset};
use crate::style::{self, alpha, rgb};
use crate::util::{self, ClipKind};
use crate::{EditCtx, NoPreview, UiAction};
use aiframes_core::{effects::TRANSITIONS, EffectArg, Id, Op, Project, Resolved, EFFECTS};
use egui::{pos2, vec2, Align2, Color32, CornerRadius, FontId, Rect, RichText, Sense, Stroke, TextureId, Ui};

/// (added) Drag-and-drop payload of the effects browser; the timeline accepts it on clips
/// (effects), near cuts (transitions) and at a time on a track (text/shape presets).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BrowserDrag {
    Effect(String),
    Transition(String),
    Text(TextPreset),
    Shape(ShapePreset),
}

impl BrowserDrag {
    /// (added) The item's name in the browser.
    pub fn label(&self) -> String {
        match self {
            BrowserDrag::Effect(k) => aiframes_core::effect_spec(k).map(|s| s.label.to_string()).unwrap_or_else(|| k.clone()),
            BrowserDrag::Transition(k) => aiframes_core::transition_spec(k).map(|s| s.label.to_string()).unwrap_or_else(|| k.clone()),
            BrowserDrag::Text(t) => t.label().to_string(),
            BrowserDrag::Shape(s) => s.label().to_string(),
        }
    }

    /// (added) Every item of the browser, in its order (effects by category, transitions, titles, shapes).
    pub fn all() -> Vec<BrowserDrag> {
        let mut cats: Vec<&str> = Vec::new();
        for e in EFFECTS {
            if !cats.contains(&e.category) {
                cats.push(e.category);
            }
        }
        let mut v: Vec<BrowserDrag> = cats.iter().flat_map(|c| EFFECTS.iter().filter(move |e| e.category == *c).map(|e| BrowserDrag::Effect(e.kind.to_string()))).collect();
        v.extend(TRANSITIONS.iter().map(|t| BrowserDrag::Transition(t.kind.to_string())));
        v.extend(TextPreset::ALL.iter().map(|t| BrowserDrag::Text(*t)));
        v.extend(ShapePreset::ALL.iter().map(|s| BrowserDrag::Shape(*s)));
        v
    }
}

/// (added) Frames of a transition thumbnail's animation while it is hovered (frame 0 is the still).
pub const ANIM_FRAMES: u32 = 8;

/// (added) Pictures for the browser items, provided by the app (it renders them in the background with
/// the project's renderer). Calls must be cheap and never block: `None` while a picture renders (the
/// app requests a repaint when it arrives).
pub trait FxPreview {
    /// The thumbnail of `item`. `frame` 0 is the still (half way through a transition); `1..=ANIM_FRAMES`
    /// are the frames of its short animation, asked for while the item is hovered.
    fn thumb(&self, item: &BrowserDrag, frame: u32) -> Option<TextureId>;
    /// What the thumbnails show ("c3 at 00:00:20:00", "a sample picture"), for the header line.
    fn source(&self) -> Option<String> {
        None
    }
}

/// (added) An [`FxPreview`] without pictures (tests, headless use): items show their icons.
pub struct NoFxPreview;

impl FxPreview for NoFxPreview {
    fn thumb(&self, _item: &BrowserDrag, _frame: u32) -> Option<TextureId> {
        None
    }
}

/// Searchable list of effects, transitions, text and shape presets from the core registries;
/// click or drag to apply to the selection.
#[derive(Default)]
pub struct EffectsBrowser {
    /// (added)
    pub search: String,
    /// (added) Show items as rows (thumbnail, name and description) instead of a grid of thumbnails.
    pub list_view: bool,
    /// (added) The item under the pointer this frame (not while dragging): the app previews it live in
    /// the program viewer.
    pub hovered: Option<BrowserDrag>,
    /// (added) Since when (egui time) `hovered` has been hovered (animation phase).
    hover_since: f64,
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

/// (added) The edit a click on a browser item would make, and when its result can be seen.
#[derive(Clone, Debug, PartialEq)]
pub struct PreviewPlan {
    pub ops: Vec<Op>,
    /// Timeline seconds to show: the playhead (moved into the clip the effect goes on), a transition's
    /// cut (half way through it), or a moment after a new title or shape has drawn itself on.
    pub at: f64,
    /// For transitions: the transition's timeline span `(start, end)` (its animation samples it).
    pub span: Option<(f64, f64)>,
}

/// (added) Seconds after a title/shape preset starts at which its preview is shown (its reveal or
/// draw-on, at most 0.35 s, has finished).
pub const PRESET_SETTLE: f64 = 1.0;

/// (added) What clicking `item` with `selection` and the playhead at `playhead` would do, as ops
/// on `p` plus the time where the result shows; `None` when a click would do nothing (an effect with no
/// picture clip selected, a transition with no cut next to the selection). Thumbnails and the
/// viewer's hover preview render exactly this on a copy of the project.
pub fn preview_plan(item: &BrowserDrag, p: &Project, r: &Resolved, selection: &[Id], playhead: f64) -> Option<PreviewPlan> {
    let cx = EditCtx { project: p, resolved: r, playhead, playing: false, selection, preview: &NoPreview, ai_highlight: &[], ai_flash: 0.0, history: &[], revision: 0, can_undo: false, can_redo: false, in_point: None, out_point: None };
    let ops = click_actions(item, &cx).into_iter().find_map(|a| match a {
        UiAction::Apply { ops, .. } if !ops.is_empty() => Some(ops),
        _ => None,
    })?;
    let frame = 1.0 / if p.settings.fps > 0.0 { p.settings.fps } else { 30.0 };
    match item {
        BrowserDrag::Effect(_) => {
            let first = ops.iter().find_map(|o| match o {
                Op::AddEffect { clip, .. } => r.span(clip),
                _ => None,
            })?;
            let at = playhead.clamp(first.start, (first.end - frame).max(first.start));
            Some(PreviewPlan { ops, at, span: None })
        }
        BrowserDrag::Transition(_) => {
            let (to, dur) = match ops.first()? {
                Op::AddTransition { to, duration, .. } => (to.clone(), duration.unwrap_or(0.5)),
                Op::SetTransition { transition, .. } => {
                    let t = p.transitions.iter().find(|t| &t.id == transition)?;
                    (t.to.clone(), t.duration)
                }
                _ => return None,
            };
            let at = r.span(&to)?.start;
            Some(PreviewPlan { ops, at, span: Some((at - dur / 2.0, at + dur / 2.0)) })
        }
        BrowserDrag::Text(_) | BrowserDrag::Shape(_) => Some(PreviewPlan { ops, at: playhead + PRESET_SETTLE, span: None }),
    }
}

/// (added) The time a still of the plan shows: its moment, or for a transition 35 % of the way through
/// (half way, a dip is only its colour and a push only the outgoing edge).
pub fn still_time(plan: &PreviewPlan) -> f64 {
    match plan.span {
        Some((a, b)) => a + (b - a) * 0.35,
        None => plan.at,
    }
}

/// (added) The time of animation frame `k` (1..=ANIM_FRAMES) of a transition spanning `span`: from just
/// after its start to just before its end.
pub fn anim_time(span: (f64, f64), k: u32) -> f64 {
    let f = (k.clamp(1, ANIM_FRAMES) as f64 - 0.5) / ANIM_FRAMES as f64;
    span.0 + (span.1 - span.0) * f
}

/// One browser item with what it is drawn with.
struct Item<'a> {
    label: &'a str,
    help: &'a str,
    color: Color32,
    payload: BrowserDrag,
}

impl EffectsBrowser {
    pub fn ui(&mut self, ui: &mut Ui, cx: &EditCtx) -> Vec<UiAction> {
        self.ui_with(ui, cx, &NoFxPreview)
    }

    /// (added) The browser with thumbnails from `fx`.
    pub fn ui_with(&mut self, ui: &mut Ui, cx: &EditCtx, fx: &dyn FxPreview) -> Vec<UiAction> {
        let mut out = Vec::new();
        let was_hovered = self.hovered.take();
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 96.0).max(60.0);
            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search effects, transitions, titles…").desired_width(w));
            if ui.selectable_label(!self.list_view, "Grid").on_hover_text("Thumbnails in a grid").clicked() {
                self.list_view = false;
            }
            if ui.selectable_label(self.list_view, "List").on_hover_text("Rows with a small thumbnail and the description").clicked() {
                self.list_view = true;
            }
        });
        let n = cx.selected_clips().len();
        let hint = if n > 0 { format!("Click: apply to {n} selected clip(s) · hover: preview in the viewer · drag onto the timeline") } else { "Click a title/shape to add it at the playhead · select a clip to preview effects on it · drag anything onto the timeline".into() };
        ui.label(RichText::new(hint).small().weak());
        if let Some(src) = fx.source() {
            ui.label(RichText::new(format!("Previews: {src}")).small().weak());
        }
        let q = self.search.trim().to_lowercase();
        let now = ui.input(|i| i.time);
        let aspect = {
            let s = &cx.project.settings;
            (s.width.max(1) as f32 / s.height.max(1) as f32).clamp(0.3, 4.0)
        };
        let mut hovered: Option<BrowserDrag> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("aiframes_fx").show(ui, |ui| {
            let mut cats: Vec<&str> = Vec::new();
            for e in EFFECTS {
                if !cats.contains(&e.category) {
                    cats.push(e.category);
                }
            }
            let mut groups: Vec<(String, String, bool, Vec<Item>)> = Vec::new();
            for cat in cats {
                let items: Vec<Item> = EFFECTS
                    .iter()
                    .filter(|e| e.category == cat && matches(&q, &[e.label, e.kind, e.help, e.category]))
                    .map(|e| Item { label: e.label, help: e.help, color: category_color(e.category), payload: BrowserDrag::Effect(e.kind.to_string()) })
                    .collect();
                groups.push((util::capitalize(cat), cat.to_string(), !q.is_empty() || cat == "color", items));
            }
            let trans: Vec<Item> = TRANSITIONS
                .iter()
                .filter(|t| matches(&q, &[t.label, t.kind, t.help, "transition"]))
                .map(|t| Item { label: t.label, help: t.help, color: category_color("transition"), payload: BrowserDrag::Transition(t.kind.to_string()) })
                .collect();
            groups.push(("Transitions".into(), "transitions".into(), true, trans));
            let texts: Vec<Item> = TextPreset::ALL.iter().filter(|t| matches(&q, &[t.label(), t.help(), "text title"])).map(|t| Item { label: t.label(), help: t.help(), color: category_color("text"), payload: BrowserDrag::Text(*t) }).collect();
            groups.push(("Titles & text".into(), "text".into(), true, texts));
            let shapes: Vec<Item> = ShapePreset::ALL.iter().filter(|s| matches(&q, &[s.label(), s.help(), "shape"])).map(|s| Item { label: s.label(), help: s.help(), color: category_color("shape"), payload: BrowserDrag::Shape(*s) }).collect();
            groups.push(("Shapes".into(), "shapes".into(), true, shapes));
            let mut any = false;
            for (title, id, open, items) in groups {
                if items.is_empty() {
                    continue;
                }
                any = true;
                group(ui, &format!("{title} · {}", items.len()), &id, open, |ui| {
                    if self.list_view {
                        for it in &items {
                            let frame = self.anim_frame(&it.payload, &was_hovered, now);
                            row(ui, cx, fx, it, aspect, frame, &mut out, &mut hovered);
                        }
                    } else {
                        let gap = 6.0;
                        let avail = ui.available_width();
                        let cols = ((avail + gap) / (104.0 + gap)).floor().max(1.0) as usize;
                        let tw = ((avail + gap) / cols as f32 - gap).floor();
                        for chunk in items.chunks(cols) {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = gap;
                                for it in chunk {
                                    let frame = self.anim_frame(&it.payload, &was_hovered, now);
                                    tile(ui, cx, fx, it, tw, aspect, frame, &mut out, &mut hovered);
                                }
                            });
                            ui.add_space(gap - ui.spacing().item_spacing.y);
                        }
                    }
                });
            }
            if !any {
                ui.label(RichText::new("Nothing matches.").weak());
            }
        });
        if hovered.is_some() && hovered != was_hovered {
            self.hover_since = now;
        }
        if matches!(hovered, Some(BrowserDrag::Transition(_))) {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(80));
        }
        self.hovered = hovered;
        out
    }

    /// The animation frame to show for `item`: transitions cycle through theirs while hovered.
    fn anim_frame(&self, item: &BrowserDrag, hovered: &Option<BrowserDrag>, now: f64) -> u32 {
        if !matches!(item, BrowserDrag::Transition(_)) || hovered.as_ref() != Some(item) {
            return 0;
        }
        // About 10 frames per second, with a short hold on the last frame.
        let k = ((now - self.hover_since).max(0.0) * 10.0) as u32 % (ANIM_FRAMES + 3);
        (k + 1).min(ANIM_FRAMES)
    }
}

fn group(ui: &mut Ui, title: &str, id: &str, open: bool, add: impl FnOnce(&mut Ui)) {
    egui::CollapsingHeader::new(RichText::new(title).strong().size(12.0)).id_salt(("fxgroup", id)).default_open(open).show(ui, add);
}

/// The item's icon on its category colour (shown while its thumbnail renders, and as its badge).
fn icon(p: &egui::Painter, sw: Rect, color: Color32, payload: &BrowserDrag) {
    p.rect_filled(sw, CornerRadius::same(2), alpha(color, 0.9));
    let s = (sw.height() / 12.0).max(1.0);
    match payload {
        BrowserDrag::Transition(_) => {
            p.line_segment([sw.left_bottom(), sw.right_top()], Stroke::new(s, Color32::WHITE));
        }
        BrowserDrag::Text(_) => {
            p.text(sw.center(), Align2::CENTER_CENTER, "T", FontId::proportional(10.0 * s), Color32::WHITE);
        }
        BrowserDrag::Shape(sh) => {
            let st = Stroke::new(1.2 * s, Color32::WHITE);
            match sh {
                ShapePreset::Circle => {
                    p.circle_stroke(sw.center(), sw.height() * 0.33, st);
                }
                ShapePreset::Arrow => p.arrow(sw.left_bottom() + vec2(sw.width() * 0.2, -sw.height() * 0.2), vec2(sw.width() * 0.55, -sw.height() * 0.55), st),
                ShapePreset::Line => {
                    p.hline(sw.x_range().shrink(sw.width() * 0.2), sw.center().y, st);
                }
                _ => {
                    p.rect_stroke(sw.shrink(sw.height() * 0.2), CornerRadius::same(1), st, egui::StrokeKind::Middle);
                }
            }
        }
        BrowserDrag::Effect(_) => {
            p.text(sw.center(), Align2::CENTER_CENTER, "fx", FontId::proportional(8.5 * s), Color32::WHITE);
        }
    }
}

/// The thumbnail (or, while it renders, the icon) inside `r`, keeping the frame's aspect.
fn picture(ui: &Ui, p: &egui::Painter, r: Rect, aspect: f32, fx: &dyn FxPreview, it: &Item, frame: u32) {
    let w = r.width().min(r.height() * aspect);
    let pr = Rect::from_center_size(r.center(), vec2(w, w / aspect));
    p.rect_filled(r, CornerRadius::same(3), Color32::from_gray(18));
    let tex = if frame > 0 { fx.thumb(&it.payload, frame).or_else(|| fx.thumb(&it.payload, 0)) } else { fx.thumb(&it.payload, 0) };
    match tex {
        Some(t) => {
            p.image(t, pr, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            // Category badge in the corner.
            let b = (r.height() * 0.16).clamp(6.0, 12.0);
            let br = Rect::from_min_size(pr.left_top() + vec2(3.0, 3.0), vec2(b * 1.4, b));
            icon(p, br, it.color, &it.payload);
        }
        None => {
            let h = (r.height() * 0.32).clamp(10.0, 22.0);
            icon(p, Rect::from_center_size(r.center(), vec2(h * 1.35, h)), it.color, &it.payload);
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
}

/// Drag payload and ghost, tooltip, click and hover of an item's response.
fn interact(ui: &Ui, cx: &EditCtx, it: &Item, resp: egui::Response, out: &mut Vec<UiAction>, hovered: &mut Option<BrowserDrag>) {
    resp.dnd_set_drag_payload(it.payload.clone());
    if resp.dragged() {
        if let Some(pos) = ui.ctx().pointer_interact_pos() {
            let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique("aiframes_fx_drag")));
            let g = painter.layout_no_wrap(it.label.to_string(), FontId::proportional(11.5), Color32::WHITE);
            let r = Rect::from_min_size(pos + vec2(-g.size().x - 26.0, -28.0), g.size() + vec2(12.0, 6.0));
            painter.rect_filled(r, CornerRadius::same(4), alpha(it.color, 0.95));
            painter.galley(r.min + vec2(6.0, 3.0), g, Color32::WHITE);
        }
    } else if resp.hovered() && !ui.ctx().dragged_id().is_some() {
        *hovered = Some(it.payload.clone());
    }
    let what = match &it.payload {
        BrowserDrag::Effect(_) => "Click: apply to the selected clips · drag onto a clip",
        BrowserDrag::Transition(_) => "Click: add on the selected clip's cut · drag onto a cut",
        BrowserDrag::Text(_) | BrowserDrag::Shape(_) => "Click: add at the playhead · drag onto a track",
    };
    let resp = resp.on_hover_ui(|ui| {
        ui.set_max_width(280.0);
        ui.label(RichText::new(it.label).strong());
        ui.label(it.help);
        ui.label(RichText::new(what).small().weak());
    });
    if resp.clicked() {
        out.extend(click_actions(&it.payload, cx));
    }
}

#[allow(clippy::too_many_arguments)]
fn tile(ui: &mut Ui, cx: &EditCtx, fx: &dyn FxPreview, it: &Item, w: f32, aspect: f32, frame: u32, out: &mut Vec<UiAction>, hovered: &mut Option<BrowserDrag>) {
    let th = (w / aspect.max(1.0)).round();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, th + 18.0), Sense::click_and_drag());
    let p = ui.painter_at(rect);
    let pic = Rect::from_min_size(rect.min, vec2(w, th));
    picture(ui, &p, pic, aspect, fx, it, frame);
    let hot = resp.hovered() || resp.dragged();
    if hot {
        p.rect_stroke(pic, CornerRadius::same(3), Stroke::new(1.5, style::ACCENT), egui::StrokeKind::Inside);
    }
    let name = util::ellipsize(it.label, ((w / 6.2) as usize).max(6));
    let tc = if hot { ui.visuals().strong_text_color() } else { ui.visuals().text_color() };
    p.text(pos2(rect.left() + 1.0, pic.bottom() + 9.0), Align2::LEFT_CENTER, name, FontId::proportional(11.5), tc);
    interact(ui, cx, it, resp, out, hovered);
}

#[allow(clippy::too_many_arguments)]
fn row(ui: &mut Ui, cx: &EditCtx, fx: &dyn FxPreview, it: &Item, aspect: f32, frame: u32, out: &mut Vec<UiAction>, hovered: &mut Option<BrowserDrag>) {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 36.0), Sense::click_and_drag());
    let p = ui.painter_at(rect);
    if resp.hovered() || resp.dragged() {
        p.rect_filled(rect, CornerRadius::same(3), alpha(Color32::WHITE, 0.06));
    }
    let pw = (32.0 * aspect).clamp(24.0, 64.0);
    let pic = Rect::from_min_size(pos2(rect.left() + 2.0, rect.top() + 2.0), vec2(pw, 32.0));
    picture(ui, &p, pic, aspect, fx, it, frame);
    let x = pic.right() + 8.0;
    p.text(pos2(x, rect.top() + 10.0), Align2::LEFT_CENTER, it.label, FontId::proportional(12.0), ui.visuals().text_color());
    let chars = (((rect.right() - x) / 5.6) as usize).max(8);
    p.text(pos2(x, rect.top() + 25.0), Align2::LEFT_CENTER, util::ellipsize(it.help, chars), FontId::proportional(10.5), ui.visuals().weak_text_color());
    interact(ui, cx, it, resp, out, hovered);
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

    #[test]
    fn preview_plans_match_clicks_and_show_the_result() {
        let p = demo();
        let r = resolve(&p);
        let c1 = r.span("c1").unwrap();
        let sel = vec!["c1".to_string()];
        // An effect: the click's ops, shown at the playhead moved into the clip it goes on.
        let fx = BrowserDrag::Effect("vignette".into());
        let plan = preview_plan(&fx, &p, &r, &sel, c1.end + 50.0).unwrap();
        assert_eq!(plan.ops, ops_of(&click_actions(&fx, &cx(&p, &r, &sel))));
        assert!(plan.at >= c1.start && plan.at < c1.end, "{} in {c1:?}", plan.at);
        assert_eq!(preview_plan(&fx, &p, &r, &sel, c1.start + 1.0).unwrap().at, c1.start + 1.0);
        // A transition: half way, on the cut, with its span for the animation.
        let tr = BrowserDrag::Transition("wipe_left".into());
        let plan = preview_plan(&tr, &p, &r, &sel, 0.0).unwrap();
        let (q, rq) = run(&p, &plan.ops);
        let t = &q.transitions[0];
        let cut = rq.span(&t.to).unwrap().start;
        assert_eq!(plan.at, cut);
        let (a, b) = plan.span.unwrap();
        assert!((b - a - t.duration).abs() < 1e-9 && (a + b) / 2.0 == cut);
        assert!(anim_time((a, b), 1) > a && anim_time((a, b), ANIM_FRAMES) < b);
        // A selected transition: changing its kind, shown at its cut.
        let tsel = vec![t.id.clone()];
        let plan2 = preview_plan(&BrowserDrag::Transition("iris".into()), &q, &rq, &tsel, 0.0).unwrap();
        assert!(matches!(&plan2.ops[0], Op::SetTransition { kind: Some(k), .. } if k == "iris"));
        assert_eq!(plan2.at, cut);
        // Nothing to apply to: no plan. Titles work without a selection, after their reveal.
        assert!(preview_plan(&fx, &p, &r, &[], 2.0).is_none());
        assert!(preview_plan(&tr, &p, &r, &["c3".to_string()], 2.0).is_none(), "the audio clip has no cut to put a picture transition on");
        let tp = preview_plan(&BrowserDrag::Text(TextPreset::Title), &p, &r, &[], 2.0).unwrap();
        assert_eq!(tp.at, 2.0 + PRESET_SETTLE);
        // Every item has a label and is listed once.
        let all = BrowserDrag::all();
        assert_eq!(all.len(), EFFECTS.len() + TRANSITIONS.len() + TextPreset::ALL.len() + ShapePreset::ALL.len());
        assert!(all.iter().all(|i| !i.label().is_empty()));
    }

    struct Fake;
    impl FxPreview for Fake {
        fn thumb(&self, item: &BrowserDrag, frame: u32) -> Option<TextureId> {
            (frame == 0 || matches!(item, BrowserDrag::Transition(_))).then_some(TextureId::Managed(0))
        }
        fn source(&self) -> Option<String> {
            Some("c1 at 0:03".into())
        }
    }

    #[test]
    fn grid_and_list_render_and_report_the_hovered_item() {
        let p = demo();
        let r = resolve(&p);
        let sel = vec!["c1".to_string()];
        let ctx = egui::Context::default();
        let mut b = EffectsBrowser::default();
        let mut time = 0.0;
        for list in [false, true] {
            b.list_view = list;
            for pos in [None, Some(pos2(60.0, 120.0)), Some(pos2(60.0, 120.0))] {
                time += 0.5;
                let mut input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 900.0))), time: Some(time), ..Default::default() };
                if let Some(pos) = pos {
                    input.events.push(egui::Event::PointerMoved(pos));
                }
                let mut o = ctx.run_ui(input, |ui| {
                    let _ = b.ui_with(ui, &cx(&p, &r, &sel), &Fake);
                });
                o.textures_delta.clear();
            }
            assert!(b.hovered.is_some(), "an item under the pointer (list {list})");
        }
        // Searching narrows the items; nothing hovered away from them.
        b.search = "dissolve".into();
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 900.0))), time: Some(time + 0.5), events: vec![egui::Event::PointerMoved(pos2(300.0, 880.0))], ..Default::default() };
        let mut o = ctx.run_ui(input, |ui| {
            let _ = b.ui(ui, &cx(&p, &r, &sel));
        });
        o.textures_delta.clear();
        assert!(b.hovered.is_none());
    }
}
