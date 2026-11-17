//! The clip inspector: identity (name/notes/tags), timing with an anchor editor, source, speed,
//! transform, compositing, fades, audio, text, shape and solid sources, and the effects stack.

use super::param::Local;
use super::*;
use crate::widgets::{fitw, narrow, NumSpec};
use aiframes_core::{
    eval, Align, Anchor, AnchorTarget, BlendMode, Clip, Edge, Fit, MediaKind, Param, RevealKind, ShapeKind, Source, SpeedKeep, Time, TrackKind, WordRef,
};
use egui::{ComboBox, Grid, RichText};
use serde_json::json;
use std::collections::HashSet;

/// The kinds offered by the anchor editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnchorKind {
    Absolute,
    After,
    With,
    Before,
    Marker,
    Word,
}

impl AnchorKind {
    pub const ALL: [AnchorKind; 6] = [AnchorKind::Absolute, AnchorKind::After, AnchorKind::With, AnchorKind::Before, AnchorKind::Marker, AnchorKind::Word];
    pub fn label(self) -> &'static str {
        match self {
            AnchorKind::Absolute => "Absolute",
            AnchorKind::After => "After clip",
            AnchorKind::With => "With clip",
            AnchorKind::Before => "Before clip",
            AnchorKind::Marker => "At marker",
            AnchorKind::Word => "At word",
        }
    }
    pub fn of(t: &Time) -> AnchorKind {
        match t {
            Time::At(_) => AnchorKind::Absolute,
            Time::Anchored(a) => match a.to {
                AnchorTarget::After(_) => AnchorKind::After,
                AnchorTarget::With(_) => AnchorKind::With,
                AnchorTarget::Before(_) => AnchorKind::Before,
                AnchorTarget::Marker(_) => AnchorKind::Marker,
                AnchorTarget::Word(_) => AnchorKind::Word,
            },
        }
    }
}

/// Clips and markers whose anchor chain leads to `id` (anchoring to them would make a cycle).
pub(crate) fn descendants(p: &Project, id: &str) -> HashSet<Id> {
    p.clips.iter().map(|c| c.id.clone()).chain(p.markers.iter().map(|m| m.id.clone())).filter(|x| util::ancestors(p, x).iter().any(|a| a == id)).collect()
}

/// Possible targets of an anchor kind for `clip`.
pub(crate) fn anchor_targets(p: &Project, r: &Resolved, clip: &str, kind: AnchorKind) -> Vec<Id> {
    let bad = descendants(p, clip);
    match kind {
        AnchorKind::Absolute => vec![],
        AnchorKind::Marker => p.markers.iter().filter(|m| !bad.contains(&m.id) && r.marker(&m.id).is_some()).map(|m| m.id.clone()).collect(),
        AnchorKind::Word => p
            .clips
            .iter()
            .filter(|c| c.id != clip && !bad.contains(&c.id) && r.span(&c.id).is_some())
            .filter(|c| util::asset_of(p, c).is_some_and(|a| p.transcripts.contains_key(&a.id)))
            .map(|c| c.id.clone())
            .collect(),
        _ => p.clips.iter().filter(|c| c.id != clip && !bad.contains(&c.id) && r.span(&c.id).is_some()).map(|c| c.id.clone()).collect(),
    }
}

/// The timeline time an anchor resolves to before its offset.
pub(crate) fn anchor_base(p: &Project, r: &Resolved, kind: AnchorKind, target: &str, word: usize, own_dur: f64) -> Option<f64> {
    match kind {
        AnchorKind::Absolute => Some(0.0),
        AnchorKind::After => r.span(target).map(|s| s.end),
        AnchorKind::With => r.span(target).map(|s| s.start),
        AnchorKind::Before => r.span(target).map(|s| s.start - own_dur).or_else(|| r.marker(target).map(|m| m - own_dur)),
        AnchorKind::Marker => r.marker(target),
        AnchorKind::Word => {
            let c = p.clips.iter().find(|c| c.id == target)?;
            let a = util::asset_of(p, c)?;
            let w = p.transcripts.get(&a.id)?.words.get(word)?;
            Some(r.span(target)?.start + eval::local_time_of_source(c, w.start)?)
        }
    }
}

pub(crate) fn make_time(p: &Project, kind: AnchorKind, target: &str, word: usize, offset: f64, abs: f64) -> Time {
    let to = match kind {
        AnchorKind::Absolute => return Time::At(abs.max(0.0)),
        AnchorKind::After => AnchorTarget::After(target.into()),
        AnchorKind::With => AnchorTarget::With(target.into()),
        AnchorKind::Before => AnchorTarget::Before(target.into()),
        AnchorKind::Marker => AnchorTarget::Marker(target.into()),
        AnchorKind::Word => AnchorTarget::Word(util::word_ref(p, target, word, false)),
    };
    let offset = (offset * 1e6).round() / 1e6;
    Time::Anchored(Anchor { to, offset })
}

/// The transcript word of `target` nearest to timeline time `t`.
pub(crate) fn nearest_word(p: &Project, r: &Resolved, target: &str, t: f64) -> Option<usize> {
    let c = p.clips.iter().find(|c| c.id == target)?;
    let a = util::asset_of(p, c)?;
    let s = r.span(target)?;
    let words = &p.transcripts.get(&a.id)?.words;
    words
        .iter()
        .enumerate()
        .filter_map(|(i, w)| eval::local_time_of_source(c, w.start).map(|l| (i, (s.start + l - t).abs())))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// A new start for `clip` anchored with `kind` to `target` that keeps the clip where it is.
pub(crate) fn anchor_keeping_place(p: &Project, r: &Resolved, clip: &str, kind: AnchorKind, target: Option<&str>) -> Option<Time> {
    let c = p.clips.iter().find(|c| c.id == clip)?;
    let start = r.span(clip)?.start;
    if kind == AnchorKind::Absolute {
        return Some(Time::At(start));
    }
    let cands = anchor_targets(p, r, clip, kind);
    let pick = |t: &str| -> Option<(Id, usize, f64)> {
        let word = if kind == AnchorKind::Word { nearest_word(p, r, t, start)? } else { 0 };
        let base = anchor_base(p, r, kind, t, word, c.duration)?;
        Some((t.to_string(), word, base))
    };
    let (target, word, base) = match target {
        Some(t) => pick(t)?,
        None => cands.iter().filter_map(|t| pick(t)).min_by(|a, b| (a.2 - start).abs().total_cmp(&(b.2 - start).abs()))?,
    };
    Some(make_time(p, kind, &target, word, start - base, start))
}

// ---------------------------------------------------------------------------------------------
// End anchors (`until`)
// ---------------------------------------------------------------------------------------------

/// (round 4) How a clip's end is decided: a fixed length, or an end anchor (`until`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndKind {
    Fixed,
    After,
    With,
    Marker,
    Word,
    At,
}

impl EndKind {
    pub const ALL: [EndKind; 6] = [EndKind::Fixed, EndKind::After, EndKind::With, EndKind::Marker, EndKind::Word, EndKind::At];
    pub fn label(self) -> &'static str {
        match self {
            EndKind::Fixed => "Duration (fixed)",
            EndKind::After => "Until end of clip",
            EndKind::With => "Until start of clip",
            EndKind::Marker => "Until marker",
            EndKind::Word => "Until word",
            EndKind::At => "Until a time",
        }
    }
    pub fn of(u: Option<&Time>) -> EndKind {
        match u {
            None => EndKind::Fixed,
            Some(Time::At(_)) => EndKind::At,
            Some(Time::Anchored(a)) => match a.to {
                AnchorTarget::After(_) => EndKind::After,
                AnchorTarget::With(_) | AnchorTarget::Before(_) => EndKind::With,
                AnchorTarget::Marker(_) => EndKind::Marker,
                AnchorTarget::Word(_) => EndKind::Word,
            },
        }
    }
}

/// The target an end anchor of kind `kind` to `target` points at (for the cycle check).
fn until_probe(kind: EndKind, target: &str) -> Option<Time> {
    let to = match kind {
        EndKind::Fixed | EndKind::At => return None,
        EndKind::After => AnchorTarget::After(target.into()),
        EndKind::With => AnchorTarget::With(target.into()),
        EndKind::Marker => AnchorTarget::Marker(target.into()),
        EndKind::Word => AnchorTarget::Word(WordRef { clip: target.into(), index: 0, end: true, text: None }),
    };
    Some(Time::Anchored(Anchor { to, offset: 0.0 }))
}

/// Possible targets of an end anchor of `kind` for `clip`: resolved, after the clip's start, and not
/// depending on the clip's end (no cycles).
pub(crate) fn until_targets(p: &Project, r: &Resolved, clip: &str, kind: EndKind) -> Vec<Id> {
    let Some(start) = r.span(clip).map(|s| s.start) else { return vec![] };
    let blocked = util::end_dependants(p, clip);
    let ok = |id: &str| until_probe(kind, id).is_some_and(|t| util::until_allowed(p, &blocked, clip, &t));
    let later = |t: Option<f64>| t.is_some_and(|t| t > start + 1e-6);
    match kind {
        EndKind::Fixed | EndKind::At => vec![],
        EndKind::Marker => p.markers.iter().filter(|m| later(r.marker(&m.id)) && ok(&m.id)).map(|m| m.id.clone()).collect(),
        EndKind::Word => p
            .clips
            .iter()
            .filter(|c| c.id != clip && r.span(&c.id).is_some_and(|s| s.end > start) && ok(&c.id))
            .filter(|c| util::asset_of(p, c).is_some_and(|a| p.transcripts.contains_key(&a.id)))
            .map(|c| c.id.clone())
            .collect(),
        EndKind::After => p.clips.iter().filter(|c| c.id != clip && later(r.span(&c.id).map(|s| s.end)) && ok(&c.id)).map(|c| c.id.clone()).collect(),
        EndKind::With => p.clips.iter().filter(|c| c.id != clip && later(r.span(&c.id).map(|s| s.start)) && ok(&c.id)).map(|c| c.id.clone()).collect(),
    }
}

