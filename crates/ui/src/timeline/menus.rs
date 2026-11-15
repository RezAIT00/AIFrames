//! Context menus (clip, transition, empty lane, below the tracks, track header, marker, ruler) and
//! the inline rename popup. Every item emits ops (or app actions).

use super::*;
use aiframes_core::{effects::TRANSITIONS, Color, MarkerKind, Source, Time};
use egui::RichText;
use serde_json::json;

fn apply(out: &mut Vec<UiAction>, label: impl Into<String>, ops: Vec<Op>) {
    if ops.is_empty() {
        out.push(UiAction::Status("Nothing to do.".into()));
    } else {
        out.push(UiAction::Apply { label: label.into(), ops });
    }
}

/// Applies the ops when the core accepts them on a copy, else says why in the status bar.
fn apply_checked(out: &mut Vec<UiAction>, cx: &EditCtx, label: impl Into<String>, ops: Vec<Op>) {
    let label = label.into();
    match simulate(cx.project, cx.resolved, &ops) {
        Ok(_) => out.push(UiAction::Apply { label, ops }),
        Err(e) => out.push(UiAction::Status(format!("{label}: {e}"))),
    }
}

/// "Set spacing…" and "Close gaps" for a track (track header and lane menus).
fn spacing_items(tl: &mut Timeline, ui: &mut Ui, cx: &EditCtx, track: &str, pos: Pos2, out: &mut Vec<UiAction>) {
    let n = cx.project.clips.iter().filter(|c| c.track == track).count();
    if item_enabled(ui, n >= 2, "Set spacing…", "") {
        tl.open_spacing_at(cx, track, pos);
    }
    if item_enabled(ui, n >= 2, "Close gaps", "") {
        apply_checked(out, cx, format!("Close gaps on {track}"), vec![Op::CloseGaps { track: track.into(), max_gap: None }]);
    }
}

fn item(ui: &mut Ui, text: &str, shortcut: &str) -> bool {
    let r = ui.add(egui::Button::new(text).shortcut_text(RichText::new(shortcut).weak()));
    if r.clicked() {
        ui.close();
        true
    } else {
        false
    }
}

fn item_enabled(ui: &mut Ui, enabled: bool, text: &str, shortcut: &str) -> bool {
    let r = ui.add_enabled(enabled, egui::Button::new(text).shortcut_text(RichText::new(shortcut).weak()));
    if r.clicked() {
        ui.close();
        true
    } else {
        false
    }
}

pub(super) fn context_menus(tl: &mut Timeline, _ui: &Ui, cx: &EditCtx, lay: &Layout, lanes: &Response, ruler: &Response, out: &mut Vec<UiAction>) {
    let target = tl.menu.clone();
    lanes.context_menu(|ui| {
        ui.set_min_width(200.0);
        match &target {
            Some(MenuTarget::Clip(id)) => clip_menu(tl, ui, cx, lay, id, out),
            Some(MenuTarget::Trans(id)) => transition_menu(ui, cx, id, out),
            Some(MenuTarget::Row { track, t }) => lane_menu(tl, ui, cx, Some(track), *t, out),
            Some(MenuTarget::Below { t }) => lane_menu(tl, ui, cx, None, *t, out),
            Some(MenuTarget::Key { clip, path, t }) => key_menu(tl, ui, cx, clip, path, *t, out),
            _ => {
                ui.close();
            }
        }
    });
    ruler.context_menu(|ui| {
        ui.set_min_width(180.0);
        match &target {
            Some(MenuTarget::Marker(id)) => marker_menu(tl, ui, cx, id, out),
            Some(MenuTarget::Ruler { t }) => ruler_menu(ui, cx, *t, out),
            _ => {
                ui.close();
            }
        }
    });
}

