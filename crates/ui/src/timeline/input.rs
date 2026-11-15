//! Drag-and-drop onto the lanes (media from the bin, items from the effects browser) and keyboard
//! shortcuts.

use super::*;

/// What a key press does (pure, so the mapping is testable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyCmd {
    Split,
    Delete,
    RippleDelete,
    RippleTrimStart,
    RippleTrimEnd,
    TrimStart,
    TrimEnd,
    Nudge(i32),
    PrevEdit,
    NextEdit,
    SetIn,
    SetOut,
    ClearIn,
    ClearOut,
    Marker,
    Chapter,
    SelectAll,
    Tool(Tool),
    ToggleSnap,
    ZoomIn,
    ZoomOut,
    Fit,
    Home,
    End,
    Play,
    Deselect,
}

/// The shortcut table, most specific first (egui ignores extra Shift/Alt when matching).
pub(crate) const KEYMAP: &[(Modifiers, Key, KeyCmd)] = &[
    (Modifiers::COMMAND, Key::K, KeyCmd::Split),
    (Modifiers::COMMAND, Key::A, KeyCmd::SelectAll),
    (Modifiers::SHIFT, Key::Delete, KeyCmd::RippleDelete),
    (Modifiers::SHIFT, Key::Backspace, KeyCmd::RippleDelete),
    (Modifiers::NONE, Key::Delete, KeyCmd::Delete),
    (Modifiers::NONE, Key::Backspace, KeyCmd::Delete),
    (Modifiers::ALT, Key::OpenBracket, KeyCmd::TrimStart),
    (Modifiers::ALT, Key::CloseBracket, KeyCmd::TrimEnd),
    (Modifiers::ALT, Key::I, KeyCmd::ClearIn),
    (Modifiers::ALT, Key::O, KeyCmd::ClearOut),
    (Modifiers::SHIFT, Key::M, KeyCmd::Chapter),
    (Modifiers::SHIFT, Key::ArrowLeft, KeyCmd::Nudge(-10)),
    (Modifiers::SHIFT, Key::ArrowRight, KeyCmd::Nudge(10)),
    (Modifiers::NONE, Key::ArrowLeft, KeyCmd::Nudge(-1)),
    (Modifiers::NONE, Key::ArrowRight, KeyCmd::Nudge(1)),
    (Modifiers::NONE, Key::ArrowUp, KeyCmd::PrevEdit),
    (Modifiers::NONE, Key::ArrowDown, KeyCmd::NextEdit),
    (Modifiers::NONE, Key::S, KeyCmd::Split),
    (Modifiers::NONE, Key::Q, KeyCmd::RippleTrimStart),
    (Modifiers::NONE, Key::W, KeyCmd::RippleTrimEnd),
    (Modifiers::NONE, Key::I, KeyCmd::SetIn),
    (Modifiers::NONE, Key::O, KeyCmd::SetOut),
    (Modifiers::NONE, Key::M, KeyCmd::Marker),
    (Modifiers::NONE, Key::V, KeyCmd::Tool(Tool::Select)),
    (Modifiers::NONE, Key::A, KeyCmd::Tool(Tool::Select)),
    (Modifiers::NONE, Key::C, KeyCmd::Tool(Tool::Razor)),
    (Modifiers::NONE, Key::Y, KeyCmd::Tool(Tool::Slip)),
    (Modifiers::NONE, Key::B, KeyCmd::Tool(Tool::Ripple)),
    (Modifiers::NONE, Key::R, KeyCmd::Tool(Tool::Roll)),
    (Modifiers::NONE, Key::U, KeyCmd::Tool(Tool::Slide)),
    (Modifiers::NONE, Key::N, KeyCmd::ToggleSnap),
    (Modifiers::NONE, Key::Equals, KeyCmd::ZoomIn),
    (Modifiers::NONE, Key::Plus, KeyCmd::ZoomIn),
    (Modifiers::NONE, Key::Minus, KeyCmd::ZoomOut),
    (Modifiers::NONE, Key::Backslash, KeyCmd::Fit),
    (Modifiers::NONE, Key::Home, KeyCmd::Home),
    (Modifiers::NONE, Key::End, KeyCmd::End),
    (Modifiers::NONE, Key::Space, KeyCmd::Play),
    (Modifiers::NONE, Key::Escape, KeyCmd::Deselect),
];