/// The timeline time an end anchor resolves to before its offset (word anchors: the word's end).
pub(crate) fn until_base(p: &Project, r: &Resolved, kind: EndKind, target: &str, word: usize) -> Option<f64> {
    match kind {
        EndKind::Fixed | EndKind::At => Some(0.0),
        EndKind::After => r.span(target).map(|s| s.end),
        EndKind::With => r.span(target).map(|s| s.start),
        EndKind::Marker => r.marker(target),
        EndKind::Word => {
            let c = p.clips.iter().find(|c| c.id == target)?;
            let a = util::asset_of(p, c)?;
            let w = p.transcripts.get(&a.id)?.words.get(word)?;
            Some(r.span(target)?.start + eval::local_time_of_source(c, w.end)?)
        }
    }
}

/// An end anchor (`until`) of `kind` to `target` (`None` for a fixed length).
pub(crate) fn make_until(p: &Project, kind: EndKind, target: &str, word: usize, offset: f64, abs: f64) -> Option<Time> {
    let to = match kind {
        EndKind::Fixed => return None,
        EndKind::At => return Some(Time::At(abs.max(0.0))),
        EndKind::After => AnchorTarget::After(target.into()),
        EndKind::With => AnchorTarget::With(target.into()),
        EndKind::Marker => AnchorTarget::Marker(target.into()),
        EndKind::Word => AnchorTarget::Word(util::word_ref(p, target, word, true)),
    };
    Some(Time::Anchored(Anchor { to, offset: (offset * 1e6).round() / 1e6 }))
}

/// The word of `target` whose end is nearest to timeline time `t`.
fn nearest_word_end(p: &Project, r: &Resolved, target: &str, t: f64) -> Option<usize> {
    let c = p.clips.iter().find(|c| c.id == target)?;
    let a = util::asset_of(p, c)?;
    let s = r.span(target)?;
    let words = &p.transcripts.get(&a.id)?.words;
    words
        .iter()
        .enumerate()
        .filter_map(|(i, w)| eval::local_time_of_source(c, w.end).map(|l| (i, (s.start + l - t).abs())))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// The `set_until` value that gives `clip` an end anchor of `kind` (to `target`, else the nearest
/// candidate) while keeping its current end: `Some(None)` clears it (fixed length).
pub(crate) fn until_keeping_place(p: &Project, r: &Resolved, clip: &str, kind: EndKind, target: Option<&str>) -> Option<Option<Time>> {
    let end = r.span(clip)?.end;
    match kind {
        EndKind::Fixed => return Some(None),
        EndKind::At => return Some(Some(Time::At(end))),
        _ => {}
    }
    let pick = |t: &str| -> Option<(Id, usize, f64)> {
        let word = if kind == EndKind::Word { nearest_word_end(p, r, t, end)? } else { 0 };
        Some((t.to_string(), word, until_base(p, r, kind, t, word)?))
    };
    let (target, word, base) = match target {
        Some(t) => pick(t)?,
        None => until_targets(p, r, clip, kind).iter().filter_map(|t| pick(t)).min_by(|a, b| (a.2 - end).abs().total_cmp(&(b.2 - end).abs()))?,
    };
    Some(make_until(p, kind, &target, word, end - base, end))
}

/// The `set_until` op for clip `id` (`None` clears the end anchor, keeping the current length).
pub(crate) fn set_until_op(id: &str, until: Option<Time>) -> Op {
    Op::SetUntil { clip: id.into(), until: Some(until) }
}

/// The clip a clip's start is anchored `with` (overlays on a base clip).
pub(crate) fn with_base<'a>(p: &'a Project, c: &Clip) -> Option<&'a Clip> {
    match &c.start {
        Time::Anchored(Anchor { to: AnchorTarget::With(b), .. }) => p.clips.iter().find(|x| &x.id == b),
        _ => None,
    }
}

/// (round 4) "Match length of <base>": an overlay anchored `with` a base clip ends when it ends.
pub(crate) fn match_length_op(p: &Project, c: &Clip) -> Option<Op> {
    let base = with_base(p, c)?;
    let until = Time::Anchored(Anchor { to: AnchorTarget::After(base.id.clone()), offset: 0.0 });
    if c.until.as_ref() == Some(&until) || !util::until_allowed(p, &util::end_dependants(p, &c.id), &c.id, &until) {
        return None;
    }
    Some(set_until_op(&c.id, Some(until)))
}

/// The anchored word's text; when the transcript changed under the anchor (the stored text differs),
/// both, in the warning colour.
fn word_label(ui: &mut Ui, p: &Project, w: &WordRef) {
    let now = util::word_text(p, &w.clip, w.index).unwrap_or_default();
    match &w.text {
        Some(was) if was.trim() != now => {
            ui.label(RichText::new(format!("“{now}” (was “{was}”)")).italics().color(style::WARN))
                .on_hover_text("The transcript changed: this anchor was made on another word. Pick the word again (or apply the fix) so the clip lands where it should.");
        }
        _ => {
            ui.label(RichText::new(format!("“{now}”")).italics());
        }
    }
}

/// Track matte modes in menu order.
const MATTE_MODES: [aiframes_core::MatteMode; 4] = [aiframes_core::MatteMode::Alpha, aiframes_core::MatteMode::AlphaInverted, aiframes_core::MatteMode::Luma, aiframes_core::MatteMode::LumaInverted];

/// (round 6) Display name of a matte mode.
pub(crate) fn matte_mode_label(m: aiframes_core::MatteMode) -> &'static str {
    match m {
        aiframes_core::MatteMode::Alpha => "Alpha",
        aiframes_core::MatteMode::AlphaInverted => "Alpha inverted",
        aiframes_core::MatteMode::Luma => "Luma",
        aiframes_core::MatteMode::LumaInverted => "Luma inverted",
    }
}

/// Width the eight speed presets need on one row; narrower panels get two rows of four.
const SPEED_PRESETS_ONE_ROW: f32 = 300.0;

fn patch(clip: &str, v: serde_json::Value) -> Op {
    Op::SetClip { clip: clip.into(), patch: v }
}

fn src_patch(clip: &str, v: serde_json::Value) -> Op {
    patch(clip, json!({ "source": v }))
}

fn hex(c: aiframes_core::Color) -> String {
    c.to_hex()
}


impl Inspector {
    /// A keyframe-aware row for a clip-level parameter, with label, help, range and units from the
    /// core registry (`CLIP_PARAMS`).
    #[allow(clippy::too_many_arguments)]
    fn clip_row(&mut self, ui: &mut Ui, cx: &EditCtx, c: &Clip, local: &Local, path: &str, param: &Param, out: &mut Vec<UiAction>) -> Option<Op> {
        let (label, help, spec) = super::clip_spec(path);
        let default = aiframes_core::clip_param_spec(path).map(|p| p.default);
        self.param_row(ui, cx, c, local, path, label, help, param, &spec, default, out)
    }

