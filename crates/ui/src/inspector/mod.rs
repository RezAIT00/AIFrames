//! The inspector: properties of the selection — clip (timing and anchors, source, speed, transform,
//! compositing, fades, audio, text, shape, effects with keyframes and curves), several clips,
//! transition, marker, track, media asset — or the project (settings, AI brief, export, ducking)
//! when nothing is selected. Every change is one op, applied when a drag or edit ends.

mod clip;
mod fx;
mod param;

pub(crate) use clip::{match_length_op, matte_mode_label, set_until_op, with_base};

use crate::style;
use crate::util::{self, ClipKind};
use crate::widgets::{fitw, narrow, Fields, NumSpec, Scale};
use crate::{EditCtx, UiAction};
use aiframes_core::{
    effects::{transition_param, TRANSITIONS},
    Clip, Color, Id, MarkerKind, MediaAsset, MediaKind, Op, Project, Resolved, Time, TrackKind, Transition,
};
use egui::{ComboBox, Grid, RichText, Ui};
use std::collections::HashSet;

/// Properties of the selection: clip (timing, source, transform/opacity/volume/speed with keyframe
/// toggles and a small curve view, fades, blend, effects stack, text and shape editing), transition,
/// marker, track, media asset; project settings when nothing is selected.
#[derive(Default)]
pub struct Inspector {
    fields: Fields,
    curves: HashSet<String>,
    collapsed: HashSet<String>,
    keep_duration: bool,
    last_selection: Vec<Id>,
    /// (added) An effect card to expand and scroll into view: `(clip, index)` (see `reveal_effect`).
    reveal: Option<(Id, usize)>,
    /// (added) The revealed card's outline flash: `(clip, index, start time)`.
    flash: Option<(Id, usize, Option<f64>)>,
    /// (added) The effect kinds of the clip shown last, to notice effects others add.
    fx_seen: Option<(Id, Vec<String>)>,
}

pub(crate) fn section(ui: &mut Ui, title: &str, id: &str, default_open: bool, add: impl FnOnce(&mut Ui)) {
    section_with(ui, title, id, default_open, false, add)
}

/// (added) A section that `force_open` opens even if the person folded it (revealing a new effect).
pub(crate) fn section_with(ui: &mut Ui, title: &str, id: &str, default_open: bool, force_open: bool, add: impl FnOnce(&mut Ui)) {
    let key = title.split(" (").next().unwrap_or(title).to_string();
    let mut h = egui::CollapsingHeader::new(RichText::new(title).strong().size(12.0)).id_salt(("insp", key, id)).default_open(default_open);
    if force_open {
        h = h.open(Some(true));
    }
    let r = h.show(ui, add);
    #[cfg(test)]
    if let Some(b) = r.body_response {
        tests::SECTION_W.with(|w| w.borrow_mut().push((title.to_string(), b.rect.width())));
    }
    #[cfg(not(test))]
    let _ = r;
}

/// The field look of a clip-level parameter from the core registry (`CLIP_PARAMS`): label, help,
/// range and units. Fractions and scale show as %, volume as dB on a fader, the rest in their units.
pub(crate) fn clip_spec(path: &str) -> (&'static str, &'static str, NumSpec) {
    let Some(s) = aiframes_core::clip_param_spec(path) else {
        return ("", "", NumSpec::drag(-1e6, 1e6, 0.01, "", 3));
    };
    let pct = |speed: f64, d: usize| NumSpec::drag(s.min * 100.0, s.max * 100.0, speed, " %", d).scaled(Scale::Linear(100.0));
    let ns = match (path, s.unit) {
        ("volume", _) => NumSpec::slider(-60.0, (20.0 * s.max.log10()).min(24.0), " dB", 1).scaled(Scale::Db),
        ("opacity" | "source.draw", _) => NumSpec::slider(s.min * 100.0, s.max * 100.0, " %", 0).scaled(Scale::Linear(100.0)),
        ("transform.scale", _) => pct(0.5, 1),
        (_, "frac") => pct(0.2, 1),
        (_, "px") => NumSpec::drag(s.min, s.max, 1.0, " px", 1),
        (_, "deg") => NumSpec::drag(s.min, s.max, 0.5, "°", 1),
        (_, "s") => NumSpec::drag(s.min, s.max, 0.01, " s", 2),
        (_, "x") => NumSpec::drag(s.min, s.max, 0.01, "×", 3),
        _ => NumSpec::drag(s.min, s.max, ((s.max - s.min) / 500.0).max(1e-3), "", 3),
    };
    (s.label, s.help, ns)
}

/// The field look of a ducking setting from `DUCKING_PARAMS`.
pub(crate) fn ducking_spec(name: &str) -> (&'static str, &'static str, NumSpec) {
    match aiframes_core::DUCKING_PARAMS.iter().find(|p| p.name == name) {
        Some(s) => {
            let suffix = match s.unit {
                "dB" => " dB",
                "dBFS" => " dBFS",
                "s" => " s",
                _ => "",
            };
            let speed = if s.unit == "s" { 0.005 } else { 0.2 };
            (s.label, s.help, NumSpec::drag(s.min, s.max, speed, suffix, if s.unit == "s" { 2 } else { 1 }))
        }
        None => ("", "", NumSpec::drag(-100.0, 100.0, 0.1, "", 2)),
    }
}

/// Output frame sizes offered for export (`None` = the project's size).
pub(crate) const EXPORT_SIZES: &[(&str, Option<(u32, u32)>)] = &[
    ("Same as project", None),
    ("4K UHD 3840×2160", Some((3840, 2160))),
    ("1440p 2560×1440", Some((2560, 1440))),
    ("1080p 1920×1080", Some((1920, 1080))),
    ("720p 1280×720", Some((1280, 720))),
    ("Vertical 1080×1920", Some((1080, 1920))),
    ("Square 1080×1080", Some((1080, 1080))),
];

/// The `set_export_size` for a preset (pure).
pub(crate) fn export_size_op(size: Option<(u32, u32)>, fit: Option<aiframes_core::Fit>) -> Op {
    match size {
        Some((w, h)) => Op::SetExportSize { width: Some(Some(w)), height: Some(Some(h)), fit },
        None => Op::SetExportSize { width: Some(None), height: Some(None), fit },
    }
}

/// One shared value applied to several clips (multi-selection): `set_param` for every clip that has
/// the parameter (`set_speed` for speed, keeping the source range), skipping clips already at it.
pub(crate) fn shared_value_ops(p: &Project, ids: &[Id], path: &str, v: f64) -> Vec<Op> {
    p.clips
        .iter()
        .filter(|c| ids.contains(&c.id))
        .filter(|c| p.tracks.iter().any(|t| t.id == c.track && !t.locked))
        .filter(|c| shared_applies(p, c, path))
        .filter(|c| match path {
            "fade_in" => (c.fade_in - v).abs() > 1e-9,
            "fade_out" => (c.fade_out - v).abs() > 1e-9,
            _ => util::param_at(c, path).is_none_or(|x| x.is_animated() || (aiframes_core::eval::sample(x, 0.0) - v).abs() > 1e-9),
        })
        .map(|c| match path {
            "speed" => Op::SetSpeed { clip: c.id.clone(), speed: v, keep: Default::default(), linked: true },
            "fade_in" | "fade_out" => Op::SetParam { clip: c.id.clone(), path: path.into(), value: Some(v.min((c.duration - if path == "fade_in" { c.fade_out } else { c.fade_in }).max(0.0))), keys: None },
            _ => Op::SetParam { clip: c.id.clone(), path: path.into(), value: Some(v), keys: None },
        })
        .collect()
}