/// The ops/actions of a key command (pure).
pub(crate) fn key_actions(cmd: KeyCmd, cx: &EditCtx) -> Vec<UiAction> {
    let p = cx.project;
    let r = cx.resolved;
    let fps = util::fps(p);
    let t = cx.playhead;
    let sel = cx.selected_clips();
    let under: Vec<Id> = sel.iter().filter(|id| r.span(id).is_some_and(|s| s.contains(t))).cloned().collect();
    // With clips selected, only the selected ones under the playhead are cut/trimmed; every track
    // only when nothing is selected.
    let targets: Option<&[Id]> = if sel.is_empty() { None } else { Some(&under) };
    let apply = |label: String, ops: Vec<Op>, none: &str| -> Vec<UiAction> {
        if ops.is_empty() {
            let why = if !sel.is_empty() && under.is_empty() { "No selected clip under the playhead (deselect to act on every track)." } else { none };
            vec![UiAction::Status(why.into())]
        } else {
            vec![UiAction::Apply { label, ops }]
        }
    };
    let tc = util::timecode(t, fps);
    match cmd {
        KeyCmd::Split => apply(format!("Split at {tc}"), edits::split_ops(p, r, targets, t), "Nothing under the playhead to split."),
        KeyCmd::Delete | KeyCmd::RippleDelete => {
            let ripple = cmd == KeyCmd::RippleDelete;
            let ids: Vec<Id> = cx.selection.to_vec();
            let ops = edits::delete_ops(p, &ids, ripple);
            let n = ops.len();
            apply(format!("{} {n} item(s)", if ripple { "Ripple delete" } else { "Delete" }), ops, "Nothing selected to delete.")
        }
        KeyCmd::RippleTrimStart => apply(format!("Ripple trim start to {tc}"), edits::trim_to_ops(p, r, targets, t, Edge::Start, Some(true)), "No clip under the playhead."),
        KeyCmd::RippleTrimEnd => apply(format!("Ripple trim end to {tc}"), edits::trim_to_ops(p, r, targets, t, Edge::End, Some(true)), "No clip under the playhead."),
        KeyCmd::TrimStart => apply(format!("Trim start to {tc}"), edits::trim_to_ops(p, r, targets, t, Edge::Start, Some(false)), "No clip under the playhead."),
        KeyCmd::TrimEnd => apply(format!("Trim end to {tc}"), edits::trim_to_ops(p, r, targets, t, Edge::End, Some(false)), "No clip under the playhead."),
        KeyCmd::Nudge(frames) => {
            let delta = frames as f64 / fps;
            let movable: Vec<Id> = sel.iter().filter(|id| p.clips.iter().any(|c| &c.id == *id && p.tracks.iter().any(|tr| tr.id == c.track && !tr.locked))).cloned().collect();
            if movable.is_empty() {
                vec![UiAction::Seek((t + delta).max(0.0))]
            } else {
                let min_start = movable.iter().filter_map(|id| r.span(id)).map(|s| s.start).fold(f64::INFINITY, f64::min);
                let delta = if min_start.is_finite() { delta.max(-min_start) } else { delta };
                if delta.abs() < 1e-9 {
                    return vec![];
                }
                vec![UiAction::Apply { label: format!("Nudge {} frame(s)", frames), ops: vec![Op::Nudge { clips: movable, delta }] }]
            }
        }
        KeyCmd::PrevEdit => util::prev_point(&util::edit_points(p, r), t, 0.5 / fps).map(UiAction::Seek).into_iter().collect(),
        KeyCmd::NextEdit => util::next_point(&util::edit_points(p, r), t, 0.5 / fps).map(UiAction::Seek).into_iter().collect(),
        KeyCmd::SetIn => vec![UiAction::SetIn(Some(t))],
        KeyCmd::SetOut => vec![UiAction::SetOut(Some(t))],
        KeyCmd::ClearIn => vec![UiAction::SetIn(None)],
        KeyCmd::ClearOut => vec![UiAction::SetOut(None)],
        KeyCmd::Marker | KeyCmd::Chapter => {
            let chapter = cmd == KeyCmd::Chapter;
            let n = p.markers.iter().filter(|m| (m.kind == aiframes_core::MarkerKind::Chapter) == chapter).count() + 1;
            vec![UiAction::Apply {
                label: format!("Add {} at {tc}", if chapter { "chapter" } else { "marker" }),
                ops: vec![Op::AddMarker {
                    time: aiframes_core::Time::At(util::quantize(t, fps)),
                    name: Some(if chapter { format!("Chapter {n}") } else { format!("Marker {n}") }),
                    kind: chapter.then_some(aiframes_core::MarkerKind::Chapter),
                    id: None,
                    color: None,
                    note: None,
                }],
            }]
        }
        KeyCmd::SelectAll => vec![UiAction::Select(p.clips.iter().map(|c| c.id.clone()).collect())],
        KeyCmd::Home => vec![UiAction::Seek(0.0)],
        KeyCmd::End => vec![UiAction::Seek(r.duration.max(r.content_end()))],
        KeyCmd::Play => vec![UiAction::TogglePlay],
        KeyCmd::Deselect => {
            if cx.selection.is_empty() {
                vec![]
            } else {
                vec![UiAction::Select(vec![])]
            }
        }
        KeyCmd::Tool(_) | KeyCmd::ToggleSnap | KeyCmd::ZoomIn | KeyCmd::ZoomOut | KeyCmd::Fit => vec![],
    }
}

