//! AIFrames editing widgets (egui): timeline, inspector, effects browser, transcript editor,
//! history, media bin. Widgets never mutate the project: they read an [`EditCtx`] and return
//! [`UiAction`]s; edits are `Op`s so the person and the AI share one history.
//!
//! The types and `ui` signatures below are the contract the app shell codes against (the
//! orchestrator's stub, kept; additions are marked "added").
//!
//! * [`timeline::Timeline`] — the multi-track editor (the main widget).
//! * [`inspector::Inspector`] — properties of the selection (or project settings).
//! * [`effects_browser::EffectsBrowser`] — effects, transitions, text and shape presets.
//! * [`transcript::TranscriptPanel`] — text-based editing.
//! * [`history::HistoryPanel`] — the attributed history.
//! * [`bin::MediaBin`] — the media bin.
//!
//! Shared helpers: [`style`] (palette), [`util`] (timecode, clip helpers), [`edits`] (op builders
//! shared by the widgets, e.g. "add a text overlay at the playhead").

pub use egui;

use aiframes_core::{HistoryEntry, Id, MediaAsset, Op, Project, Resolved};
use aiframes_media::Waveform;
use std::path::PathBuf;
use std::sync::Arc;

pub mod edits;
pub mod style;
pub mod util;

pub mod bin;
pub mod effects_browser;
pub mod history;
pub mod inspector;
pub mod review;
pub mod timeline;
pub mod transcript;

mod widgets;

/// Media previews provided by the app (it owns textures and background decoding).
/// Both calls must be cheap and non-blocking: return `None` while something is still loading
/// (the app requests a repaint when it arrives).
pub trait MediaPreview {
    /// A small frame of `asset` at source time `t` (snapped by the app), for filmstrips and the bin.
    fn thumbnail(&self, asset: &MediaAsset, t: f64) -> Option<egui::TextureId>;
    fn waveform(&self, asset: &MediaAsset) -> Option<Arc<Waveform>>;
    /// (added) The waveform of sound stream `stream` of `asset` (files with several sound streams;
    /// 0 = the first, what [`MediaPreview::waveform`] gives). The default knows only stream 0.
    fn waveform_stream(&self, asset: &MediaAsset, stream: u32) -> Option<Arc<Waveform>> {
        if stream == 0 {
            self.waveform(asset)
        } else {
            None
        }
    }
}

/// A [`MediaPreview`] that has nothing (tests, headless use).
pub struct NoPreview;

impl MediaPreview for NoPreview {
    fn thumbnail(&self, _asset: &MediaAsset, _t: f64) -> Option<egui::TextureId> {
        None
    }
    fn waveform(&self, _asset: &MediaAsset) -> Option<Arc<Waveform>> {
        None
    }
}

/// Everything a widget may read this frame.
pub struct EditCtx<'a> {
    pub project: &'a Project,
    pub resolved: &'a Resolved,
    /// Timeline seconds.
    pub playhead: f64,
    pub playing: bool,
    /// Selected ids (clips, transitions, markers, tracks, media).
    pub selection: &'a [Id],
    pub preview: &'a dyn MediaPreview,
    /// Ids the AI changed recently (draw them with an accent glow); `ai_flash` fades 1 → 0.
    pub ai_highlight: &'a [Id],
    pub ai_flash: f32,
    pub history: &'a [HistoryEntry],
    pub revision: u64,
    pub can_undo: bool,
    pub can_redo: bool,
    /// In/out points (timeline seconds) for range operations and export.
    pub in_point: Option<f64>,
    pub out_point: Option<f64>,
}

impl EditCtx<'_> {
    /// True when `id` is selected.
    pub fn is_selected(&self, id: &str) -> bool {
        self.selection.iter().any(|s| s == id)
    }

    /// True when the AI changed `id` recently.
    pub fn is_ai(&self, id: &str) -> bool {
        self.ai_highlight.iter().any(|s| s == id)
    }

    /// Selected ids that are clips, in project order.
    pub fn selected_clips(&self) -> Vec<Id> {
        self.project.clips.iter().filter(|c| self.is_selected(&c.id)).map(|c| c.id.clone()).collect()
    }
}