    pub(crate) fn clip_panel(&mut self, ui: &mut Ui, cx: &EditCtx, c: &Clip, out: &mut Vec<UiAction>) {
        let p = cx.project;
        let kind = util::clip_kind(p, c);
        let local = Local::of(cx, c);
        let fps = util::fps(p);
        let mut ops: Vec<(String, Op)> = Vec::new();

        // Header.
        ui.horizontal(|ui| {
            style::chip(ui, &format!("{kind:?}"), style::clip_color(kind));
            ui.label(RichText::new(&c.id).monospace().weak());
            if cx.is_ai(&c.id) {
                style::chip(ui, "AI changed", style::AI);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Ask AI").on_hover_text("Ask the AI about this clip").clicked() {
                    out.push(UiAction::AskAi { prompt: format!("About clip {} (\"{}\"): ", c.id, util::clip_label(p, c)), ids: vec![c.id.clone()] });
                }
                self.attributes_menu(ui, &[c], Some(c), out);
            });
        });
        if let Some(e) = cx.resolved.error_for(&c.id) {
            ui.add(egui::Label::new(RichText::new(util::plain(&format!("Unresolved: {}", e.message))).color(style::ERROR)).wrap());
        }
        // Validation findings about this clip, with their mechanical fixes.
        let all = util::findings(p, cx.resolved, cx.revision);
        for f in util::clip_findings(&all, &c.id) {
            ui.horizontal_wrapped(|ui| {
                let col = if f.severity == aiframes_core::Severity::Error { style::ERROR } else { style::WARN };
                ui.add(egui::Label::new(RichText::new(util::plain(&f.message)).small().color(col)).wrap()).on_hover_text(format!("{}{}", f.code, f.fix.as_ref().map(|x| format!("\n{x}")).unwrap_or_default()));
                if !f.fix_ops.is_empty() && ui.small_button("Fix").on_hover_text(f.fix.clone().unwrap_or_else(|| "Apply the suggested fix".into())).clicked() {
                    out.push(UiAction::Apply { label: format!("Fix {} ({})", f.code, c.id), ops: f.fix_ops.clone() });
                }
            });
        }

        // Effects first: they are what people adjust most (Premiere's Effect Controls, Resolve's
        // Inspector), and below the long transform sections they were easy to miss.
        if kind != ClipKind::Audio {
            let force = self.reveal.as_ref().is_some_and(|(id, _)| *id == c.id);
            section_with(ui, &format!("Effects ({})", c.effects.len()), &c.id, true, force, |ui| self.effects_stack(ui, cx, c, &local, &mut ops, out));
        }

        // Identity.
        section(ui, "Clip", &c.id, true, |ui| {
            Grid::new(("clip-id", &c.id)).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                doc_lbl(ui, "Name", "clip", "name");
                if let Some(v) = self.fields.text(ui, &format!("{}.name", c.id), &c.name, false, 200.0, &util::clip_label(p, c)) {
                    ops.push(("Rename".into(), patch(&c.id, json!({"name": v}))));
                }
                ui.end_row();
                doc_lbl(ui, "Notes", "clip", "notes");
                if let Some(v) = self.fields.text(ui, &format!("{}.notes", c.id), &c.notes, true, 200.0, "intent, for people and AIs") {
                    ops.push(("Clip notes".into(), patch(&c.id, json!({"notes": v}))));
                }
                ui.end_row();
                doc_lbl(ui, "Tags", "clip", "tags");
                if let Some(v) = self.fields.text(ui, &format!("{}.tags", c.id), &c.tags.join(", "), false, 200.0, "comma, separated") {
                    ops.push(("Clip tags".into(), patch(&c.id, json!({"tags": util::parse_tags(&v)}))));
                }
                ui.end_row();
                doc_lbl(ui, "Enabled", "clip", "enabled");
                let mut en = c.enabled;
                if ui.checkbox(&mut en, "").changed() {
                    ops.push((if en { "Enable" } else { "Disable" }.into(), patch(&c.id, json!({"enabled": en}))));
                }
                ui.end_row();
            });
        });

        // Timing.
        section(ui, "Timing", &c.id, true, |ui| {
            Grid::new(("clip-time", &c.id)).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| self.timing(ui, cx, c, &mut ops));
        });

        // Speed (media).
        if matches!(kind, ClipKind::Video | ClipKind::Audio) {
            section(ui, "Speed", &c.id, true, |ui| {
                Grid::new(("clip-speed", &c.id)).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                    let keep = if self.keep_duration { SpeedKeep::Duration } else { SpeedKeep::Source };
                    if let Some(op) = self.clip_row(ui, cx, c, &local, "speed", &c.speed, out) {
                        let op = match op {
                            Op::SetParam { value: Some(v), .. } => Op::SetSpeed { clip: c.id.clone(), speed: v, keep, linked: true },
                            o => o,
                        };
                        ops.push(("Speed".into(), op));
                    }
                    lbl(ui, "Presets");
                    // One row when there is room, else two even rows of four (never a lone "16×").
                    let presets = [0.25, 0.5, 1.0, 1.5, 2.0, 4.0, 8.0, 16.0];
                    let per_row = if ui.available_width() >= SPEED_PRESETS_ONE_ROW { presets.len() } else { 4 };
                    ui.vertical(|ui| {
                        for row in presets.chunks(per_row) {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 2.0;
                                for &s in row {
                                    let cur = matches!(c.speed, Param::Value(v) if (v - s).abs() < 1e-9);
                                    if ui.add(egui::Button::selectable(cur, format!("{s}×")).min_size(egui::vec2(32.0, 18.0))).on_hover_text(format!("{s}× speed")).clicked() && !cur {
                                        ops.push((format!("Speed {s}×"), Op::SetSpeed { clip: c.id.clone(), speed: s, keep, linked: true }));
                                    }
                                }
                            });
                        }
                    });
                    ui.end_row();
                    lbl(ui, "");
                    ui.horizontal(|ui| {
                        let mut rev = c.reverse;
                        if ui.checkbox(&mut rev, "Reverse").changed() {
                            ops.push(("Reverse".into(), patch(&c.id, json!({"reverse": rev}))));
                        }
                        ui.checkbox(&mut self.keep_duration, "Keep length").on_hover_text("Off: faster = shorter (same media). On: same length, more or less media.");
                    });
                    ui.end_row();
                });
            });
        }

        // Transform and compositing (pictures).
        if kind != ClipKind::Audio {
            section(ui, "Transform", &c.id, true, |ui| {
                Grid::new(("clip-xf", &c.id)).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                    let t = &c.transform;
                    let rows: [(&str, &Param); 4] = [("transform.x", &t.x), ("transform.y", &t.y), ("transform.scale", &t.scale), ("transform.rotation", &t.rotation)];
                    for (path, prm) in rows {
                        if let Some(op) = self.clip_row(ui, cx, c, &local, path, prm, out) {
                            ops.push((super::clip_spec(path).0.into(), op));
                        }
                    }
                    lbl(ui, "Anchor").on_hover_text(super::clip_spec("transform.anchor_x").1);
                    ui.horizontal(|ui| {
                        for (path, v) in [("transform.anchor_x", t.anchor_x), ("transform.anchor_y", t.anchor_y)] {
                            let spec = NumSpec { width: 62.0, ..super::clip_spec(path).2 };
                            if let Some(nv) = self.fields.num(ui, &format!("{}.{path}", c.id), v, &spec) {
                                ops.push(("Anchor point".into(), Op::SetParam { clip: c.id.clone(), path: path.into(), value: Some(nv), keys: None }));
                            }
                        }
                    });
                    ui.end_row();
                    lbl(ui, "Fit");
                    let mut fit = t.fit;
                    narrow(ui, 120.0, |ui| {
                        ComboBox::from_id_salt(("fit", &c.id)).width(fitw(ui, 120.0)).truncate().selected_text(format!("{fit:?}")).show_ui(ui, |ui| {
                            for f in [Fit::Contain, Fit::Cover, Fit::Stretch, Fit::None] {
                                ui.selectable_value(&mut fit, f, format!("{f:?}"));
                            }
                        });
                    });
                    if fit != t.fit {
                        ops.push(("Fit".into(), patch(&c.id, json!({"transform": {"fit": fit}}))));
                    }
                    ui.end_row();
                    lbl(ui, "Flip");
                    ui.horizontal(|ui| {
                        let (mut fx, mut fy) = (t.flip_x, t.flip_y);
                        if ui.checkbox(&mut fx, "Horizontal").changed() {
                            ops.push(("Flip".into(), patch(&c.id, json!({"transform": {"flip_x": fx}}))));
                        }
                        if ui.checkbox(&mut fy, "Vertical").changed() {
                            ops.push(("Flip".into(), patch(&c.id, json!({"transform": {"flip_y": fy}}))));
                        }
                    });
                    ui.end_row();
                    let crops: [(&str, &Param); 4] = [("transform.crop.left", &t.crop.left), ("transform.crop.top", &t.crop.top), ("transform.crop.right", &t.crop.right), ("transform.crop.bottom", &t.crop.bottom)];
                    for (path, prm) in crops {
                        if let Some(op) = self.clip_row(ui, cx, c, &local, path, prm, out) {
                            ops.push((super::clip_spec(path).0.into(), op));
                        }
                    }
                });
                // Round 6: 3-D card (tilt and turn are keyframable; perspective is a scalar).
                let used = crate::edits::is_3d(c) || c.transform.perspective != 2000.0;
                egui::CollapsingHeader::new(RichText::new("3D").strong().size(11.5)).id_salt(("insp-3d", &c.id)).default_open(used).show(ui, |ui| {
                    Grid::new(("clip-3d", &c.id)).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                        let t = &c.transform;
                        for (path, prm) in [("transform.rotation_x", &t.rotation_x), ("transform.rotation_y", &t.rotation_y)] {
                            if let Some(op) = self.clip_row(ui, cx, c, &local, path, prm, out) {
                                ops.push((super::clip_spec(path).0.into(), op));
                            }
                        }
                        let (label, help, spec) = super::clip_spec("transform.perspective");
                        lbl(ui, label).on_hover_text(help);
                        ui.horizontal(|ui| {
                            if let Some(v) = self.fields.num(ui, &format!("{}.persp", c.id), t.perspective, &NumSpec { speed: 5.0, ..spec }) {
                                ops.push((label.into(), Op::SetParam { clip: c.id.clone(), path: "transform.perspective".into(), value: Some(v), keys: None }));
                            }
                            if t.perspective == 0.0 {
                                ui.label(RichText::new("orthographic").small().weak());
                            }
                        });
                        ui.end_row();
                    });
                });
            });
            // Round 6: motion blur and frame blending.
            let video = kind == ClipKind::Video;
            let motion_open = c.motion_blur.is_some() || c.frame_blend;
            section(ui, "Motion", &c.id, motion_open, |ui| {
                Grid::new(("clip-motion", &c.id)).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                    doc_lbl(ui, "Motion blur", "clip", "motion_blur");
                    let mut on = c.motion_blur.is_some();
                    if ui.checkbox(&mut on, "").on_hover_text("Average sub-frame renders over the shutter: moves, zooms and spins smear like a real camera").changed() {
                        let mb = on.then(aiframes_core::MotionBlur::default);
                        ops.push((if on { "Motion blur on" } else { "Motion blur off" }.into(), crate::edits::motion_blur_op(&c.id, mb.as_ref())));
                    }
                    ui.end_row();
                    if let Some(mb) = &c.motion_blur {
                        lbl(ui, "Shutter").on_hover_text("Shutter angle: 180 degrees blurs half a frame of motion, 360 a whole frame.");
                        if let Some(v) = self.fields.num(ui, &format!("{}.shutter", c.id), mb.shutter, &NumSpec::drag(0.0, 720.0, 1.0, "°", 0)) {
                            let m = aiframes_core::MotionBlur { shutter: v, ..mb.clone() };
                            ops.push(("Shutter".into(), crate::edits::motion_blur_op(&c.id, Some(&m))));
                        }
                        ui.end_row();
                        lbl(ui, "Samples").on_hover_text("Sub-frame renders averaged per frame (2-64): more is smoother and slower.");
                        if let Some(v) = self.fields.num(ui, &format!("{}.samples", c.id), mb.samples as f64, &NumSpec::drag(2.0, 64.0, 0.2, "", 0)) {
                            let m = aiframes_core::MotionBlur { samples: v.round().clamp(2.0, 64.0) as u32, ..mb.clone() };
                            ops.push(("Motion blur samples".into(), crate::edits::motion_blur_op(&c.id, Some(&m))));
                        }
                        ui.end_row();
                    }
                    if video {
                        doc_lbl(ui, "Frame blend", "clip", "frame_blend");
                        ui.horizontal(|ui| {
                            let mut fb = c.frame_blend;
                            if ui.checkbox(&mut fb, "").changed() {
                                ops.push((if fb { "Frame blend on" } else { "Frame blend off" }.into(), crate::edits::frame_blend_op(&c.id, fb)));
                            }
                            ui.label(RichText::new("smooth speed-ups").small().weak());
                        });
                        ui.end_row();
                    }
                });
            });
            section(ui, "Compositing", &c.id, true, |ui| {
                Grid::new(("clip-comp", &c.id)).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                    if let Some(op) = self.clip_row(ui, cx, c, &local, "opacity", &c.opacity, out) {
                        ops.push(("Opacity".into(), op));
                    }
                    lbl(ui, "Blend");
                    let mut b = c.blend;
                    narrow(ui, 120.0, |ui| {
                        ComboBox::from_id_salt(("blend", &c.id)).width(fitw(ui, 120.0)).truncate().selected_text(format!("{b:?}")).show_ui(ui, |ui| {
                            for m in [BlendMode::Normal, BlendMode::Add, BlendMode::Multiply, BlendMode::Screen, BlendMode::Overlay, BlendMode::Darken, BlendMode::Lighten, BlendMode::Difference] {
                                ui.selectable_value(&mut b, m, format!("{m:?}"));
                            }
                        });
                    });
                    if b != c.blend {
                        ops.push(("Blend mode".into(), patch(&c.id, json!({"blend": b}))));
                    }
                    ui.end_row();
                    // Round 6: track matte (drawn through another clip) and its mode.
                    let clip_name = |id: &str| p.clips.iter().find(|x| x.id == id).map(|x| format!("{id} · {}", util::clip_label(p, x))).unwrap_or_else(|| id.to_string());
                    doc_lbl(ui, "Matte", "clip", "matte");
                    ui.horizontal(|ui| {
                        let cur = c.matte.as_ref().map(|m| m.clip.clone());
                        let mut pick = cur.clone();
                        let shown = cur.as_deref().map(clip_name).unwrap_or_else(|| "None".into());
                        narrow(ui, 130.0, |ui| {
                            ComboBox::from_id_salt(("matte", &c.id)).width(fitw(ui, 130.0)).truncate().selected_text(&shown).show_ui(ui, |ui| {
                                ui.selectable_value(&mut pick, None, "None");
                                for id in crate::edits::matte_candidates(p, cx.resolved, &c.id) {
                                    let label = clip_name(&id);
                                    ui.selectable_value(&mut pick, Some(id), label);
                                }
                            })
                            .response
                            .on_hover_text(format!("{shown}: this clip shows only through the matte clip, which is then not drawn itself"));
                        });
                        if pick != cur {
                            let mode = c.matte.as_ref().map(|m| m.mode).unwrap_or_default();
                            ops.push(("Matte".into(), crate::edits::matte_op(&c.id, pick.as_deref().map(|m| (m, mode)))));
                        }
                        if let Some(m) = &c.matte {
                            let mut mode = m.mode;
                            narrow(ui, 96.0, |ui| {
                                ComboBox::from_id_salt(("mattemode", &c.id)).width(fitw(ui, 96.0)).truncate().selected_text(matte_mode_label(mode)).show_ui(ui, |ui| {
                                    for x in MATTE_MODES {
                                        ui.selectable_value(&mut mode, x, matte_mode_label(x));
                                    }
                                });
                            });
                            if mode != m.mode {
                                ops.push(("Matte mode".into(), crate::edits::matte_op(&c.id, Some((&m.clip, mode)))));
                            }
                        }
                    });
                    ui.end_row();
                    let users = crate::edits::matte_users(p, &c.id);
                    if !users.is_empty() {
                        lbl(ui, "");
                        ui.add(egui::Label::new(RichText::new(format!("Used as a matte by {} (not drawn itself)", users.join(", "))).small().color(style::ACCENT)).wrap());
                        ui.end_row();
                    }
                });
            });
        }

        // Fades and audio.
        section(ui, if util::has_sound(p, c) { "Fades & audio" } else { "Fades" }, &c.id, true, |ui| {
            Grid::new(("clip-fade", &c.id)).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                for (path, v) in [("fade_in", c.fade_in), ("fade_out", c.fade_out)] {
                    let (label, help, spec) = super::clip_spec(path);
                    lbl(ui, label).on_hover_text(help);
                    if let Some(nv) = self.fields.num(ui, &format!("{}.{path}", c.id), v, &NumSpec { max: spec.max.min(c.duration), ..spec }) {
                        ops.push((label.into(), Op::SetParam { clip: c.id.clone(), path: path.into(), value: Some(nv), keys: None }));
                    }
                    ui.end_row();
                }
                if util::has_sound(p, c) {
                    if let Some(op) = self.clip_row(ui, cx, c, &local, "volume", &c.volume, out) {
                        ops.push(("Volume".into(), op));
                    }
                    lbl(ui, "Mute");
                    let mut m = c.muted;
                    if ui.checkbox(&mut m, "Silence this clip's sound").changed() {
                        ops.push(("Mute".into(), patch(&c.id, json!({"muted": m}))));
                    }
                    ui.end_row();
                }
            });
        });

        // Source-specific editors.
        match &c.source {
            Source::Text(_) => section(ui, "Text", &c.id, true, |ui| self.text_editor(ui, c, &mut ops)),
            Source::Shape(_) => section(ui, "Shape", &c.id, true, |ui| self.shape_editor(ui, cx, c, &local, &mut ops, out)),
            Source::Solid { color } => section(ui, "Solid", &c.id, true, |ui| {
                Grid::new(("solid", &c.id)).num_columns(2).show(ui, |ui| {
                    lbl(ui, "Colour");
                    if let Some(nc) = self.fields.color(ui, &format!("{}.solid", c.id), *color) {
                        ops.push(("Solid colour".into(), src_patch(&c.id, json!({"color": hex(nc)}))));
                    }
                    ui.end_row();
                });
            }),
            Source::Adjustment => {
                ui.add(egui::Label::new(RichText::new("Adjustment layer: its effects apply to everything below it.").small().weak()).wrap());
            }
            Source::Media { .. } => {}
        }

        let _ = fps;
        for (label, op) in ops {
            out.push(UiAction::Apply { label: format!("{label} ({})", c.id), ops: vec![op] });
        }
    }

    fn timing(&mut self, ui: &mut Ui, cx: &EditCtx, c: &Clip, ops: &mut Vec<(String, Op)>) {
        let p = cx.project;
        let r = cx.resolved;
        let fps = util::fps(p);
        let span = r.span(&c.id);
        let start = span.map(|s| s.start).unwrap_or(0.0);
        doc_lbl(ui, "Start", "clip", "start");
        ui.horizontal(|ui| match &c.start {
            Time::At(_) => {
                if let Some(t) = self.fields.time(ui, &format!("{}.start", c.id), start, fps, 0.0..=1e7) {
                    ops.push(("Move".into(), Op::MoveClip { clip: c.id.clone(), start: Time::At(t), track: None, overwrite: false }));
                }
            }
            Time::Anchored(a) => {
                ui.label(RichText::new(util::timecode(start, fps)).monospace());
                ui.label(RichText::new(aiframes_core::resolve::describe_anchor(a)).small().color(style::ACCENT));
            }
        });
        ui.end_row();

        // Anchor editor.
        doc_lbl(ui, "Anchor", "clip", "start");
        let kind = AnchorKind::of(&c.start);
        ui.horizontal(|ui| {
            let mut k = kind;
            narrow(ui, 96.0, |ui| {
                ComboBox::from_id_salt(("akind", &c.id)).width(fitw(ui, 96.0)).truncate().selected_text(k.label()).show_ui(ui, |ui| {
                    for a in AnchorKind::ALL {
                        let enabled = a == AnchorKind::Absolute || !anchor_targets(p, r, &c.id, a).is_empty();
                        ui.add_enabled_ui(enabled, |ui| {
                            ui.selectable_value(&mut k, a, a.label());
                        });
                    }
                });
            });
            if k != kind {
                if let Some(t) = anchor_keeping_place(p, r, &c.id, k, None) {
                    ops.push(("Anchor".into(), Op::MoveClip { clip: c.id.clone(), start: t, track: None, overwrite: false }));
                }
            }
            if let Time::Anchored(a) = &c.start {
                let target = util::anchor_parent(&c.start).unwrap_or("").to_string();
                let cands = anchor_targets(p, r, &c.id, kind);
                let name = |id: &str| -> String {
                    p.clips.iter().find(|x| x.id == id).map(|x| format!("{id} · {}", util::ellipsize(&util::clip_label(p, x), 18))).or_else(|| p.markers.iter().find(|m| m.id == id).map(|m| format!("{id} · {}", m.name))).unwrap_or(id.to_string())
                };
                let mut t2 = target.clone();
                narrow(ui, 130.0, |ui| {
                    ComboBox::from_id_salt(("atarget", &c.id)).width(fitw(ui, 130.0)).truncate().selected_text(name(&target)).show_ui(ui, |ui| {
                        for id in &cands {
                            ui.selectable_value(&mut t2, id.clone(), name(id));
                        }
                    })
                    .response
                    .on_hover_text(name(&target));
                });
                if t2 != target {
                    if let Some(t) = anchor_keeping_place(p, r, &c.id, kind, Some(&t2)) {
                        ops.push(("Anchor target".into(), Op::MoveClip { clip: c.id.clone(), start: t, track: None, overwrite: false }));
                    }
                }
                let _ = a;
            }
        });
        ui.end_row();
        if let Time::Anchored(a) = &c.start {
            if let AnchorTarget::Word(w) = &a.to {
                lbl(ui, "Word");
                ui.horizontal(|ui| {
                    if let Some(i) = self.fields.num(ui, &format!("{}.word", c.id), w.index as f64, &NumSpec::drag(0.0, 1e6, 0.2, "", 0)) {
                        let mut a2 = a.clone();
                        if let AnchorTarget::Word(w2) = &mut a2.to {
                            *w2 = util::word_ref(p, &w2.clip, i.round().max(0.0) as usize, w2.end);
                        }
                        ops.push(("Anchor word".into(), Op::MoveClip { clip: c.id.clone(), start: Time::Anchored(a2), track: None, overwrite: false }));
                    }
                    word_label(ui, p, w);
                });
                ui.end_row();
            }
            lbl(ui, "Offset");
            if let Some(o) = self.fields.num(ui, &format!("{}.offset", c.id), a.offset, &NumSpec::drag(-1e6, 1e6, 1.0 / fps, " s", 3)) {
                let mut a2 = a.clone();
                a2.offset = o;
                ops.push(("Anchor offset".into(), Op::MoveClip { clip: c.id.clone(), start: Time::Anchored(a2), track: None, overwrite: false }));
            }
            ui.end_row();
        }

        // The length shown is the frame-accurate one the renderer uses (video clips snap to frames).
        let shown = span.map(|s| s.duration()).unwrap_or(c.duration);
        doc_lbl(ui, "Duration", "clip", "duration");
        ui.horizontal(|ui| {
            if let Some(d) = self.fields.time(ui, &format!("{}.dur", c.id), shown, fps, (1.0 / fps)..=1e7) {
                if span.is_some() {
                    ops.push(("Duration".into(), Op::TrimClip { clip: c.id.clone(), edge: Edge::End, to: Some(start + d), delta: None, ripple: None }));
                }
            }
            if let Some(u) = &c.until {
                ui.label(RichText::new(format!("until {}", aiframes_core::until_text(u))).small().color(style::ACCENT))
                    .on_hover_text("The length follows the end anchor. Editing the duration (or trimming the end) shifts the anchor's offset.");
            }
        });
        ui.end_row();
        self.end_rows(ui, cx, c, ops);
        if let (Source::Media { in_point, .. }, Some(asset)) = (&c.source, util::asset_of(p, c)) {
            if asset.kind != MediaKind::Image {
                doc_lbl(ui, "In point", "source.media", "in_point");
                if let Some(ip) = self.fields.time(ui, &format!("{}.in", c.id), *in_point, fps, 0.0..=asset.info.duration.max(0.0)) {
                    ops.push(("Slip".into(), Op::SlipClip { clip: c.id.clone(), delta: ip - in_point, linked: true }));
                }
                ui.end_row();
                if let Some((a, b)) = eval::source_span(c) {
                    lbl(ui, "Source");
                    ui.label(RichText::new(format!("{} – {} of {}", util::timecode(a, fps), util::timecode(b, fps), util::short_time(asset.info.duration))).small().weak());
                    ui.end_row();
                }
            }
            lbl(ui, "Media");
            let mut chosen = asset.id.clone();
            narrow(ui, 170.0, |ui| {
                ComboBox::from_id_salt(("media", &c.id)).width(fitw(ui, 170.0)).truncate().selected_text(util::media_name(asset)).show_ui(ui, |ui| {
                    for m in p.media.iter().filter(|m| (m.kind == MediaKind::Audio) == (asset.kind == MediaKind::Audio)) {
                        ui.selectable_value(&mut chosen, m.id.clone(), util::media_name(m));
                    }
                })
                .response
                .on_hover_text(format!("{} · {}", util::media_name(asset), asset.path));
            });
            if chosen != asset.id {
                ops.push(("Replace media".into(), Op::ReplaceMedia { clip: c.id.clone(), asset: chosen, in_point: Some(0.0) }));
            }
            ui.end_row();
        }
        lbl(ui, "Track");
        let tk = p.tracks.iter().find(|t| t.id == c.track).map(|t| t.kind).unwrap_or(TrackKind::Video);
        let mut tr = c.track.clone();
        narrow(ui, 120.0, |ui| {
            ComboBox::from_id_salt(("track", &c.id)).width(fitw(ui, 120.0)).truncate().selected_text(&tr).show_ui(ui, |ui| {
                for t in p.tracks.iter().filter(|t| t.kind == tk) {
                    ui.selectable_value(&mut tr, t.id.clone(), if t.name.is_empty() { t.id.clone() } else { format!("{} · {}", t.id, t.name) });
                }
            });
        });
        if tr != c.track {
            ops.push(("Move to track".into(), Op::MoveClip { clip: c.id.clone(), start: c.start.clone(), track: Some(tr), overwrite: false }));
        }
        ui.end_row();
    }

    /// (round 4) The end editor: a fixed length, or an end anchor (`until`) after/with a clip, at a
    /// marker or a word, with the same cycle-free target pickers as the start anchor.
    fn end_rows(&mut self, ui: &mut Ui, cx: &EditCtx, c: &Clip, ops: &mut Vec<(String, Op)>) {
        let p = cx.project;
        let r = cx.resolved;
        let fps = util::fps(p);
        let kind = EndKind::of(c.until.as_ref());
        let name = |id: &str| -> String {
            p.clips
                .iter()
                .find(|x| x.id == id)
                .map(|x| format!("{id} · {}", util::ellipsize(&util::clip_label(p, x), 18)))
                .or_else(|| p.markers.iter().find(|m| m.id == id).map(|m| format!("{id} · {}", m.name)))
                .unwrap_or(id.to_string())
        };
        doc_lbl(ui, "End", "clip", "until");
        ui.horizontal(|ui| {
            let before_start = matches!(&c.start, Time::Anchored(Anchor { to: AnchorTarget::Before(_), .. }));
            if before_start && c.until.is_none() {
                ui.label(RichText::new("fixed (starts before a clip)").small().weak()).on_hover_text("A clip anchored `before` another clip needs a fixed length; anchor its start with/after something to give it an end anchor.");
                return;
            }
            let mut k = kind;
            // Room for the whole label when no target picker follows.
            let kw = if matches!(c.until, Some(Time::Anchored(_))) { 118.0 } else { 150.0 };
            narrow(ui, kw, |ui| {
                ComboBox::from_id_salt(("ukind", &c.id)).width(fitw(ui, kw)).truncate().selected_text(k.label()).show_ui(ui, |ui| {
                    for e in EndKind::ALL {
                        let enabled = matches!(e, EndKind::Fixed | EndKind::At) || !until_targets(p, r, &c.id, e).is_empty();
                        ui.add_enabled_ui(enabled, |ui| {
                            ui.selectable_value(&mut k, e, e.label());
                        });
                    }
                });
            });
            if k != kind {
                if let Some(u) = until_keeping_place(p, r, &c.id, k, None) {
                    ops.push((if u.is_some() { "End anchor" } else { "Fixed length" }.into(), set_until_op(&c.id, u)));
                }
            }
            if let Some(Time::Anchored(_)) = &c.until {
                let target = c.until.as_ref().and_then(util::anchor_parent).unwrap_or("").to_string();
                let mut t2 = target.clone();
                narrow(ui, 120.0, |ui| {
                    ComboBox::from_id_salt(("utarget", &c.id)).width(fitw(ui, 120.0)).truncate().selected_text(name(&target)).show_ui(ui, |ui| {
                        for id in until_targets(p, r, &c.id, kind) {
                            let label = name(&id);
                            ui.selectable_value(&mut t2, id, label);
                        }
                    })
                    .response
                    .on_hover_text(c.until.as_ref().map(|u| format!("until {} · {}", aiframes_core::until_text(u), name(&target))).unwrap_or_default());
                });
                if t2 != target {
                    if let Some(u) = until_keeping_place(p, r, &c.id, kind, Some(&t2)) {
                        ops.push(("End anchor target".into(), set_until_op(&c.id, u)));
                    }
                }
            }
        });
        ui.end_row();
        match &c.until {
            Some(Time::Anchored(a)) => {
                if let AnchorTarget::Word(w) = &a.to {
                    lbl(ui, "End word");
                    ui.horizontal(|ui| {
                        if let Some(i) = self.fields.num(ui, &format!("{}.uword", c.id), w.index as f64, &NumSpec::drag(0.0, 1e6, 0.2, "", 0)) {
                            let mut a2 = a.clone();
                            a2.to = AnchorTarget::Word(util::word_ref(p, &w.clip, i.round().max(0.0) as usize, w.end));
                            ops.push(("End word".into(), set_until_op(&c.id, Some(Time::Anchored(a2)))));
                        }
                        word_label(ui, p, w);
                    });
                    ui.end_row();
                }
                lbl(ui, "End offset");
                if let Some(o) = self.fields.num(ui, &format!("{}.uoffset", c.id), a.offset, &NumSpec::drag(-1e6, 1e6, 1.0 / fps, " s", 3)) {
                    let mut a2 = a.clone();
                    a2.offset = o;
                    ops.push(("End offset".into(), set_until_op(&c.id, Some(Time::Anchored(a2)))));
                }
                ui.end_row();
            }
            Some(Time::At(v)) => {
                lbl(ui, "End at");
                let lo = r.span(&c.id).map(|s| s.start + 1.0 / fps).unwrap_or(0.0);
                if let Some(t) = self.fields.time(ui, &format!("{}.uat", c.id), *v, fps, lo..=1e7) {
                    ops.push(("End time".into(), set_until_op(&c.id, Some(Time::At(t)))));
                }
                ui.end_row();
            }
            None => {}
        }
    }

    fn text_editor(&mut self, ui: &mut Ui, c: &Clip, ops: &mut Vec<(String, Op)>) {
        let Source::Text(t) = &c.source else { return };
        let id = &c.id;
        let k = |f: &str| format!("{id}.text.{f}");
        if let Some(v) = self.fields.text(ui, &k("text"), &t.text, true, ui.available_width().min(320.0), "Text") {
            ops.push(("Text".into(), src_patch(id, json!({"text": v}))));
        }
        Grid::new(("text", id)).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            lbl(ui, "Font");
            ui.horizontal(|ui| {
                let mut f = t.font.clone();
                let fonts = util::font_families();
                narrow(ui, 130.0, |ui| {
                    ComboBox::from_id_salt(("font", id)).width(fitw(ui, 130.0)).truncate().height(360.0).selected_text(&f).show_ui(ui, |ui| {
                        if !fonts.iter().any(|n| n == &t.font) {
                            ui.selectable_value(&mut f, t.font.clone(), format!("{} (not installed)", t.font));
                        }
                        for name in &fonts {
                            ui.selectable_value(&mut f, name.clone(), name.as_str());
                        }
                    });
                });
                if f != t.font {
                    ops.push(("Font".into(), src_patch(id, json!({"font": f}))));
                }
                if let Some(v) = self.fields.text(ui, &k("font_custom"), "", false, 90.0, "other font…") {
                    if !v.trim().is_empty() {
                        ops.push(("Font".into(), src_patch(id, json!({"font": v.trim()}))));
                    }
                }
            });
            ui.end_row();
            lbl(ui, "Size");
            ui.horizontal(|ui| {
                if let Some(v) = self.fields.num(ui, &k("size"), t.size, &NumSpec::drag(1.0, 2000.0, 0.5, " px", 1)) {
                    ops.push(("Text size".into(), src_patch(id, json!({"size": v}))));
                }
                let mut w = t.weight;
                narrow(ui, 70.0, |ui| {
                    ComboBox::from_id_salt(("weight", id)).width(fitw(ui, 70.0)).truncate().selected_text(format!("{w}")).show_ui(ui, |ui| {
                        for x in [100u16, 200, 300, 400, 500, 600, 700, 800, 900] {
                            ui.selectable_value(&mut w, x, format!("{x}"));
                        }
                    });
                });
                if w != t.weight {
                    ops.push(("Text weight".into(), src_patch(id, json!({"weight": w}))));
                }
                let mut it = t.italic;
                if ui.checkbox(&mut it, "Italic").changed() {
                    ops.push(("Italic".into(), src_patch(id, json!({"italic": it}))));
                }
            });
            ui.end_row();
            lbl(ui, "Colour");
            if let Some(nc) = self.fields.color(ui, &k("color"), t.color) {
                ops.push(("Text colour".into(), src_patch(id, json!({"color": hex(nc)}))));
            }
            ui.end_row();
            lbl(ui, "Align");
            ui.horizontal(|ui| {
                for (a, l) in [(Align::Left, "Left"), (Align::Center, "Centre"), (Align::Right, "Right")] {
                    if ui.selectable_label(t.align == a, l).clicked() && t.align != a {
                        ops.push(("Align".into(), src_patch(id, json!({"align": a}))));
                    }
                }
            });
            ui.end_row();
            lbl(ui, "Spacing");
            ui.horizontal(|ui| {
                if let Some(v) = self.fields.num(ui, &k("lh"), t.line_height, &NumSpec::drag(0.5, 4.0, 0.01, "× line", 2)) {
                    ops.push(("Line height".into(), src_patch(id, json!({"line_height": v}))));
                }
                if let Some(v) = self.fields.num(ui, &k("ls"), t.letter_spacing, &NumSpec::drag(-50.0, 300.0, 0.2, " px", 1)) {
                    ops.push(("Letter spacing".into(), src_patch(id, json!({"letter_spacing": v}))));
                }
            });
            ui.end_row();
            lbl(ui, "Wrap");
            ui.horizontal(|ui| {
                let mut on = t.max_width.is_some();
                if ui.checkbox(&mut on, "").changed() {
                    ops.push(("Wrap".into(), src_patch(id, json!({"max_width": if on { json!(1200.0) } else { json!(null) }}))));
                }
                if let Some(w) = t.max_width {
                    if let Some(v) = self.fields.num(ui, &k("wrap"), w, &NumSpec::drag(20.0, 20000.0, 2.0, " px", 0)) {
                        ops.push(("Wrap width".into(), src_patch(id, json!({"max_width": v}))));
                    }
                }
            });
            ui.end_row();
            // Background box.
            lbl(ui, "Box");
            ui.horizontal(|ui| {
                let mut on = t.background.is_some();
                if ui.checkbox(&mut on, "").changed() {
                    ops.push(("Text box".into(), src_patch(id, if on { json!({"background": {"color": "#000000B3", "padding": 16.0, "radius": 6.0}}) } else { json!({"background": null}) })));
                }
                if let Some(b) = &t.background {
                    if let Some(nc) = self.fields.color(ui, &k("bg"), b.color) {
                        ops.push(("Box colour".into(), src_patch(id, json!({"background": {"color": hex(nc)}}))));
                    }
                    if let Some(v) = self.fields.num(ui, &k("pad"), b.padding, &NumSpec::drag(0.0, 500.0, 0.5, " pad", 0)) {
                        ops.push(("Box padding".into(), src_patch(id, json!({"background": {"padding": v}}))));
                    }
                    if let Some(v) = self.fields.num(ui, &k("rad"), b.radius, &NumSpec::drag(0.0, 500.0, 0.5, " r", 0)) {
                        ops.push(("Box radius".into(), src_patch(id, json!({"background": {"radius": v}}))));
                    }
                }
            });
            ui.end_row();
            lbl(ui, "Outline");
            ui.horizontal(|ui| {
                let mut on = t.stroke.is_some();
                if ui.checkbox(&mut on, "").changed() {
                    ops.push(("Text outline".into(), src_patch(id, if on { json!({"stroke": {"color": "#000000", "width": 3.0}}) } else { json!({"stroke": null}) })));
                }
                if let Some(s) = &t.stroke {
                    if let Some(nc) = self.fields.color(ui, &k("stroke"), s.color) {
                        ops.push(("Outline colour".into(), src_patch(id, json!({"stroke": {"color": hex(nc), "width": s.width}}))));
                    }
                    if let Some(v) = self.fields.num(ui, &k("sw"), s.width, &NumSpec::drag(0.0, 100.0, 0.1, " px", 1)) {
                        ops.push(("Outline width".into(), src_patch(id, json!({"stroke": {"color": hex(s.color), "width": v}}))));
                    }
                }
            });
            ui.end_row();
            lbl(ui, "Shadow");
            ui.horizontal(|ui| {
                let mut on = t.shadow.is_some();
                if ui.checkbox(&mut on, "").changed() {
                    ops.push(("Text shadow".into(), src_patch(id, if on { json!({"shadow": {"color": "#000000AA", "blur": 8.0, "x": 0.0, "y": 4.0}}) } else { json!({"shadow": null}) })));
                }
                if let Some(s) = &t.shadow {
                    if let Some(nc) = self.fields.color(ui, &k("sh"), s.color) {
                        ops.push(("Shadow colour".into(), src_patch(id, json!({"shadow": {"color": hex(nc), "blur": s.blur, "x": s.x, "y": s.y}}))));
                    }
                    for (f, v, l) in [("blur", s.blur, " blur"), ("x", s.x, " x"), ("y", s.y, " y")] {
                        if let Some(nv) = self.fields.num(ui, &k(&format!("sh_{f}")), v, &NumSpec { width: 54.0, ..NumSpec::drag(-500.0, 500.0, 0.2, l, 0) }) {
                            let mut obj = json!({"color": hex(s.color), "blur": s.blur, "x": s.x, "y": s.y});
                            obj[f] = json!(nv);
                            ops.push(("Shadow".into(), src_patch(id, json!({ "shadow": obj }))));
                        }
                    }
                }
            });
            ui.end_row();
            lbl(ui, "Reveal");
            ui.horizontal(|ui| {
                let mut rk = t.reveal.kind;
                narrow(ui, 100.0, |ui| {
                    ComboBox::from_id_salt(("reveal", id)).width(fitw(ui, 100.0)).truncate().selected_text(format!("{rk:?}")).show_ui(ui, |ui| {
                        for x in [RevealKind::None, RevealKind::Fade, RevealKind::Typewriter, RevealKind::FadeWords, RevealKind::PopWords, RevealKind::SlideUp, RevealKind::Spoken] {
                            ui.selectable_value(&mut rk, x, format!("{x:?}"));
                        }
                    });
                });
                if rk != t.reveal.kind {
                    let d = if t.reveal.duration > 0.0 { t.reveal.duration } else { 0.4 };
                    ops.push(("Text reveal".into(), src_patch(id, json!({"reveal": {"kind": rk, "duration": d}}))));
                }
                if !matches!(t.reveal.kind, RevealKind::None | RevealKind::Spoken) {
                    if let Some(v) = self.fields.num(ui, &k("rd"), t.reveal.duration, &NumSpec::drag(0.0, c.duration, 0.01, " s", 2)) {
                        ops.push(("Reveal duration".into(), src_patch(id, json!({"reveal": {"kind": t.reveal.kind, "duration": v}}))));
                    }
                }
            });
            ui.end_row();
            lbl(ui, "Highlight");
            ui.horizontal(|ui| match &t.highlight {
                Some(h) => {
                    if let Some(nc) = self.fields.color(ui, &k("hl"), h.color) {
                        ops.push(("Highlight colour".into(), src_patch(id, json!({"highlight": {"color": hex(nc)}}))));
                    }
                    ui.label(RichText::new(format!("{} word windows", h.words.len())).small().weak());
                    if ui.small_button("Remove").clicked() {
                        ops.push(("Remove highlight".into(), src_patch(id, json!({"highlight": null}))));
                    }
                }
                None => {
                    if ui.small_button("Add karaoke highlight").on_hover_text("Highlights each word in turn, spread evenly over the clip").clicked() {
                        let n = t.text.split_whitespace().count().max(1);
                        let step = c.duration / n as f64;
                        let words: Vec<[f64; 2]> = (0..n).map(|i| [i as f64 * step, (i + 1) as f64 * step]).collect();
                        ops.push(("Add highlight".into(), src_patch(id, json!({"highlight": {"color": "#FFD60A", "words": words}}))));
                    }
                }
            });
            ui.end_row();
        });
    }

    fn shape_editor(&mut self, ui: &mut Ui, cx: &EditCtx, c: &Clip, local: &Local, ops: &mut Vec<(String, Op)>, out: &mut Vec<UiAction>) {
        let Source::Shape(s) = &c.source else { return };
        let id = &c.id;
        let k = |f: &str| format!("{id}.shape.{f}");
        Grid::new(("shape", id)).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            lbl(ui, "Kind");
            let mut kind = s.kind;
            narrow(ui, 100.0, |ui| {
                ComboBox::from_id_salt(("skind", id)).width(fitw(ui, 100.0)).truncate().selected_text(format!("{kind:?}")).show_ui(ui, |ui| {
                    for x in [ShapeKind::Rect, ShapeKind::Ellipse, ShapeKind::Line, ShapeKind::Arrow] {
                        ui.selectable_value(&mut kind, x, format!("{x:?}"));
                    }
                });
            });
            if kind != s.kind {
                ops.push(("Shape kind".into(), src_patch(id, json!({"kind": kind}))));
            }
            ui.end_row();
            lbl(ui, "Size");
            ui.horizontal(|ui| {
                if let Some(v) = self.fields.num(ui, &k("w"), s.width, &NumSpec::drag(-20000.0, 20000.0, 1.0, " w", 0)) {
                    ops.push(("Shape width".into(), src_patch(id, json!({"width": v}))));
                }
                if let Some(v) = self.fields.num(ui, &k("h"), s.height, &NumSpec::drag(-20000.0, 20000.0, 1.0, " h", 0)) {
                    ops.push(("Shape height".into(), src_patch(id, json!({"height": v}))));
                }
            });
            ui.end_row();
            lbl(ui, "Fill");
            ui.horizontal(|ui| {
                let mut on = s.fill.is_some();
                if ui.checkbox(&mut on, "").changed() {
                    ops.push(("Shape fill".into(), src_patch(id, json!({"fill": if on { json!("#FFFFFF") } else { json!(null) }}))));
                }
                if let Some(f) = s.fill {
                    if let Some(nc) = self.fields.color(ui, &k("fill"), f) {
                        ops.push(("Fill colour".into(), src_patch(id, json!({"fill": hex(nc)}))));
                    }
                }
            });
            ui.end_row();
            lbl(ui, "Stroke");
            ui.horizontal(|ui| {
                let mut on = s.stroke.is_some();
                if ui.checkbox(&mut on, "").changed() {
                    ops.push(("Shape stroke".into(), src_patch(id, if on { json!({"stroke": {"color": "#FFD60A", "width": 6.0}}) } else { json!({"stroke": null}) })));
                }
                if let Some(st) = &s.stroke {
                    if let Some(nc) = self.fields.color(ui, &k("stroke"), st.color) {
                        ops.push(("Stroke colour".into(), src_patch(id, json!({"stroke": {"color": hex(nc), "width": st.width}}))));
                    }
                    if let Some(v) = self.fields.num(ui, &k("sw"), st.width, &NumSpec::drag(0.0, 200.0, 0.1, " px", 1)) {
                        ops.push(("Stroke width".into(), src_patch(id, json!({"stroke": {"color": hex(st.color), "width": v}}))));
                    }
                }
            });
            ui.end_row();
            if s.kind == ShapeKind::Rect {
                lbl(ui, "Radius");
                if let Some(v) = self.fields.num(ui, &k("r"), s.radius, &NumSpec::drag(0.0, 5000.0, 0.5, " px", 0)) {
                    ops.push(("Corner radius".into(), src_patch(id, json!({"radius": v}))));
                }
                ui.end_row();
            }
            if let Some(op) = self.clip_row(ui, cx, c, local, "source.draw", &s.draw, out) {
                ops.push(("Draw".into(), op));
            }
        });
    }

}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::tests::*;
    use aiframes_core::*;

    #[test]
    fn every_patch_the_inspector_emits_applies_under_the_strict_core() {
        // The core rejects unknown keys in nested JSON: every set_clip patch shape used above must parse.
        let mut p = demo();
        p.clips.push(text("t1", "v2", Time::At(20.0), 3.0));
        p.clips.push(clip("s1", "v2", Time::At(24.0), 3.0, Source::Shape(ShapeSource { kind: ShapeKind::Rect, width: 100.0, height: 50.0, fill: None, stroke: None, radius: 0.0, draw: Param::Value(1.0) })));
        p.clips.push(clip("so1", "v2", Time::At(28.0), 3.0, Source::Solid { color: Color::BLACK }));
        let text_patches = [
            json!({"text": "x"}), json!({"font": "Arial"}), json!({"size": 80.0}), json!({"weight": 600}), json!({"italic": true}),
            json!({"color": "#FF0000"}), json!({"align": Align::Left}), json!({"line_height": 1.3}), json!({"letter_spacing": 2.0}),
            json!({"max_width": 1200.0}), json!({"max_width": null}),
            json!({"background": {"color": "#000000B3", "padding": 16.0, "radius": 6.0}}), json!({"background": {"color": "#112233"}}),
            json!({"background": {"padding": 4.0}}), json!({"background": {"radius": 3.0}}), json!({"background": null}),
            json!({"stroke": {"color": "#000000", "width": 3.0}}), json!({"stroke": null}),
            json!({"shadow": {"color": "#000000AA", "blur": 8.0, "x": 0.0, "y": 4.0}}), json!({"shadow": null}),
            json!({"reveal": {"kind": RevealKind::Fade, "duration": 0.4}}),
            json!({"highlight": {"color": "#FFD60A", "words": [[0.0, 1.0]]}}), json!({"highlight": {"color": "#FFFFFF"}}), json!({"highlight": null}),
        ];
        let shape_patches = [
            json!({"kind": ShapeKind::Ellipse}), json!({"width": 120.0}), json!({"height": 60.0}), json!({"fill": "#FFFFFF"}), json!({"fill": null}),
            json!({"stroke": {"color": "#FFD60A", "width": 6.0}}), json!({"stroke": null}), json!({"radius": 4.0}),
        ];
        let clip_patches = [
            json!({"name": "n"}), json!({"notes": "x"}), json!({"tags": ["a"]}), json!({"enabled": false}), json!({"reverse": true}),
            json!({"muted": true}), json!({"blend": BlendMode::Screen}), json!({"transform": {"fit": Fit::Cover}}),
            json!({"transform": {"flip_x": true}}), json!({"transform": {"flip_y": false}}),
        ];
        let mut q = p.clone();
        for v in text_patches {
            apply(&mut q, &src_patch("t1", v.clone())).unwrap_or_else(|e| panic!("text {v}: {e}"));
        }
        for v in shape_patches {
            apply(&mut q, &src_patch("s1", v.clone())).unwrap_or_else(|e| panic!("shape {v}: {e}"));
        }
        apply(&mut q, &src_patch("so1", json!({"color": "#123456"}))).unwrap();
        for v in clip_patches {
            apply(&mut q, &patch("c1", v.clone())).unwrap_or_else(|e| panic!("clip {v}: {e}"));
        }
    }

    #[test]
    fn anchor_editor_keeps_clips_in_place() {
        let mut p = demo();
        p.clips.push(text("t1", "v2", Time::At(12.0), 2.0));
        p.markers.push(Marker { id: "mk1".into(), time: Time::At(11.0), name: "beat".into(), kind: MarkerKind::Marker, color: None, note: String::new() });
        p.transcripts.insert("ma".into(), Transcript { language: "en".into(), words: (0..30).map(|i| Word { text: format!(" w{i}"), start: i as f64 * 0.5, end: i as f64 * 0.5 + 0.4, conf: 1.0 }).collect() });
        let r = resolve(&p);
        for kind in AnchorKind::ALL {
            let Some(t) = anchor_keeping_place(&p, &r, "t1", kind, None) else { panic!("{kind:?}: no candidate") };
            assert_eq!(AnchorKind::of(&t), kind);
            let (q, rq) = run(&p, &[Op::MoveClip { clip: "t1".into(), start: t.clone(), track: None, overwrite: false }]);
            assert!((rq.span("t1").unwrap().start - 12.0).abs() < 1e-6, "{kind:?} moved the clip: {t:?}");
            let _ = q;
        }
        // Nearest targets: After → c1? No: c2 ends at 15, c1 at 10 → |10−12| = 2 wins over |15−12| = 3.
        let t = anchor_keeping_place(&p, &r, "t1", AnchorKind::After, None).unwrap();
        assert_eq!(util::anchor_parent(&t), Some("c1"));
        // A word anchor picks the word spoken at 12 s (word 24 at 12.0).
        let t = anchor_keeping_place(&p, &r, "t1", AnchorKind::Word, None).unwrap();
        assert!(matches!(&t, Time::Anchored(Anchor { to: AnchorTarget::Word(w), offset }) if w.index == 24 && offset.abs() < 1e-9));
        // No cycles: c1 cannot anchor to c2 (c2 follows c1).
        assert!(!anchor_targets(&p, &r, "c1", AnchorKind::After).contains(&"c2".to_string()));
        assert!(descendants(&p, "c1").contains("c2"));
    }

    fn with(id: &str, offset: f64) -> Time {
        Time::Anchored(Anchor { to: AnchorTarget::With(id.into()), offset })
    }

    #[test]
    fn word_anchors_record_the_word() {
        // Round 4: WordRef.text is filled when the inspector makes a word anchor (start or end).
        let mut p = demo();
        p.clips.push(text("t1", "v2", Time::At(12.0), 2.0));
        p.transcripts.insert("ma".into(), Transcript { language: "en".into(), words: (0..30).map(|i| Word { text: format!(" w{i}"), start: i as f64 * 0.5, end: i as f64 * 0.5 + 0.4, conf: 1.0 }).collect() });
        let r = resolve(&p);
        let t = anchor_keeping_place(&p, &r, "t1", AnchorKind::Word, None).unwrap();
        assert!(matches!(&t, Time::Anchored(Anchor { to: AnchorTarget::Word(w), .. }) if w.text.as_deref() == Some("w24")), "{t:?}");
        let u = until_keeping_place(&p, &r, "t1", EndKind::Word, None).unwrap().unwrap();
        assert!(matches!(&u, Time::Anchored(Anchor { to: AnchorTarget::Word(w), .. }) if w.end && w.text.as_deref() == Some("w27")), "{u:?}");
        let (q, rq) = run(&p, &[Op::MoveClip { clip: "t1".into(), start: t, track: None, overwrite: false }, set_until_op("t1", Some(u))]);
        assert!((rq.span("t1").unwrap().start - 12.0).abs() < 1e-6 && (rq.span("t1").unwrap().end - 14.0).abs() < 1e-6, "{:?}", rq.span("t1"));
        assert!(!validate(&q, &rq).iter().any(|f| f.code == "word_anchor_text_mismatch"));
    }

    #[test]
    fn end_anchor_editor_keeps_the_end_and_avoids_cycles() {
        let mut p = demo();
        p.tracks.push(track("v3", TrackKind::Video, "overlay"));
        // A badge with c1 (0–10) at 0.5–8.5; t2 chained after the badge; t3 ends 1 s after t2 does.
        p.clips.push(text("badge", "v2", with("c1", 0.5), 8.0));
        p.clips.push(text("t2", "v2", Time::Anchored(Anchor { to: AnchorTarget::After("badge".into()), offset: 1.0 }), 2.0));
        p.clips.push(text("t3", "v3", Time::At(10.0), 1.0));
        p.markers.push(Marker { id: "mk1".into(), time: Time::At(9.0), name: "beat".into(), kind: MarkerKind::Marker, color: None, note: String::new() });
        let (p, r) = run(&p, &[set_until_op("t3", Some(Time::Anchored(Anchor { to: AnchorTarget::After("t2".into()), offset: 1.0 })))]);
        // Candidates: never the clip itself or what depends on its end (t2 follows it, t3 ends after t2).
        let after = until_targets(&p, &r, "badge", EndKind::After);
        assert!(after.contains(&"c1".to_string()) && after.contains(&"c2".to_string()), "{after:?}");
        assert!(!after.iter().any(|x| x == "badge" || x == "t2" || x == "t3"), "{after:?}");
        let blocked = util::end_dependants(&p, "badge");
        assert!(blocked.contains(&("t2".to_string(), false)) && blocked.contains(&("t3".to_string(), true)), "{blocked:?}");
        // Every kind keeps the badge's end (8.5) where it is.
        for kind in [EndKind::After, EndKind::With, EndKind::Marker, EndKind::At, EndKind::Fixed] {
            let u = until_keeping_place(&p, &r, "badge", kind, None).unwrap_or_else(|| panic!("{kind:?}: no candidate"));
            assert_eq!(EndKind::of(u.as_ref()), kind);
            let (_, rq) = run(&p, &[set_until_op("badge", u.clone())]);
            assert!((rq.span("badge").unwrap().end - 8.5).abs() < 1e-6, "{kind:?} moved the end: {u:?} → {:?}", rq.span("badge"));
        }
        // "Until end of c1" with the offset that keeps the end, then a speed change: the badge follows.
        let mut p = demo();
        p.clips.push(text("badge", "v2", with("c1", 0.5), 8.0));
        let r = resolve(&p);
        let u = until_keeping_place(&p, &r, "badge", EndKind::After, Some("c1")).unwrap();
        assert_eq!(u, Some(Time::Anchored(Anchor { to: AnchorTarget::After("c1".into()), offset: -1.5 })));
        let (q, rq) = run(&p, &[set_until_op("badge", u), Op::SetSpeed { clip: "c1".into(), speed: 2.0, keep: SpeedKeep::Source, linked: true }]);
        assert!((rq.span("c1").unwrap().end - 5.0).abs() < 1e-6);
        assert!((rq.span("badge").unwrap().end - 3.5).abs() < 1e-6, "{:?}", rq.span("badge"));
        assert_eq!(aiframes_core::until_text(q.clips.iter().find(|c| c.id == "badge").unwrap().until.as_ref().unwrap()), "end of c1 (-1.5)");
    }

    #[test]
    fn match_length_follows_the_base_clip() {
        let mut p = demo();
        p.clips.push(text("badge", "v2", with("c1", 0.5), 8.0));
        let r = resolve(&p);
        let c = p.clips.iter().find(|c| c.id == "badge").unwrap();
        assert_eq!(with_base(&p, c).map(|b| b.id.as_str()), Some("c1"));
        let op = match_length_op(&p, c).expect("offered");
        let (q, rq) = run(&p, &[op, Op::SetSpeed { clip: "c1".into(), speed: 8.0, keep: SpeedKeep::Source, linked: true }]);
        assert!((rq.span("badge").unwrap().start - 0.5).abs() < 1e-6 && (rq.span("badge").unwrap().end - rq.span("c1").unwrap().end).abs() < 1e-6, "{:?}", rq.span("badge"));
        // Already matching: not offered again; clips not anchored `with` a clip: not offered.
        assert!(match_length_op(&q, q.clips.iter().find(|c| c.id == "badge").unwrap()).is_none());
        assert!(match_length_op(&p, p.clips.iter().find(|c| c.id == "c2").unwrap()).is_none());
        let _ = r;
    }
}