impl Timeline {
    pub(super) fn keys(&mut self, ui: &Ui, cx: &EditCtx, lay: &Layout, out: &mut Vec<UiAction>) {
        let mut cmds = Vec::new();
        ui.ctx().input_mut(|i| {
            for (m, k, c) in KEYMAP {
                // Plain keys must not fire with Ctrl held (Ctrl+S is the app's save).
                if *m == Modifiers::NONE && (i.modifiers.command || i.modifiers.ctrl) {
                    continue;
                }
                if i.consume_key(*m, *k) {
                    cmds.push(*c);
                }
            }
        });
        for c in cmds {
            match c {
                KeyCmd::Tool(t) => self.tool = t,
                KeyCmd::ToggleSnap => self.snapping = !self.snapping,
                KeyCmd::ZoomIn => self.zoom_by(lay, 1.5, cx.playhead),
                KeyCmd::ZoomOut => self.zoom_by(lay, 1.0 / 1.5, cx.playhead),
                KeyCmd::Fit => {
                    let d = cx.resolved.duration.max(cx.resolved.content_end());
                    let (s, z) = geom::fit(d, lay.lanes.width(), geom::MIN_ZOOM, geom::max_zoom(lay.fps));
                    self.scroll = s;
                    self.zoom = z;
                }
                KeyCmd::Deselect if self.drag.is_some() => {
                    self.drag = None;
                    self.preview = None;
                }
                KeyCmd::Deselect if self.key_sel.is_some() => self.key_sel = None,
                KeyCmd::Delete | KeyCmd::RippleDelete if self.selected_key(cx).is_some() => {
                    if let Some(ks) = self.key_sel.take() {
                        out.push(UiAction::Apply { label: format!("Remove keyframe ({})", ks.clip), ops: vec![Op::RemoveKeyframe { clip: ks.clip, path: ks.path, t: ks.t }] });
                    }
                }
                other => out.extend(key_actions(other, cx)),
            }
        }
    }

    /// The selected keyframe, when it still exists.
    pub(crate) fn selected_key(&self, cx: &EditCtx) -> Option<aiframes_core::Key> {
        let ks = self.key_sel.as_ref()?;
        let c = cx.project.clips.iter().find(|c| c.id == ks.clip)?;
        match util::param_at(c, &ks.path)? {
            aiframes_core::Param::Animated { keys } => keys.iter().find(|k| (k.t - ks.t).abs() < 1e-4).cloned(),
            _ => None,
        }
    }