fn clip_menu(tl: &mut Timeline, ui: &mut Ui, cx: &EditCtx, lay: &Layout, id: &str, out: &mut Vec<UiAction>) {
    let p = cx.project;
    let r = cx.resolved;
    let Some(c) = p.clips.iter().find(|c| c.id == id) else {
        ui.close();
        return;
    };
    let label = util::clip_label(p, c);
    ui.label(RichText::new(format!("{label}  ·  {id}")).small().weak());
    // Mechanical fixes for what validation found on this clip.
    let all = util::findings(p, r, cx.revision);
    let fixes: Vec<&aiframes_core::Finding> = util::clip_findings(&all, id).filter(|f| !f.fix_ops.is_empty()).collect();
    for f in &fixes {
        let text = format!("Fix: {}", util::ellipsize(f.fix.as_deref().unwrap_or(f.code), 60));
        let resp = ui.add(egui::Button::new(RichText::new(text).color(crate::style::severity_color(f.severity))));
        if resp.clicked() {
            ui.close();
            apply(out, format!("Fix {} ({id})", f.code), f.fix_ops.clone());
        } else {
            resp.on_hover_text(util::plain(&f.message));
        }
    }
    if !fixes.is_empty() {
        ui.separator();
    }
    let span = r.span(id);
    let sel: Vec<Id> = if cx.is_selected(id) { cx.selection.to_vec() } else { tl.ids_with_links(cx, id) };
    let inside = span.is_some_and(|s| cx.playhead > s.start + 1e-3 && cx.playhead < s.end - 1e-3);
    if item_enabled(ui, inside, "Split at playhead", "S") {
        apply(out, format!("Split {id}"), edits::split_ops(p, r, Some(&[id.to_string()]), cx.playhead));
    }
    if item(ui, "Delete", "Del") {
        apply(out, "Delete", edits::delete_ops(p, &sel, false));
    }
    if item(ui, "Ripple delete", "Shift+Del") {
        apply(out, "Ripple delete", edits::delete_ops(p, &sel, true));
    }
    if item(ui, "Duplicate", "") {
        let start = span.filter(|s| edits::track_free(p, r, &c.track, s.end, s.end + c.duration, &[])).map(|s| Time::At(s.end));
        apply(out, format!("Duplicate {id}"), vec![Op::DuplicateClip { clip: id.into(), start, track: None, new_id: None, overwrite: false }]);
    }
    let kind = util::clip_kind(p, c);
    let media = matches!(c.source, Source::Media { .. });
    if kind == ClipKind::Video && util::has_sound(p, c) {
        let detached = !aiframes_core::linked_partners(p, id).is_empty();
        if item_enabled(ui, !detached, if detached { "Audio detached" } else { "Detach audio" }, "") {
            apply(out, format!("Detach audio of {id}"), edits::detach_audio_ops(p, r, id));
        }
    }
    ui.separator();
    if media && kind != ClipKind::Image {
        ui.menu_button("Speed", |ui| {
            for (txt, s) in [("Slow motion 0.25×", 0.25), ("Slow motion 0.5×", 0.5), ("Normal 1×", 1.0)] {
                if item(ui, txt, "") {
                    apply(out, format!("{id} at {s}×"), vec![edits::speed_op(id, s)]);
                }
            }
            ui.separator();
            for s in [1.5, 2.0, 4.0, 8.0, 16.0] {
                if item(ui, &format!("Fast-forward {s}×"), "") {
                    apply(out, format!("Fast-forward {id} {s}×"), vec![edits::speed_op(id, s)]);
                }
            }
            ui.separator();
            if item(ui, if c.reverse { "Play forwards" } else { "Reverse" }, "") {
                apply(out, format!("Reverse {id}"), vec![Op::SetClip { clip: id.into(), patch: json!({"reverse": !c.reverse}) }]);
            }
            if kind == ClipKind::Video && item_enabled(ui, inside, "Freeze frame at playhead (2 s)", "") {
                apply(out, format!("Freeze frame of {id}"), vec![Op::FreezeFrame { clip: id.into(), at: cx.playhead, duration: 2.0, new_id: None }]);
            }
        });
    }
    let next = edits::neighbour(p, r, id, true);
    ui.add_enabled_ui(next.is_some(), |ui| {
        ui.menu_button("Add transition to next", |ui| {
            for t in TRANSITIONS {
                if item(ui, t.label, "") {
                    if let Some(op) = edits::add_transition_op(p, r, id, t.kind) {
                        apply(out, format!("{} {id}", t.label), vec![op]);
                    }
                }
            }
        });
    });
    if item(ui, if c.enabled { "Disable" } else { "Enable" }, "") {
        let ops: Vec<Op> = sel.iter().filter(|s| p.clips.iter().any(|k| &k.id == *s)).map(|s| Op::SetClip { clip: s.clone(), patch: json!({"enabled": !c.enabled}) }).collect();
        apply(out, if c.enabled { "Disable clip" } else { "Enable clip" }, ops);
    }
    if util::has_sound(p, c) && item(ui, if c.muted { "Unmute clip" } else { "Mute clip" }, "") {
        apply(out, format!("Mute {id}"), vec![Op::SetClip { clip: id.into(), patch: json!({"muted": !c.muted}) }]);
    }
    if item(ui, "Rename…", "") {
        let pos = lay.boxes.iter().find(|b| b.id == id).map(|b| b.rect.left_top()).unwrap_or(lay.lanes.center());
        tl.rename = Some(Rename { target: RenameTarget::Clip(id.into()), text: c.name.clone(), pos: pos2(pos.x.max(lay.lanes.left()), pos.y), opened: false });
    }
    ui.menu_button("Keyframes", |ui| {
        ui.label(RichText::new("Show a parameter's keyframe lane on the clip (Alt+click the line adds a key)").small().weak());
        let animated: Vec<String> = aiframes_core::clip_params(c).into_iter().filter(|(_, p)| p.is_animated()).map(|(p, _)| p).collect();
        let mut paths = aiframes_core::param_paths(c);
        paths.sort_by_key(|p| !animated.contains(p));
        let shown = tl.lane_pref.get(id).cloned();
        for path in paths {
            let n = match util::param_at(c, &path) {
                Some(aiframes_core::Param::Animated { keys }) => format!("  ·  {} keys", keys.len()),
                _ => String::new(),
            };
            if ui.selectable_label(shown.as_deref() == Some(path.as_str()), format!("{}{n}", util::param_label(c, &path))).clicked() {
                tl.lane_pref.insert(id.to_string(), path.clone());
                ui.close();
            }
        }
    });
    // End anchors (`until`): overlays that follow their base clip's length.
    if let Some(base) = crate::inspector::with_base(p, c) {
        let op = crate::inspector::match_length_op(p, c);
        let text = format!("Match length of {}", util::ellipsize(&util::clip_label(p, base), 28));
        let resp = ui.add_enabled(op.is_some(), egui::Button::new(text));
        if resp.clicked() {
            ui.close();
            if let Some(op) = op {
                apply_checked(out, cx, format!("Match length of {} ({id})", base.id), vec![op]);
            }
        } else {
            resp.on_hover_text(format!("End when {} ends (follows its speed changes and trims): set_until after {}", base.id, base.id));
        }
    }
    if let Some(u) = &c.until {
        if item(ui, &format!("Fixed length (drop “until {}”)", util::ellipsize(&aiframes_core::until_text(u), 24)), "") {
            apply(out, format!("Fixed length ({id})"), vec![crate::inspector::set_until_op(id, None)]);
        }
    }
    ui.menu_button("Anchor", |ui| {
        let anchored = matches!(c.start, Time::Anchored(_));
        if item_enabled(ui, anchored && span.is_some(), "Make start absolute", "") {
            if let Some(s) = span {
                apply(out, format!("Unanchor {id}"), vec![Op::MoveClip { clip: id.into(), start: Time::At(s.start), track: None, overwrite: false }]);
            }
        }
        let prev = edits::neighbour(p, r, id, false);
        if item_enabled(ui, prev.is_some(), "Chain after the previous clip (magnetic)", "") {
            if let Some(pv) = prev {
                apply(out, format!("Chain {id} after {pv}"), vec![Op::MoveClip { clip: id.into(), start: Time::Anchored(aiframes_core::Anchor { to: aiframes_core::AnchorTarget::After(pv), offset: 0.0 }), track: None, overwrite: false }]);
            }
        }
    });
    ui.separator();
    if item(ui, "Copy id", "") {
        ui.ctx().copy_text(id.to_string());
    }
    if item(ui, "Ask AI about this clip…", "") {
        out.push(UiAction::AskAi { prompt: format!("About clip {id} (\"{label}\"): "), ids: vec![id.to_string()] });
    }
}

