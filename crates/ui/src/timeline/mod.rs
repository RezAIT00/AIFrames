//! The multi-track timeline: toolbar, ruler (adaptive timecode ticks, in/out, markers, chapters),
//! track headers (mute/solo/lock/hide, volume, resize), clips (filmstrips, waveforms, fades,
//! keyframes, badges, anchors, errors, AI glow), transitions, playhead, snapping, zoom/scroll, the
//! Select/Razor/Slip/Slide/Ripple/Roll tools, keyframe lanes and the volume line of selected clips,
//! track reordering by dragging headers, drag-and-drop from the bin and the effects browser, context
//! menus and keyboard shortcuts.
//!
//! Every manipulation is previewed live (the ops are applied to a copy of the project and the
//! result is drawn as ghosts) and emits exactly one [`UiAction::Apply`] on release.

pub mod drag;
mod draw;
pub mod geom;
mod headers;
mod input;
pub mod keys;
mod menus;
pub mod snap;
pub mod wave;

use crate::edits;
use crate::util::{self, ClipKind};
use crate::{BrowserDrag, EditCtx, MediaDrag, UiAction};
use aiframes_core::{Edge, Id, Op, Project, Resolved, Span, TrackKind};
use egui::{pos2, vec2, CursorIcon, Key, Modifiers, PointerButton, Pos2, Rect, Response, Sense, Ui};
use geom::{ClipBox, Row, TimeMap, TransBox, TransZone, Zone};
use std::collections::{HashMap, HashSet};

pub const TOOLBAR_H: f32 = 26.0;
pub const RULER_H: f32 = 30.0;
const BAR: f32 = 10.0;
const EDGE_PX: f32 = 6.0;
/// Height of the name bar at the top of a clip.
pub(crate) const LABEL_H: f32 = 14.0;
/// Extra room below the last track (drop here to create a track).
const DROP_ZONE: f32 = 56.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tool {
    #[default]
    Select,
    Razor,
    Slip,
    Ripple,
    Roll,
    /// (added, round 2) Moves a clip between its neighbours; they absorb the change.
    Slide,
}

impl Tool {
    /// (added)
    pub const ALL: [Tool; 6] = [Tool::Select, Tool::Razor, Tool::Slip, Tool::Slide, Tool::Ripple, Tool::Roll];

    /// (added)
    pub fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select",
            Tool::Razor => "Razor",
            Tool::Slip => "Slip",
            Tool::Ripple => "Ripple",
            Tool::Roll => "Roll",
            Tool::Slide => "Slide",
        }
    }

    /// (added) Keyboard shortcut.
    pub fn key(self) -> &'static str {
        match self {
            Tool::Select => "V",
            Tool::Razor => "C",
            Tool::Slip => "Y",
            Tool::Ripple => "B",
            Tool::Roll => "R",
            Tool::Slide => "U",
        }
    }

    /// (added)
    pub fn help(self) -> &'static str {
        match self {
            Tool::Select => "Select, move (Alt = copy, Ctrl = place absolutely, Shift = lock axis) and trim (magnetic).",
            Tool::Razor => "Click to split a clip; Shift+click splits every track.",
            Tool::Slip => "Drag a clip to change which part of its media plays; Alt+drag slides it between its neighbours.",
            Tool::Ripple => "Trim an edge and move everything after it on the track.",
            Tool::Roll => "Drag a cut between two clips: one gets longer, the other shorter.",
            Tool::Slide => "Drag a clip between its neighbours: the clip before gets longer, the one after shorter (or the reverse); its own media stays.",
        }
    }
}

/// The multi-track timeline: ruler, tracks (video top, audio below) with headers (mute/solo/lock/
/// hide), clips with filmstrips/waveforms/names/fades/keyframe ticks, transitions, markers,
/// playhead, in/out, snapping, zoom/scroll, move/trim/split/ripple/roll/slip, multi-select,
/// drop media, context menus, keyboard shortcuts when focused.
pub struct Timeline {
    /// Pixels per second.
    pub zoom: f64,
    /// Timeline seconds at the left edge of the lanes.
    pub scroll: f64,
    pub snapping: bool,
    pub tool: Tool,
    /// (added) Vertical scroll of the track area (points).
    pub scroll_y: f32,
    /// (added) Width of the track header column.
    pub header_width: f32,
    /// (added) Row heights by track id (default per kind).
    pub track_heights: HashMap<Id, f32>,
    /// (added) Clicking a clip also selects its linked clips (detached audio).
    pub linked_selection: bool,
    /// (added)
    pub show_waveforms: bool,
    /// (added)
    pub show_filmstrips: bool,
    /// (added) Page-scroll to keep the playhead visible while playing.
    pub follow_playhead: bool,
    drag: Option<DragState>,
    press: Option<Press>,
    preview: Option<Preview>,
    waves: draw::WaveCache,
    menu: Option<MenuTarget>,
    rename: Option<Rename>,
    fitted: bool,
    focused: bool,
    lanes_width: f32,
    volume_live: Option<(Id, f64)>,
    snap_line: Option<f64>,
    hover: Option<Hit>,
    hover_pos: Option<Pos2>,
    total: f64,
    last: Option<LastLayout>,
    /// Which parameter's keyframe lane each clip shows (set from the clip menu or the inspector).
    lane_pref: HashMap<Id, String>,
    key_sel: Option<KeySel>,
    alt_down: bool,
    /// Screen y of the insertion line while a track header is dragged.
    reorder_line: Option<f32>,
    /// Worst validation severity per clip (badges), refreshed every frame from the cached findings.
    flags: HashMap<Id, aiframes_core::Severity>,
    /// Clips used as another clip's track matte (drawn dimmed and striped: not drawn themselves).
    matte_src: HashSet<Id>,
    /// The open "Set spacing" dialog.
    spacing: Option<SpacingDlg>,
    /// [`Timeline::open_spacing`] asked for the dialog (opened next frame, when the project is known).
    spacing_request: Option<Option<Id>>,
    /// A range to bring into view next frame ([`Timeline::reveal`]).
    reveal_to: Option<(f64, f64)>,
    /// Tracks to scroll into view next frame ([`Timeline::reveal`], [`Timeline::reveal_track`]).
    reveal_tracks: Vec<Id>,
    /// A running scroll/zoom animation.
    anim: Option<ViewAnim>,
}

/// A short eased scroll/zoom animation (zoom interpolates geometrically).
#[derive(Clone, Copy, Debug)]
struct ViewAnim {
    from: (f64, f64),
    to: (f64, f64),
    t0: f64,
    dur: f64,
}

/// The "Set spacing" dialog (track header / lane menu): every gap to a value or scaled, optionally
/// clamped, previewed live on the timeline.
#[derive(Clone, Debug)]
pub(crate) struct SpacingDlg {
    pub track: Id,
    /// Selected clips on the track (used when `only_selected`).
    pub selected: Vec<Id>,
    pub only_selected: bool,
    pub by_scale: bool,
    pub gap: f64,
    /// Percent.
    pub scale: f64,
    pub min_gap: Option<f64>,
    pub max_gap: Option<f64>,
    pub pos: Pos2,
}

impl SpacingDlg {
    /// The `set_spacing` op for the dialog's values.
    pub fn op(&self) -> Op {
        let clips = if self.only_selected && self.selected.len() >= 2 { self.selected.clone() } else { vec![] };
        edits::spacing_op(&self.track, clips, if self.by_scale { None } else { Some(self.gap) }, if self.by_scale { Some(self.scale / 100.0) } else { None }, self.min_gap, self.max_gap)
    }
}

/// The keyframe selected on the timeline (Delete removes it).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct KeySel {
    pub clip: Id,
    pub path: String,
    pub t: f64,
}

/// A keyframe lane (or the volume line) drawn over a selected clip.
#[derive(Clone, Debug)]
pub(crate) struct Overlay {
    pub clip: Id,
    pub path: String,
    pub label: String,
    pub lane: keys::Lane,
    /// The clip's content area (below its name bar).
    pub rect: Rect,
    /// Timeline start and length of the clip.
    pub start: f64,
    pub dur: f64,
    pub param: aiframes_core::Param,
    pub volume: bool,
    /// Allowed values (the registry range).
    pub range: (f64, f64),
    pub spec: Option<&'static aiframes_core::ParamSpec>,
}

impl Overlay {
    pub fn keys(&self) -> &[aiframes_core::Key] {
        match &self.param {
            aiframes_core::Param::Animated { keys } => keys,
            _ => &[],
        }
    }
    pub fn key_pos(&self, tmap: &TimeMap, k: &aiframes_core::Key) -> Pos2 {
        pos2(tmap.x(self.start + k.t), self.lane.y(k.v))
    }
    /// Screen y of the curve at screen x.
    pub fn line_y(&self, tmap: &TimeMap, x: f32) -> f32 {
        let tl = util::clamp_safe(tmap.t(x) - self.start, 0.0, self.dur);
        self.lane.y(aiframes_core::eval::sample(&self.param, tl))
    }
}

/// The content area of a clip box (below the name bar when there is one).
pub(crate) fn content_of(rect: Rect) -> Rect {
    if rect.height() >= 30.0 {
        Rect::from_min_max(pos2(rect.left(), rect.top() + LABEL_H), rect.max).shrink2(vec2(0.0, 1.0))
    } else {
        rect
    }
}

/// (added, round 2) Shows `path`'s keyframe lane on `clip` in the timeline (the inspector calls this
/// when a keyframe toggle or curve view is used; the app may too).
pub fn show_keyframe_lane(ctx: &egui::Context, clip: &str, path: &str) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::unique("aiframes_lane_param"), (clip.to_string(), path.to_string())));
}

/// The geometry of the last frame, for [`Timeline::time_at`] / [`Timeline::track_at`].
#[derive(Clone, Debug)]
struct LastLayout {
    lanes: Rect,
    y0: f32,
    rows: Vec<(Id, f32, f32)>,
}

impl Default for Timeline {
    fn default() -> Self {
        Timeline {
            zoom: 60.0,
            scroll: 0.0,
            snapping: true,
            tool: Tool::Select,
            scroll_y: 0.0,
            header_width: 176.0,
            track_heights: HashMap::new(),
            linked_selection: true,
            show_waveforms: true,
            show_filmstrips: true,
            follow_playhead: true,
            drag: None,
            press: None,
            preview: None,
            waves: Default::default(),
            menu: None,
            rename: None,
            fitted: false,
            focused: false,
            lanes_width: 800.0,
            volume_live: None,
            snap_line: None,
            hover: None,
            hover_pos: None,
            total: 0.0,
            last: None,
            lane_pref: HashMap::new(),
            key_sel: None,
            alt_down: false,
            reorder_line: None,
            flags: HashMap::new(),
            matte_src: HashSet::new(),
            spacing: None,
            spacing_request: None,
            reveal_to: None,
            reveal_tracks: Vec::new(),
            anim: None,
        }
    }
}