/// What a widget asks the app to do.
#[derive(Clone, Debug)]
pub enum UiAction {
    Seek(f64),
    Select(Vec<Id>),
    /// Apply ops as one undoable step authored by the user.
    Apply { label: String, ops: Vec<Op> },
    Undo,
    Redo,
    /// Jump history to just after this revision (undo/redo until there).
    RestoreRevision(u64),
    TogglePlay,
    SetIn(Option<f64>),
    SetOut(Option<f64>),
    /// Open a file dialog to import media (or import these paths directly).
    Import(Vec<PathBuf>),
    /// Run Whisper on this media asset (the app shows progress).
    Transcribe(Id),
    /// (added) Transcribe sound stream `.1` of the asset (files with several sound streams).
    TranscribeStream(Id, u32),
    /// Show something in the status bar.
    Status(String),
    /// Ask the AI panel to do something about these ids (pre-fills the prompt).
    AskAi { prompt: String, ids: Vec<Id> },
    /// (added) Show this media asset's file in Explorer.
    RevealMedia(Id),
    /// (added) Ask the person for a new file for this (offline/moved) media asset, then `relink_media`.
    RelinkMedia(Id),
    /// (added) Show the program viewer as if these ops were applied, without touching the document
    /// or its history (live slider drags, hover previews). Send it every frame the preview should
    /// last; the first frame without one ends it. Ops that do not apply are skipped quietly.
    Preview(Vec<Op>),
    /// (added) Ask the person for a file for string parameter `name` of effect `index` of `clip` (a
    /// LUT's `.cube`), then `set_effect` it.
    PickEffectFile { clip: Id, index: usize, name: String },
}

/// Drag-and-drop payload for media dragged from the bin onto the timeline or viewer.
#[derive(Clone, Debug)]
pub struct MediaDrag {
    pub asset: Id,
}

/// (added) Drag-and-drop payload for items dragged from the effects browser: drop an effect on a
/// clip, a transition on a cut, a text/shape preset at a time on a track.
pub use effects_browser::BrowserDrag;

#[cfg(test)]
mod smoke {
    //! Renders every widget headlessly (several selections, a hovering pointer, a drag) to catch
    //! panics in layout and painting code.
    use super::*;
    use aiframes_core::{resolve, Op, Time};