    /// Drag-and-drop from the media bin and the effects browser.
    pub(super) fn dnd(&mut self, ui: &Ui, cx: &EditCtx, lay: &Layout, resp: &Response, out: &mut Vec<UiAction>) {
        let p = cx.project;
        let r = cx.resolved;
        let mods = ui.input(|i| i.modifiers);
        let pos = ui.input(|i| i.pointer.hover_pos());
        let mut active = false;
        if let (Some(drag), Some(pos)) = (resp.dnd_hover_payload::<MediaDrag>(), pos) {
            active = true;
            if let Some((label, ops, hints, info)) = self.media_drop(cx, lay, &drag.asset, pos, mods) {
                self.set_preview(cx, Owner::Drop, label, ops, hints, info);
            }
        }
        if let (Some(drag), Some(pos)) = (resp.dnd_release_payload::<MediaDrag>(), pos) {
            if let Some((label, ops, _, _)) = self.media_drop(cx, lay, &drag.asset, pos, mods) {
                if !ops.is_empty() {
                    out.push(match simulate(cx.project, cx.resolved, &ops) {
                        Ok(_) => UiAction::Apply { label, ops },
                        Err(e) => UiAction::Status(format!("Not added: {e}")),
                    });
                }
            }
            self.preview = None;
            active = false;
        }
        if let (Some(item), Some(pos)) = (resp.dnd_hover_payload::<BrowserDrag>(), pos) {
            active = true;
            let (label, ops, info) = self.browser_drop(cx, lay, &item, pos);
            self.set_preview(cx, Owner::Drop, label, ops, vec![], info);
        }
        if let (Some(item), Some(pos)) = (resp.dnd_release_payload::<BrowserDrag>(), pos) {
            let (label, ops, info) = self.browser_drop(cx, lay, &item, pos);
            if ops.is_empty() {
                out.push(UiAction::Status(info));
            } else {
                out.push(match simulate(cx.project, cx.resolved, &ops) {
                    Ok(_) => UiAction::Apply { label, ops },
                    Err(e) => UiAction::Status(format!("Not applied: {e}")),
                });
            }
            self.preview = None;
            active = false;
        }
        if !active && self.preview.as_ref().is_some_and(|pv| pv.owner == Owner::Drop) {
            self.preview = None;
        }
        let _ = (p, r);
    }

    #[allow(clippy::type_complexity)]
    fn media_drop(&mut self, cx: &EditCtx, lay: &Layout, asset: &str, pos: Pos2, mods: Modifiers) -> Option<(String, Vec<Op>, Vec<(Id, Span)>, String)> {
        let p = cx.project;
        let m = p.media.iter().find(|m| m.id == asset)?;
        let kind = if m.kind == aiframes_core::MediaKind::Audio { TrackKind::Audio } else { TrackKind::Video };
        let below = pos.y >= lay.rows_bottom();
        let row = if below { None } else { geom::nearest_row_of_kind(&lay.rows, pos.y - lay.y0, kind) };
        let track = row.map(|i| lay.rows[i].track.clone());
        let len = edits::media_clip_len(p, asset);
        let mut t = util::quantize(lay.tmap.t(pos.x).max(0.0), lay.fps);
        if self.snapping && !mods.alt {
            if let Some((d, at)) = snap::snap_edges(&[t, t + len], &self.snap_targets(cx, &HashSet::new()), self.zoom, snap::SNAP_PX) {
                t = (t + d).max(0.0);
                self.snap_line = Some(at);
            }
        }
        let insert = mods.command;
        let ops = edits::drop_media_ops(p, asset, track.as_deref(), t, insert);
        let name = util::media_name(m);
        let label = format!("{} {name}", if insert { "Insert" } else { "Add" });
        let info = format!(
            "{}  ·  {}{}",
            util::timecode(t, lay.fps),
            if insert { "insert (ripple)" } else { "overwrite · Ctrl = insert" },
            if track.is_none() { "  ·  new track" } else { "" }
        );
        Some((label, ops, track.map(|tr| vec![(tr, Span::new(t, t + len))]).unwrap_or_default(), info))
    }