#[allow(clippy::too_many_arguments)]
fn key_menu(tl: &mut Timeline, ui: &mut Ui, cx: &EditCtx, clip: &str, path: &str, t: f64, out: &mut Vec<UiAction>) {
    let p = cx.project;
    let Some(c) = p.clips.iter().find(|c| c.id == clip) else {
        ui.close();
        return;
    };
    let key = match util::param_at(c, path) {
        Some(aiframes_core::Param::Animated { keys }) => keys.iter().find(|k| (k.t - t).abs() < 1e-4).cloned(),
        _ => None,
    };
    let Some(key) = key else {
        ui.close();
        return;
    };
    let fps = util::fps(p);
    let start = cx.resolved.span(clip).map(|s| s.start).unwrap_or(0.0);
    ui.label(RichText::new(format!("{} · {} · {}", util::param_label(c, path), keys::format_value(util::param_spec(c, path), path, key.v), util::timecode(start + key.t, fps))).small().weak());
    ui.label(RichText::new("Ease to the next key").small());
    for (name, ease) in keys::EASES {
        if ui.selectable_label(key.ease == *ease, *name).clicked() {
            ui.close();
            apply(out, format!("Keyframe ease {name} ({clip})"), vec![keys::ease_op(clip, path, &key, ease.clone())]);
        }
    }
    ui.separator();
    if item(ui, "Go to keyframe", "") {
        out.push(UiAction::Seek(start + key.t));
    }
    if item(ui, "Delete keyframe", "Del") {
        tl.key_sel = None;
        apply(out, format!("Remove keyframe ({clip})"), vec![Op::RemoveKeyframe { clip: clip.into(), path: path.into(), t: key.t }]);
    }
}

fn transition_menu(ui: &mut Ui, cx: &EditCtx, id: &str, out: &mut Vec<UiAction>) {
    let Some(t) = cx.project.transitions.iter().find(|t| t.id == id) else {
        ui.close();
        return;
    };
    ui.label(RichText::new(format!("{} · {:.2} s", t.kind, t.duration)).small().weak());
    ui.menu_button("Kind", |ui| {
        for s in TRANSITIONS {
            if ui.selectable_label(s.kind == t.kind, s.label).clicked() {
                ui.close();
                apply(out, format!("Transition {id}: {}", s.label), vec![Op::SetTransition { transition: id.into(), kind: Some(s.kind.into()), duration: None, params: Default::default() }]);
            }
        }
    });
    ui.menu_button("Duration", |ui| {
        let max = drag::transition_max(cx.resolved, &t.from, &t.to);
        for d in [0.25, 0.5, 1.0, 2.0] {
            if item_enabled(ui, d <= max + 1e-9, &format!("{d} s"), "") {
                apply(out, format!("Transition {id} {d} s"), vec![Op::SetTransition { transition: id.into(), kind: None, duration: Some(d), params: Default::default() }]);
            }
        }
    });
    if item(ui, "Remove transition", "Del") {
        apply(out, format!("Remove transition {id}"), vec![Op::RemoveTransition { transition: id.into() }]);
    }
}

fn lane_menu(tl: &mut Timeline, ui: &mut Ui, cx: &EditCtx, track: Option<&Id>, t: f64, out: &mut Vec<UiAction>) {
    let p = cx.project;
    let r = cx.resolved;
    let fps = util::fps(p);
    let t = util::quantize(t.max(0.0), fps);
    ui.label(RichText::new(util::timecode(t, fps)).small().weak());
    // Generators go at the playhead, on the clicked video track when it is free there.
    let video_track = track.filter(|id| p.tracks.iter().any(|tr| &tr.id == *id && tr.kind == TrackKind::Video)).map(String::as_str);
    let ph = util::quantize(cx.playhead.max(0.0), fps);
    if item(ui, "Add text at playhead", "") {
        apply(out, "Add text", edits::generator_ops(p, r, "text", ph, video_track));
    }
    if item(ui, "Add solid at playhead", "") {
        apply(out, "Add solid", edits::generator_ops(p, r, "solid", ph, video_track));
    }
    if item(ui, "Add adjustment layer at playhead", "") {
        apply(out, "Add adjustment layer", edits::generator_ops(p, r, "adjustment", ph, video_track));
    }
    if item(ui, "Add marker here", "M") {
        apply(out, "Add marker", vec![Op::AddMarker { time: Time::At(t), name: None, kind: None, id: None, color: None, note: None }]);
    }
    if let Some(track) = track {
        ui.separator();
        let pos = ui.ctx().input(|i| i.pointer.interact_pos()).unwrap_or_default();
        spacing_items(tl, ui, cx, track, pos, out);
    }
    ui.separator();
    if item(ui, "Add video track", "") {
        apply(out, "Add video track", vec![Op::AddTrack { id: None, kind: TrackKind::Video, name: None, role: None, index: None }]);
    }
    if item(ui, "Add audio track", "") {
        apply(out, "Add audio track", vec![Op::AddTrack { id: None, kind: TrackKind::Audio, name: None, role: None, index: None }]);
    }
    if item(ui, "Select all clips", "Ctrl+A") {
        out.push(UiAction::Select(p.clips.iter().map(|c| c.id.clone()).collect()));
    }
}