/// Whether a shared field applies to a clip (volume needs sound, speed media, transforms a picture).
pub(crate) fn shared_applies(p: &Project, c: &Clip, path: &str) -> bool {
    let kind = util::clip_kind_on_track(p, c);
    match path {
        "volume" => util::has_sound(p, c),
        "speed" => matches!(kind, ClipKind::Video | ClipKind::Audio),
        "fade_in" | "fade_out" => true,
        _ => kind != ClipKind::Audio,
    }
}

/// Fields shared by a multi-clip selection.
pub(crate) const SHARED: &[&str] = &["opacity", "volume", "speed", "fade_in", "fade_out", "transform.x", "transform.y", "transform.scale", "transform.rotation"];

/// A row label (first grid column): short, never truncated (the value column shrinks instead).
pub(crate) fn lbl(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add(egui::Label::new(RichText::new(text).color(ui.visuals().weak_text_color())).extend())
}

/// (round 4) A row label whose tooltip is the field's documentation from the core's `schema_docs()`.
pub(crate) fn doc_lbl(ui: &mut Ui, text: &str, ty: &str, field: &str) -> egui::Response {
    let r = lbl(ui, text);
    match util::schema_help(ty, field) {
        Some(h) => r.on_hover_text(h),
        None => r,
    }
}