    fn frame(ctx: &egui::Context, pos: Option<egui::Pos2>, down: bool, f: &mut dyn FnMut(&mut egui::Ui)) {
        let mut input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))), ..Default::default() };
        if let Some(p) = pos {
            input.events.push(egui::Event::PointerMoved(p));
            input.events.push(egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: down, modifiers: Default::default() });
        }
        let mut out = ctx.run_ui(input, |ui| f(ui));
        out.textures_delta.clear();
    }

    #[test]
    fn every_widget_renders_every_selection() {
        let mut p = crate::edits::tests::demo();
        let r0 = resolve(&p);
        let mut ops = crate::edits::text_preset_ops(&p, &r0, crate::edits::TextPreset::Callout, 2.0, None);
        ops.push(Op::AddMarker { time: Time::At(3.0), name: Some("m".into()), kind: None, id: Some("mk1".into()), color: None, note: None });
        ops.push(crate::edits::add_transition_op(&p, &r0, "c1", "crossfade").unwrap());
        // Animated opacity and volume on c1, so its keyframe lane and volume line show when selected.
        for (path, t, v) in [("opacity", 1.0, 0.2), ("opacity", 6.0, 1.0), ("volume", 2.0, 0.5), ("volume", 8.0, 1.0)] {
            ops.push(Op::AddKeyframe { clip: "c1".into(), path: path.into(), t, v, ease: None });
        }
        aiframes_core::apply_batch(&mut p, &ops).unwrap();
        // Round 4: the callout ends with c1 (end anchor), so the end editor and the end link render.
        let callout = p.clips.last().unwrap().id.clone();
        let until = Time::Anchored(aiframes_core::Anchor { to: aiframes_core::AnchorTarget::After("c1".into()), offset: -1.0 });
        aiframes_core::apply(&mut p, &Op::SetUntil { clip: callout.clone(), until: Some(Some(until)) }).unwrap();
        // Round 6: c1 has motion blur, frame blending, a 3-D turn and the callout as its matte (badges,
        // the matte link and the striped matte source render).
        let look = [
            crate::edits::motion_blur_op("c1", Some(&aiframes_core::MotionBlur::default())),
            crate::edits::frame_blend_op("c1", true),
            crate::edits::matte_op("c1", Some((&callout, aiframes_core::MatteMode::Luma))),
            Op::SetParam { clip: "c1".into(), path: "transform.rotation_y".into(), value: Some(30.0), keys: None },
        ];
        aiframes_core::apply_batch(&mut p, &look).unwrap();
        // Two pending suggestions (one with a preview time and a reason), so the review panel lists cards.
        let empty = p.clone();
        let suggest = [
            Op::AddSuggestion { id: None, label: "Tighten the pause after the hook".into(), reason: Some("It may be a deliberate beat".into()), at: Some(3.0), ops: vec![Op::AddMarker { time: Time::At(5.0), name: None, kind: None, id: None, color: None, note: None }] },
            Op::AddSuggestion { id: None, label: "Mark the end".into(), reason: None, at: None, ops: vec![Op::AddMarker { time: Time::At(6.0), name: None, kind: None, id: None, color: None, note: None }] },
        ];
        aiframes_core::apply_batch(&mut p, &suggest).unwrap();
        p.transcripts.insert("ma".into(), aiframes_core::Transcript { language: "en".into(), words: (0..40).map(|i| aiframes_core::Word { text: format!(" w{i}"), start: i as f64 * 0.4, end: i as f64 * 0.4 + 0.3, conf: 0.5 }).collect(), ..Default::default() });
        let r = resolve(&p);
        let tr = p.transitions[0].id.clone();
        let selections: Vec<Vec<aiframes_core::Id>> = vec![vec![], vec!["c1".into()], vec!["c3".into()], vec!["c1".into(), "c3".into()], vec![tr], vec!["mk1".into()], vec!["v1".into()], vec!["mv".into()], vec![p.clips.last().unwrap().id.clone()]];
        let ctx = egui::Context::default();
        let mut tl = timeline::Timeline::default();
        let mut insp = inspector::Inspector::default();
        let mut fx = effects_browser::EffectsBrowser::default();
        let mut trp = transcript::TranscriptPanel::default();
        let mut hist = history::HistoryPanel::default();
        let mut rev = review::ReviewPanel::default();
        let mut bin = bin::MediaBin::default();
        let history = vec![aiframes_core::HistoryEntry { revision: 1, author: "ai".into(), label: "x".into(), time: "2026-11-08T21:41:00Z".into(), kind: Default::default(), target: None, ops: vec![], summary: "a; b".into() }];
        for (k, sel) in selections.iter().enumerate() {
            let cx = EditCtx {
                project: &p,
                resolved: &r,
                playhead: 1.0 + k as f64,
                playing: k % 2 == 0,
                selection: sel,
                preview: &NoPreview,
                ai_highlight: &["c1".to_string()],
                ai_flash: 0.5,
                history: &history,
                revision: 1,
                can_undo: true,
                can_redo: false,
                in_point: Some(1.0),
                out_point: Some(4.0),
            };
            // Hover, press and drag across the timeline lanes.
            for (i, (pos, down)) in [(None, false), (Some(egui::pos2(400.0, 120.0)), false), (Some(egui::pos2(400.0, 120.0)), true), (Some(egui::pos2(460.0, 160.0)), true), (Some(egui::pos2(470.0, 170.0)), false)].into_iter().enumerate() {
                let mut acts = Vec::new();
                frame(&ctx, pos, down, &mut |ui| {
                    acts.extend(tl.ui(ui, &cx));
                });
                let _ = (i, acts);
            }
            frame(&ctx, None, false, &mut |ui| {
                let _ = insp.ui(ui, &cx);
            });
            frame(&ctx, None, false, &mut |ui| {
                let _ = fx.ui(ui, &cx);
                let _ = trp.ui(ui, &cx);
            });
            frame(&ctx, None, false, &mut |ui| {
                let _ = hist.ui(ui, &cx);
                let _ = bin.ui(ui, &cx);
            });
            frame(&ctx, Some(egui::pos2(120.0, 90.0)), k % 2 == 1, &mut |ui| {
                let _ = rev.ui(ui, &cx);
            });
        }
        // The review panel with nothing pending, and its actions apply.
        {
            let re = resolve(&empty);
            let cx = EditCtx { project: &empty, resolved: &re, playhead: 0.0, playing: false, selection: &[], preview: &NoPreview, ai_highlight: &[], ai_flash: 0.0, history: &[], revision: 0, can_undo: false, can_redo: false, in_point: None, out_point: None };
            frame(&ctx, None, false, &mut |ui| {
                assert!(rev.ui(ui, &cx).is_empty());
            });
            for s in &p.suggestions {
                let mut q = p.clone();
                aiframes_core::apply_batch(&mut q, &aiframes_core::suggestion_ops(&p, &s.id).unwrap()).unwrap();
                assert_eq!(q.suggestions.len(), p.suggestions.len() - 1);
            }
        }
        bin.list_view = true;
        // Every tool, hovering and dragging over the selected clip's lanes.
        let sel = vec!["c1".to_string()];
        for tool in timeline::Tool::ALL {
            tl.tool = tool;
            let cx = EditCtx { project: &p, resolved: &r, playhead: 2.0, playing: false, selection: &sel, preview: &NoPreview, ai_highlight: &[], ai_flash: 0.0, history: &[], revision: 0, can_undo: false, can_redo: false, in_point: None, out_point: None };
            for (pos, down) in [(egui::pos2(260.0, 170.0), false), (egui::pos2(260.0, 170.0), true), (egui::pos2(300.0, 150.0), true), (egui::pos2(300.0, 150.0), false)] {
                frame(&ctx, Some(pos), down, &mut |ui| {
                    let _ = tl.ui(ui, &cx);
                });
            }
        }
        tl.tool = timeline::Tool::Razor;
        let cx = EditCtx { project: &p, resolved: &r, playhead: 2.0, playing: false, selection: &[], preview: &NoPreview, ai_highlight: &[], ai_flash: 0.0, history: &[], revision: 0, can_undo: false, can_redo: false, in_point: None, out_point: None };
        frame(&ctx, Some(egui::pos2(500.0, 130.0)), false, &mut |ui| {
            let _ = tl.ui(ui, &cx);
            let _ = bin.ui(ui, &cx);
        });
        // The last layout answers "what is under this point" for the app (file drops).
        let lanes = tl.lanes_rect().expect("laid out");
        let t = tl.time_at(lanes.left() + 50.0).expect("over the lanes");
        assert!(t >= 0.0);
        assert!(tl.time_at(lanes.left() - 5.0).is_none());
        let top_row = tl.track_at(lanes.top() + 5.0);
        let topmost = p.tracks.iter().rev().find(|t| t.kind == aiframes_core::TrackKind::Video).map(|t| t.id.as_str());
        assert_eq!(top_row.as_deref(), topmost, "the top row is the topmost video track");
        tl.fit(100.0);
        assert!((tl.time_at(lanes.right() - 1.0).unwrap() - 100.0 / 0.94).abs() < 1.0);
    }
}