fn marker_menu(tl: &mut Timeline, ui: &mut Ui, cx: &EditCtx, id: &str, out: &mut Vec<UiAction>) {
    let Some(m) = cx.project.markers.iter().find(|m| m.id == id) else {
        ui.close();
        return;
    };
    ui.label(RichText::new(if m.name.is_empty() { id.to_string() } else { m.name.clone() }).small().weak());
    if item(ui, "Rename…", "") {
        let x = cx.resolved.marker(id).unwrap_or(0.0);
        let _ = x;
        tl.rename = Some(Rename { target: RenameTarget::Marker(id.into()), text: m.name.clone(), pos: ui.ctx().input(|i| i.pointer.hover_pos()).unwrap_or_default(), opened: false });
    }
    ui.menu_button("Kind", |ui| {
        for (k, l) in [(MarkerKind::Marker, "Marker"), (MarkerKind::Chapter, "Chapter (YouTube)"), (MarkerKind::Beat, "Beat"), (MarkerKind::Todo, "To do")] {
            if ui.selectable_label(m.kind == k, l).clicked() {
                ui.close();
                apply(out, format!("Marker {id} kind"), vec![Op::SetMarker { marker: id.into(), time: None, name: None, kind: Some(k), color: None, note: None }]);
            }
        }
    });
    ui.menu_button("Colour", |ui| {
        let colors: [(&str, Option<&str>); 7] = [("Default", None), ("Red", Some("#FF453A")), ("Orange", Some("#FF9F0A")), ("Yellow", Some("#FFD60A")), ("Green", Some("#30D158")), ("Blue", Some("#0A84FF")), ("Purple", Some("#BF5AF2"))];
        for (name, hex) in colors {
            let c = hex.and_then(Color::parse);
            if ui.selectable_label(m.color == c, name).clicked() {
                ui.close();
                apply(out, format!("Marker {id} colour"), vec![Op::SetMarker { marker: id.into(), time: None, name: None, kind: None, color: Some(c), note: None }]);
            }
        }
    });
    if item(ui, "Go to", "") {
        if let Some(t) = cx.resolved.marker(id) {
            out.push(UiAction::Seek(t));
        }
    }
    if item(ui, "Delete marker", "Del") {
        apply(out, format!("Remove marker {id}"), vec![Op::RemoveMarker { marker: id.into() }]);
    }
    if item(ui, "Ask AI about this marker…", "") {
        out.push(UiAction::AskAi { prompt: format!("About marker {id} (\"{}\"): ", m.name), ids: vec![id.into()] });
    }
}

fn ruler_menu(ui: &mut Ui, cx: &EditCtx, t: f64, out: &mut Vec<UiAction>) {
    let fps = util::fps(cx.project);
    ui.label(RichText::new(util::timecode(t, fps)).small().weak());
    if item(ui, "Add marker here", "M") {
        apply(out, "Add marker", vec![Op::AddMarker { time: Time::At(t), name: None, kind: None, id: None, color: None, note: None }]);
    }
    if item(ui, "Add chapter here", "Shift+M") {
        let n = cx.project.markers.iter().filter(|m| m.kind == MarkerKind::Chapter).count() + 1;
        apply(out, "Add chapter", vec![Op::AddMarker { time: Time::At(t), name: Some(format!("Chapter {n}")), kind: Some(MarkerKind::Chapter), id: None, color: None, note: None }]);
    }
    ui.separator();
    if item(ui, "Set in here", "I") {
        out.push(UiAction::SetIn(Some(t)));
    }
    if item(ui, "Set out here", "O") {
        out.push(UiAction::SetOut(Some(t)));
    }
    if item_enabled(ui, cx.in_point.is_some() || cx.out_point.is_some(), "Clear in/out", "Alt+I / Alt+O") {
        out.push(UiAction::SetIn(None));
        out.push(UiAction::SetOut(None));
    }
    if let (Some(a), Some(b)) = (cx.in_point, cx.out_point) {
        if b > a {
            if item(ui, "Lift in–out (leave a gap)", "") {
                apply(out, "Lift in–out", vec![Op::LiftRange { from: a, to: b, tracks: None }]);
            }
            if item(ui, "Extract in–out (ripple)", "") {
                apply(out, "Extract in–out", vec![Op::ExtractRange { from: a, to: b, tracks: None }]);
            }
        }
    }
}