    fn browser_drop(&self, cx: &EditCtx, lay: &Layout, item: &BrowserDrag, pos: Pos2) -> (String, Vec<Op>, String) {
        let p = cx.project;
        let r = cx.resolved;
        let t = util::quantize(lay.tmap.t(pos.x).max(0.0), lay.fps);
        let clip = match self.hit_at(lay, pos) {
            Hit::Clip { id, .. } => Some(id),
            _ => None,
        };
        let row_track = lay.row_at(pos.y).map(|i| lay.rows[i].track.clone());
        match item {
            BrowserDrag::Effect(kind) => match clip {
                Some(c) if p.clips.iter().any(|k| k.id == c && util::clip_kind(p, k) != ClipKind::Audio) => (
                    format!("Add {kind} to {c}"),
                    vec![Op::AddEffect { clip: c.clone(), effect: aiframes_core::EffectArg::Kind(kind.clone()), index: None }],
                    format!("add {kind} to {c}"),
                ),
                _ => (String::new(), vec![], "Drop the effect on a picture clip.".into()),
            },
            BrowserDrag::Transition(kind) => match clip.and_then(|c| drag::transition_drop_op(p, r, &c, t, kind).map(|op| (c, op))) {
                Some((c, op)) => (format!("Transition {kind} at {c}"), vec![op], format!("{kind} on the nearest cut of {c}")),
                None => (String::new(), vec![], "Drop the transition near a cut between two touching clips.".into()),
            },
            BrowserDrag::Text(preset) => {
                let ops = edits::text_preset_ops(p, r, *preset, t, row_track.as_deref());
                (format!("Add {}", preset.label()), ops, format!("{} at {}", preset.label(), util::timecode(t, lay.fps)))
            }
            BrowserDrag::Shape(preset) => {
                let ops = edits::shape_preset_ops(p, r, *preset, t, row_track.as_deref());
                (format!("Add {}", preset.label()), ops, format!("{} at {}", preset.label(), util::timecode(t, lay.fps)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::tests::*;
    use crate::NoPreview;
    use aiframes_core::*;

    fn cx<'a>(p: &'a Project, r: &'a Resolved, playhead: f64, sel: &'a [Id]) -> EditCtx<'a> {
        EditCtx {
            project: p,
            resolved: r,
            playhead,
            playing: false,
            selection: sel,
            preview: &NoPreview,
            ai_highlight: &[],
            ai_flash: 0.0,
            history: &[],
            revision: 0,
            can_undo: false,
            can_redo: false,
            in_point: None,
            out_point: None,
        }
    }

    fn applied(p: &Project, acts: &[UiAction]) -> (Project, Resolved) {
        let ops: Vec<Op> = acts.iter().flat_map(|a| if let UiAction::Apply { ops, .. } = a { ops.clone() } else { vec![] }).collect();
        assert!(!ops.is_empty(), "expected ops, got {acts:?}");
        run(p, &ops)
    }

    #[test]
    fn keymap_has_no_duplicate_bindings() {
        let mut seen = HashSet::new();
        for (m, k, _) in KEYMAP {
            assert!(seen.insert((format!("{m:?}"), *k)), "{m:?}+{k:?} bound twice");
        }
    }

    #[test]
    fn split_delete_and_ripple_keys() {
        let p = demo();
        let r = resolve(&p);
        // S with nothing selected splits everything under the playhead.
        let (q, _) = applied(&p, &key_actions(KeyCmd::Split, &cx(&p, &r, 4.0, &[])));
        assert_eq!(q.clips.len(), p.clips.len() + 2);
        // With c3 selected only c3 splits.
        let sel = vec!["c3".to_string()];
        let (q, _) = applied(&p, &key_actions(KeyCmd::Split, &cx(&p, &r, 4.0, &sel)));
        assert_eq!(q.clips.len(), p.clips.len() + 1);
        // Shift+Delete on c1 ripples c2 back to 0.
        let sel = vec!["c1".to_string()];
        let (_, rq) = applied(&p, &key_actions(KeyCmd::RippleDelete, &cx(&p, &r, 0.0, &sel)));
        assert_eq!(rq.span("c2").unwrap().start, 0.0);
        // Delete without ripple: c2 keeps its time (now absolute).
        let (q, rq) = applied(&p, &key_actions(KeyCmd::Delete, &cx(&p, &r, 0.0, &sel)));
        assert_eq!(rq.span("c2").unwrap().start, 10.0);
        assert_eq!(q.clips.iter().find(|c| c.id == "c2").unwrap().start, Time::At(10.0));
        // Q/W ripple trims.
        let sel = vec!["c1".to_string()];
        let (_, rq) = applied(&p, &key_actions(KeyCmd::RippleTrimEnd, &cx(&p, &r, 6.0, &sel)));
        assert_eq!(rq.span("c2").unwrap().start, 6.0);
        let (_, rq) = applied(&p, &key_actions(KeyCmd::RippleTrimStart, &cx(&p, &r, 2.0, &sel)));
        assert_eq!(rq.span("c1").unwrap(), Span::new(0.0, 8.0));
        let (_, rq) = applied(&p, &key_actions(KeyCmd::TrimStart, &cx(&p, &r, 2.0, &sel)));
        assert_eq!(rq.span("c1").unwrap(), Span::new(2.0, 10.0));
        assert!(matches!(key_actions(KeyCmd::Delete, &cx(&p, &r, 0.0, &[]))[0], UiAction::Status(_)));
    }

    #[test]
    fn trims_and_splits_respect_the_selection() {
        // Regression (review): with a selection that is not under the playhead, Q/W/S acted on every
        // clip of every track. Now they act on the selected clips only (all tracks when none is selected).
        let mut p = demo();
        p.clips.push(text("t1", "v2", Time::At(15.0), 3.0));
        let r = resolve(&p);
        let sel = vec!["t1".to_string()];
        for cmd in [KeyCmd::RippleTrimEnd, KeyCmd::RippleTrimStart, KeyCmd::TrimStart, KeyCmd::TrimEnd, KeyCmd::Split] {
            let acts = key_actions(cmd, &cx(&p, &r, 4.0, &sel));
            assert!(acts.iter().all(|a| matches!(a, UiAction::Status(_))), "{cmd:?} with t1 selected (not under the playhead): {acts:?}");
        }
        // Nothing selected: every track.
        let acts = key_actions(KeyCmd::RippleTrimEnd, &cx(&p, &r, 4.0, &[]));
        assert!(matches!(&acts[0], UiAction::Apply { ops, .. } if ops.len() == 2), "{acts:?}");
    }

    #[test]
    fn nudge_navigation_markers() {
        let p = demo();
        let r = resolve(&p);
        let sel = vec!["c3".to_string()];
        let (_, rq) = applied(&p, &key_actions(KeyCmd::Nudge(10), &cx(&p, &r, 0.0, &sel)));
        assert!((rq.span("c3").unwrap().start - 10.0 / 30.0).abs() < 1e-9);
        // Can't nudge before 0.
        assert!(key_actions(KeyCmd::Nudge(-1), &cx(&p, &r, 0.0, &sel)).is_empty());
        // Nothing selected: the playhead steps.
        assert!(matches!(key_actions(KeyCmd::Nudge(1), &cx(&p, &r, 1.0, &[]))[0], UiAction::Seek(t) if (t - (1.0 + 1.0 / 30.0)).abs() < 1e-9));
        assert!(matches!(key_actions(KeyCmd::NextEdit, &cx(&p, &r, 1.0, &[]))[0], UiAction::Seek(t) if t == 10.0));
        assert!(matches!(key_actions(KeyCmd::PrevEdit, &cx(&p, &r, 12.0, &[]))[0], UiAction::Seek(t) if t == 10.0));
        let (q, rq) = applied(&p, &key_actions(KeyCmd::Chapter, &cx(&p, &r, 3.0, &[])));
        assert_eq!(q.markers[0].kind, MarkerKind::Chapter);
        assert_eq!(rq.marker(&q.markers[0].id), Some(3.0));
        assert!(matches!(key_actions(KeyCmd::SelectAll, &cx(&p, &r, 0.0, &[]))[0], UiAction::Select(ref v) if v.len() == 3));
        assert!(matches!(key_actions(KeyCmd::End, &cx(&p, &r, 0.0, &[]))[0], UiAction::Seek(t) if t == 20.0));
    }
}