/// What is selected (clips win over other ids).
pub(crate) enum Selected<'a> {
    Nothing,
    Clip(&'a Clip),
    Clips(Vec<&'a Clip>),
    Transition(&'a Transition),
    Marker(&'a aiframes_core::Marker),
    Track(&'a aiframes_core::Track),
    Media(&'a MediaAsset),
}

pub(crate) fn selected<'a>(p: &'a Project, sel: &[Id]) -> Selected<'a> {
    let clips: Vec<&Clip> = p.clips.iter().filter(|c| sel.contains(&c.id)).collect();
    match clips.len() {
        1 => return Selected::Clip(clips[0]),
        n if n > 1 => {
            // A clip selected together with its linked (detached) audio edits as that clip.
            let leader = clips.iter().find(|c| {
                let partners = aiframes_core::linked_partners(p, &c.id);
                let is_follower = matches!(&c.start, Time::Anchored(aiframes_core::Anchor { to: aiframes_core::AnchorTarget::With(t), .. }) if partners.contains(t));
                !is_follower && clips.iter().all(|o| o.id == c.id || partners.contains(&o.id))
            });
            return match leader {
                Some(c) => Selected::Clip(c),
                None => Selected::Clips(clips),
            };
        }
        _ => {}
    }
    for id in sel {
        if let Some(t) = p.transitions.iter().find(|t| &t.id == id) {
            return Selected::Transition(t);
        }
        if let Some(m) = p.markers.iter().find(|m| &m.id == id) {
            return Selected::Marker(m);
        }
        if let Some(t) = p.tracks.iter().find(|t| &t.id == id) {
            return Selected::Track(t);
        }
        if let Some(m) = p.media.iter().find(|m| &m.id == id) {
            return Selected::Media(m);
        }
    }
    Selected::Nothing
}

const RESOLUTIONS: &[(&str, u32, u32)] = &[
    ("1080p 16:9", 1920, 1080),
    ("4K UHD 16:9", 3840, 2160),
    ("1440p 16:9", 2560, 1440),
    ("720p 16:9", 1280, 720),
    ("Vertical 9:16 (Shorts)", 1080, 1920),
    ("Square 1:1", 1080, 1080),
    ("Portrait 4:5", 1080, 1350),
];
const FPS: &[f64] = &[23.976, 24.0, 25.0, 29.97, 30.0, 50.0, 59.94, 60.0];

/// `set_settings` arguments (enum variants have no struct-update syntax).
#[derive(Default)]
pub(crate) struct S {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub sample_rate: Option<u32>,
    pub background: Option<Color>,
    pub duration: Option<Option<f64>>,
    pub title_safe: Option<f64>,
    pub action_safe: Option<f64>,
}

impl S {
    pub fn op(self) -> Op {
        Op::SetSettings { width: self.width, height: self.height, fps: self.fps, sample_rate: self.sample_rate, background: self.background, duration: self.duration, title_safe: self.title_safe, action_safe: self.action_safe }
    }
}

/// `set_export` arguments.
#[derive(Default)]
pub(crate) struct E {
    pub path: Option<Option<String>>,
    pub codec: Option<String>,
    pub quality: Option<String>,
    pub hardware: Option<bool>,
    pub audio_bitrate_kbps: Option<u32>,
    pub loudness_lufs: Option<Option<f64>>,
    pub true_peak_db: Option<f64>,
}

impl E {
    pub fn op(self) -> Op {
        Op::SetExport { path: self.path, codec: self.codec, quality: self.quality, hardware: self.hardware, audio_bitrate_kbps: self.audio_bitrate_kbps, loudness_lufs: self.loudness_lufs, true_peak_db: self.true_peak_db, range: None }
    }
}

impl Inspector {
    pub fn ui(&mut self, ui: &mut Ui, cx: &EditCtx) -> Vec<UiAction> {
        let mut out = Vec::new();
        if self.last_selection.as_slice() != cx.selection {
            self.fields.clear();
            self.last_selection = cx.selection.to_vec();
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("aiframes_inspector").show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            // Round 5: never push a narrow side panel wider — text that does not fit is cut with an
            // ellipsis (the full text shows on hover); paragraphs opt into wrapping.
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            match selected(cx.project, cx.selection) {
                Selected::Clip(c) => self.clip_panel(ui, cx, c, &mut out),
                Selected::Clips(cs) => self.clips_panel(ui, cx, &cs, &mut out),
                Selected::Transition(t) => self.transition_panel(ui, cx, t, &mut out),
                Selected::Marker(m) => self.marker_panel(ui, cx, m, &mut out),
                Selected::Track(t) => self.track_panel(ui, cx, t, &mut out),
                Selected::Media(m) => self.media_panel(ui, cx, m, &mut out),
                Selected::Nothing => self.project_panel(ui, cx, &mut out),
            }
        });
        out
    }

    fn clips_panel(&mut self, ui: &mut Ui, cx: &EditCtx, cs: &[&Clip], out: &mut Vec<UiAction>) {
        let p = cx.project;
        ui.heading(RichText::new(format!("{} clips", cs.len())).size(15.0));
        let ids: Vec<Id> = cs.iter().map(|c| c.id.clone()).collect();
        let total: f64 = cs.iter().map(|c| c.duration).sum();
        ui.label(RichText::new(format!("{} of material", util::short_time(total))).weak());
        egui::ScrollArea::vertical().max_height(140.0).id_salt("multi_list").show(ui, |ui| {
            for c in cs {
                ui.horizontal(|ui| {
                    let k = util::clip_kind_on_track(p, c);
                    style::chip(ui, &format!("{k:?}"), style::clip_color(k));
                    if ui.link(format!("{} · {}", c.id, util::ellipsize(&util::clip_label(p, c), 28))).clicked() {
                        out.push(UiAction::Select(vec![c.id.clone()]));
                    }
                });
            }
        });
        ui.separator();
        // Shared values: edit once, applied to every selected clip it fits (one undo step).
        crate::style::heading(ui, "Shared values");
        Grid::new("multi_shared").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            for path in SHARED {
                let fit: Vec<&Clip> = cs.iter().copied().filter(|c| shared_applies(p, c, path)).collect();
                if fit.is_empty() {
                    continue;
                }
                let values: Vec<Option<f64>> = fit
                    .iter()
                    .map(|c| match *path {
                        "fade_in" => Some(c.fade_in),
                        "fade_out" => Some(c.fade_out),
                        _ => util::param_at(c, path).map(|x| if x.is_animated() { f64::NAN } else { aiframes_core::eval::sample(x, 0.0) }),
                    })
                    .collect();
                let first = values.first().copied().flatten().unwrap_or(0.0);
                let mixed = values.iter().any(|v| v.is_none_or(|v| v.is_nan() || (v - first).abs() > 1e-9));
                let (label, help, mut spec) = clip_spec(path);
                if spec.slider {
                    spec.slider = false;
                    spec.width = 90.0;
                    spec.speed = (spec.max - spec.min) / 300.0;
                }
                lbl(ui, label).on_hover_text(format!("{help}\nApplies to {} of the selected clips{}.", fit.len(), if mixed { " (values differ; editing sets them all, replacing any animation)" } else { "" }));
                ui.horizontal(|ui| {
                    let cur = if first.is_nan() { 0.0 } else { first };
                    if let Some(v) = self.fields.num(ui, &format!("multi.{path}"), cur, &spec) {
                        let ops = shared_value_ops(p, &ids, path, v);
                        if !ops.is_empty() {
                            out.push(UiAction::Apply { label: format!("{label} for {} clips", ops.len()), ops });
                        }
                    }
                    if mixed {
                        ui.label(RichText::new("mixed").small().color(ui.visuals().weak_text_color()));
                    }
                });
                ui.end_row();
            }
        });
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if ui.button("Delete").clicked() {
                out.push(UiAction::Apply { label: format!("Delete {} clips", ids.len()), ops: crate::edits::delete_ops(p, &ids, false) });
            }
            if ui.button("Ripple delete").clicked() {
                out.push(UiAction::Apply { label: format!("Ripple delete {} clips", ids.len()), ops: crate::edits::delete_ops(p, &ids, true) });
            }
            let any_on = cs.iter().any(|c| c.enabled);
            if ui.button(if any_on { "Disable all" } else { "Enable all" }).clicked() {
                let ops = ids.iter().map(|id| Op::SetClip { clip: id.clone(), patch: serde_json::json!({"enabled": !any_on}) }).collect();
                out.push(UiAction::Apply { label: "Enable/disable clips".into(), ops });
            }
            ui.menu_button("Add effect to all", |ui| {
                for e in aiframes_core::EFFECTS {
                    if ui.button(e.label).on_hover_text(e.help).clicked() {
                        let ops = cs
                            .iter()
                            .filter(|c| util::clip_kind(p, c) != ClipKind::Audio)
                            .map(|c| Op::AddEffect { clip: c.id.clone(), effect: aiframes_core::EffectArg::Kind(e.kind.into()), index: None })
                            .collect();
                        out.push(UiAction::Apply { label: format!("Add {} to clips", e.label), ops });
                        ui.close();
                    }
                }
            });
            self.attributes_menu(ui, cs, None, out);
            if ui.button("Ask AI").clicked() {
                out.push(UiAction::AskAi { prompt: format!("About clips {}: ", ids.join(", ")), ids: ids.clone() });
            }
        });
    }

    fn transition_panel(&mut self, ui: &mut Ui, cx: &EditCtx, t: &Transition, out: &mut Vec<UiAction>) {
        let fps = util::fps(cx.project);
        let spec = aiframes_core::transition_spec(&t.kind);
        ui.horizontal(|ui| {
            style::chip(ui, "Transition", style::rgb(0x6D, 0x5A, 0xC8));
            ui.label(RichText::new(&t.id).monospace().weak());
        });
        let cut = cx.resolved.span(&t.from).map(|s| s.end).unwrap_or(0.0);
        ui.horizontal(|ui| {
            ui.label(format!("Cut {} | {} at {}", t.from, t.to, util::timecode(cut, fps)));
            if ui.small_button("Go to cut").on_hover_text("Move the playhead to the middle of the transition").clicked() {
                out.push(UiAction::Seek(cut));
            }
        });
        let mut ops: Vec<(String, Op)> = Vec::new();
        let previews_before = out.iter().filter(|a| matches!(a, UiAction::Preview(_))).count();
        Grid::new(("trans", &t.id)).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            lbl(ui, "Kind");
            let mut k = t.kind.clone();
            narrow(ui, 150.0, |ui| {
                ComboBox::from_id_salt(("tkind", &t.id)).width(fitw(ui, 150.0)).truncate().selected_text(spec.map(|s| s.label).unwrap_or(&t.kind)).show_ui(ui, |ui| {
                    for s in TRANSITIONS {
                        ui.selectable_value(&mut k, s.kind.to_string(), s.label).on_hover_text(s.help);
                    }
                });
            });
            if k != t.kind {
                ops.push(("Transition kind".into(), Op::SetTransition { transition: t.id.clone(), kind: Some(k), duration: None, params: Default::default() }));
            }
            ui.end_row();
            self.transition_duration_row(ui, cx, t, &mut ops, out);
            self.transition_params(ui, t, &mut ops, out);
        });
        Self::seek_into_transition(cx, t, previews_before, out);
        if let Some(s) = spec {
            ui.add(egui::Label::new(RichText::new(s.help).small().weak()).wrap());
            if s.params.is_empty() && s.colors.is_empty() {
                ui.add(egui::Label::new(RichText::new("This transition has no settings besides its kind and length.").small().weak()).wrap());
            }
        }
        if ui.button("Remove transition").clicked() {
            ops.push(("Remove transition".into(), Op::RemoveTransition { transition: t.id.clone() }));
        }
        for (l, op) in ops {
            out.push(UiAction::Apply { label: format!("{l} ({})", t.id), ops: vec![op] });
        }
    }

    fn marker_panel(&mut self, ui: &mut Ui, cx: &EditCtx, m: &aiframes_core::Marker, out: &mut Vec<UiAction>) {
        let fps = util::fps(cx.project);
        let col = style::marker_color(m.kind, m.color);
        ui.horizontal(|ui| {
            style::chip(ui, &format!("{:?}", m.kind), col);
            ui.label(RichText::new(&m.id).monospace().weak());
        });
        let set = |name: Option<String>, kind: Option<MarkerKind>, color: Option<Option<Color>>, note: Option<String>, time: Option<Time>| Op::SetMarker { marker: m.id.clone(), time, name, kind, color, note };
        let mut ops: Vec<Op> = Vec::new();
        Grid::new(("marker", &m.id)).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            lbl(ui, "Name");
            if let Some(v) = self.fields.text(ui, &format!("{}.name", m.id), &m.name, false, 200.0, "name") {
                ops.push(set(Some(v), None, None, None, None));
            }
            ui.end_row();
            lbl(ui, "Kind");
            let mut k = m.kind;
            narrow(ui, 130.0, |ui| {
                ComboBox::from_id_salt(("mkind", &m.id)).width(fitw(ui, 130.0)).truncate().selected_text(format!("{k:?}")).show_ui(ui, |ui| {
                    for (x, l) in [(MarkerKind::Marker, "Marker"), (MarkerKind::Chapter, "Chapter (YouTube)"), (MarkerKind::Beat, "Beat"), (MarkerKind::Todo, "To do")] {
                        ui.selectable_value(&mut k, x, l);
                    }
                });
            });
            if k != m.kind {
                ops.push(set(None, Some(k), None, None, None));
            }
            ui.end_row();
            lbl(ui, "Colour");
            ui.horizontal(|ui| {
                let mut custom = m.color.is_some();
                if ui.checkbox(&mut custom, "Custom").changed() {
                    ops.push(set(None, None, Some(if custom { Some(style::core_color(col)) } else { None }), None, None));
                }
                if let Some(c) = m.color {
                    if let Some(nc) = self.fields.color(ui, &format!("{}.color", m.id), c) {
                        ops.push(set(None, None, Some(Some(nc)), None, None));
                    }
                }
            });
            ui.end_row();
            lbl(ui, "Time");
            let cur = cx.resolved.marker(&m.id).unwrap_or(0.0);
            ui.horizontal(|ui| match &m.time {
                Time::At(_) => {
                    if let Some(t) = self.fields.time(ui, &format!("{}.time", m.id), cur, fps, 0.0..=1e7) {
                        ops.push(set(None, None, None, None, Some(Time::At(t))));
                    }
                }
                Time::Anchored(a) => {
                    ui.label(RichText::new(util::timecode(cur, fps)).monospace());
                    ui.label(RichText::new(aiframes_core::resolve::describe_anchor(a)).small().color(style::ACCENT));
                    if ui.small_button("Make absolute").clicked() {
                        ops.push(set(None, None, None, None, Some(Time::At(cur))));
                    }
                }
            });
            ui.end_row();
            lbl(ui, "Note");
            if let Some(v) = self.fields.text(ui, &format!("{}.note", m.id), &m.note, true, 200.0, "what to do here") {
                ops.push(set(None, None, None, Some(v), None));
            }
            ui.end_row();
        });
        ui.horizontal(|ui| {
            if ui.button("Go to").clicked() {
                if let Some(t) = cx.resolved.marker(&m.id) {
                    out.push(UiAction::Seek(t));
                }
            }
            if ui.button("Delete marker").clicked() {
                ops.push(Op::RemoveMarker { marker: m.id.clone() });
            }
        });
        for op in ops {
            out.push(UiAction::Apply { label: format!("Marker {}", m.id), ops: vec![op] });
        }
    }

    fn track_panel(&mut self, ui: &mut Ui, cx: &EditCtx, t: &aiframes_core::Track, out: &mut Vec<UiAction>) {
        let p = cx.project;
        ui.horizontal(|ui| {
            style::chip(ui, if t.kind == TrackKind::Video { "Video track" } else { "Audio track" }, style::track_kind_color(t.kind));
            ui.label(RichText::new(&t.id).monospace().weak());
        });
        let set = |name: Option<String>, role: Option<String>, muted: Option<bool>, hidden: Option<bool>, locked: Option<bool>, solo: Option<bool>, volume: Option<f64>| Op::SetTrack { track: t.id.clone(), name, role, muted, hidden, locked, solo, volume };
        let mut ops = Vec::new();
        Grid::new(("track", &t.id)).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            lbl(ui, "Name");
            if let Some(v) = self.fields.text(ui, &format!("{}.name", t.id), &t.name, false, 180.0, "name") {
                ops.push(set(Some(v), None, None, None, None, None, None));
            }
            ui.end_row();
            lbl(ui, "Role");
            let mut role = t.role.clone();
            narrow(ui, 120.0, |ui| {
                ComboBox::from_id_salt(("role", &t.id)).width(fitw(ui, 120.0)).truncate().selected_text(if role.is_empty() { "(none)" } else { &role }).show_ui(ui, |ui| {
                    for r in ["main", "overlay", "captions", "voice", "music", "sfx", ""] {
                        ui.selectable_value(&mut role, r.to_string(), if r.is_empty() { "(none)" } else { r });
                    }
                });
            });
            if role != t.role {
                ops.push(set(None, Some(role), None, None, None, None, None));
            }
            ui.end_row();
            lbl(ui, "Flags");
            ui.horizontal(|ui| {
                let (mut m, mut h, mut l, mut s) = (t.muted, t.hidden, t.locked, t.solo);
                if ui.checkbox(&mut m, "Mute").changed() {
                    ops.push(set(None, None, Some(m), None, None, None, None));
                }
                if t.kind == TrackKind::Video && ui.checkbox(&mut h, "Hide").changed() {
                    ops.push(set(None, None, None, Some(h), None, None, None));
                }
                if t.kind == TrackKind::Audio && ui.checkbox(&mut s, "Solo").changed() {
                    ops.push(set(None, None, None, None, None, Some(s), None));
                }
                if ui.checkbox(&mut l, "Lock").changed() {
                    ops.push(set(None, None, None, None, Some(l), None, None));
                }
            });
            ui.end_row();
            lbl(ui, "Volume");
            if let Some(v) = self.fields.num(ui, &format!("{}.vol", t.id), t.volume, &NumSpec::slider(-60.0, 12.0, " dB", 1).scaled(Scale::Db)) {
                ops.push(set(None, None, None, None, None, None, Some((v * 10000.0).round() / 10000.0)));
            }
            ui.end_row();
        });
        let n = p.clips.iter().filter(|c| c.track == t.id).count();
        ui.label(RichText::new(format!("{n} clip(s)")).weak());
        let ducks: Vec<&aiframes_core::Ducking> = p.ducking.iter().filter(|d| d.target == t.id || d.trigger == t.id).collect();
        for d in ducks {
            ui.add(egui::Label::new(RichText::new(format!("ducking {}: {} ducks {} dB under {}", d.id, d.target, d.amount_db, d.trigger)).small().weak()).wrap());
        }
        ui.horizontal(|ui| {
            if ui.button("Select clips").clicked() {
                out.push(UiAction::Select(p.clips.iter().filter(|c| c.track == t.id).map(|c| c.id.clone()).collect()));
            }
            if ui.button(if n == 0 { "Remove track".to_string() } else { format!("Remove track + {n} clips") }).clicked() {
                ops.push(Op::RemoveTrack { track: t.id.clone(), force: n > 0 });
            }
        });
        for op in ops {
            out.push(UiAction::Apply { label: format!("Track {}", t.id), ops: vec![op] });
        }
    }

    fn media_panel(&mut self, ui: &mut Ui, cx: &EditCtx, m: &MediaAsset, out: &mut Vec<UiAction>) {
        let p = cx.project;
        let fps = util::fps(p);
        ui.horizontal(|ui| {
            style::chip(ui, &format!("{:?}", m.kind), crate::bin::kind_color(m.kind));
            ui.label(RichText::new(&m.id).monospace().weak());
        });
        let w = ui.available_width().min(300.0);
        let aspect = if m.info.width > 0 && m.info.height > 0 { m.info.width as f32 / m.info.height as f32 } else { 16.0 / 9.0 };
        let (rect, _) = ui.allocate_exact_size(egui::vec2(w, w / aspect.max(0.3)), egui::Sense::hover());
        crate::bin::paint_thumb(&ui.painter_at(rect), cx, m, rect, m.info.duration * 0.1);
        let mut ops = Vec::new();
        Grid::new(("media", &m.id)).num_columns(2).spacing([8.0, 3.0]).show(ui, |ui| {
            lbl(ui, "Name");
            if let Some(v) = self.fields.text(ui, &format!("{}.name", m.id), &m.name, false, 190.0, &util::media_name(m)) {
                ops.push(Op::SetMedia { asset: m.id.clone(), name: Some(v), notes: None, tags: None });
            }
            ui.end_row();
            lbl(ui, "File");
            ui.label(RichText::new(&m.path).small()).on_hover_text(&m.path);
            ui.end_row();
            let i = &m.info;
            lbl(ui, "Duration");
            ui.label(if m.kind == MediaKind::Image { "still image".to_string() } else { format!("{} ({})", util::timecode(i.duration, fps), util::short_time(i.duration)) });
            ui.end_row();
            if i.width > 0 {
                lbl(ui, "Picture");
                ui.label(format!("{}×{}{}{}", i.width, i.height, if i.fps > 0.0 { format!(" · {} fps", i.fps) } else { String::new() }, if i.rotation != 0 { format!(" · rotated {}°", i.rotation) } else { String::new() }));
                ui.end_row();
            }
            if i.has_audio {
                lbl(ui, "Sound");
                ui.label(format!("{} ch · {} Hz", i.audio_channels, i.audio_rate));
                ui.end_row();
            }
            if !i.codec.is_empty() {
                lbl(ui, "Codec");
                ui.label(&i.codec);
                ui.end_row();
            }
            lbl(ui, "Used");
            let n = p.clips.iter().filter(|c| matches!(&c.source, aiframes_core::Source::Media { asset, .. } if *asset == m.id)).count();
            ui.label(format!("{n} clip(s) on the timeline"));
            ui.end_row();
            lbl(ui, "Notes");
            if let Some(v) = self.fields.text(ui, &format!("{}.notes", m.id), &m.notes, true, 190.0, "what is in it") {
                ops.push(Op::SetMedia { asset: m.id.clone(), name: None, notes: Some(v), tags: None });
            }
            ui.end_row();
            lbl(ui, "Tags");
            if let Some(v) = self.fields.text(ui, &format!("{}.tags", m.id), &m.tags.join(", "), false, 190.0, "comma, separated") {
                ops.push(Op::SetMedia { asset: m.id.clone(), name: None, notes: None, tags: Some(util::parse_tags(&v)) });
            }
            ui.end_row();
            lbl(ui, "Transcript");
            ui.horizontal(|ui| match p.transcripts.get(&m.id) {
                Some(t) => {
                    ui.label(format!("{} words{}", t.words.len(), if t.language.is_empty() { String::new() } else { format!(" · {}", t.language) }));
                    if ui.small_button("Re-transcribe").clicked() {
                        out.push(UiAction::Transcribe(m.id.clone()));
                    }
                }
                None if m.info.has_audio || m.kind == MediaKind::Audio => {
                    if ui.button("Transcribe").on_hover_text("Run Whisper for word timings (text-based editing, captions, word anchors)").clicked() {
                        out.push(UiAction::Transcribe(m.id.clone()));
                    }
                }
                None => {
                    ui.label(RichText::new("no sound").weak());
                }
            });
            ui.end_row();
        });
        ui.horizontal(|ui| {
            if ui.button("Add at playhead").clicked() {
                out.push(UiAction::Apply { label: format!("Add {}", util::media_name(m)), ops: crate::edits::add_media_at_ops(p, cx.resolved, &m.id, cx.playhead) });
            }
            if ui.button("Reveal").clicked() {
                out.push(UiAction::RevealMedia(m.id.clone()));
            }
            if ui.button("Relink…").clicked() {
                out.push(UiAction::RelinkMedia(m.id.clone()));
            }
        });
        for op in ops {
            out.push(UiAction::Apply { label: format!("Media {}", m.id), ops: vec![op] });
        }
    }

    fn project_panel(&mut self, ui: &mut Ui, cx: &EditCtx, out: &mut Vec<UiAction>) {
        let p = cx.project;
        let r = cx.resolved;
        let s = &p.settings;
        let mut ops: Vec<(String, Op)> = Vec::new();
        ui.horizontal(|ui| {
            style::chip(ui, "Project", style::ACCENT);
            ui.label(RichText::new(format!("{} clips · {} tracks · {} media · {}", p.clips.len(), p.tracks.len(), p.media.len(), util::short_time(r.duration))).small().weak());
        });
        if !r.errors.is_empty() {
            ui.colored_label(style::ERROR, format!("{} unresolved anchor(s)", r.errors.len())).on_hover_text(r.errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>().join("\n"));
        }
        section(ui, "Project", "project", true, |ui| {
            Grid::new("proj").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                lbl(ui, "Name");
                if let Some(v) = self.fields.text(ui, "project.name", &p.name, false, 200.0, "project name") {
                    ops.push(("Project name".into(), Op::SetProject { name: Some(v), notes: None }));
                }
                ui.end_row();
                lbl(ui, "Format");
                let cur = RESOLUTIONS.iter().find(|(_, w, h)| *w == s.width && *h == s.height).map(|(n, _, _)| *n).unwrap_or("Custom");
                let mut pick: Option<(u32, u32)> = None;
                narrow(ui, 170.0, |ui| {
                    ComboBox::from_id_salt("res").width(fitw(ui, 170.0)).truncate().selected_text(cur).show_ui(ui, |ui| {
                        for (n, w, h) in RESOLUTIONS {
                            if ui.selectable_label(cur == *n, format!("{n}  {w}×{h}")).clicked() {
                                pick = Some((*w, *h));
                            }
                        }
                    });
                });
                if let Some((w, h)) = pick {
                    ops.push(("Resolution".into(), S { width: Some(w), height: Some(h), ..Default::default() }.op()));
                }
                ui.end_row();
                lbl(ui, "Size");
                ui.horizontal(|ui| {
                    if let Some(w) = self.fields.num(ui, "project.w", s.width as f64, &NumSpec::drag(16.0, 16384.0, 2.0, " w", 0)) {
                        ops.push(("Width".into(), S { width: Some((w.round() as u32 / 2 * 2).max(2)), ..Default::default() }.op()));
                    }
                    if let Some(h) = self.fields.num(ui, "project.h", s.height as f64, &NumSpec::drag(16.0, 16384.0, 2.0, " h", 0)) {
                        ops.push(("Height".into(), S { height: Some((h.round() as u32 / 2 * 2).max(2)), ..Default::default() }.op()));
                    }
                });
                ui.end_row();
                lbl(ui, "Frame rate");
                ui.horizontal(|ui| {
                    let mut f = s.fps;
                    narrow(ui, 80.0, |ui| {
                        ComboBox::from_id_salt("fps").width(fitw(ui, 80.0)).truncate().selected_text(format!("{}", s.fps)).show_ui(ui, |ui| {
                            for x in FPS {
                                ui.selectable_value(&mut f, *x, format!("{x} fps"));
                            }
                        });
                    });
                    if (f - s.fps).abs() > 1e-9 {
                        ops.push(("Frame rate".into(), S { fps: Some(f), ..Default::default() }.op()));
                    }
                    let mut sr = s.sample_rate;
                    narrow(ui, 80.0, |ui| {
                        ComboBox::from_id_salt("sr").width(fitw(ui, 80.0)).truncate().selected_text(format!("{} Hz", s.sample_rate)).show_ui(ui, |ui| {
                            for x in [44_100u32, 48_000, 96_000] {
                                ui.selectable_value(&mut sr, x, format!("{x} Hz"));
                            }
                        });
                    });
                    if sr != s.sample_rate {
                        ops.push(("Sample rate".into(), S { sample_rate: Some(sr), ..Default::default() }.op()));
                    }
                });
                ui.end_row();
                lbl(ui, "Background");
                if let Some(c) = self.fields.color(ui, "project.bg", s.background) {
                    ops.push(("Background".into(), S { background: Some(c), ..Default::default() }.op()));
                }
                ui.end_row();
                lbl(ui, "Length");
                ui.horizontal(|ui| {
                    let mut fixed = s.duration.is_some();
                    if ui.checkbox(&mut fixed, "Fixed").on_hover_text("Off: the edit ends with its last clip").changed() {
                        ops.push(("Length".into(), S { duration: Some(if fixed { Some(r.content_end().max(1.0)) } else { None }), ..Default::default() }.op()));
                    }
                    if let Some(d) = s.duration {
                        if let Some(v) = self.fields.time(ui, "project.dur", d, s.fps, (1.0 / s.fps)..=1e7) {
                            ops.push(("Length".into(), S { duration: Some(Some(v)), ..Default::default() }.op()));
                        }
                    } else {
                        ui.label(RichText::new(util::timecode(r.duration, s.fps)).monospace().weak());
                    }
                });
                ui.end_row();
                lbl(ui, "Safe areas");
                ui.horizontal(|ui| {
                    if let Some(v) = self.fields.num(ui, "project.ts", s.title_safe, &NumSpec::drag(10.0, 100.0, 0.5, " % title", 0).scaled(Scale::Linear(100.0))) {
                        ops.push(("Title safe".into(), S { title_safe: Some(v), ..Default::default() }.op()));
                    }
                    if let Some(v) = self.fields.num(ui, "project.as", s.action_safe, &NumSpec::drag(10.0, 100.0, 0.5, " % action", 0).scaled(Scale::Linear(100.0))) {
                        ops.push(("Action safe".into(), S { action_safe: Some(v), ..Default::default() }.op()));
                    }
                });
                ui.end_row();
            });
        });
        section(ui, "Brief (notes for collaborators and the AI)", "project", true, |ui| {
            if let Some(v) = self.fields.text(ui, "project.notes", &p.notes, true, ui.available_width().min(420.0), "Goal, audience, style rules, to-do…") {
                ops.push(("Project notes".into(), Op::SetProject { name: None, notes: Some(v) }));
            }
        });
        section(ui, "Export defaults", "project", true, |ui| {
            let e = &p.export;
            Grid::new("export").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                lbl(ui, "Codec");
                let mut codec = e.codec.clone();
                narrow(ui, 120.0, |ui| {
                    ComboBox::from_id_salt("codec").width(fitw(ui, 120.0)).truncate().selected_text(&codec).show_ui(ui, |ui| {
                        for c in ["h264", "hevc", "av1", "prores", "gif", "png_sequence", "wav"] {
                            ui.selectable_value(&mut codec, c.to_string(), c);
                        }
                    });
                });
                if codec != e.codec {
                    ops.push(("Export codec".into(), E { codec: Some(codec), ..Default::default() }.op()));
                }
                ui.end_row();
                lbl(ui, "Quality");
                let mut q = e.quality.clone();
                narrow(ui, 120.0, |ui| {
                    ComboBox::from_id_salt("quality").width(fitw(ui, 120.0)).truncate().selected_text(&q).show_ui(ui, |ui| {
                        for c in ["draft", "standard", "high", "lossless"] {
                            ui.selectable_value(&mut q, c.to_string(), c);
                        }
                    });
                });
                if q != e.quality {
                    ops.push(("Export quality".into(), E { quality: Some(q), ..Default::default() }.op()));
                }
                ui.end_row();
                lbl(ui, "Output size").on_hover_text("The exported frame size; the project frame is fitted into it.");
                let (ow, oh) = e.output_size(&p.settings);
                let cur = EXPORT_SIZES.iter().find(|(_, sz)| match sz {
                    None => e.width.is_none() && e.height.is_none(),
                    Some((w, h)) => e.width == Some(*w) && e.height == Some(*h),
                });
                let mut pick: Option<Option<(u32, u32)>> = None;
                ui.horizontal(|ui| {
                    narrow(ui, 170.0, |ui| {
                        ComboBox::from_id_salt("export_size").width(fitw(ui, 170.0)).truncate().selected_text(cur.map(|c| c.0.to_string()).unwrap_or_else(|| format!("Custom {ow}×{oh}"))).show_ui(ui, |ui| {
                            for (name, sz) in EXPORT_SIZES {
                                if ui.selectable_label(cur.is_some_and(|c| c.0 == *name), *name).clicked() {
                                    pick = Some(*sz);
                                }
                            }
                        });
                    });
                });
                if let Some(sz) = pick {
                    ops.push(("Export size".into(), export_size_op(sz, None)));
                }
                ui.end_row();
                lbl(ui, "");
                ui.horizontal(|ui| {
                    if let Some(w) = self.fields.num(ui, "export.w", ow as f64, &NumSpec::drag(2.0, 16384.0, 2.0, " w", 0)) {
                        ops.push(("Export width".into(), Op::SetExportSize { width: Some(Some((w.round() as u32 / 2 * 2).max(2))), height: Some(Some(oh)), fit: None }));
                    }
                    if let Some(h) = self.fields.num(ui, "export.h", oh as f64, &NumSpec::drag(2.0, 16384.0, 2.0, " h", 0)) {
                        ops.push(("Export height".into(), Op::SetExportSize { width: Some(Some(ow)), height: Some(Some((h.round() as u32 / 2 * 2).max(2))), fit: None }));
                    }
                });
                ui.end_row();
                lbl(ui, "Fit").on_hover_text("How the project frame fills an output of another aspect: contain letterboxes, cover crops, stretch distorts.");
                ui.horizontal(|ui| {
                    let mut fit = e.fit;
                    narrow(ui, 100.0, |ui| {
                        ComboBox::from_id_salt("export_fit").width(fitw(ui, 100.0)).truncate().selected_text(format!("{fit:?}")).show_ui(ui, |ui| {
                            for f in [aiframes_core::Fit::Contain, aiframes_core::Fit::Cover, aiframes_core::Fit::Stretch, aiframes_core::Fit::None] {
                                ui.selectable_value(&mut fit, f, format!("{f:?}"));
                            }
                        });
                    });
                    if fit != e.fit {
                        ops.push(("Export fit".into(), Op::SetExportSize { width: None, height: None, fit: Some(fit) }));
                    }
                    let (pa, oa) = (p.settings.width as f64 / p.settings.height.max(1) as f64, ow as f64 / oh.max(1) as f64);
                    if (pa - oa).abs() / pa > 0.005 {
                        ui.label(RichText::new(match e.fit {
                            aiframes_core::Fit::Contain => "bars (other aspect)",
                            aiframes_core::Fit::Cover => "cropped (other aspect)",
                            _ => "other aspect",
                        }).small().color(crate::style::WARN));
                    }
                });
                ui.end_row();
                lbl(ui, "GPU");
                let mut hw = e.hardware;
                if ui.checkbox(&mut hw, "Use NVENC when available").changed() {
                    ops.push(("Export GPU".into(), E { hardware: Some(hw), ..Default::default() }.op()));
                }
                ui.end_row();
                lbl(ui, "Audio");
                if let Some(v) = self.fields.num(ui, "export.abr", e.audio_bitrate_kbps as f64, &NumSpec::drag(32.0, 1024.0, 2.0, " kbps", 0)) {
                    ops.push(("Audio bitrate".into(), E { audio_bitrate_kbps: Some(v.round() as u32), ..Default::default() }.op()));
                }
                ui.end_row();
                lbl(ui, "Loudness");
                ui.horizontal_wrapped(|ui| {
                    let mut on = e.loudness_lufs.is_some();
                    if ui.checkbox(&mut on, "Normalise").changed() {
                        ops.push(("Loudness".into(), E { loudness_lufs: Some(if on { Some(-14.0) } else { None }), ..Default::default() }.op()));
                    }
                    if let Some(l) = e.loudness_lufs {
                        if let Some(v) = self.fields.num(ui, "export.lufs", l, &NumSpec::drag(-70.0, 0.0, 0.1, " LUFS", 1)) {
                            ops.push(("Loudness".into(), E { loudness_lufs: Some(Some(v)), ..Default::default() }.op()));
                        }
                    }
                    if let Some(v) = self.fields.num(ui, "export.tp", e.true_peak_db, &NumSpec::drag(-20.0, 0.0, 0.1, " dBTP", 1)) {
                        ops.push(("True peak".into(), E { true_peak_db: Some(v), ..Default::default() }.op()));
                    }
                });
                ui.end_row();
                lbl(ui, "File");
                let path = e.path.clone().unwrap_or_default();
                if let Some(v) = self.fields.text(ui, "export.path", &path, false, 200.0, "next to the project") {
                    ops.push(("Export path".into(), E { path: Some(if v.trim().is_empty() { None } else { Some(v.trim().to_string()) }), ..Default::default() }.op()));
                }
                ui.end_row();
            });
        });
        section(ui, &format!("Ducking ({})", p.ducking.len()), "project", true, |ui| {
            let tracks: Vec<&aiframes_core::Track> = p.tracks.iter().collect();
            let name = |id: &str| tracks.iter().find(|t| t.id == id).map(|t| if t.name.is_empty() { format!("{} {}", t.id, t.role) } else { format!("{} {}", t.id, t.name) }).unwrap_or(id.into());
            for d in &p.ducking {
                egui::Frame::new().fill(style::rgb(0x23, 0x24, 0x28)).corner_radius(4).inner_margin(egui::Margin::same(5)).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        let mut target = d.target.clone();
                        narrow(ui, 90.0, |ui| {
                            ComboBox::from_id_salt(("dt", &d.id)).width(fitw(ui, 90.0)).truncate().selected_text(name(&target)).show_ui(ui, |ui| {
                                for t in &tracks {
                                    ui.selectable_value(&mut target, t.id.clone(), name(&t.id));
                                }
                            });
                        });
                        ui.label("ducks under");
                        let mut trigger = d.trigger.clone();
                        narrow(ui, 90.0, |ui| {
                            ComboBox::from_id_salt(("dg", &d.id)).width(fitw(ui, 90.0)).truncate().selected_text(name(&trigger)).show_ui(ui, |ui| {
                                for t in &tracks {
                                    ui.selectable_value(&mut trigger, t.id.clone(), name(&t.id));
                                }
                            });
                        });
                        if target != d.target || trigger != d.trigger {
                            ops.push(("Ducking tracks".into(), Op::SetDucking { ducking: d.id.clone(), target: Some(target), trigger: Some(trigger), amount_db: None, threshold_db: None, attack: None, release: None }));
                        }
                        if ui.small_button("×").on_hover_text("Remove").clicked() {
                            ops.push(("Remove ducking".into(), Op::RemoveDucking { ducking: d.id.clone() }));
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        let fields: [(&str, f64); 4] = [("amount_db", d.amount_db), ("threshold_db", d.threshold_db), ("attack", d.attack), ("release", d.release)];
                        for (f, v) in fields {
                            let (label, help, spec) = ducking_spec(f);
                            let r = ui.scope(|ui| self.fields.num(ui, &format!("{}.{f}", d.id), v, &NumSpec { width: 62.0, ..spec }));
                            r.response.on_hover_text(format!("{label}: {help}"));
                            if let Some(nv) = r.inner {
                                let (mut a, mut th, mut at, mut re) = (None, None, None, None);
                                match f {
                                    "amount_db" => a = Some(nv),
                                    "threshold_db" => th = Some(nv),
                                    "attack" => at = Some(nv),
                                    _ => re = Some(nv),
                                }
                                ops.push(("Ducking".into(), Op::SetDucking { ducking: d.id.clone(), target: None, trigger: None, amount_db: a, threshold_db: th, attack: at, release: re }));
                            }
                        }
                    });
                });
            }
            let voice = p.tracks.iter().find(|t| t.kind == TrackKind::Audio && t.role == "voice");
            let music = p.tracks.iter().find(|t| t.kind == TrackKind::Audio && t.role == "music");
            let (target, trigger) = match (music, voice) {
                (Some(m), Some(v)) => (Some(m.id.clone()), Some(v.id.clone())),
                _ => {
                    let auds: Vec<&aiframes_core::Track> = p.tracks.iter().filter(|t| t.kind == TrackKind::Audio).collect();
                    (auds.get(1).map(|t| t.id.clone()), auds.first().map(|t| t.id.clone()))
                }
            };
            ui.add_enabled_ui(target.is_some() && trigger.is_some(), |ui| {
                if ui.button("+ Add ducking (music under voice)").clicked() {
                    if let (Some(target), Some(trigger)) = (target, trigger) {
                        ops.push(("Add ducking".into(), Op::AddDucking { target, trigger, amount_db: None, threshold_db: None, attack: None, release: None, id: None }));
                    }
                }
            });
        });
        for (l, op) in ops {
            out.push(UiAction::Apply { label: l, ops: vec![op] });
        }
        let _: Option<&Resolved> = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::tests::*;
    use aiframes_core::Source;

    thread_local! {
        /// Body widths of the sections drawn (test instrumentation for the width audit).
        pub(crate) static SECTION_W: std::cell::RefCell<Vec<(String, f32)>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Width the inspector takes in a resizable 330 px right panel (the app's side panel), after a
    /// few frames (grids size their columns from the previous frame).
    pub(crate) fn panel_width(p: &Project, sel: &[Id], playhead: f64) -> f32 {
        let r = aiframes_core::resolve(p);
        let cx = EditCtx {
            project: p,
            resolved: &r,
            playhead,
            playing: false,
            selection: sel,
            preview: &crate::NoPreview,
            ai_highlight: &[],
            ai_flash: 0.0,
            history: &[],
            revision: 0,
            can_undo: false,
            can_redo: false,
            in_point: None,
            out_point: None,
        };
        let ctx = egui::Context::default();
        crate::style::apply_dark(&ctx);
        let mut insp = Inspector::default();
        let mut w = 0.0;
        for _ in 0..4 {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 2400.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                let resp = egui::Panel::right("insp330").resizable(true).default_size(330.0).size_range(260.0..=900.0).show(ui, |ui| {
                    let _ = insp.ui(ui, &cx);
                });
                w = resp.response.rect.width();
            });
            out.textures_delta.clear();
        }
        w
    }

    /// Every section at 330 px with long names, long words, anchors and effects: nothing may push
    /// the panel wider (round 5: the Anchor row made a 360 px panel 388 px wide).
    #[test]
    fn every_section_fits_a_330_px_panel() {
        let long = "An extremely long clip name for the round forty boss fight segment";
        let mut p = demo();
        p.name = "A project with a rather long name for the recap of round forty".into();
        p.notes = "Notes ".repeat(30);
        p.markers.push(aiframes_core::Marker { id: "mk1".into(), time: Time::At(11.0), name: long.into(), kind: MarkerKind::Chapter, color: None, note: "a note ".repeat(20) });
        p.media[0].name = format!("{long}.mp4");
        p.media[0].path = format!("C:/Users/someone/Videos/Projects/2026/{long}/{long}.mp4");
        p.tracks[1].name = long.into();
        let word = "Supercalifragilisticexpialidociously";
        p.transcripts.insert("ma".into(), aiframes_core::Transcript { language: "en".into(), words: (0..40).map(|i| aiframes_core::Word { text: format!(" {word}{i}"), start: i as f64 * 0.5, end: i as f64 * 0.5 + 0.4, conf: 1.0 }).collect() });
        for c in p.clips.iter_mut() {
            c.name = format!("{long} {}", c.id);
        }
        let mut t1 = text("t1", "v2", Time::At(10.5), 3.0);
        t1.name = long.into();
        if let Source::Text(t) = &mut t1.source {
            t.text = format!("{long}\n{long}");
            t.font = "A Very Long Font Family Name Display Condensed".into();
        }
        p.clips.push(t1);
        let r = aiframes_core::resolve(&p);
        let ops = vec![
            Op::MoveClip { clip: "t1".into(), start: clip::anchor_keeping_place(&p, &r, "t1", clip::AnchorKind::After, Some("c1")).unwrap(), track: None, overwrite: false },
            clip::set_until_op("t1", clip::until_keeping_place(&p, &r, "t1", clip::EndKind::After, Some("c2")).unwrap()),
            Op::AddEffect { clip: "c1".into(), effect: aiframes_core::EffectArg::Kind("color".into()), index: None },
            Op::AddEffect { clip: "c1".into(), effect: aiframes_core::EffectArg::Kind("blur".into()), index: None },
            Op::AddEffect { clip: "c1".into(), effect: aiframes_core::EffectArg::Kind("vignette".into()), index: None },
            Op::AddTransition { from: "c1".into(), to: "c2".into(), kind: "film_burn".into(), duration: Some(0.5), id: Some("tr1".into()), params: Default::default() },
            // Round 6: the look tools on c2 (overlapping t1, its matte).
            crate::edits::motion_blur_op("c2", Some(&aiframes_core::MotionBlur::default())),
            crate::edits::frame_blend_op("c2", true),
            crate::edits::matte_op("c2", Some(("t1", aiframes_core::MatteMode::LumaInverted))),
            Op::AddKeyframe { clip: "c2".into(), path: "transform.rotation_y".into(), t: 0.0, v: -90.0, ease: None },
            Op::AddKeyframe { clip: "c2".into(), path: "transform.rotation_y".into(), t: 1.0, v: 0.0, ease: None },
            Op::AddDucking { target: "a2".into(), trigger: "a1".into(), amount_db: Some(-12.0), threshold_db: None, attack: None, release: None, id: Some("d1".into()) },
        ];
        let (mut p, r) = run(&p, &ops);
        // A word-anchored title (start and end on long words) and a shape.
        let mut t2 = text("t2", "v2", Time::At(16.0), 2.0);
        t2.name = long.into();
        p.clips.push(t2);
        p.clips.push(clip("s1", "v2", Time::At(19.0), 1.0, Source::Shape(aiframes_core::ShapeSource { kind: aiframes_core::ShapeKind::Rect, width: 100.0, height: 50.0, fill: None, stroke: None, radius: 0.0, draw: aiframes_core::Param::Value(1.0) })));
        let r2 = aiframes_core::resolve(&p);
        let ops = vec![
            Op::MoveClip { clip: "t2".into(), start: clip::anchor_keeping_place(&p, &r2, "t2", clip::AnchorKind::Word, Some("c3")).unwrap(), track: None, overwrite: false },
            clip::set_until_op("t2", clip::until_keeping_place(&p, &r2, "t2", clip::EndKind::Word, Some("c3")).unwrap()),
        ];
        let (p, _) = run(&p, &ops);
        let _ = r;
        let sels: Vec<Vec<Id>> = vec![
            vec!["t1".into()],
            vec!["c2".into()],
            vec!["t2".into()],
            vec!["c1".into()],
            vec!["c3".into()],
            vec!["s1".into()],
            vec!["c1".into(), "c2".into(), "c3".into()],
            vec!["tr1".into()],
            vec!["mk1".into()],
            vec!["v2".into()],
            vec!["mv".into()],
            vec![],
        ];
        let mut bad = Vec::new();
        for sel in &sels {
            SECTION_W.with(|w| w.borrow_mut().clear());
            let w = panel_width(&p, sel, 12.0);
            if w > 331.0 {
                let widest: Vec<String> = SECTION_W.with(|s| {
                    let mut v = s.borrow().clone();
                    v.sort_by(|a, b| b.1.total_cmp(&a.1));
                    v.dedup_by(|a, b| a.0 == b.0);
                    v.iter().take(3).map(|(t, w)| format!("{t} {w:.0}")).collect()
                });
                bad.push(format!("{sel:?}: {w:.0} px (widest sections: {widest:?})"));
            }
        }
        assert!(bad.is_empty(), "the inspector grew the 330 px panel: {bad:?}");
    }

    #[test]
    fn selection_kinds() {
        let mut p = demo();
        p.markers.push(aiframes_core::Marker { id: "mk1".into(), time: Time::At(1.0), name: String::new(), kind: MarkerKind::Marker, color: None, note: String::new() });
        assert!(matches!(selected(&p, &[]), Selected::Nothing));
        assert!(matches!(selected(&p, &["c1".into()]), Selected::Clip(c) if c.id == "c1"));
        assert!(matches!(selected(&p, &["mk1".into(), "c1".into(), "c2".into()]), Selected::Clips(v) if v.len() == 2));
        assert!(matches!(selected(&p, &["mk1".into()]), Selected::Marker(_)));
        assert!(matches!(selected(&p, &["v1".into()]), Selected::Track(_)));
        assert!(matches!(selected(&p, &["mv".into()]), Selected::Media(_)));
        assert!(matches!(selected(&p, &["zzz".into()]), Selected::Nothing));
        // A clip with its detached audio edits as the clip.
        let r = aiframes_core::resolve(&p);
        let (q, _) = run(&p, &crate::edits::detach_audio_ops(&p, &r, "c1"));
        let audio = q.clips.iter().find(|c| c.track == "a2").unwrap().id.clone();
        assert!(matches!(selected(&q, &["c1".into(), audio.clone()]), Selected::Clip(c) if c.id == "c1"));
        assert!(matches!(selected(&q, &[audio.clone(), "c1".into()]), Selected::Clip(c) if c.id == "c1"));
        assert!(matches!(selected(&q, &["c2".into(), audio]), Selected::Clips(_)));
    }

    #[test]
    fn shared_values_and_export_sizes() {
        let p = demo();
        let ids: Vec<Id> = vec!["c1".into(), "c2".into(), "c3".into()];
        // Opacity applies to the two picture clips, not the audio clip.
        let ops = shared_value_ops(&p, &ids, "opacity", 0.5);
        assert_eq!(ops.len(), 2);
        let (q, _) = run(&p, &ops);
        assert!(q.clips.iter().filter(|c| c.id != "c3").all(|c| c.opacity == aiframes_core::Param::Value(0.5)));
        // Volume applies to all three (video with sound + audio); one op each.
        assert_eq!(shared_value_ops(&p, &ids, "volume", 0.5).len(), 3);
        // Speed keeps the source range: c1 (10 s) becomes 5 s at 2×.
        let (_, rq) = run(&p, &shared_value_ops(&p, &ids, "speed", 2.0));
        assert!((rq.span("c1").unwrap().duration() - 5.0).abs() < 1e-9);
        // Fades are clamped to each clip and skipped where already equal.
        let (q, _) = run(&p, &shared_value_ops(&p, &ids, "fade_in", 7.0));
        assert_eq!(q.clips.iter().find(|c| c.id == "c2").unwrap().fade_in, 5.0);
        assert!(shared_value_ops(&q, &ids, "fade_in", 7.0).len() < 3);
        // Export sizes.
        let (q, _) = run(&p, &[export_size_op(Some((1080, 1920)), Some(aiframes_core::Fit::Cover))]);
        assert_eq!(q.export.output_size(&q.settings), (1080, 1920));
        assert_eq!(q.export.fit, aiframes_core::Fit::Cover);
        let (q, _) = run(&q, &[export_size_op(None, None)]);
        assert_eq!(q.export.output_size(&q.settings), (1920, 1080));
        // Registry-driven fields.
        let (label, _, spec) = clip_spec("transform.scale");
        assert_eq!(label, "Scale");
        assert_eq!(spec.max, 5000.0);
        assert_eq!(clip_spec("volume").2.scale, Scale::Db);
        assert_eq!(ducking_spec("attack").2.suffix, " s");
    }

    #[test]
    fn settings_ops_apply() {
        let p = demo();
        let ops = vec![
            S { width: Some(1080), height: Some(1920), ..Default::default() }.op(),
            S { duration: Some(Some(30.0)), ..Default::default() }.op(),
            E { loudness_lufs: Some(None), codec: Some("hevc".into()), ..Default::default() }.op(),
            Op::AddDucking { target: "a2".into(), trigger: "a1".into(), amount_db: None, threshold_db: None, attack: None, release: None, id: None },
        ];
        let (q, r) = run(&p, &ops);
        assert_eq!((q.settings.width, q.settings.height), (1080, 1920));
        assert_eq!(r.duration, 30.0);
        assert_eq!(q.export.codec, "hevc");
        assert_eq!(q.ducking.len(), 1);
    }
}