/// The track header menu (called from the header's own response).
pub(super) fn track_menu(tl: &mut Timeline, ui: &mut Ui, cx: &EditCtx, id: &str, header_pos: Pos2, out: &mut Vec<UiAction>) {
    let p = cx.project;
    let Some(ti) = p.tracks.iter().position(|t| t.id == id) else {
        ui.close();
        return;
    };
    let t = &p.tracks[ti];
    ui.set_min_width(200.0);
    ui.label(RichText::new(format!("{} · {}", id, if t.kind == TrackKind::Video { "video" } else { "audio" })).small().weak());
    if item(ui, "Rename…", "") {
        tl.rename = Some(Rename { target: RenameTarget::Track(id.into()), text: t.name.clone(), pos: header_pos, opened: false });
    }
    let set = |muted: Option<bool>, hidden: Option<bool>, locked: Option<bool>, solo: Option<bool>, role: Option<String>| Op::SetTrack { track: id.into(), name: None, role, muted, hidden, locked, solo, volume: None };
    ui.menu_button("Role", |ui| {
        for role in ["main", "overlay", "captions", "voice", "music", "sfx", ""] {
            if ui.selectable_label(t.role == role, if role.is_empty() { "(none)" } else { role }).clicked() {
                ui.close();
                apply(out, format!("Track {id} role"), vec![set(None, None, None, None, Some(role.into()))]);
            }
        }
    });
    if item(ui, if t.muted { "Unmute" } else { "Mute" }, "") {
        apply(out, format!("Mute {id}"), vec![set(Some(!t.muted), None, None, None, None)]);
    }
    if t.kind == TrackKind::Audio && item(ui, if t.solo { "Unsolo" } else { "Solo" }, "") {
        apply(out, format!("Solo {id}"), vec![set(None, None, None, Some(!t.solo), None)]);
    }
    if t.kind == TrackKind::Video && item(ui, if t.hidden { "Show" } else { "Hide" }, "") {
        apply(out, format!("Hide {id}"), vec![set(None, Some(!t.hidden), None, None, None)]);
    }
    if item(ui, if t.locked { "Unlock" } else { "Lock" }, "") {
        apply(out, format!("Lock {id}"), vec![set(None, None, Some(!t.locked), None, None)]);
    }
    ui.separator();
    // Up/down among the rows of the same kind (as displayed), whatever the order in project.tracks.
    let (up, down) = track_up_down(p, &tl.track_heights, id);
    if item_enabled(ui, up.is_some(), "Move up", "") {
        if let Some(op) = up {
            apply(out, format!("Move track {id} up"), vec![op]);
        }
    }
    if item_enabled(ui, down.is_some(), "Move down", "") {
        if let Some(op) = down {
            apply(out, format!("Move track {id} down"), vec![op]);
        }
    }
    if t.kind == TrackKind::Video {
        if item(ui, "Add video track above", "") {
            apply(out, "Add video track", vec![Op::AddTrack { id: None, kind: TrackKind::Video, name: None, role: None, index: Some(ti + 1) }]);
        }
        if item(ui, "Add video track below", "") {
            apply(out, "Add video track", vec![Op::AddTrack { id: None, kind: TrackKind::Video, name: None, role: None, index: Some(ti) }]);
        }
    } else {
        if item(ui, "Add audio track above", "") {
            apply(out, "Add audio track", vec![Op::AddTrack { id: None, kind: TrackKind::Audio, name: None, role: None, index: Some(ti) }]);
        }
        if item(ui, "Add audio track below", "") {
            apply(out, "Add audio track", vec![Op::AddTrack { id: None, kind: TrackKind::Audio, name: None, role: None, index: Some(ti + 1) }]);
        }
    }
    ui.menu_button("Height", |ui| {
        for (name, h) in [("Small", 30.0), ("Medium", if t.kind == TrackKind::Video { geom::DEFAULT_VIDEO_H } else { geom::DEFAULT_AUDIO_H }), ("Large", 110.0)] {
            if item(ui, name, "") {
                tl.track_heights.insert(id.into(), h);
            }
        }
    });
    if item(ui, "Select all clips on track", "") {
        out.push(UiAction::Select(p.clips.iter().filter(|c| c.track == id).map(|c| c.id.clone()).collect()));
    }
    spacing_items(tl, ui, cx, id, pos2(header_pos.x + 190.0, header_pos.y), out);
    ui.separator();
    let n = p.clips.iter().filter(|c| c.track == id).count();
    if n == 0 {
        if item(ui, "Remove track", "") {
            apply(out, format!("Remove track {id}"), vec![Op::RemoveTrack { track: id.into(), force: false }]);
        }
    } else if item(ui, &format!("Remove track and its {n} clip(s)"), "") {
        apply(out, format!("Remove track {id}"), vec![Op::RemoveTrack { track: id.into(), force: true }]);
    }
    if item(ui, "Ask AI about this track…", "") {
        out.push(UiAction::AskAi { prompt: format!("About track {id}: "), ids: vec![id.into()] });
    }
}

/// The `move_track` ops that move a track one row up / down among the rows of its kind.
pub(crate) fn track_up_down(p: &aiframes_core::Project, heights: &std::collections::HashMap<Id, f32>, id: &str) -> (Option<Op>, Option<Op>) {
    let rows = geom::layout_rows(p, heights);
    let Some(kind) = p.tracks.iter().find(|t| t.id == id).map(|t| t.kind) else { return (None, None) };
    let kr: Vec<&geom::Row> = rows.iter().filter(|r| r.kind == kind).collect();
    let Some(a) = kr.iter().position(|r| r.track == id) else { return (None, None) };
    let up = if a > 0 { keys::reorder_op(p, &rows, id, a - 1) } else { None };
    let down = if a + 1 < kr.len() { keys::reorder_op(p, &rows, id, a + 2) } else { None };
    (up, down)
}