#[cfg(test)]
mod regress {
    //! Widget-level regressions from the independent review (ported from its `ui_*` repros): the
    //! timeline driven headlessly with synthetic pointer/key events.
    use super::*;
    use aiframes_core::{resolve, Op, Project};
    use egui::{pos2, vec2, Event, Modifiers, PointerButton, Pos2, RawInput, Rect};
    use serde_json::json;

    fn base(fps: f64, clips: serde_json::Value) -> Project {
        serde_json::from_value(json!({
            "format": 1,
            "settings": {"width": 1920, "height": 1080, "fps": fps},
            "media": [
                {"id": "mv", "path": "mv.mp4", "kind": "video", "info": {"duration": 120.0, "width": 1920, "height": 1080, "fps": fps, "has_video": true, "has_audio": true}},
                {"id": "ma", "path": "ma.wav", "kind": "audio", "info": {"duration": 60.0, "has_audio": true, "audio_channels": 2, "audio_rate": 48000}}
            ],
            "tracks": [{"id": "v1", "kind": "video"}, {"id": "v2", "kind": "video"}, {"id": "a1", "kind": "audio"}],
            "clips": clips
        }))
        .expect("project json")
    }

    fn cx<'a>(p: &'a Project, r: &'a aiframes_core::Resolved, playhead: f64, sel: &'a [String]) -> EditCtx<'a> {
        EditCtx { project: p, resolved: r, playhead, playing: false, selection: sel, preview: &NoPreview, ai_highlight: &[], ai_flash: 0.0, history: &[], revision: 0, can_undo: false, can_redo: false, in_point: None, out_point: None }
    }

    struct Harness {
        ctx: egui::Context,
        time: f64,
    }

    impl Harness {
        fn new() -> Self {
            Harness { ctx: egui::Context::default(), time: 0.0 }
        }
        fn frame(&mut self, tl: &mut timeline::Timeline, cx: &EditCtx, events: Vec<Event>) -> Vec<UiAction> {
            self.time += 1.0 / 60.0;
            let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))), time: Some(self.time), events, ..Default::default() };
            let mut acts = Vec::new();
            let mut out = self.ctx.run_ui(input, |ui| acts.extend(tl.ui(ui, cx)));
            out.textures_delta.clear();
            acts
        }
        fn row_y(tl: &timeline::Timeline, track: &str) -> (f32, f32) {
            let lanes = tl.lanes_rect().expect("laid out");
            let (mut y0, mut y1) = (None, None);
            let mut y = lanes.top();
            while y < lanes.bottom() {
                if tl.track_at(y).as_deref() == Some(track) {
                    y0.get_or_insert(y);
                    y1 = Some(y);
                }
                y += 0.5;
            }
            (y0.expect("row visible"), y1.unwrap())
        }
        fn press_drag(&mut self, tl: &mut timeline::Timeline, cx: &EditCtx, from: Pos2, to: Pos2) -> Vec<UiAction> {
            let mut acts = Vec::new();
            acts.extend(self.frame(tl, cx, vec![Event::PointerMoved(from)]));
            acts.extend(self.frame(tl, cx, vec![Event::PointerButton { pos: from, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE }]));
            for k in 1..=8 {
                acts.extend(self.frame(tl, cx, vec![Event::PointerMoved(from + (to - from) * (k as f32 / 8.0))]));
            }
            acts.extend(self.frame(tl, cx, vec![Event::PointerButton { pos: to, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE }]));
            acts.extend(self.frame(tl, cx, vec![]));
            acts
        }
    }

    fn ops_of(acts: &[UiAction]) -> Vec<Op> {
        acts.iter().flat_map(|a| if let UiAction::Apply { ops, .. } = a { ops.clone() } else { vec![] }).collect()
    }

    #[test]
    fn trim_drag_on_a_subframe_clip_does_not_panic() {
        let p = base(30.0, json!([
            {"id": "c1", "track": "v1", "start": 0.0, "duration": 4.0, "source": {"type": "media", "asset": "mv", "in_point": 10.0}},
            {"id": "t1", "track": "v2", "start": 1.0, "duration": 0.033, "source": {"type": "media", "asset": "mv", "in_point": 0.0}}
        ]));
        let r = resolve(&p);
        let c = cx(&p, &r, 0.0, &[]);
        let mut tl = timeline::Timeline::default();
        let mut h = Harness::new();
        h.frame(&mut tl, &c, vec![]);
        tl.fit(4.0);
        tl.zoom = 2000.0;
        tl.scroll = 0.9;
        h.frame(&mut tl, &c, vec![]);
        let lanes = tl.lanes_rect().unwrap();
        let (ya, yb) = Harness::row_y(&tl, "v2");
        let x0 = lanes.left() + ((1.0 - tl.scroll) * tl.zoom) as f32;
        let from = pos2(x0 + 2.0, (ya + yb) / 2.0 + 6.0);
        // Start edge, end edge, and a roll on the same tiny clip: none may panic.
        h.press_drag(&mut tl, &c, from, pos2(x0 + 30.0, from.y));
        h.press_drag(&mut tl, &c, pos2(x0 + 64.0, from.y), pos2(x0 + 10.0, from.y));
        tl.tool = timeline::Tool::Roll;
        h.press_drag(&mut tl, &c, from, pos2(x0 - 30.0, from.y));
    }

    #[test]
    fn sideways_volume_line_drag_keeps_the_gain() {
        let p = base(30.0, json!([
            {"id": "c3", "track": "a1", "start": 0.0, "duration": 20.0, "volume": 8.0, "source": {"type": "media", "asset": "ma", "in_point": 0.0}}
        ]));
        let r = resolve(&p);
        let sel = vec!["c3".to_string()];
        let c = cx(&p, &r, 0.0, &sel);
        let mut tl = timeline::Timeline::default();
        let mut h = Harness::new();
        h.frame(&mut tl, &c, vec![]);
        h.frame(&mut tl, &c, vec![]);
        let lanes = tl.lanes_rect().unwrap();
        let (ya, yb) = Harness::row_y(&tl, "a1");
        // Grab the line wherever it is (+18 dB) and drag it sideways only: no change.
        let content_top = ya + 16.0;
        let content_bottom = yb - 1.0;
        let lane = timeline::keys::Lane { top: content_top, bottom: content_bottom, scale: timeline::keys::LaneScale::Gain };
        let y = lane.y(8.0);
        for from_y in [y, ya + 19.0] {
            let from = pos2(lanes.center().x, from_y);
            let acts = h.press_drag(&mut tl, &c, from, pos2(from.x + 40.0, from.y));
            assert!(ops_of(&acts).is_empty(), "sideways drag at y {from_y} emitted {:?}", ops_of(&acts));
        }
        // A vertical drag (past egui's drag threshold) changes it, as a dB delta from +18 dB.
        let from = pos2(lanes.center().x, y);
        let ops = ops_of(&h.press_drag(&mut tl, &c, from, pos2(from.x, from.y + 10.0)));
        match ops.as_slice() {
            [Op::SetParam { value: Some(v), .. }] => assert!(*v < 8.0 && *v > 0.0, "{v}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ntsc_trims_and_moves_land_where_the_core_snaps() {
        // Round 4: video spans are frame-accurate in the core. A trim/move the timeline emits must land
        // on the same frame the ghost showed — the emitted time is already on the project's grid.
        let p = base(29.97, json!([
            {"id": "c1", "track": "v1", "start": 0.0, "duration": 4.0, "source": {"type": "media", "asset": "mv", "in_point": 10.0}},
            {"id": "t1", "track": "v2", "start": 1.0, "duration": 1.0, "source": {"type": "text", "text": "x"}}
        ]));
        let r = resolve(&p);
        let on_grid = |t: f64| ((t * 30000.0 / 1001.0) - (t * 30000.0 / 1001.0).round()).abs() < 1e-6;
        assert!(on_grid(r.span("t1").unwrap().start) && (r.span("t1").unwrap().start - 1.001).abs() < 1e-9, "the core snaps 1.0 to frame 30");
        let c = cx(&p, &r, 0.0, &[]);
        let mut tl = timeline::Timeline::default();
        let mut h = Harness::new();
        h.frame(&mut tl, &c, vec![]);
        tl.zoom = 137.0;
        tl.scroll = 0.0;
        tl.snapping = false;
        h.frame(&mut tl, &c, vec![]);
        let lanes = tl.lanes_rect().unwrap();
        let (ya, yb) = Harness::row_y(&tl, "v1");
        let y = (ya + yb) / 2.0 + 6.0;
        let x_end = lanes.left() + (r.span("c1").unwrap().end * tl.zoom) as f32;
        let ops = ops_of(&h.press_drag(&mut tl, &c, pos2(x_end - 2.0, y), pos2(x_end - 2.0 - 71.3, y)));
        let [Op::TrimClip { to: Some(to), .. }] = ops.as_slice() else { panic!("{ops:?}") };
        assert!(on_grid(*to), "trim to {to} is on the 30000/1001 grid");
        let mut q = p.clone();
        aiframes_core::apply_batch(&mut q, &ops).unwrap();
        assert!((resolve(&q).span("c1").unwrap().end - to).abs() < 1e-9, "the core lands the edge where the ghost was");
        // Moving the text: the emitted start is on the grid too.
        let (ya, yb) = Harness::row_y(&tl, "v2");
        let y = (ya + yb) / 2.0 + 6.0;
        let x = lanes.left() + (1.5 * tl.zoom) as f32;
        let ops = ops_of(&h.press_drag(&mut tl, &c, pos2(x, y), pos2(x + 53.7, y)));
        let [Op::MoveClip { start: aiframes_core::Time::At(s), .. }] = ops.as_slice() else { panic!("{ops:?}") };
        assert!(on_grid(*s), "move to {s}");
        let mut q = p.clone();
        aiframes_core::apply_batch(&mut q, &ops).unwrap();
        assert!((resolve(&q).span("t1").unwrap().start - s).abs() < 1e-9);
    }

    #[test]
    fn ripple_trim_key_respects_the_selection() {
        let p = base(30.0, json!([
            {"id": "c1", "track": "v1", "start": 0.0, "duration": 10.0, "source": {"type": "media", "asset": "mv", "in_point": 0.0}},
            {"id": "c3", "track": "a1", "start": 0.0, "duration": 20.0, "source": {"type": "media", "asset": "ma", "in_point": 0.0}},
            {"id": "t1", "track": "v2", "start": 15.0, "duration": 3.0, "source": {"type": "text", "text": "title"}}
        ]));
        let r = resolve(&p);
        let sel = vec!["t1".to_string()];
        let c = cx(&p, &r, 4.0, &sel);
        let mut tl = timeline::Timeline::default();
        let mut h = Harness::new();
        h.frame(&mut tl, &c, vec![]);
        let lanes = tl.lanes_rect().unwrap();
        h.frame(&mut tl, &c, vec![Event::PointerMoved(pos2(lanes.center().x, lanes.bottom() - 4.0))]);
        for key in [egui::Key::W, egui::Key::Q, egui::Key::S] {
            let ev = |pressed| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE };
            let acts = h.frame(&mut tl, &c, vec![ev(true), ev(false)]);
            assert!(ops_of(&acts).is_empty(), "{key:?} with only t1 selected (not under the playhead): {acts:?}");
        }
    }
}