/// What is under the pointer in the lanes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Hit {
    Clip { id: Id, zone: Zone },
    Trans { id: Id, zone: TransZone },
    Row { row: usize, t: f64 },
    Below { t: f64 },
    /// A keyframe diamond of a selected clip's lane (index into the lane's keys).
    Key { clip: Id, path: String, index: usize },
    /// The curve of a lane: the volume line (drag = volume, Ctrl+click adds a key) or, with Alt, a
    /// keyframe lane's curve (click adds a key).
    Line { clip: Id, path: String },
}

#[derive(Clone, Debug)]
struct Press {
    hit: Hit,
    pos: Pos2,
    mods: Modifiers,
    /// Selection to apply on release when the press turns out to be a click (pressing an already
    /// selected clip keeps the multi-selection for dragging).
    click_select: Option<Vec<Id>>,
}

#[derive(Clone, Debug)]
struct DragState {
    kind: DragKind,
    origin: Pos2,
    origin_t: f64,
}

#[derive(Clone, Debug)]
enum DragKind {
    Move { ids: Vec<Id>, grabbed: Id, kind: TrackKind, grab_row: usize },
    Trim { id: Id, edge: Edge, ripple: Option<bool>, offset: f64 },
    Roll { left: Id, right: Id, cut: f64, offset: f64 },
    Slip { id: Id },
    Slide { id: Id },
    Fade { id: Id, out: bool },
    TransEdge { id: Id, cut: f64 },
    Marker { id: Id, offset: f64 },
    Scrub,
    Box { additive: bool, base: Vec<Id>, last: Vec<Id> },
    Pan { scroll: f64, scroll_y: f32 },
    RowResize { track: Id, h0: f32 },
    HBar { grab: f32 },
    VBar { grab: f32 },
    Key { clip: Id, path: String, index: usize, lane: keys::Lane, start: f64, dur: f64, keys0: Vec<aiframes_core::Key>, range: (f64, f64), label: String, spec: Option<&'static aiframes_core::ParamSpec> },
    Volume { clip: Id, lane: keys::Lane, param0: aiframes_core::Param, g0: f64 },
    TrackReorder { track: Id },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    Drag,
    Drop,
    /// A dialog's live preview (Set spacing).
    Dialog,
}

/// The result of applying the pending ops to a copy of the project (drawn as ghosts).
pub(crate) struct Sim {
    pub project: Project,
    pub resolved: Resolved,
    /// Clips created or whose span, track or content changed.
    pub changed: HashSet<Id>,
    pub removed: HashSet<Id>,
    /// The ops' outcome summaries (e.g. the gaps `set_spacing` changed).
    pub summary: String,
}

struct Preview {
    owner: Owner,
    key: String,
    label: String,
    ops: Vec<Op>,
    result: Result<Sim, String>,
    /// Where the edit is expected to put things (drawn red when the edit is refused).
    hints: Vec<(Id, Span)>,
    info: String,
    /// Draw the result as clip ghosts (false for keyframe/volume/track edits, drawn in place).
    ghost: bool,
}

/// Applies the ops to a copy of the project.
pub(crate) fn simulate(p: &Project, r: &Resolved, ops: &[Op]) -> Result<Sim, String> {
    let mut q = p.clone();
    let outcomes = aiframes_core::apply_batch(&mut q, ops).map_err(|e| e.to_string())?;
    let summary = outcomes.iter().map(|o| o.summary.as_str()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("; ");
    let rq = aiframes_core::resolve(&q);
    let before: HashMap<&str, &aiframes_core::Clip> = p.clips.iter().map(|c| (c.id.as_str(), c)).collect();
    let changed: HashSet<Id> = q.clips.iter().filter(|c| before.get(c.id.as_str()).copied() != Some(c) || r.span(&c.id) != rq.span(&c.id)).map(|c| c.id.clone()).collect();
    let after: HashSet<&str> = q.clips.iter().map(|c| c.id.as_str()).collect();
    let removed: HashSet<Id> = p.clips.iter().filter(|c| !after.contains(c.id.as_str())).map(|c| c.id.clone()).collect();
    Ok(Sim { project: q, resolved: rq, changed, removed, summary })
}

#[derive(Clone, Debug)]
pub(crate) enum MenuTarget {
    Clip(Id),
    Trans(Id),
    Row { track: Id, t: f64 },
    Below { t: f64 },
    Marker(Id),
    Ruler { t: f64 },
    Key { clip: Id, path: String, t: f64 },
}

#[derive(Clone, Debug)]
pub(crate) enum RenameTarget {
    Clip(Id),
    Marker(Id),
    Track(Id),
}

pub(crate) struct Rename {
    target: RenameTarget,
    text: String,
    pos: Pos2,
    opened: bool,
}

/// Geometry of this frame.
pub(crate) struct Layout {
    pub toolbar: Rect,
    pub corner: Rect,
    pub ruler: Rect,
    pub headers: Rect,
    pub lanes: Rect,
    pub hbar: Rect,
    pub vbar: Rect,
    pub rows: Vec<Row>,
    /// Screen y of row coordinate 0.
    pub y0: f32,
    pub tmap: TimeMap,
    pub boxes: Vec<ClipBox>,
    pub tboxes: Vec<TransBox>,
    pub markers: Vec<(Id, f32)>,
    pub fps: f64,
    pub content_h: f32,
    pub overlays: Vec<Overlay>,
}

impl Layout {
    pub fn row_rect(&self, i: usize) -> Rect {
        let r = &self.rows[i];
        Rect::from_min_max(pos2(self.lanes.left(), self.y0 + r.y), pos2(self.lanes.right(), self.y0 + r.bottom()))
    }
    pub fn header_rect(&self, i: usize) -> Rect {
        let r = &self.rows[i];
        Rect::from_min_max(pos2(self.headers.left(), self.y0 + r.y), pos2(self.headers.right(), self.y0 + r.bottom()))
    }
    pub fn row_of_track(&self, track: &str) -> Option<usize> {
        self.rows.iter().position(|r| r.track == track)
    }
    pub fn row_at(&self, y: f32) -> Option<usize> {
        geom::row_at(&self.rows, y - self.y0)
    }
    /// Screen y below the last row.
    pub fn rows_bottom(&self) -> f32 {
        self.y0 + geom::rows_height(&self.rows)
    }
    pub fn clip_rect(&self, row: usize, s: Span) -> Rect {
        let rr = self.row_rect(row);
        let x0 = self.tmap.x(s.start);
        let x1 = self.tmap.x(s.end).max(x0 + 1.0);
        Rect::from_min_max(pos2(x0, rr.top() + 1.0), pos2(x1, rr.bottom() - 1.0))
    }
    /// The band of a transition inside its row.
    pub fn trans_rect(&self, row: usize, cut: f64, d: f64) -> Rect {
        let rr = self.row_rect(row);
        let top = rr.top() + 1.0 + if rr.height() >= 34.0 { LABEL_H } else { 0.0 };
        let h = (rr.height() - (top - rr.top()) - 3.0).clamp(6.0, 20.0);
        Rect::from_min_max(pos2(self.tmap.x(cut - d * 0.5), top + 1.0), pos2(self.tmap.x(cut + d * 0.5).max(self.tmap.x(cut - d * 0.5) + 4.0), top + 1.0 + h))
    }
}

/// The hover text of a clip: what it is, where, its source, anchor, effects and problems.
pub(crate) fn clip_tooltip(cx: &EditCtx, id: &str) -> String {
    let p = cx.project;
    let r = cx.resolved;
    let Some(c) = p.clips.iter().find(|c| c.id == id) else { return String::new() };
    let fps = util::fps(p);
    let mut lines = vec![format!("{}  ({id}, {:?})", util::clip_label(p, c), util::clip_kind(p, c))];
    match r.span(id) {
        Some(s) => lines.push(format!("{} – {}  ·  {}", util::timecode(s.start, fps), util::timecode(s.end, fps), util::short_time(s.duration()))),
        None => {
            if let Some(e) = r.error_for(id) {
                lines.push(format!("Unresolved: {}", e.message));
            }
        }
    }
    if let Some((a, b)) = aiframes_core::eval::source_span(c) {
        lines.push(format!("source {} – {}", util::timecode(a, fps), util::timecode(b, fps)));
    }
    if let aiframes_core::Time::Anchored(a) = &c.start {
        lines.push(format!("anchored {}", aiframes_core::resolve::describe_anchor(a)));
    }
    if let Some(u) = &c.until {
        lines.push(format!("until {}", aiframes_core::until_text(u)));
    }
    // Round 6: the look tools.
    if let Some(mb) = &c.motion_blur {
        lines.push(format!("motion blur {}° · {} samples", mb.shutter, mb.samples));
    }
    if c.frame_blend {
        lines.push("frame blend (smooth speed-ups)".into());
    }
    if edits::is_3d(c) || c.transform.perspective != 2000.0 {
        let s = |prm: &aiframes_core::Param| match prm {
            aiframes_core::Param::Value(v) => format!("{v}°"),
            aiframes_core::Param::Animated { keys } => format!("{} keys", keys.len()),
        };
        lines.push(format!("3-D tilt {} · turn {} · perspective {} px", s(&c.transform.rotation_x), s(&c.transform.rotation_y), c.transform.perspective));
    }
    if let Some(m) = &c.matte {
        lines.push(format!("matte: {} ({})", m.clip, crate::inspector::matte_mode_label(m.mode).to_lowercase()));
    }
    let users = edits::matte_users(p, id);
    if !users.is_empty() {
        lines.push(format!("used as a matte by {} (not drawn itself)", users.join(", ")));
    }
    match &c.speed {
        aiframes_core::Param::Value(s) if (*s - 1.0).abs() > 1e-9 => lines.push(format!("speed {s}×{}", if c.reverse { ", reversed" } else { "" })),
        aiframes_core::Param::Animated { keys } => lines.push(format!("speed ramp ({} keys)", keys.len())),
        _ if c.reverse => lines.push("reversed".into()),
        _ => {}
    }
    if !c.effects.is_empty() {
        lines.push(format!("effects: {}", c.effects.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if c.fade_in > 0.0 || c.fade_out > 0.0 {
        lines.push(format!("fades {:.2} s / {:.2} s", c.fade_in, c.fade_out));
    }
    if !c.enabled {
        lines.push("disabled".into());
    }
    if !c.notes.is_empty() {
        lines.push(format!("“{}”", util::ellipsize(&c.notes, 160)));
    }
    if cx.is_ai(id) {
        lines.push("changed by the AI".into());
    }
    for f in util::clip_findings(&util::findings(p, r, cx.revision), id) {
        lines.push(format!("Problem: {}{}", f.message, if f.fix_ops.is_empty() { "" } else { " (right-click to fix)" }));
    }
    let lines: Vec<String> = lines.iter().map(|l| util::plain(l)).collect();
    lines.join("
")
}

/// What a pending edit of clip `id` does to end anchors (`until`): its own (shifted or cleared) and the
/// clips whose end follows it.
pub(crate) fn until_note(p: &Project, sim: &Sim, id: &str) -> Option<String> {
    let mut parts = Vec::new();
    let old = p.clips.iter().find(|c| c.id == id).and_then(|c| c.until.clone());
    let new = sim.project.clips.iter().find(|c| c.id == id).and_then(|c| c.until.clone());
    match (&old, &new) {
        (Some(o), Some(n)) if o != n => parts.push(format!("end anchor now until {}", aiframes_core::until_text(n))),
        (Some(_), None) => parts.push("end anchor cleared (fixed length now)".to_string()),
        _ => {}
    }
    let mut followers: Vec<&str> = sim
        .project
        .clips
        .iter()
        .filter(|c| c.id != id && sim.changed.contains(&c.id) && c.until.as_ref().and_then(util::anchor_parent) == Some(id))
        .map(|c| c.id.as_str())
        .collect();
    followers.sort();
    if !followers.is_empty() {
        parts.push(format!("{} end{} with it", followers.join(", "), if followers.len() == 1 { "s" } else { "" }));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn shift_t(t: f64) -> String {
    let s = if t < 0.0 { "-" } else { "+" };
    format!("{s}{}", util::short_time(t.abs()))
}

impl Timeline {
    /// (added) True when the timeline was the last panel clicked: it then handles (and consumes)
    /// its keys — S, Q, W, Delete, arrows, I/O, M, … — unless a text field has the keyboard. It never
    /// takes egui keyboard focus, so `ctx.egui_wants_keyboard_input()` stays false for app shortcuts.
    pub fn has_focus(&self) -> bool {
        self.focused
    }

    /// (added) True while a drag (move, trim, scrub, …) is in progress.
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// (added, round 2) Shows `path`'s keyframe lane on `clip` (see also [`show_keyframe_lane`]).
    pub fn show_lane(&mut self, clip: &str, path: &str) {
        self.lane_pref.insert(clip.to_string(), path.to_string());
    }

    /// (added) Zoom so the whole edit fits (next frame).
    pub fn fit_all(&mut self) {
        self.fitted = false;
    }

    /// (added) Zoom/scroll now so `[0, duration]` fits the lanes of the last frame.
    pub fn fit(&mut self, duration: f64) {
        let w = self.last.as_ref().map(|l| l.lanes.width()).unwrap_or(self.lanes_width);
        let (s, z) = geom::fit(duration, w, geom::MIN_ZOOM, geom::max_zoom(240.0));
        self.scroll = s;
        self.zoom = z;
        self.fitted = true;
    }

    /// (added) Timeline seconds under screen x, when x is over the track lanes (last frame's layout).
    pub fn time_at(&self, x: f32) -> Option<f64> {
        let l = self.last.as_ref()?;
        let tmap = TimeMap { x0: l.lanes.left(), scroll: self.scroll, zoom: self.zoom };
        l.lanes.x_range().contains(x).then(|| tmap.t(x).max(0.0))
    }

    /// (added) The track whose row is under screen y (last frame's layout).
    pub fn track_at(&self, y: f32) -> Option<Id> {
        let l = self.last.as_ref()?;
        if !l.lanes.y_range().contains(y) {
            return None;
        }
        l.rows.iter().find(|(_, ry, h)| y >= l.y0 + ry && y < l.y0 + ry + h).map(|(id, _, _)| id.clone())
    }

    /// (added) Screen rect of the track lanes in the last frame.
    pub fn lanes_rect(&self) -> Option<Rect> {
        self.last.as_ref().map(|l| l.lanes)
    }

    /// (added, round 4) The timeline seconds visible in the lanes (last frame's width; the target of a
    /// running [`Timeline::reveal`] animation, so callers see where the view is going).
    pub fn visible_range(&self) -> (f64, f64) {
        let (scroll, zoom) = self.anim.map(|a| a.to).unwrap_or((self.scroll, self.zoom));
        let w = self.last.as_ref().map(|l| l.lanes.width()).unwrap_or(self.lanes_width) as f64;
        (scroll, scroll + w / zoom.max(1e-9))
    }

    /// (added, round 4) Brings `[t0, t1]` and the rows of `tracks` into view next frame with the
    /// smallest change: nothing when they are visible, else a scroll, and a zoom out only when the
    /// range is wider than the lanes (a short eased animation); rows scroll vertically (when they
    /// cannot all fit, the topmost is shown). For "follow the AI": pass the span and tracks of what
    /// the AI just changed. An empty `tracks` leaves the vertical scroll alone.
    pub fn reveal(&mut self, t0: f64, t1: f64, tracks: &[Id]) {
        if t0.is_finite() && t1.is_finite() {
            self.reveal_to = Some((t0.min(t1), t0.max(t1)));
        }
        self.reveal_tracks.extend(tracks.iter().cloned());
    }

    /// (added, round 4) Scrolls the track rows so `track` is visible next frame.
    pub fn reveal_track(&mut self, track: &str) {
        self.reveal_tracks.push(track.to_string());
    }

    /// (added, round 5) Opens the "Set spacing" dialog (live ghost preview, one `set_spacing` on
    /// Apply) next frame, for `track` — or, with `None`, the track of the first selected clip, else
    /// the main track, else the first track with two clips or more. The dialog can switch tracks.
    pub fn open_spacing(&mut self, track: Option<Id>) {
        self.spacing_request = Some(track);
    }

    /// The track [`Timeline::open_spacing`] picks when none is given.
    pub(crate) fn spacing_track(cx: &EditCtx, track: Option<&str>) -> Option<Id> {
        let p = cx.project;
        let spaced = |id: &str| p.clips.iter().filter(|c| c.track == id).count() >= 2;
        track
            .filter(|t| p.tracks.iter().any(|x| x.id == *t))
            .map(str::to_string)
            .or_else(|| p.clips.iter().find(|c| cx.is_selected(&c.id) && spaced(&c.track)).map(|c| c.track.clone()))
            .or_else(|| edits::main_track(p).filter(|t| spaced(t)))
            .or_else(|| p.tracks.iter().find(|t| spaced(&t.id)).map(|t| t.id.clone()))
    }

    /// Opens the "Set spacing" dialog for `track` (selected clips on it can be spaced alone). It
    /// opens just above the timeline (its bottom-left at `pos` when the layout is unknown), so the
    /// ghosts of the result stay visible.
    pub(crate) fn open_spacing_at(&mut self, cx: &EditCtx, track: &str, pos: Pos2) {
        let pos = self.last.as_ref().map(|l| pos2(l.lanes.left() + 24.0, l.lanes.top() - RULER_H - TOOLBAR_H - 6.0)).unwrap_or(pos);
        let mut selected: Vec<(f64, Id)> = cx.project.clips.iter().filter(|c| c.track == track && cx.is_selected(&c.id)).map(|c| (cx.resolved.span(&c.id).map(|s| s.start).unwrap_or(0.0), c.id.clone())).collect();
        selected.sort_by(|a, b| a.0.total_cmp(&b.0));
        let selected: Vec<Id> = selected.into_iter().map(|(_, id)| id).collect();
        self.spacing = Some(SpacingDlg { track: track.into(), only_selected: selected.len() >= 2, selected, by_scale: false, gap: 0.25, scale: 70.0, min_gap: None, max_gap: None, pos });
    }

    /// (added, round 4) True while the "Set spacing" dialog is open.
    pub fn spacing_dialog_open(&self) -> bool {
        self.spacing.is_some()
    }

    fn ids_with_links(&self, cx: &EditCtx, id: &str) -> Vec<Id> {
        let mut v = vec![id.to_string()];
        if self.linked_selection {
            v.extend(aiframes_core::linked_partners(cx.project, id));
        }
        v
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: &EditCtx) -> Vec<UiAction> {
        let mut out = Vec::new();
        let avail = ui.available_size().max(vec2(320.0, 160.0));
        let (full, _) = ui.allocate_exact_size(avail, Sense::hover());
        let base_id = ui.scope_id().with("aiframes_timeline");
        let p = cx.project;
        let r = cx.resolved;
        let fps = util::fps(p);
        self.zoom = self.zoom.clamp(geom::MIN_ZOOM, geom::max_zoom(fps));
        self.snap_line = None;
        if let Some((clip, path)) = ui.ctx().data_mut(|d| d.remove_temp::<(String, String)>(egui::Id::unique("aiframes_lane_param"))) {
            self.lane_pref.insert(clip, path);
        }
        self.alt_down = ui.input(|i| i.modifiers.alt);
        self.matte_src = aiframes_core::matte_sources(p);
        // Validation badges (findings are cached until the project changes).
        self.flags.clear();
        for f in util::findings(p, r, cx.revision).iter().filter(|f| util::shown_on_clip(f)) {
            for id in &f.ids {
                let e = self.flags.entry(id.clone()).or_insert(f.severity);
                if f.severity < *e {
                    *e = f.severity;
                }
            }
        }

        // ----- regions -----
        let toolbar = Rect::from_min_size(full.min, vec2(full.width(), TOOLBAR_H));
        let body = Rect::from_min_max(pos2(full.left(), toolbar.bottom()), full.max);
        let hw = self.header_width.clamp(120.0, (body.width() * 0.45).max(120.0));
        let corner = Rect::from_min_size(body.min, vec2(hw, RULER_H));
        let ruler = Rect::from_min_max(pos2(body.left() + hw, body.top()), pos2(body.right() - BAR, body.top() + RULER_H));
        let tracks = Rect::from_min_max(pos2(body.left(), ruler.bottom()), pos2(body.right() - BAR, body.bottom() - BAR));
        let headers = Rect::from_min_max(tracks.min, pos2(tracks.left() + hw, tracks.bottom()));
        let lanes = Rect::from_min_max(pos2(headers.right(), tracks.top()), tracks.max);
        let hbar = Rect::from_min_max(pos2(lanes.left(), lanes.bottom()), pos2(lanes.right(), body.bottom()));
        let vbar = Rect::from_min_max(pos2(lanes.right(), lanes.top()), pos2(body.right(), lanes.bottom()));
        self.lanes_width = lanes.width();

        let rows = geom::layout_rows(p, &self.track_heights);
        let content_h = geom::rows_height(&rows) + DROP_ZONE;
        self.scroll_y = self.scroll_y.clamp(0.0, (content_h - lanes.height()).max(0.0));
        let duration = r.duration.max(r.content_end());
        if !self.fitted {
            if duration > 0.0 {
                let (s, z) = geom::fit(duration, lanes.width(), geom::MIN_ZOOM, geom::max_zoom(fps));
                self.scroll = s;
                self.zoom = z;
            }
            self.fitted = true;
        }
        // Follow the playhead while playing.
        if cx.playing && self.follow_playhead && self.drag.is_none() {
            let view = lanes.width() as f64 / self.zoom;
            if cx.playhead > self.scroll + view * 0.98 || cx.playhead < self.scroll {
                self.scroll = (cx.playhead - view * 0.05).max(0.0);
            }
        }
        // Reveal requests ("follow the AI") and their animation; reveal_track scrolls rows.
        let now = ui.input(|i| i.time);
        if let Some((a, b)) = self.reveal_to.take() {
            let (s, z) = geom::reveal(self.scroll, self.zoom, lanes.width() as f64, a, b, geom::MIN_ZOOM, geom::max_zoom(fps));
            if (s - self.scroll).abs() > 1e-9 || (z - self.zoom).abs() > 1e-9 * self.zoom {
                self.anim = Some(ViewAnim { from: (self.scroll, self.zoom), to: (s, z), t0: now, dur: 0.22 });
            }
        }
        if let Some(an) = self.anim {
            let k = if an.dur > 0.0 { ((now - an.t0) / an.dur).clamp(0.0, 1.0) } else { 1.0 };
            let e = 1.0 - (1.0 - k).powi(3);
            self.zoom = (an.from.1.ln() + (an.to.1.ln() - an.from.1.ln()) * e).exp();
            self.scroll = an.from.0 + (an.to.0 - an.from.0) * e;
            if k >= 1.0 {
                (self.scroll, self.zoom) = an.to;
                self.anim = None;
            } else {
                ui.ctx().request_repaint();
            }
        }
        if !self.reveal_tracks.is_empty() {
            let ids = std::mem::take(&mut self.reveal_tracks);
            let picked: Vec<&Row> = rows.iter().filter(|r| ids.contains(&r.track)).collect();
            if let (Some(top), Some(bottom)) = (picked.iter().map(|r| r.y).reduce(f32::min), picked.iter().map(|r| r.y + r.h).reduce(f32::max)) {
                let h = lanes.height();
                self.scroll_y = geom::reveal_rows(self.scroll_y, h, top, bottom).clamp(0.0, (content_h - h).max(0.0));
            }
        }
        let view = lanes.width() as f64 / self.zoom;
        let total = duration.max(cx.playhead).max(self.anim.map(|a| a.to.0 + view).unwrap_or(0.0)) + view * 0.5;
        self.scroll = self.scroll.clamp(0.0, (total - view * 0.5).max(0.0));
        self.total = total.max(self.scroll + view);

        let mut lay = self.layout(cx, toolbar, corner, ruler, headers, lanes, hbar, vbar, rows, fps, content_h);

        // ----- toolbar -----
        self.toolbar(ui, cx, &lay, duration, &mut out);

        // ----- interaction surfaces -----
        let lane_resp = ui.interact(lanes, base_id.with("lanes"), Sense::click_and_drag());
        let ruler_resp = ui.interact(ruler, base_id.with("ruler"), Sense::click_and_drag());
        let corner_resp = ui.interact(corner, base_id.with("corner"), Sense::click());
        let hbar_resp = ui.interact(hbar, base_id.with("hbar"), Sense::click_and_drag());
        let vbar_resp = ui.interact(vbar, base_id.with("vbar"), Sense::click_and_drag());
        // "Active" = the timeline was the last panel clicked. It does not take egui keyboard focus,
        // so the app's global shortcuts (Ctrl+Z, Ctrl+S, …) keep working while it is active.
        if ui.input(|i| i.pointer.any_pressed()) {
            if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
                self.focused = full.contains(p);
            }
        }
        let _ = corner_resp;
        let hovered_any = lane_resp.contains_pointer() || ruler_resp.contains_pointer() || ui.rect_contains_pointer(headers) || ui.rect_contains_pointer(corner);

        // ----- wheel: zoom / scroll -----
        if hovered_any {
            self.wheel(ui, &lay, ui.rect_contains_pointer(headers));
            lay.tmap = TimeMap { x0: lanes.left(), scroll: self.scroll, zoom: self.zoom };
            lay = self.layout(cx, toolbar, corner, ruler, headers, lanes, hbar, vbar, std::mem::take(&mut lay.rows), fps, content_h);
        }

        self.last = Some(LastLayout { lanes: lay.lanes, y0: lay.y0, rows: lay.rows.iter().map(|r| (r.track.clone(), r.y, r.h)).collect() });

        // ----- headers -----
        self.headers_ui(ui, cx, &lay, base_id, &mut out);

        // ----- lanes: hover, press, drag, click -----
        let pointer = ui.input(|i| i.pointer.hover_pos());
        self.hover_pos = pointer.filter(|pos| lanes.contains(*pos));
        self.hover = self.hover_pos.map(|pos| self.hit_at(&lay, pos));
        self.lanes_input(ui, cx, &lay, &lane_resp, &mut out);
        self.ruler_input(ui, cx, &lay, &ruler_resp, &mut out);
        self.scrollbars_input(&lay, &hbar_resp, &vbar_resp);
        self.dnd(ui, cx, &lay, &lane_resp, &mut out);

        // ----- keyboard -----
        let nothing_focused = ui.memory(|m| m.focused().is_none());
        if (self.focused || hovered_any) && nothing_focused && self.rename.is_none() {
            self.keys(ui, cx, &lay, &mut out);
        }

        // ----- paint -----
        self.paint(ui, cx, &lay);

        // ----- hover tooltip -----
        if self.drag.is_none() && self.preview.is_none() && self.rename.is_none() && self.tool != Tool::Razor {
            match &self.hover {
                Some(Hit::Clip { id, .. }) => {
                    let text = clip_tooltip(cx, id);
                    lane_resp.clone().on_hover_ui_at_pointer(|ui| {
                        ui.set_max_width(320.0);
                        ui.label(text);
                    });
                }
                Some(Hit::Key { clip, path, index }) => {
                    if let Some(ov) = lay.overlays.iter().find(|o| &o.clip == clip && &o.path == path) {
                        if let Some(k) = ov.keys().get(*index) {
                            let text = format!(
                                "{} {} at {}\nDrag to move (Shift locks the axis) · Delete removes · right-click for the ease",
                                ov.label,
                                keys::format_value(ov.spec, path, k.v),
                                util::timecode(ov.start + k.t, fps)
                            );
                            lane_resp.clone().on_hover_text_at_pointer(text);
                        }
                    }
                }
                Some(Hit::Line { clip, path }) => {
                    if let (Some(ov), Some(pos)) = (lay.overlays.iter().find(|o| &o.clip == clip && &o.path == path), self.hover_pos) {
                        let v = aiframes_core::eval::sample(&ov.param, util::clamp_safe(lay.tmap.t(pos.x) - ov.start, 0.0, ov.dur));
                        let how = if ov.volume { "drag to change · Ctrl+click adds a key" } else { "Alt+click adds a key" };
                        lane_resp.clone().on_hover_text_at_pointer(format!("{} {} · {how}", ov.label, keys::format_value(ov.spec, path, v)));
                    }
                }
                Some(Hit::Trans { id, .. }) => {
                    if let Some(t) = p.transitions.iter().find(|t| &t.id == id) {
                        let label = aiframes_core::transition_spec(&t.kind).map(|s| s.label).unwrap_or(&t.kind);
                        let text = format!("{label} ({id})
{:.2} s across {} | {}
Drag an edge to change the length.", t.duration, t.from, t.to);
                        lane_resp.clone().on_hover_text_at_pointer(text);
                    }
                }
                _ => {}
            }
        }

        // ----- menus and popups -----
        menus::context_menus(self, ui, cx, &lay, &lane_resp, &ruler_resp, &mut out);
        menus::rename_popup(self, ui, cx, base_id, &mut out);
        if let Some(req) = self.spacing_request.take() {
            match Self::spacing_track(cx, req.as_deref()) {
                Some(t) => self.open_spacing_at(cx, &t, lay.lanes.left_top()),
                None => out.push(UiAction::Status("Set spacing: no track has two clips or more.".into())),
            }
        }
        menus::spacing_dialog(self, ui, cx, base_id, &mut out);
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn layout(&self, cx: &EditCtx, toolbar: Rect, corner: Rect, ruler: Rect, headers: Rect, lanes: Rect, hbar: Rect, vbar: Rect, rows: Vec<Row>, fps: f64, content_h: f32) -> Layout {
        let p = cx.project;
        let r = cx.resolved;
        let tmap = TimeMap { x0: lanes.left(), scroll: self.scroll, zoom: self.zoom };
        let mut lay = Layout { toolbar, corner, ruler, headers, lanes, hbar, vbar, rows, y0: lanes.top() - self.scroll_y, tmap, boxes: vec![], tboxes: vec![], markers: vec![], fps, content_h, overlays: vec![] };
        let locked: HashSet<&str> = p.tracks.iter().filter(|t| t.locked).map(|t| t.id.as_str()).collect();
        for c in &p.clips {
            let Some(row) = lay.row_of_track(&c.track) else { continue };
            let (s, ok) = util::span_or_fallback(p, r, c);
            let rect = lay.clip_rect(row, s);
            if rect.right() < lanes.left() - 2.0 || rect.left() > lanes.right() + 2.0 || rect.bottom() < lanes.top() || rect.top() > lanes.bottom() {
                continue;
            }
            lay.boxes.push(ClipBox {
                id: c.id.clone(),
                row,
                rect,
                fade_in_x: tmap.x(s.start + c.fade_in),
                fade_out_x: tmap.x(s.end - c.fade_out),
                resolved: ok,
                locked: locked.contains(c.track.as_str()),
            });
        }
        for t in &p.transitions {
            let (Some(sf), Some(fc)) = (r.span(&t.from), p.clips.iter().find(|c| c.id == t.from)) else { continue };
            let Some(row) = lay.row_of_track(&fc.track) else { continue };
            let rect = lay.trans_rect(row, sf.end, t.duration);
            if rect.right() < lanes.left() || rect.left() > lanes.right() {
                continue;
            }
            lay.tboxes.push(TransBox { id: t.id.clone(), rect, cut_x: tmap.x(sf.end) });
        }
        for m in &p.markers {
            if let Some(t) = r.marker(&m.id) {
                lay.markers.push((m.id.clone(), tmap.x(t)));
            }
        }
        // Keyframe lanes and the volume line of selected clips.
        for b in &lay.boxes {
            if !b.resolved || b.locked || !cx.is_selected(&b.id) {
                continue;
            }
            let (Some(c), Some(span)) = (p.clips.iter().find(|c| c.id == b.id), r.span(&b.id)) else { continue };
            let content = content_of(b.rect);
            if content.height() < 16.0 {
                continue;
            }
            let sound = util::has_sound(p, c) && self.show_waveforms;
            if sound {
                lay.overlays.push(Overlay {
                    clip: c.id.clone(),
                    path: "volume".into(),
                    label: "Volume".into(),
                    lane: keys::Lane { top: content.top(), bottom: content.bottom(), scale: keys::LaneScale::Gain },
                    rect: content,
                    start: span.start,
                    dur: span.duration().max(0.0),
                    param: c.volume.clone(),
                    volume: true,
                    range: (0.0, 16.0),
                    spec: aiframes_core::clip_param_spec("volume"),
                });
            }
            if let Some(path) = keys::lane_param(c, self.lane_pref.get(&c.id).map(String::as_str), sound) {
                let spec = util::param_spec(c, &path);
                let param = util::param_at(c, &path).cloned().unwrap_or(aiframes_core::Param::Value(spec.map(|s| s.default).unwrap_or(0.0)));
                let (lo, hi) = keys::lane_range(&param, spec);
                lay.overlays.push(Overlay {
                    clip: c.id.clone(),
                    label: util::param_label(c, &path),
                    path,
                    lane: keys::Lane { top: content.top(), bottom: content.bottom(), scale: keys::LaneScale::Linear { lo, hi } },
                    rect: content,
                    start: span.start,
                    dur: span.duration().max(0.0),
                    param,
                    volume: false,
                    range: spec.map(|s| (s.min, s.max)).unwrap_or((-1e9, 1e9)),
                    spec,
                });
            }
        }
        lay
    }

    pub(crate) fn hit_at(&self, lay: &Layout, pos: Pos2) -> Hit {
        // Keyframes and lane curves of selected clips come first.
        let mut best: Option<(f32, Hit)> = None;
        for ov in &lay.overlays {
            if !ov.rect.expand(6.0).contains(pos) {
                continue;
            }
            for (i, k) in ov.keys().iter().enumerate() {
                let d = ov.key_pos(&lay.tmap, k).distance(pos);
                if d <= 6.5 && best.as_ref().is_none_or(|(bd, _)| d < *bd) {
                    best = Some((d, Hit::Key { clip: ov.clip.clone(), path: ov.path.clone(), index: i }));
                }
            }
        }
        if let Some((_, h)) = best {
            return h;
        }
        for ov in &lay.overlays {
            let inner = pos.x > ov.rect.left() + EDGE_PX && pos.x < ov.rect.right() - EDGE_PX && pos.y >= ov.rect.top() && pos.y <= ov.rect.bottom();
            if inner && (ov.volume || self.alt_down) && (pos.y - ov.line_y(&lay.tmap, pos.x)).abs() <= 4.0 {
                return Hit::Line { clip: ov.clip.clone(), path: ov.path.clone() };
            }
        }
        if let Some((i, z)) = geom::hit_transition(&lay.tboxes, pos, EDGE_PX) {
            return Hit::Trans { id: lay.tboxes[i].id.clone(), zone: z };
        }
        if let Some((i, z)) = geom::hit_clip(&lay.boxes, pos, EDGE_PX) {
            return Hit::Clip { id: lay.boxes[i].id.clone(), zone: z };
        }
        let t = lay.tmap.t(pos.x);
        match lay.row_at(pos.y) {
            Some(row) => Hit::Row { row, t },
            None if pos.y >= lay.rows_bottom() => Hit::Below { t },
            None => Hit::Row { row: geom::nearest_row_of_kind(&lay.rows, pos.y - lay.y0, TrackKind::Audio).unwrap_or(0), t },
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Wheel
    // ---------------------------------------------------------------------------------------------

    fn wheel(&mut self, ui: &Ui, lay: &Layout, over_headers: bool) {
        let fps = lay.fps;
        let (zoom_delta, scroll, mods, pos) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta, i.modifiers, i.pointer.hover_pos()));
        if (zoom_delta - 1.0).abs() > 1e-4 || scroll != egui::Vec2::ZERO {
            self.anim = None;
        }
        if (zoom_delta - 1.0).abs() > 1e-4 {
            let x = pos.map(|p| p.x).unwrap_or(lay.lanes.center().x).max(lay.lanes.left());
            let (s, z) = geom::zoom_at(lay.tmap, x, zoom_delta as f64, geom::MIN_ZOOM, geom::max_zoom(fps));
            self.scroll = s;
            self.zoom = z;
            return;
        }
        if scroll == egui::Vec2::ZERO {
            return;
        }
        if over_headers || mods.alt {
            self.scroll_y = (self.scroll_y - scroll.y - scroll.x).clamp(0.0, (lay.content_h - lay.lanes.height()).max(0.0));
        } else {
            self.scroll = (self.scroll - (scroll.x + scroll.y) as f64 / self.zoom).max(0.0);
        }
        ui.ctx().input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
    }

    fn zoom_by(&mut self, lay: &Layout, factor: f64, around_t: f64) {
        let x = lay.tmap.x(around_t).clamp(lay.lanes.left(), lay.lanes.right());
        let (s, z) = geom::zoom_at(lay.tmap, x, factor, geom::MIN_ZOOM, geom::max_zoom(lay.fps));
        self.scroll = s;
        self.zoom = z;
    }

    // ---------------------------------------------------------------------------------------------
    // Lanes
    // ---------------------------------------------------------------------------------------------

    fn snap_targets(&self, cx: &EditCtx, exclude: &HashSet<Id>) -> Vec<f64> {
        snap::targets(cx.project, cx.resolved, cx.playhead, [cx.in_point, cx.out_point], exclude)
    }

    fn lanes_input(&mut self, ui: &Ui, cx: &EditCtx, lay: &Layout, resp: &Response, out: &mut Vec<UiAction>) {
        let p = cx.project;
        let (mods, pressed, pos) = ui.input(|i| (i.modifiers, i.pointer.primary_pressed(), i.pointer.interact_pos()));

        // Cursor feedback.
        if self.drag.is_none() {
            if let Some(h) = &self.hover {
                let icon = match (self.tool, h) {
                    (Tool::Razor, Hit::Clip { .. }) => CursorIcon::Crosshair,
                    (Tool::Slip, Hit::Clip { zone: Zone::Body, .. }) => CursorIcon::ResizeColumn,
                    (_, Hit::Clip { zone: Zone::Start | Zone::End, .. }) => CursorIcon::ResizeHorizontal,
                    (_, Hit::Clip { zone: Zone::FadeIn | Zone::FadeOut, .. }) => CursorIcon::PointingHand,
                    (_, Hit::Trans { zone: TransZone::Left | TransZone::Right, .. }) => CursorIcon::ResizeHorizontal,
                    (Tool::Slide, Hit::Clip { zone: Zone::Body, .. }) => CursorIcon::ResizeColumn,
                    (_, Hit::Key { .. }) => CursorIcon::Grab,
                    (_, Hit::Line { path, .. }) if path == "volume" => CursorIcon::ResizeVertical,
                    (_, Hit::Line { .. }) => CursorIcon::Copy,
                    _ => CursorIcon::Default,
                };
                if icon != CursorIcon::Default {
                    ui.ctx().set_cursor_icon(icon);
                }
            }
        }

        // Press: select immediately (pro NLE behaviour), remember what was pressed.
        if pressed && resp.contains_pointer() {
            if let Some(pos) = pos {
                let hit = self.hit_at(lay, pos);
                let mut click_select = None;
                match &hit {
                    Hit::Key { clip, path, index } => {
                        let t = lay.overlays.iter().find(|o| &o.clip == clip && &o.path == path).and_then(|o| o.keys().get(*index)).map(|k| k.t);
                        self.key_sel = t.map(|t| KeySel { clip: clip.clone(), path: path.clone(), t });
                    }
                    Hit::Line { .. } => {}
                    _ => self.key_sel = None,
                }
                if self.tool != Tool::Razor {
                    match &hit {
                        Hit::Clip { id, .. } => {
                            let ids = self.ids_with_links(cx, id);
                            let selected = cx.is_selected(id);
                            if mods.command {
                                let mut sel: Vec<Id> = cx.selection.to_vec();
                                if selected {
                                    sel.retain(|s| !ids.contains(s));
                                } else {
                                    sel.extend(ids);
                                }
                                out.push(UiAction::Select(sel));
                            } else if mods.shift {
                                let mut sel: Vec<Id> = cx.selection.to_vec();
                                for i in ids {
                                    if !sel.contains(&i) {
                                        sel.push(i);
                                    }
                                }
                                out.push(UiAction::Select(sel));
                            } else if !selected {
                                out.push(UiAction::Select(ids));
                            } else {
                                click_select = Some(ids);
                            }
                        }
                        Hit::Trans { id, .. } => out.push(UiAction::Select(vec![id.clone()])),
                        _ => {}
                    }
                }
                self.press = Some(Press { hit, pos, mods, click_select });
            }
        }

        // Drag start.
        if resp.drag_started_by(PointerButton::Middle) {
            let origin = resp.interact_pointer_pos().unwrap_or(lay.lanes.center());
            self.drag = Some(DragState { kind: DragKind::Pan { scroll: self.scroll, scroll_y: self.scroll_y }, origin, origin_t: lay.tmap.t(origin.x) });
        } else if resp.drag_started_by(PointerButton::Primary) {
            if let Some(press) = self.press.clone() {
                let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or(lay.lanes.center());
                let origin_t = lay.tmap.t(origin.x);
                self.drag = self.start_drag(cx, lay, &press, origin, origin_t).map(|kind| DragState { kind, origin, origin_t });
            }
        }

        // Drag update / release.
        if let Some(st) = self.drag.clone() {
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                self.drag = None;
                self.preview = None;
                return;
            }
            let pos = ui.input(|i| i.pointer.interact_pos().or(i.pointer.hover_pos())).unwrap_or(st.origin);
            self.update_drag(ui, cx, lay, &st, pos, mods, out);
            let released = resp.drag_stopped() || !ui.input(|i| i.pointer.any_down());
            if released {
                self.finish_drag(cx, &st, out);
            }
        }

        // Clicks (no drag).
        if resp.clicked() {
            if let Some(press) = self.press.take() {
                self.click(cx, lay, &press, mods, out);
            }
        }
        if resp.secondary_clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let hit = self.hit_at(lay, pos);
                self.menu = Some(match hit {
                    Hit::Clip { id, .. } => {
                        if !cx.is_selected(&id) {
                            out.push(UiAction::Select(self.ids_with_links(cx, &id)));
                        }
                        MenuTarget::Clip(id)
                    }
                    Hit::Trans { id, .. } => {
                        out.push(UiAction::Select(vec![id.clone()]));
                        MenuTarget::Trans(id)
                    }
                    Hit::Row { row, t } => MenuTarget::Row { track: lay.rows[row].track.clone(), t },
                    Hit::Below { t } => MenuTarget::Below { t },
                    Hit::Key { clip, path, index } => match lay.overlays.iter().find(|o| o.clip == clip && o.path == path).and_then(|o| o.keys().get(index)).map(|k| k.t) {
                        Some(t) => {
                            self.key_sel = Some(KeySel { clip: clip.clone(), path: path.clone(), t });
                            MenuTarget::Key { clip, path, t }
                        }
                        None => MenuTarget::Clip(clip),
                    },
                    Hit::Line { clip, .. } => MenuTarget::Clip(clip),
                });
            }
        }
        let _ = p;
    }

    fn start_drag(&mut self, cx: &EditCtx, lay: &Layout, press: &Press, origin: Pos2, origin_t: f64) -> Option<DragKind> {
        self.anim = None;
        let p = cx.project;
        let r = cx.resolved;
        match &press.hit {
            Hit::Trans { id, zone: TransZone::Left | TransZone::Right } => {
                let t = p.transitions.iter().find(|t| &t.id == id)?;
                let cut = r.span(&t.from)?.end;
                Some(DragKind::TransEdge { id: id.clone(), cut })
            }
            Hit::Trans { .. } => None,
            Hit::Key { clip, path, index } => {
                let ov = lay.overlays.iter().find(|o| &o.clip == clip && &o.path == path)?;
                ov.keys().get(*index)?;
                Some(DragKind::Key { clip: clip.clone(), path: path.clone(), index: *index, lane: ov.lane, start: ov.start, dur: ov.dur, keys0: ov.keys().to_vec(), range: ov.range, label: ov.label.clone(), spec: ov.spec })
            }
            Hit::Line { clip, path } if path == "volume" => {
                let ov = lay.overlays.iter().find(|o| &o.clip == clip && o.volume)?;
                let tl = util::clamp_safe(origin_t - ov.start, 0.0, ov.dur);
                Some(DragKind::Volume { clip: clip.clone(), lane: ov.lane, param0: ov.param.clone(), g0: aiframes_core::eval::sample(&ov.param, tl) })
            }
            Hit::Line { .. } => None,
            Hit::Clip { id, zone } => {
                let c = p.clips.iter().find(|c| &c.id == id)?;
                let span = r.span(id);
                match (self.tool, zone) {
                    (Tool::Razor, _) => None,
                    (_, Zone::FadeIn) => Some(DragKind::Fade { id: id.clone(), out: false }),
                    (_, Zone::FadeOut) => Some(DragKind::Fade { id: id.clone(), out: true }),
                    (Tool::Roll, Zone::Start | Zone::End) => {
                        let s = span?;
                        let (left, right) = if *zone == Zone::End { (id.clone(), edits::neighbour(p, r, id, true)?) } else { (edits::neighbour(p, r, id, false)?, id.clone()) };
                        let cut = if *zone == Zone::End { s.end } else { s.start };
                        Some(DragKind::Roll { left, right, cut, offset: cut - origin_t })
                    }
                    (tool, Zone::Start | Zone::End) => {
                        let s = span?;
                        let edge = if *zone == Zone::Start { Edge::Start } else { Edge::End };
                        let edge_t = if edge == Edge::Start { s.start } else { s.end };
                        let ripple = if tool == Tool::Ripple { Some(true) } else { None };
                        Some(DragKind::Trim { id: id.clone(), edge, ripple, offset: edge_t - origin_t })
                    }
                    (Tool::Slide, Zone::Body) => Some(DragKind::Slide { id: id.clone() }),
                    (Tool::Slip, Zone::Body) if press.mods.alt => Some(DragKind::Slide { id: id.clone() }),
                    (Tool::Slip, Zone::Body) => Some(DragKind::Slip { id: id.clone() }),
                    (_, Zone::Body) => {
                        let mut ids: Vec<Id> = if cx.is_selected(id) { cx.selected_clips() } else { self.ids_with_links(cx, id) };
                        if press.mods.shift || press.mods.command {
                            for x in self.ids_with_links(cx, id) {
                                if !ids.contains(&x) {
                                    ids.push(x);
                                }
                            }
                        }
                        let locked: HashSet<&str> = p.tracks.iter().filter(|t| t.locked).map(|t| t.id.as_str()).collect();
                        ids.retain(|i| p.clips.iter().any(|c| &c.id == i && !locked.contains(c.track.as_str())));
                        if ids.is_empty() {
                            return None;
                        }
                        let kind = p.tracks.iter().find(|t| t.id == c.track).map(|t| t.kind).unwrap_or(TrackKind::Video);
                        let grab_row = lay.row_of_track(&c.track).unwrap_or(0);
                        Some(DragKind::Move { ids, grabbed: id.clone(), kind, grab_row })
                    }
                }
            }
            Hit::Row { .. } | Hit::Below { .. } => {
                let _ = origin;
                if self.tool == Tool::Razor {
                    return None;
                }
                let additive = press.mods.shift || press.mods.command;
                Some(DragKind::Box { additive, base: if additive { cx.selection.to_vec() } else { vec![] }, last: vec![] })
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn update_drag(&mut self, ui: &Ui, cx: &EditCtx, lay: &Layout, st: &DragState, pos: Pos2, mods: Modifiers, out: &mut Vec<UiAction>) {
        let p = cx.project;
        let r = cx.resolved;
        let fps = lay.fps;
        let t_ptr = lay.tmap.t(pos.x);
        // Auto-scroll near the lane edges while editing.
        let edits_time = !matches!(st.kind, DragKind::Pan { .. } | DragKind::RowResize { .. } | DragKind::HBar { .. } | DragKind::VBar { .. } | DragKind::Box { .. });
        if edits_time {
            let edge = 24.0;
            let step = if pos.x > lay.lanes.right() - edge {
                (pos.x - (lay.lanes.right() - edge)) as f64
            } else if pos.x < lay.lanes.left() + edge && self.scroll > 0.0 {
                (pos.x - (lay.lanes.left() + edge)) as f64
            } else {
                0.0
            };
            if step != 0.0 {
                self.scroll = (self.scroll + step * 0.4 / self.zoom).max(0.0);
                ui.ctx().request_repaint();
            }
        }
        match &st.kind {
            DragKind::Pan { scroll, scroll_y } => {
                let d = pos - st.origin;
                self.scroll = (scroll - d.x as f64 / self.zoom).max(0.0);
                self.scroll_y = (scroll_y - d.y).clamp(0.0, (lay.content_h - lay.lanes.height()).max(0.0));
            }
            DragKind::Box { additive, base, last } => {
                let rect = Rect::from_two_pos(st.origin, pos);
                let mut ids = geom::clips_in_rect(&lay.boxes, rect);
                if self.linked_selection {
                    let extra: Vec<Id> = ids.iter().flat_map(|i| aiframes_core::linked_partners(p, i)).collect();
                    for e in extra {
                        if !ids.contains(&e) {
                            ids.push(e);
                        }
                    }
                }
                let mut sel = if *additive { base.clone() } else { vec![] };
                for i in ids {
                    if !sel.contains(&i) {
                        sel.push(i);
                    }
                }
                if &sel != last {
                    out.push(UiAction::Select(sel.clone()));
                    if let Some(d) = &mut self.drag {
                        if let DragKind::Box { last, .. } = &mut d.kind {
                            *last = sel;
                        }
                    }
                }
            }
            DragKind::Move { ids, grabbed, kind, grab_row } => {
                let (ops, hints, info, label) = self.move_drag(cx, lay, st, ids, grabbed, *kind, *grab_row, pos, mods);
                self.set_preview(cx, Owner::Drag, label, ops, hints, info);
            }
            DragKind::Trim { id, edge, ripple, offset } => {
                let Some((lo, hi)) = drag::trim_bounds_with(p, r, id, *edge, *ripple) else { return };
                let mut t = util::quantize(t_ptr + offset, fps);
                if self.snapping && !mods.alt {
                    let ex: HashSet<Id> = std::iter::once(id.clone()).chain(aiframes_core::linked_partners(p, id)).collect();
                    if let Some(s) = snap::snap(t, &self.snap_targets(cx, &ex), self.zoom, snap::SNAP_PX) {
                        t = s;
                        self.snap_line = Some(s);
                    }
                }
                let t = t.clamp(lo, hi);
                let span = r.span(id).unwrap_or(Span::new(0.0, 0.0));
                let (old, new_len) = match edge {
                    Edge::Start => (span.start, span.end - t),
                    Edge::End => (span.end, t - span.start),
                };
                let ops = if (t - old).abs() < 1e-9 { vec![] } else { vec![drag::trim_op(id, *edge, t, *ripple)] };
                let hint = match edge {
                    Edge::Start => Span::new(t, span.end),
                    Edge::End => Span::new(span.start, t),
                };
                let track = p.clips.iter().find(|c| &c.id == id).map(|c| c.track.clone()).unwrap_or_default();
                let info = format!("{}  ·  length {}", shift_t(t - old), util::short_time(new_len));
                let label = format!("{} {id}", if ripple == &Some(true) { "Ripple trim" } else { "Trim" });
                self.set_preview(cx, Owner::Drag, label, ops, vec![(track, hint)], info);
                // Say what the core makes of it: the frame-accurate length, the clip's end anchor, the
                // clips whose end anchors follow this edge.
                if let Some(pv) = &mut self.preview {
                    if let (Ok(sim), false) = (&pv.result, pv.ops.is_empty()) {
                        if let Some(s2) = sim.resolved.span(id) {
                            let old = if *edge == Edge::Start { span.start } else { span.end };
                            let new = if *edge == Edge::Start { s2.start } else { s2.end };
                            pv.info = format!("{}  ·  length {}", shift_t(new - old), util::short_time(s2.duration()));
                        }
                        if let Some(note) = until_note(p, sim, id) {
                            pv.info.push('\n');
                            pv.info.push_str(&note);
                        }
                    }
                }
            }
            DragKind::Roll { left, right, cut, offset } => {
                let Some((lo, hi)) = drag::roll_bounds(p, r, left, right) else { return };
                let mut t = util::quantize(t_ptr + offset, fps);
                if self.snapping && !mods.alt {
                    let ex: HashSet<Id> = [left.clone(), right.clone()].into_iter().collect();
                    if let Some(s) = snap::snap(t, &self.snap_targets(cx, &ex), self.zoom, snap::SNAP_PX) {
                        t = s;
                        self.snap_line = Some(s);
                    }
                }
                let t = t.clamp(lo, hi);
                let delta = t - cut;
                let ops = if delta.abs() < 1e-9 { vec![] } else { vec![drag::roll_op(left, right, delta)] };
                self.set_preview(cx, Owner::Drag, format!("Roll {left}|{right}"), ops, vec![], format!("cut {}  ({})", util::timecode(t, fps), shift_t(delta)));
            }
            DragKind::Slip { id } => {
                let dt = util::quantize(t_ptr - st.origin_t, fps);
                let ops: Vec<Op> = drag::slip_op(p, id, dt).into_iter().collect();
                let info = match ops.first() {
                    Some(Op::SlipClip { delta, .. }) => {
                        let ip = p.clips.iter().find(|c| &c.id == id).and_then(aiframes_core::eval::in_point).unwrap_or(0.0);
                        format!("in {}  ({} media)", util::timecode(ip + delta, fps), shift_t(*delta))
                    }
                    _ => "at the media's limit".into(),
                };
                self.set_preview(cx, Owner::Drag, format!("Slip {id}"), ops, vec![], info);
            }
            DragKind::Slide { id } => {
                let dt = util::quantize(t_ptr - st.origin_t, fps);
                let ops: Vec<Op> = drag::slide_op(id, dt).into_iter().collect();
                self.set_preview(cx, Owner::Drag, format!("Slide {id}"), ops, vec![], format!("slide {}", shift_t(dt)));
            }
            DragKind::Fade { id, out: fade_out } => {
                let (Some(c), Some(s)) = (p.clips.iter().find(|c| &c.id == id), r.span(id)) else { return };
                let t = util::quantize(t_ptr, fps);
                let len = if *fade_out { s.end - t } else { t - s.start };
                let op = drag::fade_op(c, *fade_out, len);
                let v = match &op {
                    Op::SetParam { value: Some(v), .. } => *v,
                    _ => 0.0,
                };
                let cur = if *fade_out { c.fade_out } else { c.fade_in };
                let ops = if (v - cur).abs() < 1e-9 { vec![] } else { vec![op] };
                self.set_preview(cx, Owner::Drag, format!("Fade {} {id}", if *fade_out { "out" } else { "in" }), ops, vec![], format!("fade {} {:.2} s", if *fade_out { "out" } else { "in" }, v));
            }
            DragKind::TransEdge { id, cut } => {
                let len = 2.0 * (t_ptr - cut).abs();
                let ops: Vec<Op> = drag::transition_len_op(p, r, id, util::quantize(len, fps * 0.5)).into_iter().collect();
                let d = match ops.first() {
                    Some(Op::SetTransition { duration: Some(d), .. }) => *d,
                    _ => p.transitions.iter().find(|t| &t.id == id).map(|t| t.duration).unwrap_or(0.0),
                };
                self.set_preview(cx, Owner::Drag, format!("Transition {id} length"), ops, vec![], format!("transition {:.2} s", d));
            }
            DragKind::Marker { id, offset } => {
                let mut t = util::quantize(t_ptr + offset, fps).max(0.0);
                if self.snapping && !mods.alt {
                    let mut targets = self.snap_targets(cx, &HashSet::new());
                    if let Some(cur) = r.marker(id) {
                        targets.retain(|x| (x - cur).abs() > 1e-9);
                    }
                    if let Some(s) = snap::snap(t, &targets, self.zoom, snap::SNAP_PX) {
                        t = s;
                        self.snap_line = Some(s);
                    }
                }
                let ops: Vec<Op> = drag::marker_move_op(p, r, id, t).into_iter().collect();
                self.set_preview(cx, Owner::Drag, format!("Move marker {id}"), ops, vec![], util::timecode(t, fps));
            }
            DragKind::Scrub => {
                let mut t = t_ptr.max(0.0);
                if self.snapping && !mods.alt {
                    if let Some(s) = snap::snap(t, &util::edit_points(p, r), self.zoom, snap::SNAP_PX) {
                        t = s;
                        self.snap_line = Some(s);
                    }
                }
                out.push(UiAction::Seek(util::quantize(t, fps)));
            }
            DragKind::RowResize { track, h0 } => {
                let h = (h0 + (pos.y - st.origin.y)).clamp(geom::MIN_ROW_H, geom::MAX_ROW_H);
                self.track_heights.insert(track.clone(), h);
            }
            DragKind::HBar { .. } | DragKind::VBar { .. } => {}
            DragKind::Key { clip, path, index, lane, start, dur, keys0, range, label, spec } => {
                let Some(k0) = keys0.get(*index) else { return };
                let d = pos - st.origin;
                let (lock_t, lock_v) = if mods.shift { (d.x.abs() < d.y.abs(), d.x.abs() >= d.y.abs()) } else { (false, false) };
                let mut t_abs = if lock_t { start + k0.t } else { util::quantize(start + k0.t + (t_ptr - st.origin_t), fps) };
                if self.snapping && !mods.alt && !lock_t && ((lay.tmap.x(t_abs) - lay.tmap.x(cx.playhead)).abs() as f64) <= snap::SNAP_PX {
                    t_abs = cx.playhead;
                    self.snap_line = Some(cx.playhead);
                }
                let v = if lock_v { k0.v } else { lane.shift(k0.v, d.y) };
                let keys = keys::move_key(keys0, *index, t_abs - start, v, *dur, 0.5 / fps, *range);
                let nk = keys[*index].clone();
                let ops = if &keys != keys0 { vec![keys::set_keys_op(clip, path, keys)] } else { vec![] };
                self.key_sel = Some(KeySel { clip: clip.clone(), path: path.clone(), t: nk.t });
                let info = format!("{}  ·  {label} {}", util::timecode(start + nk.t, fps), keys::format_value(*spec, path, nk.v));
                self.set_preview_g(cx, Owner::Drag, format!("Keyframe {label} ({clip})"), ops, vec![], info, false);
            }
            DragKind::Volume { clip, lane, param0, g0 } => {
                let g1 = lane.shift(*g0, pos.y - st.origin.y);
                let ops: Vec<Op> = keys::volume_drag_op(clip, param0, *g0, g1).into_iter().collect();
                self.set_preview_g(cx, Owner::Drag, format!("Volume ({clip})"), ops, vec![], format!("Volume {}", headers::db_label(g1)), false);
            }
            DragKind::TrackReorder { track } => {
                let (ops, info) = match keys::reorder_slot(p, &lay.rows, track, pos.y - lay.y0) {
                    Some((slot, line)) => {
                        self.reorder_line = Some(lay.y0 + line);
                        (keys::reorder_op(p, &lay.rows, track, slot).into_iter().collect::<Vec<Op>>(), format!("Move track {track}"))
                    }
                    None => (vec![], String::new()),
                };
                self.set_preview_g(cx, Owner::Drag, format!("Move track {track}"), ops, vec![], info, false);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn move_drag(&mut self, cx: &EditCtx, lay: &Layout, st: &DragState, ids: &[Id], grabbed: &str, kind: TrackKind, grab_row: usize, pos: Pos2, mods: Modifiers) -> (Vec<Op>, Vec<(Id, Span)>, String, String) {
        let p = cx.project;
        let r = cx.resolved;
        let fps = lay.fps;
        let mut dt = lay.tmap.t(pos.x) - st.origin_t;
        // Rows: among tracks of the grabbed clip's kind.
        let kind_rows: Vec<usize> = (0..lay.rows.len()).filter(|&i| lay.rows[i].kind == kind).collect();
        let grab_k = kind_rows.iter().position(|&i| i == grab_row).unwrap_or(0) as i32;
        let target = geom::nearest_row_of_kind(&lay.rows, pos.y - lay.y0, kind);
        let mut delta = target.and_then(|t| kind_rows.iter().position(|&i| i == t)).map(|k| k as i32 - grab_k).unwrap_or(0);
        if mods.shift {
            let d = pos - st.origin;
            if d.x.abs() >= d.y.abs() {
                delta = 0;
            } else {
                dt = 0.0;
            }
        }
        let delta = drag::clamp_track_delta(p, &lay.rows, ids, kind, delta);
        let tracks = drag::track_shift(p, &lay.rows, ids, kind, delta).unwrap_or_default();
        dt = util::quantize(dt, fps);
        // Everything that moves: the clips plus what is anchored to them.
        let moved: HashSet<Id> = ids.iter().cloned().collect();
        let mut moving: HashSet<Id> = moved.clone();
        for c in &p.clips {
            if util::ancestors(p, &c.id).iter().any(|a| moved.contains(a)) {
                moving.insert(c.id.clone());
            }
        }
        let spans: Vec<(&aiframes_core::Clip, Span)> = ids.iter().filter_map(|i| p.clips.iter().find(|c| &c.id == i)).map(|c| (c, util::span_or_fallback(p, r, c).0)).collect();
        if self.snapping && !mods.alt || (self.snapping && mods.alt && mods.command) {
            let edges: Vec<f64> = spans.iter().flat_map(|(_, s)| [s.start + dt, s.end + dt]).collect();
            let ex = if mods.alt { moved.clone() } else { moving.clone() };
            if let Some((d, at)) = snap::snap_edges(&edges, &self.snap_targets(cx, &ex), self.zoom, snap::SNAP_PX) {
                dt += d;
                self.snap_line = Some(at);
            }
        }
        let min_start = spans.iter().map(|(_, s)| s.start).fold(f64::INFINITY, f64::min);
        if min_start.is_finite() {
            dt = dt.max(-min_start);
        }
        let hints: Vec<(Id, Span)> = spans.iter().map(|(c, s)| (tracks.get(&c.track).cloned().unwrap_or_else(|| c.track.clone()), Span::new(s.start + dt, s.end + dt))).collect();
        let others_ignore: Vec<Id> = if mods.alt { vec![] } else { moving.iter().cloned().collect() };
        let overlap = hints.iter().any(|(t, s)| !edits::track_free(p, r, t, s.start, s.end, &others_ignore));
        let plan = drag::MovePlan { ids: ids.to_vec(), dt, tracks, duplicate: mods.alt, absolute: mods.command, overwrite: overlap };
        let ops = drag::move_ops(p, r, &plan);
        let grabbed_clip = p.clips.iter().find(|c| c.id == grabbed);
        let anchor = match grabbed_clip.map(|c| &c.start) {
            Some(aiframes_core::Time::Anchored(a)) if !mods.command && !mods.alt => {
                let mut a2 = a.clone();
                if !util::ancestors(p, grabbed).iter().any(|x| moved.contains(x)) {
                    a2.offset += dt;
                }
                format!("  ·  anchored {}", aiframes_core::resolve::describe_anchor(&a2))
            }
            Some(aiframes_core::Time::Anchored(_)) if mods.command => "  ·  absolute (anchor dropped)".into(),
            _ => String::new(),
        };
        let start = grabbed_clip.and_then(|c| spans.iter().find(|(k, _)| k.id == c.id)).map(|(_, s)| s.start + dt).unwrap_or(0.0);
        let info = format!(
            "{}  {}{}{}{}",
            util::timecode(start, fps),
            shift_t(dt),
            if mods.alt { "  ·  copy" } else { "" },
            if overlap { "  ·  overwrite" } else { "" },
            anchor
        );
        let n = ids.len();
        let label = if mods.alt {
            if n == 1 {
                format!("Duplicate {grabbed}")
            } else {
                format!("Duplicate {n} clips")
            }
        } else if n == 1 {
            format!("Move {grabbed}")
        } else {
            format!("Move {n} clips")
        };
        (ops, hints, info, label)
    }

    fn set_preview(&mut self, cx: &EditCtx, owner: Owner, label: String, ops: Vec<Op>, hints: Vec<(Id, Span)>, info: String) {
        self.set_preview_g(cx, owner, label, ops, hints, info, true);
    }

    #[allow(clippy::too_many_arguments)]
    fn set_preview_g(&mut self, cx: &EditCtx, owner: Owner, label: String, ops: Vec<Op>, hints: Vec<(Id, Span)>, info: String, ghost: bool) {
        // The revision is part of the key: a preview computed on an older project is stale.
        let key = format!("{}:{}", cx.revision, serde_json::to_string(&ops).unwrap_or_default());
        if let Some(pv) = &mut self.preview {
            if pv.key == key && pv.owner == owner {
                pv.info = info;
                pv.hints = hints;
                pv.label = label;
                return;
            }
        }
        let result = if ops.is_empty() {
            Ok(Sim { project: Project::default(), resolved: Resolved::default(), changed: HashSet::new(), removed: HashSet::new(), summary: String::new() })
        } else {
            simulate(cx.project, cx.resolved, &ops)
        };
        self.preview = Some(Preview { owner, key, label, ops, result, hints, info, ghost });
    }

    fn finish_drag(&mut self, cx: &EditCtx, st: &DragState, out: &mut Vec<UiAction>) {
        self.drag = None;
        self.press = None;
        self.reorder_line = None;
        match &st.kind {
            DragKind::RowResize { .. } | DragKind::Pan { .. } | DragKind::Scrub | DragKind::HBar { .. } | DragKind::VBar { .. } | DragKind::Box { .. } => {}
            _ => {
                if let Some(pv) = self.preview.take() {
                    if pv.ops.is_empty() {
                        return;
                    }
                    match pv.result {
                        Ok(_) => out.push(UiAction::Apply { label: pv.label, ops: pv.ops }),
                        Err(e) => out.push(UiAction::Status(format!("Not applied: {e}"))),
                    }
                }
            }
        }
        let _ = cx;
    }

    fn click(&mut self, cx: &EditCtx, lay: &Layout, press: &Press, mods: Modifiers, out: &mut Vec<UiAction>) {
        let p = cx.project;
        let r = cx.resolved;
        match (&press.hit, self.tool) {
            (Hit::Clip { id, .. }, Tool::Razor) => {
                let pos_t = self.hover_time(lay).unwrap_or(cx.playhead);
                let t = self.razor_time(cx, lay, pos_t, mods);
                let ops = if mods.shift { edits::split_ops(p, r, None, t) } else { edits::split_ops(p, r, Some(&self.ids_with_links(cx, id)[..1]), t) };
                if ops.is_empty() {
                    out.push(UiAction::Status("Nothing to split there.".into()));
                } else {
                    let label = if mods.shift { format!("Split all at {}", util::timecode(t, lay.fps)) } else { format!("Split {id} at {}", util::timecode(t, lay.fps)) };
                    out.push(UiAction::Apply { label, ops });
                }
            }
            (Hit::Clip { .. }, _) => {
                if let Some(ids) = &press.click_select {
                    if !mods.shift && !mods.command {
                        out.push(UiAction::Select(ids.clone()));
                    }
                }
            }
            (Hit::Row { t, .. } | Hit::Below { t }, Tool::Razor) if mods.shift => {
                let t = self.razor_time(cx, lay, *t, mods);
                let ops = edits::split_ops(p, r, None, t);
                if !ops.is_empty() {
                    out.push(UiAction::Apply { label: format!("Split all at {}", util::timecode(t, lay.fps)), ops });
                }
            }
            (Hit::Row { .. } | Hit::Below { .. }, _) => {
                if !mods.shift && !mods.command && !cx.selection.is_empty() {
                    out.push(UiAction::Select(vec![]));
                }
            }
            (Hit::Trans { .. }, _) | (Hit::Key { .. }, _) => {}
            (Hit::Line { clip, path }, _) => {
                let Some(ov) = lay.overlays.iter().find(|o| &o.clip == clip && &o.path == path) else { return };
                let tl = util::clamp_safe(util::quantize(lay.tmap.t(press.pos.x), lay.fps) - ov.start, 0.0, ov.dur);
                let add = if ov.volume { press.mods.command } else { press.mods.alt };
                if add {
                    out.push(UiAction::Apply { label: format!("Add {} keyframe ({clip})", ov.label), ops: vec![keys::add_key_op(clip, path, &ov.param, tl)] });
                    self.key_sel = Some(KeySel { clip: clip.clone(), path: path.clone(), t: (tl * 1e6).round() / 1e6 });
                }
            }
        }
    }

    fn hover_time(&self, lay: &Layout) -> Option<f64> {
        self.hover_pos.map(|p| lay.tmap.t(p.x))
    }

    /// The razor time under the pointer: frame-quantised, snapped to the playhead/markers/edits.
    fn razor_time(&self, cx: &EditCtx, lay: &Layout, t: f64, mods: Modifiers) -> f64 {
        let t = util::quantize(t, lay.fps);
        if self.snapping && !mods.alt {
            if let Some(s) = snap::snap(t, &self.snap_targets(cx, &HashSet::new()), self.zoom, snap::SNAP_PX) {
                return s;
            }
        }
        t
    }

    // ---------------------------------------------------------------------------------------------
    // Ruler
    // ---------------------------------------------------------------------------------------------

    fn ruler_input(&mut self, ui: &Ui, cx: &EditCtx, lay: &Layout, resp: &Response, out: &mut Vec<UiAction>) {
        let p = cx.project;
        let pos = ui.input(|i| i.pointer.interact_pos());
        let marker_at = |pos: Pos2| -> Option<Id> {
            if pos.y < lay.ruler.bottom() - 14.0 {
                return None;
            }
            geom::hit_marker(&lay.markers, pos.x, 6.0).map(|i| lay.markers[i].0.clone())
        };
        if let Some(h) = resp.hover_pos() {
            if marker_at(h).is_some() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
        }
        if resp.drag_started_by(PointerButton::Primary) {
            let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or(lay.ruler.center());
            let origin_t = lay.tmap.t(origin.x);
            let kind = match marker_at(origin) {
                Some(id) => {
                    let cur = cx.resolved.marker(&id).unwrap_or(origin_t);
                    out.push(UiAction::Select(vec![id.clone()]));
                    DragKind::Marker { id, offset: cur - origin_t }
                }
                None => DragKind::Scrub,
            };
            self.drag = Some(DragState { kind, origin, origin_t });
        }
        if resp.clicked() {
            if let Some(pos) = pos {
                match marker_at(pos) {
                    Some(id) => {
                        out.push(UiAction::Select(vec![id.clone()]));
                        if let Some(t) = cx.resolved.marker(&id) {
                            out.push(UiAction::Seek(t));
                        }
                    }
                    None => out.push(UiAction::Seek(util::quantize(lay.tmap.t(pos.x).max(0.0), lay.fps))),
                }
            }
        }
        if resp.double_clicked() {
            if let Some(pos) = pos {
                if let Some(id) = marker_at(pos) {
                    let name = p.markers.iter().find(|m| m.id == id).map(|m| m.name.clone()).unwrap_or_default();
                    self.rename = Some(Rename { target: RenameTarget::Marker(id), text: name, pos: pos + vec2(-40.0, 8.0), opened: false });
                }
            }
        }
        if resp.secondary_clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                self.menu = Some(match marker_at(pos) {
                    Some(id) => MenuTarget::Marker(id),
                    None => MenuTarget::Ruler { t: util::quantize(lay.tmap.t(pos.x).max(0.0), lay.fps) },
                });
            }
        }
        if let Some(h) = resp.hover_pos() {
            if let Some(id) = marker_at(h) {
                if let Some(m) = p.markers.iter().find(|m| m.id == id) {
                    let t = cx.resolved.marker(&id).unwrap_or(0.0);
                    let txt = format!(
                        "{} {}\n{} · {:?}{}",
                        if m.name.is_empty() { "(unnamed)" } else { &m.name },
                        id,
                        util::timecode(t, lay.fps),
                        m.kind,
                        if m.note.is_empty() { String::new() } else { format!("\n{}", m.note) }
                    );
                    resp.clone().on_hover_text_at_pointer(txt);
                }
            }
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Scrollbars
    // ---------------------------------------------------------------------------------------------

    /// The horizontal scrollbar thumb.
    pub(crate) fn hbar_thumb(&self, lay: &Layout) -> Rect {
        let view = lay.lanes.width() as f64 / self.zoom;
        let total = self.total.max(view);
        let w = lay.hbar.width();
        let tw = ((view / total) as f32 * w).clamp(24.0_f32.min(w), w);
        let frac = if total > view { (self.scroll / (total - view)).clamp(0.0, 1.0) as f32 } else { 0.0 };
        Rect::from_min_size(pos2(lay.hbar.left() + frac * (w - tw), lay.hbar.top() + 2.0), vec2(tw, lay.hbar.height() - 4.0))
    }

    /// The vertical scrollbar thumb, when the tracks do not fit.
    pub(crate) fn vbar_thumb(&self, lay: &Layout) -> Option<Rect> {
        let vh = lay.lanes.height();
        if lay.content_h <= vh + 0.5 {
            return None;
        }
        let h = lay.vbar.height();
        let th = (vh / lay.content_h * h).clamp(20.0_f32.min(h), h);
        let frac = (self.scroll_y / (lay.content_h - vh)).clamp(0.0, 1.0);
        Some(Rect::from_min_size(pos2(lay.vbar.left() + 2.0, lay.vbar.top() + frac * (h - th)), vec2(lay.vbar.width() - 4.0, th)))
    }

    fn scrollbars_input(&mut self, lay: &Layout, hbar: &Response, vbar: &Response) {
        let view = lay.lanes.width() as f64 / self.zoom;
        let total = self.total.max(view);
        let thumb = self.hbar_thumb(lay);
        if hbar.drag_started() {
            let x = hbar.interact_pointer_pos().map(|p| p.x).unwrap_or(thumb.center().x);
            let grab = if thumb.x_range().contains(x) { x - thumb.left() } else { thumb.width() * 0.5 };
            self.drag = Some(DragState { kind: DragKind::HBar { grab }, origin: pos2(x, 0.0), origin_t: 0.0 });
        }
        if let (Some(DragState { kind: DragKind::HBar { grab }, .. }), Some(pos)) = (&self.drag, hbar.interact_pointer_pos()) {
            let room = (lay.hbar.width() - thumb.width()).max(1.0);
            let frac = ((pos.x - grab - lay.hbar.left()) / room).clamp(0.0, 1.0) as f64;
            self.scroll = frac * (total - view).max(0.0);
        }
        if hbar.clicked() {
            if let Some(pos) = hbar.interact_pointer_pos() {
                if !thumb.x_range().contains(pos.x) {
                    let frac = ((pos.x - lay.hbar.left()) / lay.hbar.width()).clamp(0.0, 1.0) as f64;
                    self.scroll = (frac * total - view * 0.5).max(0.0);
                }
            }
        }
        if let Some(th) = self.vbar_thumb(lay) {
            let vh = lay.lanes.height();
            if vbar.drag_started() {
                let y = vbar.interact_pointer_pos().map(|p| p.y).unwrap_or(th.center().y);
                let grab = if th.y_range().contains(y) { y - th.top() } else { th.height() * 0.5 };
                self.drag = Some(DragState { kind: DragKind::VBar { grab }, origin: pos2(0.0, y), origin_t: 0.0 });
            }
            if let (Some(DragState { kind: DragKind::VBar { grab }, .. }), Some(pos)) = (&self.drag, vbar.interact_pointer_pos()) {
                let room = (lay.vbar.height() - th.height()).max(1.0);
                self.scroll_y = ((pos.y - grab - lay.vbar.top()) / room).clamp(0.0, 1.0) * (lay.content_h - vh);
            }
        }
        if (hbar.drag_stopped() || vbar.drag_stopped()) && matches!(self.drag.as_ref().map(|d| &d.kind), Some(DragKind::HBar { .. } | DragKind::VBar { .. })) {
            self.drag = None;
        }
    }
}