/// The "Set spacing" dialog: every gap to a value or scaled (optionally clamped), previewed live as
/// ghosts on the timeline; Apply emits one `set_spacing`.
pub(super) fn spacing_dialog(tl: &mut Timeline, ui: &mut Ui, cx: &EditCtx, base: egui::Id, out: &mut Vec<UiAction>) {
    let Some(mut dlg) = tl.spacing.clone() else { return };
    let p = cx.project;
    let r = cx.resolved;
    let close = |tl: &mut Timeline| {
        tl.spacing = None;
        if tl.preview.as_ref().is_some_and(|pv| pv.owner == Owner::Dialog) {
            tl.preview = None;
        }
    };
    let Some(track) = p.tracks.iter().find(|t| t.id == dlg.track) else {
        close(tl);
        return;
    };
    let (summary, refused): (String, Option<String>) = match &tl.preview {
        Some(pv) if pv.owner == Owner::Dialog => match &pv.result {
            Ok(s) => (s.summary.clone(), None),
            Err(e) => (String::new(), Some(e.clone())),
        },
        _ => (String::new(), None),
    };
    let (mut open, mut apply_now, mut cancel) = (true, false, false);
    let tname = if track.name.is_empty() { track.id.clone() } else { format!("{} · {}", track.id, track.name) };
    egui::Window::new("Set spacing").id(base.with("spacing_dialog")).default_pos(dlg.pos).pivot(egui::Align2::LEFT_BOTTOM).collapsible(false).resizable(false).open(&mut open).show(ui.ctx(), |ui| {
        ui.set_max_width(300.0);
        // Which track (the app's menu opens the dialog without one).
        let spaced: Vec<&aiframes_core::Track> = p.tracks.iter().filter(|t| p.clips.iter().filter(|c| c.track == t.id).count() >= 2).collect();
        let mut pick = dlg.track.clone();
        ui.horizontal(|ui| {
            ui.label("Track");
            egui::ComboBox::from_id_salt(base.with("spacing_track")).truncate().width(200.0).selected_text(&tname).show_ui(ui, |ui| {
                for t in &spaced {
                    let label = if t.name.is_empty() { t.id.clone() } else { format!("{} · {}", t.id, t.name) };
                    ui.selectable_value(&mut pick, t.id.clone(), label);
                }
            });
        });
        if pick != dlg.track {
            dlg.selected = p.clips.iter().filter(|c| c.track == pick && cx.is_selected(&c.id)).map(|c| c.id.clone()).collect();
            dlg.only_selected = dlg.selected.len() >= 2;
            dlg.track = pick;
        }
        ui.label(RichText::new("Pauses between consecutive clips; anchored clips follow.").small().weak());
        if dlg.selected.len() >= 2 {
            ui.checkbox(&mut dlg.only_selected, format!("Only the {} selected clips", dlg.selected.len()));
        }
        let now = edits::gaps(p, r, &dlg.track, if dlg.only_selected && dlg.selected.len() >= 2 { &dlg.selected } else { &[] });
        let list: Vec<String> = now.iter().take(8).map(|g| format!("{g:.2}")).collect();
        ui.label(RichText::new(format!("{} gap(s) now: {}{} s", now.len(), list.join(", "), if now.len() > 8 { ", …" } else { "" })).small());
        ui.horizontal(|ui| {
            ui.radio_value(&mut dlg.by_scale, false, "Every gap");
            ui.add_enabled(!dlg.by_scale, egui::DragValue::new(&mut dlg.gap).range(0.0..=600.0).speed(0.01).max_decimals(2).suffix(" s"));
        });
        ui.horizontal(|ui| {
            ui.radio_value(&mut dlg.by_scale, true, "Scale gaps");
            ui.add_enabled(dlg.by_scale, egui::DragValue::new(&mut dlg.scale).range(0.0..=1000.0).speed(1.0).max_decimals(0).suffix(" %"));
        });
        ui.horizontal(|ui| {
            for (label, v, default) in [("at least", &mut dlg.min_gap, 0.1), ("at most", &mut dlg.max_gap, 1.0)] {
                let mut on = v.is_some();
                ui.checkbox(&mut on, label);
                let mut x = v.unwrap_or(default);
                ui.add_enabled(on, egui::DragValue::new(&mut x).range(0.0..=600.0).speed(0.01).max_decimals(2).suffix(" s"));
                *v = on.then_some(x);
            }
        });
        if let Some(e) = &refused {
            ui.label(RichText::new(util::plain(&util::ellipsize(e, 220))).small().color(crate::style::ERROR));
        } else if !summary.is_empty() {
            ui.label(RichText::new(util::plain(&util::ellipsize(&summary, 260))).small().color(crate::style::ACCENT));
        }
        ui.horizontal(|ui| {
            if ui.add_enabled(refused.is_none(), egui::Button::new("Apply")).clicked() {
                apply_now = true;
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
        if ui.input(|i| i.key_pressed(Key::Escape)) {
            cancel = true;
        }
    });
    if apply_now {
        apply_checked(out, cx, format!("Set spacing on {}", dlg.track), vec![dlg.op()]);
        close(tl);
    } else if cancel || !open {
        close(tl);
    } else {
        // Live preview (unless a drag owns the preview right now).
        if tl.drag.is_none() {
            let before = tl.preview.as_ref().map(|pv| pv.key.clone());
            tl.set_preview_g(cx, Owner::Dialog, format!("Set spacing on {}", dlg.track), vec![dlg.op()], vec![], String::new(), true);
            if tl.preview.as_ref().map(|pv| pv.key.clone()) != before {
                ui.ctx().request_repaint();
            }
        }
        tl.spacing = Some(dlg);
    }
}

/// The inline rename field (Enter commits, Esc or clicking away cancels).
pub(super) fn rename_popup(tl: &mut Timeline, ui: &mut Ui, cx: &EditCtx, base: egui::Id, out: &mut Vec<UiAction>) {
    let Some(rn) = &mut tl.rename else { return };
    let mut commit = false;
    let mut cancel = false;
    let edit_id = base.with("rename_edit");
    egui::Area::new(base.with("rename_area")).order(egui::Order::Foreground).fixed_pos(rn.pos).show(ui.ctx(), |ui| {
        egui::Frame::popup(ui.style()).inner_margin(egui::Margin::same(6)).show(ui, |ui| {
            ui.label(RichText::new("Rename").small().weak());
            let r = ui.add(egui::TextEdit::singleline(&mut rn.text).id(edit_id).desired_width(200.0));
            if !rn.opened {
                r.request_focus();
                rn.opened = true;
            } else if r.lost_focus() {
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    commit = true;
                } else {
                    cancel = true;
                }
            }
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                cancel = true;
            }
        });
    });
    if commit {
        let text = rn.text.trim().to_string();
        let op = match &rn.target {
            RenameTarget::Clip(id) => Op::SetClip { clip: id.clone(), patch: json!({ "name": text }) },
            RenameTarget::Marker(id) => Op::SetMarker { marker: id.clone(), time: None, name: Some(text), kind: None, color: None, note: None },
            RenameTarget::Track(id) => Op::SetTrack { track: id.clone(), name: Some(text), role: None, muted: None, hidden: None, locked: None, solo: None, volume: None },
        };
        out.push(UiAction::Apply { label: "Rename".into(), ops: vec![op] });
        tl.rename = None;
    } else if cancel {
        tl.rename = None;
    }
    let _ = cx;
}

#[cfg(test)]
mod tests {
    use crate::edits::tests::*;
    use crate::util;
    use aiframes_core::*;

    #[test]
    fn track_reorder_indices_match_display() {
        // v1 (index 0) is below v2 (index 1): "move up" for v1 is index 1.
        let p = demo();
        let (q, _) = run(&p, &[Op::MoveTrack { track: "v1".into(), index: 1 }]);
        assert_eq!(q.tracks[1].id, "v1");
        let rows = crate::timeline::geom::layout_rows(&q, &Default::default());
        assert_eq!(rows[0].track, "v1", "v1 is now the top row");
    }

    #[test]
    fn move_up_down_works_when_kinds_interleave() {
        // Regression (review): with video and audio tracks interleaved in project.tracks, the menu's
        // neighbour index was the other kind and Move up/down stayed disabled.
        let mut p = demo();
        p.tracks = vec![track("v1", TrackKind::Video, "main"), track("a1", TrackKind::Audio, "voice"), track("v2", TrackKind::Video, "overlay"), track("a2", TrackKind::Audio, "music")];
        let h = Default::default();
        let (up, down) = super::track_up_down(&p, &h, "v1");
        assert!(down.is_none(), "v1 is the bottom video row");
        let (q, _) = run(&p, &[up.expect("v1 can move up")]);
        assert_eq!(crate::timeline::geom::layout_rows(&q, &h)[0].track, "v1");
        let (up, down) = super::track_up_down(&p, &h, "a2");
        assert!(down.is_none());
        let (q, _) = run(&p, &[up.expect("a2 can move up")]);
        let audio: Vec<String> = crate::timeline::geom::layout_rows(&q, &h).into_iter().filter(|r| r.kind == TrackKind::Audio).map(|r| r.track).collect();
        assert_eq!(audio, vec!["a2", "a1"]);
    }

    fn cx<'a>(p: &'a Project, r: &'a Resolved, sel: &'a [String], revision: u64) -> crate::EditCtx<'a> {
        crate::EditCtx { project: p, resolved: r, playhead: 0.0, playing: false, selection: sel, preview: &crate::NoPreview, ai_highlight: &[], ai_flash: 0.0, history: &[], revision, can_undo: false, can_redo: false, in_point: None, out_point: None }
    }

    fn frame(ctx: &egui::Context, time: f64, tl: &mut crate::timeline::Timeline, c: &crate::EditCtx) -> Vec<crate::UiAction> {
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 700.0))), time: Some(time), ..Default::default() };
        let mut acts = Vec::new();
        let mut out = ctx.run_ui(input, |ui| acts.extend(tl.ui(ui, c)));
        out.textures_delta.clear();
        acts
    }

    fn with(id: &str, offset: f64) -> Time {
        Time::Anchored(Anchor { to: AnchorTarget::With(id.into()), offset })
    }

    #[test]
    fn spacing_dialog_previews_live_and_applies_one_op() {
        let mut p = demo();
        p.clips.retain(|c| c.track != "a1");
        p.clips.push(media("a", "a1", "ma", 0.0, 1.0, 0.0));
        p.clips.push(media("b", "a1", "ma", 2.4, 1.0, 1.0));
        p.clips.push(media("c", "a1", "ma", 4.0, 1.0, 2.0));
        p.clips.push(text("lbl", "v2", with("b", 0.0), 0.5));
        let r = resolve(&p);
        let c = cx(&p, &r, &[], 0);
        let ctx = egui::Context::default();
        let mut tl = crate::timeline::Timeline::default();
        frame(&ctx, 0.0, &mut tl, &c);
        // The app opens it without a track: the main track (v1 has two clips), or the selection's track.
        assert_eq!(crate::timeline::Timeline::spacing_track(&c, None).as_deref(), Some("v1"));
        let sel = vec!["b".to_string()];
        assert_eq!(crate::timeline::Timeline::spacing_track(&cx(&p, &r, &sel, 0), None).as_deref(), Some("a1"));
        tl.open_spacing(Some("a1".into()));
        frame(&ctx, 0.05, &mut tl, &c);
        assert!(tl.spacing_dialog_open());
        frame(&ctx, 0.1, &mut tl, &c);
        frame(&ctx, 0.2, &mut tl, &c);
        let pv = tl.preview.as_ref().expect("live preview");
        assert_eq!(pv.owner, super::Owner::Dialog);
        let sim = pv.result.as_ref().expect("the default spacing applies");
        assert!(sim.changed.contains("b") && sim.changed.contains("lbl"), "anchored clips follow in the ghosts: {:?}", sim.changed);
        assert!(sim.summary.contains("→"), "the gap changes are listed: {}", sim.summary);
        // The dialog's op is what Apply sends.
        let op = tl.spacing.as_ref().unwrap().op();
        assert!(matches!(&op, Op::SetSpacing { track: Some(t), gap: Some(g), .. } if t == "a1" && (*g - 0.25).abs() < 1e-9), "{op:?}");
        // Scale mode with a clamp.
        let dlg = tl.spacing.as_mut().unwrap();
        dlg.by_scale = true;
        dlg.max_gap = Some(0.8);
        let op = dlg.op();
        let (q, rq) = run(&p, &[op]);
        let g = crate::edits::gaps(&q, &rq, "a1", &[]);
        assert!((g[0] - 0.8).abs() < 1e-9 && (g[1] - 0.42).abs() < 1e-9, "{g:?}");
    }

    #[test]
    fn end_anchors_are_explained_during_trims_and_flagged_when_missing() {
        let mut p = demo();
        p.clips.push(text("badge", "v2", with("c1", 0.5), 9.5));
        // Without an end anchor, slowing the base leaves the badge running past it: a warning badge.
        let (q, rq) = run(&p, &[Op::SetSpeed { clip: "c1".into(), speed: 4.0, keep: SpeedKeep::Source, linked: true }]);
        let c = cx(&q, &rq, &[], 7);
        let ctx = egui::Context::default();
        let mut tl = crate::timeline::Timeline::default();
        frame(&ctx, 0.0, &mut tl, &c);
        assert_eq!(tl.flags.get("badge"), Some(&Severity::Warning), "{:?}", tl.flags);
        assert!(super::clip_tooltip(&c, "badge").contains("Problem:"));
        // With "match length", trimming the base says the badge follows; trimming the badge itself
        // says how its end anchor changes.
        let op = crate::inspector::match_length_op(&p, p.clips.iter().find(|c| c.id == "badge").unwrap()).unwrap();
        let (p2, r2) = run(&p, &[op]);
        let sim = super::simulate(&p2, &r2, &[Op::TrimClip { clip: "c1".into(), edge: Edge::End, to: Some(8.0), delta: None, ripple: None }]).unwrap();
        assert_eq!(super::until_note(&p2, &sim, "c1").as_deref(), Some("badge ends with it"));
        let sim = super::simulate(&p2, &r2, &[Op::TrimClip { clip: "badge".into(), edge: Edge::End, to: Some(9.0), delta: None, ripple: None }]).unwrap();
        let note = super::until_note(&p2, &sim, "badge").unwrap();
        assert!(note.starts_with("end anchor now until end of c1 (-1"), "{note}");
        // The clip menu offers the fix for the warning.
        let all = util::findings(&q, &rq, 7);
        assert!(util::clip_findings(&all, "badge").any(|f| f.code == "overlay_outlives_base" && !f.fix_ops.is_empty()));
    }

    #[test]
    fn reveal_brings_ranges_and_tracks_into_view() {
        let mut p = demo();
        p.clips.push(media("late", "v2", "mv", 100.0, 5.0, 0.0));
        for i in 3..9 {
            p.tracks.push(track(&format!("v{i}"), TrackKind::Video, "overlay"));
        }
        let r = resolve(&p);
        let c = cx(&p, &r, &[], 0);
        let ctx = egui::Context::default();
        let mut tl = crate::timeline::Timeline::default();
        frame(&ctx, 0.0, &mut tl, &c);
        tl.zoom = 100.0;
        tl.scroll = 0.0;
        frame(&ctx, 0.02, &mut tl, &c);
        let (a, b) = tl.visible_range();
        assert!(a.abs() < 1e-9 && b > 5.0 && b < 100.0, "{a} {b}");
        tl.reveal(100.0, 105.0, &[]);
        frame(&ctx, 0.04, &mut tl, &c);
        let (a, b) = tl.visible_range();
        assert!(a <= 100.0 && b >= 105.0, "visible_range reports the target at once: {a} {b}");
        for k in 1..20 {
            frame(&ctx, 0.04 + k as f64 * 0.02, &mut tl, &c);
        }
        assert!(tl.scroll <= 100.0 && tl.scroll + (b - a) >= 105.0 && (tl.zoom - 100.0).abs() < 1e-9, "scrolled, not zoomed: {} {}", tl.scroll, tl.zoom);
        // A range wider than the view zooms out.
        tl.reveal(0.0, 105.0, &[]);
        for k in 0..20 {
            frame(&ctx, 1.0 + k as f64 * 0.02, &mut tl, &c);
        }
        let (a, b) = tl.visible_range();
        assert!(a <= 0.0 + 1e-9 && b >= 105.0 && tl.zoom < 100.0, "{a} {b} {}", tl.zoom);
        // Tracks: the bottom audio row scrolls into view of a short timeline.
        let lanes = tl.lanes_rect().unwrap();
        let visible = |tl: &crate::timeline::Timeline, id: &str| {
            let mut y = lanes.top();
            while y < lanes.bottom() {
                if tl.track_at(y).as_deref() == Some(id) {
                    return true;
                }
                y += 2.0;
            }
            false
        };
        tl.track_heights.extend(p.tracks.iter().map(|t| (t.id.clone(), 110.0)));
        frame(&ctx, 2.0, &mut tl, &c);
        assert!(!visible(&tl, "a2"), "a2 starts below the fold");
        tl.reveal_track("a2");
        frame(&ctx, 2.1, &mut tl, &c);
        frame(&ctx, 2.2, &mut tl, &c);
        assert!(visible(&tl, "a2"));
        // reveal(range, tracks): back to the top rows and an early range in one call.
        let top = p.tracks.iter().rev().find(|t| t.kind == TrackKind::Video).unwrap().id.clone();
        assert!(!visible(&tl, &top));
        tl.reveal(0.0, 2.0, std::slice::from_ref(&top));
        for k in 0..20 {
            frame(&ctx, 3.0 + k as f64 * 0.02, &mut tl, &c);
        }
        let (a, _) = tl.visible_range();
        assert!(visible(&tl, &top) && a <= 1e-9, "{a}");
    }
}
