//! The application shell: panels, hub integration (live state, AI requests, flashes, notices,
//! screenshots), action/command handling, files (open/save/autosave/recovery), import/export jobs.

use crate::ai::{AiPanel, LogKind, RunContext};
use crate::dialogs::{self, AfterSave, DialogCx, DialogOut, Dialogs, Modal};
use crate::edits;
use crate::export::{ExportCx, ExportDialog, ExportRequest};
use crate::panels::{self, Cmd, MenuInfo, PanelId, SideTab, StatusInfo, Toast, ToastKind};
use crate::playback::{clock::Shuttle, Engine, Quality, Snapshot};
use crate::prefs::{self, Prefs};
use crate::previews::{self, Previews};
use crate::shortcuts::{self, Command};
use crate::theme;
use crate::timecode::{self, frame_index, frame_time};
use crate::viewer::{LivePreview, Viewer, ViewerCx, Zoom, PARAM_PATHS};
use aiframes_core::{Document, Id, MediaKind, Op, Param, TrackKind};
use aiframes_mcp::{actions, suite_hub::lock, JobStatus, ProjectDoc, Services};
use aiframes_render::LayerBox;
use aiframes_ui::{bin::MediaBin, effects_browser::EffectsBrowser, history::HistoryPanel, inspector::Inspector, review::ReviewPanel, timeline::Timeline, transcript::TranscriptPanel, EditCtx, UiAction};
use eframe::egui::{self, Event, Key};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;

pub const APP_NAME: &str = "AIFrames";
const MEDIA_EXTS: &[&str] = &["mp4", "mov", "mkv", "webm", "avi", "m4v", "mxf", "mts", "ts", "wmv", "flv", "mp3", "wav", "aac", "m4a", "flac", "ogg", "opus", "aif", "aiff", "wma", "png", "jpg", "jpeg", "webp", "bmp", "gif", "tif", "tiff", "exr"];

/// The editing widgets of one project.
#[derive(Default)]
pub struct Widgets {
    pub timeline: Timeline,
    pub inspector: Inspector,
    pub effects: EffectsBrowser,
    pub transcript: TranscriptPanel,
    pub history: HistoryPanel,
    pub review: ReviewPanel,
    pub bin: MediaBin,
}

/// Per-project UI state.
pub struct DocUi {
    pub w: Widgets,
    pub selection: Vec<Id>,
    pub playhead: f64,
    pub in_point: Option<f64>,
    pub out_point: Option<f64>,
    snap: Option<Arc<Snapshot>>,
    epoch: u64,
    ai_seen_rev: u64,
    flash_ids: Vec<Id>,
    flash_start: f64,
    last_autosave: f64,
    autosaved_key: (u64, u64),
    checked_recovery: bool,
    /// The project file on disk (external changes).
    watch: crate::external::Watch,
    /// The timeline's (scroll, zoom) last frame, and when the person last changed it (Follow AI
    /// keeps a view the person set recently).
    view: Option<(f64, f64)>,
    view_changed_at: f64,
}

impl DocUi {
    fn new(now: f64) -> DocUi {
        DocUi {
            w: Widgets::default(),
            selection: Vec::new(),
            playhead: 0.0,
            in_point: None,
            out_point: None,
            snap: None,
            epoch: 0,
            ai_seen_rev: 0,
            flash_ids: Vec::new(),
            flash_start: -10.0,
            last_autosave: now,
            autosaved_key: (0, 0),
            checked_recovery: false,
            watch: Default::default(),
            view: None,
            view_changed_at: f64::NEG_INFINITY,
        }
    }

    fn flash(&self, now: f64) -> f32 {
        if self.flash_ids.is_empty() {
            0.0
        } else {
            (1.0 - (now - self.flash_start) / 2.0).clamp(0.0, 1.0) as f32
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Viewer,
    Timeline,
    Other,
}

type ImportResult = Result<(Vec<Op>, Vec<String>), String>;

struct ImportJob {
    doc: u64,
    /// Ops to apply and "file: reason" lines for files that were skipped.
    rx: Receiver<ImportResult>,
    /// Place the imported files on the timeline at this time (and on this track when it fits).
    place_at: Option<(f64, Option<Id>)>,
    /// History label ("" = "Import N file(s)").
    label: String,
}

/// Self-test / screenshot knobs from the environment.
struct EnvKnobs {
    screenshot: Option<(PathBuf, u32)>,
    shot_frames: u32,
    shot_exit: bool,
    autoplay: bool,
    perf: Option<PathBuf>,
    select: Option<Vec<Id>>,
    seek: Option<f64>,
    /// Runs this prompt in the AI panel once the window is up (self-test of the Claude path).
    ai_prompt: Option<String>,
    ai_model: Option<String>,
}

impl EnvKnobs {
    fn from_env() -> EnvKnobs {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        EnvKnobs {
            screenshot: var("AIFRAMES_SCREENSHOT").map(|p| (PathBuf::from(p), 0)),
            shot_frames: var("AIFRAMES_SHOT_FRAMES").and_then(|v| v.parse().ok()).unwrap_or(90),
            shot_exit: var("AIFRAMES_SCREENSHOT_EXIT").is_some(),
            autoplay: var("AIFRAMES_AUTOPLAY").is_some(),
            perf: var("AIFRAMES_PERF").map(PathBuf::from),
            select: var("AIFRAMES_SELECT").map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()),
            seek: var("AIFRAMES_SEEK").and_then(|v| v.parse().ok()),
            ai_prompt: var("AIFRAMES_AI_PROMPT"),
            ai_model: var("AIFRAMES_AI_MODEL"),
        }
    }
}

pub struct AiFramesApp {
    services: Arc<Services>,
    port: Option<u16>,
    exe: String,
    prefs: Prefs,
    docs: HashMap<u64, DocUi>,
    engine: Engine,
    engine_doc: Option<u64>,
    previews: Previews,
    viewer: Viewer,
    ai: AiPanel,
    export: ExportDialog,
    dialogs: Dialogs,
    side_tab: SideTab,
    toasts: Vec<Toast>,
    status: String,
    notice: Option<(String, f64)>,
    recent: Vec<PathBuf>,
    clipboard: Option<String>,
    fonts: suite_ui::fonts::FontManager,
    last_title: String,
    now: f64,
    exit_queue: Option<Vec<u64>>,
    exit_confirmed: bool,
    shot_waiters: Vec<aiframes_mcp::suite_hub::ScreenshotReply>,
    waker_set: bool,
    imports: Vec<ImportJob>,
    layers: ((u64, u64, u64), i64, Vec<LayerBox>),
    live_preview: Option<LivePreview>,
    preview_epoch: u64,
    jobs_seen: HashSet<u64>,
    export_paths: HashMap<u64, PathBuf>,
    last_export: Option<PathBuf>,
    focus: Focus,
    viewer_rect: egui::Rect,
    timeline_rect: egui::Rect,
    knobs: EnvKnobs,
    perf_next: f64,
    frames: u64,
    pending_select: Option<Vec<Id>>,
    startup_imports: Vec<PathBuf>,
    /// Media whose audio was decoded in the background already.
    warmed: HashSet<PathBuf>,
    /// Last time the person clicked, dragged, scrolled or typed (Follow AI holds off then).
    last_interaction: f64,
    /// AI edits the view did not (fully) follow: "AI edited … ▸".
    ai_chip: Option<AiChip>,
    /// AI edits to bring into view once they settle: (doc, ids so far, time of the last one, ids of
    /// the latest edit). AI scaffolding (probe clips added and removed within seconds) is gone by
    /// then and not chased.
    pending_follow: Option<(u64, Vec<Id>, f64, Vec<Id>)>,
    /// When the app last scrolled the timeline itself (so that is not taken for the person's doing).
    last_reveal_at: f64,
    /// AI edits made from other MCP sessions (a terminal `claude`), grouped until they go quiet.
    ext_bursts: Vec<ExtBurst>,
    /// Media files used by open projects (pool resources for others are released).
    used_media: HashSet<PathBuf>,
    last_media_sweep: f64,
    /// Scripted input for self-tests (`AIFRAMES_SCRIPT`).
    script: Option<crate::script::Script>,
    script_action: Option<crate::script::Frame>,
    exit_in: Option<u32>,
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

impl AiFramesApp {
    pub fn new(cc: &eframe::CreationContext<'_>, services: Arc<Services>, port: Option<u16>, startup_errors: Vec<String>, startup_imports: Vec<PathBuf>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let fonts = suite_ui::fonts::FontManager::new(&ctx);
        theme::apply(&ctx);
        let (prefs, prefs_problem) = Prefs::load_reported();
        services.pool.set_cache_bytes(cache_bytes(&prefs));
        services.pool.set_readahead_bytes(prefs.readahead_mb as usize * (1 << 20));
        if let Some(h) = aiframes_media::HwAccel::parse(&prefs.hw_decode) {
            services.pool.set_hwaccel(h);
        }
        let first = services.hub.active().map(|(id, d)| (id, snapshot_of(id, &lock(&d), 0)));
        let snap = match first {
            Some((_, s)) => s,
            None => Arc::new(Snapshot { doc: 0, revision: 0, epoch: 0, project: dialogs::new_project(1920, 1080, 30.0), resolved: Default::default(), dir: std::env::temp_dir() }),
        };
        // The first text layout loads the system font database (up to ~1 s): do it off the UI thread.
        let r = services.renderer.clone();
        let _ = std::thread::Builder::new().name("aiframes-fonts".into()).spawn(move || r.warm_up());
        let mut engine = Engine::new(ctx.clone(), services.renderer.clone(), prefs.render_threads as usize, snap);
        engine.set_volume(prefs.volume, false);
        engine.scrub_audio = prefs.scrub_audio;
        // A fixed preview quality is known before the first layout (Auto needs the viewer size).
        let q = Quality::from_id(&prefs.quality);
        if q != Quality::Auto {
            engine.set_scale(q.scale(0.0, 1920));
        }
        let previews = Previews::new(ctx.clone(), services.pool.clone());
        let exe = std::env::current_exe().map(|p| p.display().to_string().trim_start_matches(r"\\?\").to_string()).unwrap_or_else(|_| "aiframes.exe".into());
        let mut viewer = Viewer::default();
        viewer.quality = Quality::from_id(&prefs.quality);
        viewer.safe = prefs.layout.safe_guides;
        viewer.zoom = Zoom::Fit;
        let mut app = AiFramesApp {
            side_tab: SideTab::from_id(&std::env::var("AIFRAMES_SIDE_TAB").unwrap_or_else(|_| prefs.layout.side_tab.clone())),
            services,
            port,
            exe,
            prefs,
            docs: HashMap::new(),
            engine,
            engine_doc: None,
            previews,
            viewer,
            ai: AiPanel::default(),
            export: ExportDialog::default(),
            dialogs: Dialogs::default(),
            toasts: Vec::new(),
            status: String::new(),
            notice: None,
            recent: suite_ui::recent::load(APP_NAME),
            clipboard: None,
            fonts,
            last_title: String::new(),
            now: 0.0,
            exit_queue: None,
            exit_confirmed: false,
            shot_waiters: Vec::new(),
            waker_set: false,
            imports: Vec::new(),
            layers: ((u64::MAX, 0, 0), 0, Vec::new()),
            live_preview: None,
            preview_epoch: 0,
            jobs_seen: HashSet::new(),
            export_paths: HashMap::new(),
            last_export: None,
            focus: Focus::Other,
            viewer_rect: egui::Rect::NOTHING,
            timeline_rect: egui::Rect::NOTHING,
            knobs: EnvKnobs::from_env(),
            perf_next: 0.0,
            frames: 0,
            pending_select: None,
            startup_imports,
            warmed: HashSet::new(),
            last_interaction: -100.0,
            ai_chip: None,
            pending_follow: None,
            last_reveal_at: f64::NEG_INFINITY,
            ext_bursts: Vec::new(),
            used_media: HashSet::new(),
            last_media_sweep: 0.0,
            script: crate::script::Script::from_env(),
            script_action: None,
            exit_in: None,
        };
        for e in startup_errors {
            app.dialogs.alert("Couldn't open file", &e);
        }
        if let Some(msg) = prefs_problem {
            app.dialogs.alert("Preferences reset", &msg);
        }
        match port {
            Some(p) => app.ai.push(LogKind::System, format!("MCP server listening on {}", crate::ai::mcp_url(p))),
            None => app.ai.push(LogKind::Error, "The MCP server could not start, so AI tools cannot reach this window."),
        }
        if let Some(e) = &app.engine.audio.error {
            app.ai.push(LogKind::System, format!("Audio: {e} (playback runs on the wall clock, silently)."));
        }
        app.pending_select = app.knobs.select.take();
        app
    }

    // -----------------------------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------------------------

    fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        let text = text.into();
        if kind == ToastKind::Error {
            eprintln!("aiframes: {text}");
        }
        self.toasts.push(Toast { text, kind, at: self.now });
    }

    fn active(&self) -> Option<(u64, aiframes_mcp::suite_hub::DocRef<ProjectDoc>)> {
        self.services.hub.active()
    }

    fn active_id(&self) -> Option<u64> {
        self.active().map(|(id, _)| id)
    }

    fn playhead(&self) -> f64 {
        self.active_id().and_then(|id| self.docs.get(&id)).map(|d| d.playhead).unwrap_or(0.0)
    }

    fn with_doc<R>(&self, id: u64, f: impl FnOnce(&mut ProjectDoc) -> R) -> Option<R> {
        let d = self.services.hub.get(id)?;
        let mut g = lock(&d);
        Some(f(&mut g))
    }

    /// Applies ops to a document as one undoable user step.
    fn apply(&mut self, id: u64, label: &str, ops: Vec<Op>) -> bool {
        if ops.is_empty() {
            return false;
        }
        let res = self.with_doc(id, |d| d.doc.apply("user", label, ops));
        match res {
            Some(Ok(r)) => {
                self.status = r.label.clone();
                if let Some(w) = r.warnings.first() {
                    self.toast(ToastKind::Info, w.clone());
                }
                true
            }
            Some(Err(e)) => {
                self.toast(ToastKind::Error, format!("{label}: {e}"));
                false
            }
            None => false,
        }
    }

    fn range(&self) -> Option<(f64, f64)> {
        let d = self.docs.get(&self.active_id()?)?;
        match (d.in_point, d.out_point) {
            (Some(a), Some(b)) if b > a => Some((a, b)),
            _ => None,
        }
    }

    fn seek(&mut self, t: f64, scrubbing: bool) {
        if let Some(id) = self.active_id() {
            if self.engine_doc == Some(id) {
                self.engine.seek(t, scrubbing);
                let t = self.engine.time();
                if let Some(d) = self.docs.get_mut(&id) {
                    d.playhead = t;
                }
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Hub plumbing
    // -----------------------------------------------------------------------------------------

    fn collect_notices(&mut self) {
        for n in self.services.hub.take_notices() {
            self.ai.push(LogKind::Notice, n.clone());
            self.notice = Some((n.clone(), self.now));
            self.toast(ToastKind::Ai, n);
        }
        if self.notice.as_ref().is_some_and(|(_, t)| self.now - t > 8.0) {
            self.notice = None;
        }
    }

    fn handle_screenshots(&mut self, ctx: &egui::Context) {
        self.shot_waiters.extend(self.services.hub.take_screenshot_waiters());
        let mut file = None;
        if let Some((path, frames)) = &mut self.knobs.screenshot {
            *frames += 1;
            ctx.request_repaint();
            if *frames >= self.knobs.shot_frames {
                file = Some(path.clone());
            }
        }
        if file.is_some() {
            self.knobs.screenshot = None;
        }
        if self.shot_waiters.is_empty() && file.is_none() {
            return;
        }
        let waiters = std::mem::take(&mut self.shot_waiters);
        let exit = file.is_some() && self.knobs.shot_exit;
        let c = ctx.clone();
        let cb = egui::ScreenshotCallback::new(move |img| {
            let png = encode_png(&img);
            for w in waiters {
                let _ = w.send(png.clone());
            }
            if let (Some(p), Ok(bytes)) = (file, &png) {
                let _ = std::fs::write(p, bytes);
            }
            if exit {
                c.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            c.request_repaint();
        });
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(cb));
    }

    fn run_script(&mut self, ctx: &egui::Context) {
        if let Some(n) = &mut self.exit_in {
            *n = n.saturating_sub(1);
            if *n == 0 {
                std::process::exit(0);
            }
            ctx.request_repaint();
        }
        let Some(f) = self.script_action.take() else { return };
        if let Some(path) = f.shot {
            let waiting = self.script.as_ref().map(|s| s.waiting.clone());
            let cb = egui::ScreenshotCallback::new(move |img| {
                if let Ok(bytes) = encode_png(&img) {
                    let _ = std::fs::write(&path, bytes);
                }
                if let Some(w) = &waiting {
                    w.store(false, std::sync::atomic::Ordering::SeqCst);
                }
            });
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(cb));
        }
        if f.exit {
            self.exit_in = Some(30);
        }
    }

    fn sync_docs(&mut self) {
        let live: Vec<u64> = self.services.hub.list().into_iter().map(|(id, _)| id).collect();
        self.docs.retain(|id, _| live.contains(id));
        for id in live {
            if !self.docs.contains_key(&id) {
                // Cap undo snapshots (an editor keeps many). The file's stamp lives in the document.
                self.with_doc(id, |d| d.doc.undo_budget_bytes = d.doc.undo_budget_bytes.min(128 << 20));
                self.docs.insert(id, DocUi::new(self.now));
            }
        }
    }

    /// Polls the open project files for changes made by other programs (once a second).
    fn watch_files(&mut self) {
        let ids: Vec<u64> = self.docs.keys().copied().collect();
        for id in ids {
            let due = self.docs.get(&id).is_some_and(|d| self.now - d.watch.last_check >= 1.0);
            if !due {
                continue;
            }
            if let Some(d) = self.docs.get_mut(&id) {
                d.watch.last_check = self.now;
            }
            let Some((true, Some(path))) = self.with_doc(id, |d| (d.doc.disk_changed(), d.doc.path.clone())) else { continue };
            let now = aiframes_core::FileStamp::of(&path);
            if !self.docs.get(&id).is_some_and(|d| d.watch.needs_look(now)) {
                continue;
            }
            self.check_external(id, &path, now);
        }
    }

    /// Looks at a changed project file: adopt, reload (clean window) or raise a conflict (dirty).
    fn check_external(&mut self, id: u64, path: &Path, now: Option<aiframes_core::FileStamp>) -> bool {
        use crate::external::{decide, Conflict, Decision};
        let Ok(text) = std::fs::read_to_string(path) else { return false };
        let dirty = self.is_dirty(id);
        let decision = match self.with_doc(id, |d| decide(&d.doc.project, dirty, &text)) {
            Some(d) => d,
            None => return false,
        };
        let name = file_name(path);
        match decision {
            Decision::Same => {
                // The file holds what the window has (our own save, a touch): it matches again.
                self.with_doc(id, |d| d.doc.mark_saved());
                false
            }
            Decision::Reload => {
                // Merges the other program's history entries; the document then counts as saved.
                let res = self.with_doc(id, |d| d.doc.reload_from_disk("external", &format!("Reload {name} (changed on disk)")).map(|r| r.revision));
                match res {
                    Some(Ok(_)) => {
                        if let Some(d) = self.docs.get_mut(&id) {
                            d.epoch += 1;
                        }
                        self.toast(ToastKind::Info, format!("{name} was changed by another program: reloaded (Ctrl+Z goes back)."));
                    }
                    Some(Err(e)) => self.toast(ToastKind::Error, format!("Could not reload {name}: {e}")),
                    None => {}
                }
                false
            }
            Decision::Conflict(p) => {
                if let Some(d) = self.docs.get_mut(&id) {
                    d.watch.conflict = Some(Conflict { stamp: now, project: *p });
                }
                true
            }
            Decision::Retry => false,
        }
    }

    /// The person's answer to a conflict banner.
    fn resolve_conflict(&mut self, id: u64, reload: bool) {
        let Some(c) = self.docs.get_mut(&id).and_then(|d| d.watch.conflict.take()) else { return };
        if let Some(d) = self.docs.get_mut(&id) {
            d.watch.comparing = false;
        }
        if reload {
            let name = self.with_doc(id, |d| d.title()).unwrap_or_default();
            // The newest version on disk (it may have changed again since the banner appeared).
            let res = self.with_doc(id, |d| d.doc.reload_from_disk("external", &format!("Reload {name} from disk (unsaved changes replaced)")).map(|r| r.revision));
            match res {
                Some(Ok(_)) => {
                    if let Some(d) = self.docs.get_mut(&id) {
                        d.epoch += 1;
                    }
                    self.toast(ToastKind::Info, "Reloaded from disk (your version is one Ctrl+Z away).");
                }
                Some(Err(e)) => self.toast(ToastKind::Error, format!("Could not reload {name}: {e}")),
                None => {}
            }
        } else {
            if let Some(d) = self.docs.get_mut(&id) {
                d.watch.acknowledged = c.stamp;
            }
            self.toast(ToastKind::Info, "Keeping your version: saving will replace the file on disk.");
        }
    }

    /// Releases decode sessions, cached frames and thumbnails of media no longer in any open project.
    fn sweep_media(&mut self) {
        if self.now - self.last_media_sweep < 5.0 {
            return;
        }
        self.last_media_sweep = self.now;
        let mut used = HashSet::new();
        for (_, d) in self.services.hub.list() {
            let g = lock(&d);
            let dir = g.doc.project_dir();
            used.extend(g.doc.project.media.iter().map(|m| previews::media_path(&dir, m)));
        }
        for p in self.used_media.difference(&used) {
            self.services.pool.release(p);
            self.previews.forget(p);
            self.warmed.remove(p);
        }
        self.used_media = used;
    }

    fn note_interaction(&mut self, ctx: &egui::Context) {
        let busy = ctx.input(|i| {
            i.pointer.any_down()
                || i.pointer.any_pressed()
                || i.smooth_scroll_delta != egui::Vec2::ZERO
                || i.events.iter().any(|e| matches!(e, Event::Key { pressed: true, .. } | Event::Text(_)))
        });
        if busy {
            self.last_interaction = self.now;
        }
    }

    /// Scrolls the timeline to an AI edit, or offers a chip when the person is busy.
    fn follow_ai(&mut self, id: u64, ids: Vec<Id>, latest: Vec<Id>) {
        use crate::follow::Follow;
        let Some(target) = self.follow_target(id, &ids) else { return };
        let busy = self.person_busy(id);
        let Some((visible, recent)) = self.docs.get(&id).map(|d| (d.w.timeline.visible_range(), self.now - d.view_changed_at < VIEW_KEEP_SECS)) else { return };
        if self.prefs.follow_ai && !busy {
            // Never zoom in; never zoom out over a view the person set recently or for an edit
            // wider than the view: scroll to its first item and offer the rest.
            match crate::follow::plan(&target, visible, recent) {
                Follow::All(t) | Follow::InView(t) if t.items.len() == target.items.len() => {
                    self.reveal(id, &t);
                    self.ai_chip = None;
                }
                Follow::All(t) | Follow::InView(t) | Follow::First(t) => {
                    self.reveal(id, &t);
                    self.ai_chip = Some(AiChip { doc: id, t: target.t0, count: target.items.len(), ids, at: self.now });
                }
            }
        } else if !target.on_screen(visible) {
            self.ai_chip = Some(AiChip { doc: id, t: target.t0, count: target.items.len(), ids, at: self.now });
        }
        // "Follow AI playhead": the viewer shows the latest change while the person is idle.
        if self.prefs.follow_ai_playhead && !busy {
            if let Some(t) = self.follow_target(id, &latest).or(Some(target)) {
                self.seek(t.t0, false);
            }
        }
    }

    /// Playing, dragging in the timeline, or clicked/typed in the last 3 s.
    fn person_busy(&self, id: u64) -> bool {
        self.engine.playing() || self.now - self.last_interaction < 3.0 || self.docs.get(&id).is_some_and(|d| d.w.timeline.is_dragging())
    }

    /// Notes when the person scrolled or zoomed the timeline (not the app's own reveals, not the
    /// page-follow of playback).
    fn track_view(&mut self, id: u64) {
        let playing = self.engine.playing();
        let ours = self.now - self.last_reveal_at < 1.0;
        let now = self.now;
        if let Some(d) = self.docs.get_mut(&id) {
            let v = (d.w.timeline.scroll, d.w.timeline.zoom);
            if let Some(old) = d.view {
                let zoomed = (v.1 - old.1).abs() > 1e-9 * old.1.abs().max(1.0);
                let scrolled = (v.0 - old.0).abs() > 1e-9;
                if (zoomed || scrolled) && !ours && (zoomed || !playing) {
                    d.view_changed_at = now;
                }
            }
            d.view = Some(v);
        }
    }

    /// AI edits from other MCP sessions (not the AI panel's run): grouped per session until quiet,
    /// then summarized in the AI log like a panel run.
    fn note_external_edit(&mut self, doc: u64, key: String, label: String, before: Option<Arc<Snapshot>>, ids: &[Id]) {
        let now = self.now;
        match self.ext_bursts.iter_mut().find(|b| b.doc == doc && b.key == key) {
            Some(b) => {
                b.last = now;
                b.edits += 1;
                for i in ids {
                    if !b.touched.contains(i) {
                        b.touched.push(i.clone());
                    }
                }
            }
            None => {
                let Some(before) = before else { return };
                self.ext_bursts.push(ExtBurst { doc, key, label, before, touched: ids.to_vec(), last: now, edits: 1 });
            }
        }
    }

    fn summarize_external(&mut self, ctx: &egui::Context) {
        if self.ext_bursts.is_empty() {
            return;
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
        let now = self.now;
        let (done, keep): (Vec<ExtBurst>, Vec<ExtBurst>) = std::mem::take(&mut self.ext_bursts).into_iter().partition(|b| now - b.last >= EXT_QUIET_SECS);
        self.ext_bursts = keep;
        for b in done {
            let Some(res) = self.with_doc(b.doc, |d| (d.title(), crate::follow::session_recap(&b.before.project, &d.doc.project, &b.touched))) else { continue };
            let (title, groups) = res;
            let who = b.label.clone();
            let n: usize = groups.iter().map(Vec::len).sum();
            if n == 0 {
                continue;
            }
            self.ai.push_summary(format!("{who} finished editing {title}: {} edit(s), {n} item(s):", b.edits), groups);
            self.toast(ToastKind::Ai, format!("{who} finished: {n} item(s) changed in {title}. The list is in the AI tab."));
        }
    }

    /// Selects a clip/marker named in the AI log and shows it (timeline scrolled, playhead at it).
    fn show_item(&mut self, id: u64, item: &str) {
        let ids = vec![item.to_string()];
        let Some(target) = self.follow_target(id, &ids) else {
            self.status = format!("{item} is no longer in the project");
            return;
        };
        if let Some(d) = self.docs.get_mut(&id) {
            d.selection = ids;
        }
        self.reveal(id, &target);
        self.seek(target.t0, false);
    }

    /// "AI edited at 00:41 ▸": shown instead of scrolling while the person is busy.
    fn draw_ai_chip(&mut self, ctx: &egui::Context, id: u64) {
        // The chip waits while the person is still busy (playing, dragging, typing) and expires
        // 30 s after they stop, so a long playback does not swallow it.
        let busy = self.engine.playing() || self.now - self.last_interaction < 3.0;
        if busy {
            if let Some(c) = self.ai_chip.as_mut() {
                c.at = c.at.max(self.now);
            }
        }
        let Some(AiChip { doc, t, count, ids, at }) = self.ai_chip.clone() else { return };
        if doc != id || self.now - at > 30.0 || self.timeline_rect == egui::Rect::NOTHING {
            if self.now - at > 30.0 {
                self.ai_chip = None;
            }
            return;
        }
        let pal = theme::palette();
        let fps = self.engine.snapshot().fps();
        let tc = timecode::timecode(t, fps);
        let pos = self.timeline_rect.right_top() + egui::vec2(-12.0, 34.0);
        let mut go = false;
        let mut close = false;
        egui::Area::new(egui::Id::unique("aiframes-ai-chip")).fixed_pos(pos).pivot(egui::Align2::RIGHT_TOP).order(egui::Order::Foreground).show(ctx, |ui| {
            egui::Frame::NONE.fill(egui::Color32::from_rgb(0x3a, 0x2c, 0x10)).stroke(egui::Stroke::new(1.0, pal.ai)).corner_radius(10).inner_margin(egui::Margin::symmetric(8, 3)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    let text = if count > 1 { format!("◆ AI edited {count} items from {} ▸", &tc[3..]) } else { format!("◆ AI edited at {} ▸", &tc[3..]) };
                    let r = ui.add(egui::Button::new(egui::RichText::new(text).color(pal.ai)).frame(false));
                    if r.on_hover_text(format!("Show {} in the timeline", ids.join(", "))).clicked() {
                        go = true;
                    }
                    if ui.add(egui::Button::new(egui::RichText::new("×").color(pal.text_dim)).frame(false)).clicked() {
                        close = true;
                    }
                });
            });
        });
        if go {
            if let Some(target) = self.follow_target(id, &ids) {
                self.reveal(id, &target);
                self.seek(target.t0, false);
            }
            self.ai_chip = None;
        } else if close {
            self.ai_chip = None;
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }

    /// Side-by-side descriptions of the window's project and the version on disk.
    fn draw_compare(&mut self, ctx: &egui::Context, id: u64) {
        let open_now = self.docs.get(&id).is_some_and(|d| d.watch.comparing && d.watch.conflict.is_some());
        if !open_now {
            return;
        }
        let mine = self.with_doc(id, |d| d.doc.describe()).unwrap_or_default();
        let disk = self.docs.get(&id).and_then(|d| d.watch.conflict.as_ref()).map(|c| aiframes_core::describe(&c.project, &aiframes_core::resolve(&c.project))).unwrap_or_default();
        let mut open = true;
        let mut answer = None;
        egui::Window::new("Compare with the file on disk").open(&mut open).default_size([900.0, 560.0]).pivot(egui::Align2::CENTER_CENTER).default_pos(ctx.content_rect().center()).show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Reload (use the disk version)").clicked() {
                    answer = Some(true);
                }
                if ui.button("Keep mine").clicked() {
                    answer = Some(false);
                }
            });
            ui.separator();
            ui.columns(2, |cols| {
                for (ui, (title, text)) in cols.iter_mut().zip([("In this window (unsaved)", &mine), ("On disk (changed by another program)", &disk)]) {
                    ui.label(egui::RichText::new(title).strong());
                    egui::ScrollArea::both().id_salt(title).max_height(460.0).show(ui, |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(text).monospace().size(11.0)).extend());
                    });
                }
            });
        });
        if let Some(reload) = answer {
            self.resolve_conflict(id, reload);
        } else if !open {
            if let Some(d) = self.docs.get_mut(&id) {
                d.watch.comparing = false;
            }
        }
    }

    fn follow_target(&self, id: u64, ids: &[Id]) -> Option<crate::follow::Target> {
        self.with_doc(id, |d| crate::follow::target(&d.doc.project, d.doc.resolved(), ids)).flatten()
    }

    /// Brings a target into view next frame (time and track rows); never moves the playhead.
    fn reveal(&mut self, id: u64, target: &crate::follow::Target) {
        self.last_reveal_at = self.now;
        if let Some(d) = self.docs.get_mut(&id) {
            d.w.timeline.reveal(target.t0, target.t1, &target.tracks);
        }
    }

    /// Per frame, before drawing: live requests from the AI, flashes, snapshot refresh, recovery.
    fn pre_frame(&mut self, id: u64) {
        let Some(d) = self.services.hub.get(id) else { return };
        let now = self.now;
        let mut g = lock(&d);
        let dui = self.docs.entry(id).or_insert_with(|| DocUi::new(now));
        let mut ext_edit: Option<ExtEdit> = None;
        // AI requests.
        let seek = g.live.seek_request.take();
        let play = g.live.play_request.take();
        let play_until = if play == Some(true) { g.live.play_until } else { None };
        if g.live.play_until.is_some() && play.is_none() && !self.engine.playing() {
            // The AI's bounded play finished.
            g.live.play_until = None;
        }
        if let Some(panel) = g.live.panel_request.take() {
            // (added) The AI wants the person to see a side panel (e.g. its suggestions in Review).
            if let Some(t) = SideTab::ALL.into_iter().find(|t| t.id() == panel) {
                self.side_tab = t;
                self.prefs.layout.show_side = true;
            }
        }
        if let Some(sel) = g.live.select_request.take() {
            // The AI selected something to show the person: follow it like an edit.
            if !sel.is_empty() {
                let mut ids = match self.pending_follow.take() {
                    Some((d, v, _, _)) if d == id => v,
                    _ => Vec::new(),
                };
                ids.extend(sel.iter().filter(|x| !ids.contains(x)).cloned().collect::<Vec<_>>());
                self.pending_follow = Some((id, ids, now, sel.clone()));
            }
            dui.selection = sel;
        }
        if g.live.ai_changed_revision != dui.ai_seen_rev {
            dui.ai_seen_rev = g.live.ai_changed_revision;
            if !g.live.ai_changed.is_empty() {
                dui.flash_ids = g.live.ai_changed.clone();
                dui.flash_start = now;
                let mut ids = match self.pending_follow.take() {
                    Some((d, v, _, _)) if d == id => v,
                    _ => Vec::new(),
                };
                for x in &g.live.ai_changed {
                    if !ids.contains(x) {
                        ids.push(x.clone());
                    }
                }
                self.pending_follow = Some((id, ids, now, g.live.ai_changed.clone()));
                // Edits from another MCP session (a terminal Claude): recapped when it goes quiet.
                if !self.ai.running() {
                    let rev = g.live.ai_changed_revision;
                    let session = g.live.ai_session.clone().or_else(|| g.live.ai_sessions.get(&rev).cloned());
                    let (key, label) = match session {
                        Some(s) => (format!("session:{}", s.id), s.label),
                        // Session-less edits (the CLI, plain HTTP): by author.
                        None => {
                            let author = g.doc.history().iter().rev().find(|e| e.revision == rev).map(|e| e.author.clone()).unwrap_or_else(|| aiframes_mcp::AI_AUTHOR.to_string());
                            let label = if author == aiframes_mcp::AI_AUTHOR { "Claude (another session)".to_string() } else { format!("{author} (another session)") };
                            (format!("author:{author}"), label)
                        }
                    };
                    ext_edit = Some((key, label, dui.snap.clone(), g.live.ai_changed.clone()));
                }
            }
        }
        if dui.flash(now) <= 0.0 {
            dui.flash_ids.clear();
        }
        // Snapshot of what is rendered.
        let key = (id, g.doc.revision(), dui.epoch);
        let mut warm: Vec<PathBuf> = Vec::new();
        if dui.snap.as_ref().is_none_or(|s| s.key() != key) {
            dui.snap = Some(snapshot_of(id, &g, dui.epoch));
            // Drop selected ids that no longer exist (e.g. AI scaffolding it removed again).
            let p = &g.doc.project;
            dui.selection.retain(|x| {
                p.clips.iter().any(|c| &c.id == x) || p.markers.iter().any(|m| &m.id == x) || p.transitions.iter().any(|t| &t.id == x) || p.tracks.iter().any(|t| &t.id == x) || p.media.iter().any(|m| &m.id == x)
            });
            // Decode the sound of newly used media in the background so Play starts at once.
            let dir = g.doc.project_dir();
            for m in g.doc.project.media.iter().filter(|m| m.kind != MediaKind::Image && (m.info.has_audio || m.kind == MediaKind::Audio)) {
                let p = previews::media_path(&dir, m);
                if self.warmed.insert(p.clone()) {
                    warm.push(p);
                }
            }
        }
        let snap = dui.snap.clone().expect("snapshot");
        let recovery = if !dui.checked_recovery {
            dui.checked_recovery = true;
            g.doc.path.as_deref().and_then(prefs::recoverable).map(|a| (a, g.title()))
        } else {
            None
        };
        let rate = g.doc.project.settings.sample_rate;
        let dur = g.doc.resolved().duration;
        drop(g);
        if let Some((key, label, before, ids)) = ext_edit {
            self.note_external_edit(id, key, label, before, &ids);
        }
        if let Some((autosave, title)) = recovery {
            self.dialogs.modal = Some(Modal::Recover { doc: id, autosave, title });
        }
        if !warm.is_empty() {
            // Waveforms for the timeline. Sound itself is decoded on demand (prefetch on seek/Play).
            self.previews.prefetch_waves(warm);
        }
        let _ = rate;
        // Engine follows the active document.
        if self.engine_doc != Some(id) {
            self.engine.pause();
            self.engine.set_snapshot(snap.clone());
            self.engine_doc = Some(id);
            let t = self.docs.get(&id).map(|d| d.playhead).unwrap_or(0.0);
            self.engine.seek(t.min(dur.max(0.0)), false);
        } else if self.live_preview.is_none() {
            self.engine.set_snapshot(snap);
        }
        if let Some(t) = seek {
            self.engine.seek(t, false);
        }
        match play {
            Some(true) => {
                let t = self.engine.time();
                let range = play_until.filter(|u| *u > t + 1e-3).map(|u| (t, u));
                if range.is_some() || !self.engine.playing() {
                    self.engine.play(t, 1.0, range);
                }
            }
            Some(false) => self.engine.pause(),
            _ => {}
        }
        if let Some(sel) = self.pending_select.take() {
            if let Some(d) = self.docs.get_mut(&id) {
                d.selection = sel;
            }
        }
        if let Some(t) = self.knobs.seek.take() {
            self.engine.seek(t, false);
        }
        if self.knobs.autoplay {
            self.knobs.autoplay = false;
            self.engine.looping = true;
            let t = self.engine.time();
            self.engine.play(t, 1.0, None);
        }
    }

    /// A transient snapshot with the dragged transform values (live preview while dragging).
    fn apply_live_preview(&mut self, id: u64, lp: &LivePreview) {
        let Some(base) = self.docs.get(&id).and_then(|d| d.snap.clone()) else { return };
        let mut project = base.project.clone();
        if let Some(c) = project.clips.iter_mut().find(|c| c.id == lp.clip) {
            c.transform.x = Param::Value(lp.values[0]);
            c.transform.y = Param::Value(lp.values[1]);
            c.transform.scale = Param::Value(lp.values[2]);
            c.transform.rotation = Param::Value(lp.values[3]);
        }
        self.preview_epoch += 1;
        let snap = Arc::new(Snapshot { doc: base.doc, revision: base.revision, epoch: 1_000_000 + self.preview_epoch, project, resolved: base.resolved.clone(), dir: base.dir.clone() });
        self.engine.set_snapshot(snap);
        self.engine.set_scrubbing(true);
        let _ = PARAM_PATHS;
    }

    fn layers_for(&mut self, snap: &Snapshot, t: f64) -> Vec<LayerBox> {
        let f = frame_index(t, snap.fps());
        if self.layers.0 != snap.key() || self.layers.1 != f {
            let l = self.services.renderer.layers_at(&snap.dir, &snap.project, &snap.resolved, frame_time(f, snap.fps()));
            self.layers = (snap.key(), f, l);
        }
        self.layers.2.clone()
    }

    // -----------------------------------------------------------------------------------------
    // Actions and commands
    // -----------------------------------------------------------------------------------------

    fn handle_action(&mut self, id: u64, a: UiAction, ctx: &egui::Context) {
        match a {
            UiAction::Seek(t) => {
                let scrub = ctx.input(|i| i.pointer.primary_down());
                self.seek(t, scrub);
            }
            UiAction::Select(ids) => {
                if let Some(d) = self.docs.get_mut(&id) {
                    d.selection = ids;
                }
            }
            UiAction::Apply { label, ops } => {
                self.apply(id, &label, ops);
            }
            UiAction::Undo => self.run(Command::Undo, ctx),
            UiAction::Redo => self.run(Command::Redo, ctx),
            UiAction::RestoreRevision(rev) => {
                let res = self.with_doc(id, |d| {
                    let Some((u, r)) = edits::restore_plan(d.doc.history(), rev) else { return Err("that revision cannot be restored (it was replaced by later edits)".to_string()) };
                    for _ in 0..u {
                        if d.doc.undo().is_none() {
                            return Err("the undo history does not go back that far".to_string());
                        }
                    }
                    for _ in 0..r {
                        if d.doc.redo().is_none() {
                            break;
                        }
                    }
                    Ok(u + r)
                });
                match res {
                    Some(Ok(n)) => self.status = format!("Restored revision {rev} ({n} steps)"),
                    Some(Err(e)) => self.toast(ToastKind::Error, e),
                    None => {}
                }
            }
            UiAction::TogglePlay => self.run(Command::PlayPause, ctx),
            UiAction::SetIn(v) => {
                if let Some(d) = self.docs.get_mut(&id) {
                    d.in_point = v;
                }
            }
            UiAction::SetOut(v) => {
                if let Some(d) = self.docs.get_mut(&id) {
                    d.out_point = v;
                }
            }
            UiAction::Import(paths) => self.import(id, paths, None),
            UiAction::Transcribe(asset) => match actions::start_transcribe(&self.services, id, &asset, "user") {
                Ok(_) => self.status = format!("Transcribing {asset}…"),
                Err(e) => self.toast(ToastKind::Error, format!("Transcribe: {e}")),
            },
            UiAction::TranscribeStream(asset, stream) => {
                let opts = aiframes_media::TranscribeOptions { stream: Some(stream), ..Default::default() };
                match actions::start_transcribe_with(&self.services, id, &asset, "user", opts) {
                    Ok(_) => self.status = format!("Transcribing {asset} (stream {})…", stream + 1),
                    Err(e) => self.toast(ToastKind::Error, format!("Transcribe: {e}")),
                }
            }
            UiAction::Status(s) => self.status = s,
            UiAction::AskAi { prompt, ids } => {
                self.side_tab = SideTab::Ai;
                self.prefs.layout.show_side = true;
                self.ai.ask(&prompt, ids);
            }
            UiAction::RevealMedia(asset) => {
                if let Some(p) = self.with_doc(id, |d| d.doc.project.media.iter().find(|m| m.id == asset).map(|m| d.doc.media_path(m))).flatten() {
                    reveal(&p);
                }
            }
            UiAction::RelinkMedia(asset) => self.relink(id, &asset),
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }

    /// Runs a shortcut / menu / transport command.
    fn run(&mut self, c: Command, ctx: &egui::Context) {
        let id = self.active_id();
        let t = self.playhead();
        let fps = self.engine.snapshot().fps();
        match c {
            Command::PlayPause => {
                let range = self.range().filter(|_| self.engine.looping);
                self.engine.shuttle(t, Shuttle::Toggle, range);
            }
            Command::PlayFromIn => {
                let d = id.and_then(|i| self.docs.get(&i));
                let a = d.and_then(|d| d.in_point).unwrap_or(0.0);
                let b = d.and_then(|d| d.out_point).filter(|b| *b > a);
                let end = b.unwrap_or_else(|| self.engine.snapshot().duration());
                self.engine.play(a, 1.0, Some((a, end)));
            }
            Command::ShuttleForward | Command::ShuttleReverse => {
                // K held + J/L = one frame step.
                if ctx.input(|i| i.key_down(Key::K)) {
                    let step = if c == Command::ShuttleForward { 1.0 } else { -1.0 };
                    self.engine.pause();
                    self.seek(timecode::snap(t, fps) + step / fps, false);
                } else {
                    let s = if c == Command::ShuttleForward { Shuttle::Forward } else { Shuttle::Reverse };
                    self.engine.shuttle(t, s, None);
                }
            }
            Command::ShuttleStop => self.engine.shuttle(t, Shuttle::Stop, None),
            Command::FrameBack | Command::FrameForward | Command::FrameBack10 | Command::FrameForward10 => {
                let n = match c {
                    Command::FrameBack => -1.0,
                    Command::FrameForward => 1.0,
                    Command::FrameBack10 => -10.0,
                    _ => 10.0,
                };
                if self.engine.playing() {
                    self.engine.pause();
                }
                let base = frame_time(frame_index(t, fps), fps);
                self.seek(base + n / fps + 1e-9, false);
            }
            Command::PrevEdit | Command::NextEdit => {
                let pts = edits::edit_points(&self.engine.snapshot().resolved);
                let to = if c == Command::PrevEdit { edits::prev_edit(&pts, t) } else { edits::next_edit(&pts, t) };
                if let Some(to) = to {
                    self.seek(to, false);
                }
            }
            Command::GoStart => self.seek(0.0, false),
            Command::GoEnd => {
                let snap = self.engine.snapshot();
                self.seek((snap.duration() - 1.0 / snap.fps()).max(0.0), false);
            }
            Command::SetIn | Command::SetOut | Command::ClearInOut => {
                if let Some(d) = id.and_then(|i| self.docs.get_mut(&i)) {
                    match c {
                        Command::SetIn => d.in_point = Some(t),
                        Command::SetOut => d.out_point = Some(t),
                        _ => {
                            d.in_point = None;
                            d.out_point = None;
                        }
                    }
                }
            }
            Command::ToggleLoop => {
                self.engine.looping = !self.engine.looping;
                self.status = format!("Loop {}", if self.engine.looping { "on" } else { "off" });
            }
            Command::Undo | Command::Redo => {
                if let Some(id) = id {
                    let undo = c == Command::Undo;
                    let r = self.with_doc(id, |d| if undo { d.doc.undo() } else { d.doc.redo() }).flatten();
                    match r {
                        Some(r) => self.status = r.label,
                        None => self.status = format!("Nothing to {}", if undo { "undo" } else { "redo" }),
                    }
                }
            }
            Command::New => self.new_doc(0),
            Command::Open => self.open_dialog(),
            Command::Save => {
                if let Some(id) = id {
                    self.save(id, false);
                }
            }
            Command::SaveAs => {
                if let Some(id) = id {
                    self.save(id, true);
                }
            }
            Command::Close => {
                if let Some(id) = id {
                    self.close(id, AfterSave::Close);
                }
            }
            Command::Import => {
                if let Some(id) = id {
                    self.import(id, Vec::new(), None);
                }
            }
            Command::Export => {
                if let Some(id) = id {
                    let has_range = self.range().is_some();
                    if let Some(p) = self.with_doc(id, |d| d.doc.project.clone()) {
                        self.export.open_for(&p, has_range);
                    }
                }
            }
            Command::Split | Command::Delete | Command::RippleDelete | Command::SelectAll | Command::AddMarker => {
                let Some(id) = id else { return };
                let sel = self.docs.get(&id).map(|d| d.selection.clone()).unwrap_or_default();
                let snap = self.engine.snapshot();
                let (p, r) = (&snap.project, &snap.resolved);
                match c {
                    Command::Split => {
                        let ops = edits::split_ops(p, r, &sel, t);
                        if ops.is_empty() {
                            self.status = "Nothing under the playhead to split".into();
                        }
                        self.apply(id, "Split", ops);
                    }
                    Command::Delete | Command::RippleDelete => {
                        let ripple = c == Command::RippleDelete;
                        let ops = edits::delete_ops(p, &sel, ripple);
                        if self.apply(id, if ripple { "Ripple delete" } else { "Delete" }, ops) {
                            if let Some(d) = self.docs.get_mut(&id) {
                                d.selection.clear();
                            }
                        }
                    }
                    Command::SelectAll => {
                        let all = edits::all_clips(p);
                        if let Some(d) = self.docs.get_mut(&id) {
                            d.selection = all;
                        }
                    }
                    _ => {
                        let ops = edits::marker_op(t).into_iter().collect();
                        self.apply(id, "Add marker", ops);
                    }
                }
            }
            Command::Deselect => {
                if let Some(d) = id.and_then(|i| self.docs.get_mut(&i)) {
                    d.selection.clear();
                }
            }
            Command::ZoomIn | Command::ZoomOut | Command::ZoomFit => {
                let dur = self.engine.snapshot().duration().max(1.0);
                if let Some(d) = id.and_then(|i| self.docs.get_mut(&i)) {
                    let tl = &mut d.w.timeline;
                    match c {
                        Command::ZoomIn => tl.zoom = (tl.zoom * 1.4).min(2000.0),
                        Command::ZoomOut => tl.zoom = (tl.zoom / 1.4).max(0.5),
                        _ => tl.fit(dur),
                    }
                }
            }
            Command::ToggleSafe => self.viewer.safe = !self.viewer.safe,
            Command::ToolSelect | Command::ToolRazor | Command::ToolSlip | Command::ToolSlide | Command::ToolRipple | Command::ToolRoll => {
                if let (Some(d), Some(t)) = (id.and_then(|i| self.docs.get_mut(&i)), shortcuts::tool_of(c)) {
                    d.w.timeline.tool = t;
                    self.status = format!("{} tool", t.label());
                }
            }
            Command::ProjectSettings => {
                if let Some(id) = id {
                    let form = self.with_doc(id, |d| dialogs::ProjectForm::from(&d.doc.project));
                    self.dialogs.project = form;
                }
            }
            Command::Preferences => self.dialogs.prefs = Some(self.prefs.clone()),
            Command::AiPanel => {
                self.side_tab = SideTab::Ai;
                self.prefs.layout.show_side = true;
                self.ai.focus_prompt = true;
            }
            Command::Shortcuts => self.dialogs.shortcuts = true,
        }
    }

    fn cmd(&mut self, c: Cmd, ctx: &egui::Context) {
        let id = self.active_id();
        match c {
            Cmd::Key(k) => self.run(k, ctx),
            Cmd::NewFormat(i) => self.new_doc(i),
            Cmd::OpenPath(p) => self.open_path(p),
            Cmd::ClearRecent => {
                for p in self.recent.clone() {
                    suite_ui::recent::remove(APP_NAME, &p);
                }
                self.recent.clear();
            }
            Cmd::CloseDoc(d) => self.close(d, AfterSave::Close),
            Cmd::Activate(d) => {
                self.services.hub.set_active(d);
            }
            Cmd::Exit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Cmd::Cut | Cmd::Copy => {
                if let Some(id) = id {
                    let sel = self.docs.get(&id).map(|d| d.selection.clone()).unwrap_or_default();
                    let snap = self.engine.snapshot();
                    if let Some(text) = edits::copy_clips(&snap.project, &snap.resolved, &sel) {
                        ctx.copy_text(text.clone());
                        self.clipboard = Some(text);
                        self.status = format!("{} {} clip(s)", if matches!(c, Cmd::Cut) { "Cut" } else { "Copied" }, sel.len());
                        if matches!(c, Cmd::Cut) {
                            let ops = edits::delete_ops(&snap.project, &sel, false);
                            self.apply(id, "Cut", ops);
                            if let Some(d) = self.docs.get_mut(&id) {
                                d.selection.clear();
                            }
                        }
                    }
                }
            }
            Cmd::Paste => {
                if let (Some(id), Some(text)) = (id, self.clipboard.clone()) {
                    self.paste(id, &text);
                }
            }
            Cmd::Speed => {
                // Start from the (first) selected clip's speed.
                let speed = id
                    .and_then(|i| {
                        let sel = self.docs.get(&i)?.selection.clone();
                        self.with_doc(i, |d| d.doc.project.clips.iter().find(|c| sel.contains(&c.id)).map(|c| aiframes_core::sample(&c.speed, 0.0)))
                    })
                    .flatten()
                    .unwrap_or(1.0);
                self.dialogs.modal = Some(Modal::Speed { speed, keep_duration: false });
            }
            Cmd::Freeze | Cmd::DetachAudio | Cmd::ToggleEnable => {
                let Some(id) = id else { return };
                let sel = self.docs.get(&id).map(|d| d.selection.clone()).unwrap_or_default();
                let snap = self.engine.snapshot();
                let t = self.playhead();
                let (label, ops) = match c {
                    Cmd::Freeze => ("Freeze frame", edits::freeze_ops(&snap.project, &snap.resolved, &sel, t, 2.0)),
                    Cmd::DetachAudio => ("Detach audio", edits::detach_audio_ops(&snap.project, &snap.resolved, &sel)),
                    _ => ("Enable/disable clips", edits::set_enabled_ops(&snap.project, &sel)),
                };
                if ops.is_empty() {
                    self.status = format!("{label}: nothing applicable in the selection");
                } else {
                    self.apply(id, label, ops);
                }
            }
            Cmd::AddTrack(kind) => {
                if let Some(id) = id {
                    let ops = edits::add_track_op(kind, None).into_iter().collect();
                    self.apply(id, if kind == TrackKind::Audio { "Add audio track" } else { "Add video track" }, ops);
                }
            }
            Cmd::ToggleSnapping => {
                if let Some(d) = id.and_then(|i| self.docs.get_mut(&i)) {
                    d.w.timeline.snapping = !d.w.timeline.snapping;
                }
            }
            Cmd::Tool(t) => {
                if let Some(d) = id.and_then(|i| self.docs.get_mut(&i)) {
                    d.w.timeline.tool = t;
                }
            }
            Cmd::TogglePanel(p) => {
                let l = &mut self.prefs.layout;
                match p {
                    PanelId::Bin => l.show_bin = !l.show_bin,
                    PanelId::Side => l.show_side = !l.show_side,
                    PanelId::Timeline => l.show_timeline = !l.show_timeline,
                }
            }
            Cmd::Side(t) => {
                self.side_tab = t;
                self.prefs.layout.show_side = true;
            }
            Cmd::Zoom(z) => {
                self.viewer.zoom = z;
                self.viewer.pan = egui::Vec2::ZERO;
            }
            Cmd::Quality(q) => self.viewer.quality = q,
            Cmd::Connect => self.dialogs.connect = true,
            Cmd::About => self.dialogs.about = true,
            Cmd::CopyEndpoint => {
                if let Some(p) = self.port {
                    ctx.copy_text(crate::ai::mcp_url(p));
                    self.toast(ToastKind::Info, format!("Copied {}", crate::ai::mcp_url(p)));
                }
            }
            Cmd::ResetStats => self.engine.reset_stats(),
            Cmd::ToggleFollowAi => {
                self.prefs.follow_ai = !self.prefs.follow_ai;
                self.status = format!("Follow AI {}", if self.prefs.follow_ai { "on" } else { "off" });
            }
            Cmd::ToggleFollowAiPlayhead => {
                self.prefs.follow_ai_playhead = !self.prefs.follow_ai_playhead;
                self.status = format!("Follow AI playhead {}", if self.prefs.follow_ai_playhead { "on" } else { "off" });
            }
            Cmd::ToggleScrubAudio => {
                self.engine.scrub_audio = !self.engine.scrub_audio;
                self.prefs.scrub_audio = self.engine.scrub_audio;
                self.status = format!("Audio scrubbing {}", if self.engine.scrub_audio { "on" } else { "off" });
            }
            Cmd::CancelJob(j) => {
                self.services.jobs.cancel(j);
            }
            Cmd::OpenFolder(p) => reveal(&p),
            Cmd::MatchLength | Cmd::FixedLength => {
                let Some(id) = id else { return };
                let sel = self.docs.get(&id).map(|d| d.selection.clone()).unwrap_or_default();
                let res = self.with_doc(id, |d| match c {
                    Cmd::MatchLength => edits::match_length_ops(&d.doc.project, &sel).map(|(l, ops)| (format!("Match length of {l}"), ops)),
                    _ => Some(("Fixed length".to_string(), edits::fixed_length_ops(&d.doc.project, &sel))),
                });
                match res.flatten() {
                    Some((label, ops)) if !ops.is_empty() => {
                        self.apply(id, &label, ops);
                    }
                    _ => self.status = "Nothing in the selection to change.".into(),
                }
            }
            Cmd::CloseGaps | Cmd::SetSpacing => {
                let Some(id) = id else { return };
                let sel = self.docs.get(&id).map(|d| d.selection.clone()).unwrap_or_default();
                let Some(Some((track, label))) = self.with_doc(id, |d| edits::gap_track(&d.doc.project, &sel).map(|(t, _, _)| (t.clone(), track_label(&d.doc.project, &t)))) else {
                    self.status = "No track with two clips or more.".into();
                    return;
                };
                if matches!(c, Cmd::CloseGaps) {
                    self.apply(id, &format!("Close gaps on {label}"), vec![Op::CloseGaps { track, max_gap: None }]);
                } else if let Some(d) = self.docs.get_mut(&id) {
                    // The timeline's own dialog (live ghost preview; it can switch tracks).
                    d.w.timeline.open_spacing(Some(track));
                }
            }
            Cmd::Captions(replace) => {
                let Some(id) = id else { return };
                let sel = self.docs.get(&id).map(|d| d.selection.clone()).unwrap_or_default();
                let plan = self.with_doc(id, |d| {
                    let p = &d.doc.project;
                    let clips = aiframes_ui::edits::transcript_clips(p, d.doc.resolved(), &sel);
                    let (n, edited) = aiframes_ui::edits::caption_counts(p);
                    let ops = aiframes_ui::edits::caption_ops_with(p, &clips, None, Some(5), replace);
                    (clips.is_empty(), n, edited, aiframes_ui::edits::caption_receipt(p, &ops), ops)
                });
                let Some((no_clips, n, edited, receipt, ops)) = plan else { return };
                if no_clips {
                    self.status = "No transcribed clips: transcribe one first (Transcript tab).".into();
                    return;
                }
                let receipt = match receipt {
                    Ok(r) => r,
                    Err(e) => {
                        self.toast(ToastKind::Error, format!("Captions: {e}"));
                        return;
                    }
                };
                if replace {
                    let edits = match edited {
                        0 => String::new(),
                        1 => " The one you edited is replaced too.".to_string(),
                        k => format!(" The {k} you edited are replaced too."),
                    };
                    let text = format!("Regenerate all {n} caption(s) from the transcript?{edits} Captions you deleted come back.\n\n{receipt}");
                    self.dialogs.modal = Some(Modal::Confirm { title: "Replace all captions?".into(), text, ok: "Replace All".into(), label: "Replace all captions".into(), ops });
                } else {
                    let label = if n > 0 { "Regenerate captions" } else { "Generate captions" };
                    if self.apply(id, label, ops) {
                        self.status = receipt;
                    }
                }
            }
            Cmd::ExportOtio => {
                if let Some(id) = id {
                    self.export_otio(id);
                }
            }
            Cmd::Validate => {
                if let Some(id) = id {
                    let findings = self.with_doc(id, |d| d.doc.validate()).unwrap_or_default();
                    if findings.is_empty() {
                        self.toast(ToastKind::Info, "No problems found.");
                    } else {
                        let text: Vec<String> = findings.iter().take(12).map(|f| format!("• [{}] {}", f.code, f.message)).collect();
                        let more = if findings.len() > 12 { format!("\n… and {} more", findings.len() - 12) } else { String::new() };
                        self.dialogs.alert(&format!("{} finding(s)", findings.len()), &format!("{}{more}", text.join("\n")));
                    }
                }
            }
        }
    }

    fn paste(&mut self, id: u64, text: &str) {
        let snap = self.engine.snapshot();
        let t = self.playhead();
        match edits::paste_ops(&snap.project, text, t) {
            Ok(ops) => {
                self.apply(id, "Paste", ops);
            }
            Err(e) => self.status = e,
        }
    }

    // -----------------------------------------------------------------------------------------
    // Files
    // -----------------------------------------------------------------------------------------

    fn new_doc(&mut self, format: usize) {
        let (_, w, h, fps) = dialogs::FORMATS.get(format).copied().unwrap_or(dialogs::FORMATS[0]);
        let doc = Document::new(dialogs::new_project(w, h, fps));
        self.services.hub.add(ProjectDoc::new(doc));
    }

    fn open_dialog(&mut self) {
        let mut dlg = rfd::FileDialog::new().set_title("Open project").add_filter("AIFrames project", &["aifr"]).add_filter("All files", &["*"]);
        if let Some(d) = self.recent.first().and_then(|p| p.parent()) {
            dlg = dlg.set_directory(d);
        }
        if let Some(files) = dlg.pick_files() {
            for f in files {
                self.open_path(f);
            }
        }
    }

    fn open_path(&mut self, p: PathBuf) {
        let p = std::fs::canonicalize(&p).map(|c| PathBuf::from(c.to_string_lossy().trim_start_matches(r"\\?\"))).unwrap_or(p);
        if let Some(id) = self.services.hub.find_path(&p) {
            self.services.hub.set_active(id);
            return;
        }
        match Document::open(&p) {
            Ok(doc) => {
                // Replace an untouched blank project.
                let list = self.services.hub.list();
                if list.len() == 1 {
                    let (bid, d) = &list[0];
                    let blank = {
                        let g = lock(d);
                        g.doc.path.is_none() && g.doc.revision() == 0 && g.doc.project.clips.is_empty() && g.doc.project.media.is_empty()
                    };
                    if blank {
                        self.services.hub.remove(*bid);
                        self.docs.remove(bid);
                    }
                }
                self.services.hub.add(ProjectDoc::new(doc));
                suite_ui::recent::push(APP_NAME, &p);
                self.recent = suite_ui::recent::load(APP_NAME);
            }
            Err(e) => {
                suite_ui::recent::remove(APP_NAME, &p);
                self.recent = suite_ui::recent::load(APP_NAME);
                self.dialogs.alert("Couldn't open project", &format!("{}\n\n{e}", p.display()));
            }
        }
    }

    /// (added) Asks where to save the cut as an OpenTimelineIO file and writes it.
    fn export_otio(&mut self, id: u64) {
        let Some((path, title)) = self.with_doc(id, |d| (d.doc.path.clone(), d.title())) else { return };
        let stem = path.as_deref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| title.clone());
        let mut dlg = rfd::FileDialog::new().set_title("Export timeline (.otio)").set_file_name(format!("{stem}.otio")).add_filter("OpenTimelineIO", &["otio"]);
        if let Some(d) = path.as_ref().and_then(|p| p.parent()) {
            dlg = dlg.set_directory(d);
        }
        let Some(target) = dlg.save_file() else { return };
        let target = crate::export::with_ext(&target, "otio");
        let made = self.with_doc(id, |d| {
            let doc = &d.doc;
            let (v, rep) = aiframes_core::otio::to_otio(&doc.project, doc.resolved(), &|m| doc.media_path(m));
            let srt = aiframes_core::otio::captions_srt(&doc.project, doc.resolved());
            (serde_json::to_string_pretty(&v).unwrap_or_default(), srt, rep)
        });
        let Some((text, srt, rep)) = made else { return };
        let name = |p: &Path| p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
        // Captions go next to the timeline as subtitles (dozens of offline clips are noise in an editor).
        let srt_path = target.with_extension("srt");
        let srt_written = match &srt {
            Some(s) => match std::fs::write(&srt_path, s) {
                Ok(()) => Some(true),
                Err(e) => {
                    self.toast(ToastKind::Error, format!("Could not write {}: {e}", srt_path.display()));
                    Some(false)
                }
            },
            None => None,
        };
        match std::fs::write(&target, text) {
            Ok(()) => {
                let extra = if rep.notes.is_empty() { String::new() } else { format!(" Not carried over: {}.", rep.notes.join("; ")) };
                let subs = if srt_written == Some(true) { format!(" {} caption(s) are in {} (File > Import > Subtitle).", rep.captions, name(&srt_path)) } else { String::new() };
                self.toast(ToastKind::Info, format!("Exported {} clip(s) to {}. In Resolve: File > Import > Timeline.{subs}{extra}", rep.clips, name(&target)));
            }
            Err(e) => self.toast(ToastKind::Error, format!("Could not write {}: {e}", target.display())),
        }
    }

    /// Saves; returns true on success.
    fn save(&mut self, id: u64, save_as: bool) -> bool {
        let Some((path, title)) = self.with_doc(id, |d| (d.doc.path.clone(), d.title())) else { return false };
        let target = match (&path, save_as) {
            (Some(p), false) => p.clone(),
            _ => {
                let stem = Path::new(&title).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Untitled".into());
                let mut dlg = rfd::FileDialog::new().set_title("Save project").set_file_name(format!("{stem}.aifr")).add_filter("AIFrames project", &["aifr"]);
                if let Some(d) = path.as_ref().and_then(|p| p.parent()).or_else(|| self.recent.first().and_then(|p| p.parent())) {
                    dlg = dlg.set_directory(d);
                }
                match dlg.save_file() {
                    Some(p) => crate::export::with_ext(&p, "aifr"),
                    None => return false,
                }
            }
        };
        // The core refuses to overwrite a file another program changed (DiskChanged) unless told to:
        // only "Keep mine" for exactly the version on disk forces it.
        let same = path.as_deref() == Some(target.as_path());
        let keep_mine = same && self.docs.get(&id).is_some_and(|d| d.watch.acknowledged.is_some() && d.watch.acknowledged == aiframes_core::FileStamp::of(&target));
        let write = |d: &mut ProjectDoc| match (same, keep_mine) {
            (true, true) => d.doc.save_force(),
            (true, false) => d.doc.save(),
            _ => d.doc.save_as(&target),
        };
        let mut res = self.with_doc(id, write);
        if matches!(&res, Some(Err(e)) if e.kind == aiframes_core::document::DocErrorKind::DiskChanged) {
            // Look at the newer file: a different project raises the banner (and the save is
            // refused); the same project (a touch) or a clean window's reload lets the save go on.
            if self.check_external(id, &target, aiframes_core::FileStamp::of(&target)) {
                self.toast(ToastKind::Error, format!("Not saved: {} was changed by another program. Choose Reload or Keep mine first.", file_name(&target)));
                return false;
            }
            res = self.with_doc(id, write);
        }
        match res {
            Some(Ok(())) => {
                if let Some(d) = self.docs.get_mut(&id) {
                    d.watch.acknowledged = None;
                    d.watch.conflict = None;
                    d.watch.comparing = false;
                }
                if let Some(old) = &path {
                    let _ = std::fs::remove_file(prefs::autosave_path(old));
                }
                let _ = std::fs::remove_file(prefs::autosave_path(&target));
                let _ = std::fs::remove_file(prefs::untitled_autosave_path(id));
                suite_ui::recent::push(APP_NAME, &target);
                self.recent = suite_ui::recent::load(APP_NAME);
                self.toast(ToastKind::Info, format!("Saved {}", file_name(&target)));
                true
            }
            Some(Err(e)) => {
                self.dialogs.alert("Couldn't save", &format!("{}\n\n{e}", target.display()));
                false
            }
            None => false,
        }
    }

    /// Unsaved changes (a reload from disk leaves the document saved).
    fn is_dirty(&self, id: u64) -> bool {
        self.with_doc(id, |d| d.doc.dirty()).unwrap_or(false)
    }

    fn close(&mut self, id: u64, then: AfterSave) {
        if self.is_dirty(id) {
            let title = self.with_doc(id, |d| d.title()).unwrap_or_default();
            self.services.hub.set_active(id);
            self.dialogs.modal = Some(Modal::SaveChanges { doc: id, title, then });
        } else {
            self.close_doc(id);
        }
    }

    fn close_doc(&mut self, id: u64) {
        if let Some(p) = self.with_doc(id, |d| d.doc.path.clone()).flatten() {
            let _ = std::fs::remove_file(prefs::autosave_path(&p));
        }
        let _ = std::fs::remove_file(prefs::untitled_autosave_path(id));
        if self.engine_doc == Some(id) {
            self.engine.pause();
            self.engine_doc = None;
        }
        self.services.hub.remove(id);
        self.docs.remove(&id);
    }

    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && !self.exit_confirmed {
            let dirty: Vec<u64> = self.services.hub.list().into_iter().map(|(id, _)| id).filter(|id| self.is_dirty(*id)).collect();
            if !dirty.is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.exit_queue = Some(dirty);
            } else {
                self.on_exit_save();
            }
        }
        if let Some(q) = &mut self.exit_queue {
            if self.dialogs.modal.is_none() {
                let hub = self.services.hub.clone();
                q.retain(|id| hub.get(*id).is_some());
                let next = q.first().copied();
                match next {
                    Some(id) if self.is_dirty(id) => {
                        let title = self.with_doc(id, |d| d.title()).unwrap_or_default();
                        self.services.hub.set_active(id);
                        self.dialogs.modal = Some(Modal::SaveChanges { doc: id, title, then: AfterSave::Exit });
                    }
                    Some(_) => {
                        if let Some(q) = &mut self.exit_queue {
                            q.remove(0);
                        }
                    }
                    None => {
                        self.exit_queue = None;
                        self.exit_confirmed = true;
                        self.on_exit_save();
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
        }
    }

    fn on_exit_save(&mut self) {
        self.prefs.layout.side_tab = self.side_tab.id().to_string();
        self.prefs.layout.safe_guides = self.viewer.safe;
        self.prefs.quality = self.viewer.quality.id().to_string();
        self.prefs.volume = self.engine.volume;
        self.prefs.save();
        let clean: Vec<u64> = self.docs.keys().copied().filter(|id| !self.is_dirty(*id)).collect();
        for id in clean {
            let _ = std::fs::remove_file(prefs::untitled_autosave_path(id));
        }
    }

    fn autosave(&mut self) {
        let secs = self.prefs.autosave_secs as f64;
        if secs <= 0.0 {
            return;
        }
        let ids: Vec<u64> = self.docs.keys().copied().collect();
        for id in ids {
            let due = self.docs.get(&id).is_some_and(|d| self.now - d.last_autosave >= secs);
            if !due {
                continue;
            }
            let epoch = self.docs.get(&id).map(|d| d.epoch).unwrap_or(0);
            let info = self.with_doc(id, |d| (d.doc.dirty(), d.doc.revision(), d.doc.path.clone(), aiframes_core::document::canonical_json(&d.doc.project)));
            let Some((dirty, rev, path, text)) = info else { continue };
            let Some(dui) = self.docs.get_mut(&id) else { continue };
            dui.last_autosave = self.now;
            if !dirty || dui.autosaved_key == (rev, epoch) {
                continue;
            }
            dui.autosaved_key = (rev, epoch);
            let target = path.as_deref().map(prefs::autosave_path).unwrap_or_else(|| prefs::untitled_autosave_path(id));
            std::thread::spawn(move || {
                if let Err(e) = prefs::write_atomic(&target, &text) {
                    eprintln!("aiframes: autosave {} failed: {e}", target.display());
                }
            });
            self.status = "Autosaved".into();
        }
    }

    fn recover(&mut self, id: u64, autosave: &Path, yes: bool) {
        if !yes {
            let _ = std::fs::remove_file(autosave);
            return;
        }
        let parsed = std::fs::read_to_string(autosave).map_err(|e| e.to_string()).and_then(|t| aiframes_core::document::parse_project(&t).map_err(|e| e.to_string()));
        // One undoable revision: Ctrl+Z goes back to the saved file.
        let res = parsed.and_then(|p| self.with_doc(id, |d| d.doc.replace_project("user", "Recover autosave", p).map_err(|e| e.to_string())).unwrap_or_else(|| Err("the project is no longer open".into())));
        match res {
            Ok(_) => self.toast(ToastKind::Info, "Recovered the autosaved version (save to keep it; Ctrl+Z returns to the saved file)."),
            Err(e) => self.dialogs.alert("Couldn't recover", &e),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Import / export / jobs
    // -----------------------------------------------------------------------------------------

    fn import(&mut self, id: u64, paths: Vec<PathBuf>, place_at: Option<(f64, Option<Id>)>) {
        let paths = if paths.is_empty() {
            let mut dlg = rfd::FileDialog::new().set_title("Import media").add_filter("Media", MEDIA_EXTS).add_filter("All files", &["*"]);
            let dir = if self.prefs.import_dir.is_empty() { None } else { Some(PathBuf::from(&self.prefs.import_dir)) };
            if let Some(d) = dir.filter(|d| d.is_dir()) {
                dlg = dlg.set_directory(d);
            }
            match dlg.pick_files() {
                Some(f) if !f.is_empty() => f,
                _ => return,
            }
        } else {
            paths
        };
        if let Some(d) = paths.first().and_then(|p| p.parent()) {
            self.prefs.import_dir = d.to_string_lossy().to_string();
        }
        let Some((project, path)) = self.with_doc(id, |d| (d.doc.project.clone(), d.doc.path.clone())) else { return };
        let pool = self.services.pool.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let n = paths.len();
        std::thread::spawn(move || {
            let r = actions::import_plan_for(&project, path.as_deref(), &pool, &paths).map(|plan| {
                let skipped = plan.skipped.iter().map(|(p, why)| format!("{}: {why}", file_name(p))).collect();
                (plan.ops, skipped)
            });
            let _ = tx.send(r);
        });
        self.status = format!("Importing {n} item(s)…");
        self.imports.push(ImportJob { doc: id, rx, place_at, label: String::new() });
    }

    /// Asks for a new file for a media asset and relinks it (probing in the background).
    fn relink(&mut self, id: u64, asset: &str) {
        let Some((name, old)) = self.with_doc(id, |d| d.doc.project.media.iter().find(|m| m.id == asset).map(|m| (m.name.clone(), d.doc.media_path(m)))).flatten() else { return };
        let mut dlg = rfd::FileDialog::new().set_title(format!("Relink {asset} {name}")).add_filter("Media", MEDIA_EXTS).add_filter("All files", &["*"]);
        if let Some(d) = old.parent().filter(|d| d.is_dir()) {
            dlg = dlg.set_directory(d);
        }
        if let Some(n) = old.file_name() {
            dlg = dlg.set_file_name(n.to_string_lossy());
        }
        let Some(path) = dlg.pick_file() else { return };
        let pool = self.services.pool.clone();
        let asset = asset.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = pool.probe(&path).map_err(|e| e.to_string()).and_then(|(_, info)| {
                let v = serde_json::json!({"op": "relink_media", "asset": asset, "path": path.to_string_lossy().replace('\\', "/"), "info": info});
                Op::from_json(&v).map(|o| (vec![o], Vec::new())).map_err(|e| e.to_string())
            });
            let _ = tx.send(r);
        });
        self.imports.push(ImportJob { doc: id, rx, place_at: None, label: "Relink media".into() });
    }

    fn poll_imports(&mut self) {
        let mut done = Vec::new();
        for (i, j) in self.imports.iter().enumerate() {
            match j.rx.try_recv() {
                Ok(r) => done.push((i, Some(r))),
                Err(TryRecvError::Disconnected) => done.push((i, None)),
                Err(TryRecvError::Empty) => {}
            }
        }
        for (i, r) in done.into_iter().rev() {
            let j = self.imports.remove(i);
            let r = r.map(|r| {
                r.map(|(ops, skipped)| {
                    if !skipped.is_empty() {
                        let more = if skipped.len() > 3 { format!(" (+{} more)", skipped.len() - 3) } else { String::new() };
                        self.toast(ToastKind::Info, format!("Skipped {}{more}", skipped.iter().take(3).cloned().collect::<Vec<_>>().join("; ")));
                    }
                    ops
                })
            });
            match r {
                Some(Ok(ops)) if ops.is_empty() => self.status = "Nothing new to import (already in the project)".into(),
                Some(Ok(ops)) if !j.label.is_empty() => {
                    self.apply(j.doc, &j.label, ops);
                }
                Some(Ok(ops)) => {
                    let n = ops.len();
                    let res = self.with_doc(j.doc, |d| d.doc.apply("user", &format!("Import {n} file(s)"), ops));
                    match res {
                        Some(Ok(receipt)) => {
                            let created: Vec<Id> = receipt.outcomes.iter().flat_map(|o| o.created.iter().cloned()).collect();
                            self.toast(ToastKind::Info, format!("Imported {} file(s)", created.len().max(n)));
                            if let Some((at, track)) = j.place_at.clone() {
                                let ops = self
                                    .with_doc(j.doc, |d| {
                                        let assets: Vec<(Id, MediaKind, f64)> = d.doc.project.media.iter().filter(|m| created.contains(&m.id)).map(|m| (m.id.clone(), m.kind, m.info.duration)).collect();
                                        edits::place_ops(&d.doc.project, d.doc.resolved(), &assets, at, track.as_deref())
                                    })
                                    .unwrap_or_default();
                                self.apply(j.doc, "Place on timeline", ops);
                            }
                        }
                        Some(Err(e)) => self.toast(ToastKind::Error, format!("Import: {e}")),
                        None => {}
                    }
                }
                Some(Err(e)) => self.toast(ToastKind::Error, format!("Import: {e}")),
                None => self.toast(ToastKind::Error, "Import failed"),
            }
        }
    }

    fn poll_jobs(&mut self) -> Vec<aiframes_mcp::JobInfo> {
        let jobs = self.services.jobs.list();
        for j in &jobs {
            if j.status == JobStatus::Running || self.jobs_seen.contains(&j.id) {
                continue;
            }
            self.jobs_seen.insert(j.id);
            match j.status {
                JobStatus::Done => {
                    if j.kind == "export" {
                        let p = self.export_paths.get(&j.id).cloned().or_else(|| j.result.as_ref().and_then(|r| r.get("path")).and_then(|p| p.as_str()).map(PathBuf::from));
                        if let Some(p) = p {
                            self.last_export = Some(p.clone());
                            self.toast(ToastKind::Info, format!("Exported {} — click \"Show export\" in the status bar", file_name(&p)));
                        } else {
                            self.toast(ToastKind::Info, format!("{} finished", j.label));
                        }
                    } else {
                        self.toast(ToastKind::Info, format!("{} finished", j.label));
                    }
                }
                JobStatus::Failed => self.toast(ToastKind::Error, format!("{} failed: {}", j.label, j.message)),
                JobStatus::Cancelled => self.status = format!("{} cancelled", j.label),
                JobStatus::Running => {}
            }
        }
        jobs
    }

    fn start_export(&mut self, id: u64, settings: aiframes_core::ExportSettings, out: PathBuf) {
        if let Some(dir) = out.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match actions::start_export(&self.services, id, settings, out.clone()) {
            Ok(job) => {
                self.export_paths.insert(job, out.clone());
                self.status = format!("Exporting {}…", file_name(&out));
            }
            Err(e) => self.dialogs.alert("Export failed to start", &e),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------------------------

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let (dropped, pos) = ctx.input(|i| (i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect::<Vec<_>>(), i.pointer.latest_pos()));
        let pos = if self.script.is_some() { pos } else { os_cursor_pos(ctx).or(pos) };
        if dropped.is_empty() {
            return;
        }
        let (projects, media): (Vec<PathBuf>, Vec<PathBuf>) = dropped.into_iter().partition(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("aifr")));
        for p in projects {
            self.open_path(p);
        }
        if !media.is_empty() {
            let id = match self.active_id() {
                Some(id) => id,
                None => {
                    self.new_doc(0);
                    match self.active_id() {
                        Some(id) => id,
                        None => return,
                    }
                }
            };
            let over_timeline = pos.filter(|p| self.timeline_rect.contains(*p));
            // Over the lanes: the time and track under the pointer; elsewhere on the timeline: the playhead.
            let place = over_timeline.map(|p| {
                let tl = self.docs.get(&id).map(|d| (d.w.timeline.time_at(p.x), d.w.timeline.track_at(p.y)));
                match tl {
                    Some((Some(t), track)) => (t, track),
                    _ => (self.playhead(), None),
                }
            });
            self.import(id, media, place);
        }
    }

    fn global_keys(&mut self, ctx: &egui::Context) -> Vec<Command> {
        let mut out = Vec::new();
        if ctx.egui_wants_keyboard_input() || self.dialogs.modal.is_some() {
            return out;
        }
        let viewer_focus = matches!(self.focus, Focus::Viewer | Focus::Timeline);
        ctx.input_mut(|i| {
            let mut consumed = Vec::new();
            for (n, e) in i.events.iter().enumerate() {
                if let Event::Key { key, pressed: true, modifiers, .. } = e {
                    if let Some(c) = shortcuts::lookup(*key, *modifiers, viewer_focus) {
                        out.push(c);
                        consumed.push(n);
                    }
                }
            }
            for n in consumed.into_iter().rev() {
                i.events.remove(n);
            }
        });
        out
    }

    /// Ctrl+C / Ctrl+X / Ctrl+V on clips (egui turns them into Copy/Cut/Paste events).
    fn clipboard_events(&mut self, ctx: &egui::Context) -> Vec<Cmd> {
        if ctx.egui_wants_keyboard_input() || self.dialogs.modal.is_some() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut paste = None;
        ctx.input(|i| {
            for e in &i.events {
                match e {
                    Event::Copy => out.push(Cmd::Copy),
                    Event::Cut => out.push(Cmd::Cut),
                    Event::Paste(s) => paste = Some(s.clone()),
                    _ => {}
                }
            }
        });
        if let Some(s) = paste {
            if s.contains(edits::CLIP_MAGIC) {
                // Clips copied here or in another AIFrames window.
                self.clipboard = Some(s);
                out.push(Cmd::Paste);
            } else if s.trim().is_empty() {
                out.push(Cmd::Paste);
            } else {
                self.status = "The clipboard holds text, not clips (copy clips in the timeline first)".into();
            }
        }
        out
    }

    fn update_focus(&mut self, ctx: &egui::Context) {
        let (pressed, pos) = ctx.input(|i| (i.pointer.any_pressed(), i.pointer.interact_pos()));
        if pressed {
            if let Some(p) = pos {
                self.focus = if self.viewer_rect.contains(p) {
                    Focus::Viewer
                } else if self.timeline_rect.contains(p) {
                    Focus::Timeline
                } else {
                    Focus::Other
                };
            }
        }
        self.viewer.focused = self.focus == Focus::Viewer;
    }

    /// Window position/size for the next start.
    fn remember_window(&mut self, ctx: &egui::Context) {
        let (outer, inner, max, min) = ctx.input(|i| {
            let v = i.viewport();
            (v.outer_rect, v.inner_rect, v.maximized, v.minimized)
        });
        if min == Some(true) {
            return;
        }
        if let Some(m) = max {
            self.prefs.layout.maximized = m;
        }
        if max != Some(true) {
            if let (Some(o), Some(i)) = (outer, inner) {
                self.prefs.layout.window = Some([o.min.x, o.min.y, i.width(), i.height()]);
            }
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match self.active() {
            Some((_, d)) => {
                let (t, dirty) = {
                    let g = lock(&d);
                    (g.title(), g.doc.dirty())
                };
                format!("{t}{} — AIFrames", if dirty { " •" } else { "" })
            }
            None => "AIFrames".into(),
        };
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }
    }

    fn perf_log(&mut self) {
        let Some(path) = self.knobs.perf.clone() else { return };
        if self.now < self.perf_next {
            return;
        }
        self.perf_next = self.now + 1.0;
        let mem = crate::sysmem::process_memory().unwrap_or((0, 0));
        let pool = self.services.pool.stats();
        let engine_mb = self.engine.buffered_bytes() as f64 / 1048576.0;
        let history: usize = self.services.hub.list().iter().map(|(_, d)| lock(d).doc.history().len()).sum();
        let s = &self.engine.stats;
        let line = format!(
            "{{\"t\":{:.2},\"playing\":{},\"fps\":{:.1},\"dropped\":{},\"render_ms\":{:.2},\"scale\":{:.3},\"audio\":{},\"master\":{},\"wait_ms\":{:.0},\"buffering\":{},\"grains\":{},\"underruns\":{},\"playhead\":{:.3},\"ui_frames\":{},\"thumbs\":{},\"thumb_queue\":{},\"private_mb\":{},\"ws_mb\":{},\"cache_mb\":{:.0},\"cache_frames\":{},\"sessions\":{},\"session_mb\":{:.0},\"engine_mb\":{:.0},\"history\":{}}}\n",
            self.now,
            self.engine.playing(),
            s.fps,
            s.dropped,
            s.render_ms,
            s.scale,
            s.audio,
            s.audio_master,
            s.audio_wait_ms,
            s.buffering,
            s.grains,
            s.underruns,
            self.playhead(),
            self.frames,
            self.previews.stats().0,
            self.previews.stats().1,
            mem.0 >> 20,
            mem.1 >> 20,
            pool.cache_bytes as f64 / 1048576.0,
            pool.cached_frames,
            pool.sessions,
            pool.session_bytes as f64 / 1048576.0,
            engine_mb,
            history
        );
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// Frame-cache budget from the preferences (automatic = scaled to the installed RAM).
fn cache_bytes(p: &Prefs) -> usize {
    let mb = if p.cache_auto { auto_cache_mb() } else { p.cache_mb };
    mb as usize * (1 << 20)
}

fn auto_cache_mb() -> u32 {
    crate::sysmem::default_cache_mb(crate::sysmem::total_ram())
}

/// "Memory now: 812 MB · frame cache 210 of 512 MB · 2 decoders, 40 of 128 MB".
fn memory_line(pool: &aiframes_media::MediaPool) -> String {
    let s = pool.stats();
    let proc = crate::sysmem::process_memory().map(|(private, _)| format!("{} MB", private >> 20)).unwrap_or_else(|| "?".into());
    format!(
        "Memory now: {proc} · frame cache {} of {} MB · {} decoder(s), {} of {} MB",
        s.cache_bytes >> 20,
        s.cache_budget >> 20,
        s.sessions,
        s.session_bytes >> 20,
        s.readahead_budget >> 20
    )
}

fn snapshot_of(id: u64, d: &ProjectDoc, epoch: u64) -> Arc<Snapshot> {
    Arc::new(Snapshot { doc: id, revision: d.doc.revision(), epoch, project: d.doc.project.clone(), resolved: d.doc.resolved().clone(), dir: d.doc.project_dir() })
}

fn encode_png(img: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let [w, h] = img.size;
    let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
    let out = aiframes_render::encode_png(&aiframes_render::Image { width: w as u32, height: h as u32, rgba });
    if out.is_empty() {
        Err("PNG encoding failed".into())
    } else {
        Ok(out)
    }
}

/// The mouse position in window points from the OS. Needed while files are dragged in from
/// Explorer: Windows sends no mouse moves during OLE drag-and-drop and winit 0.30's
/// `HoveredFile`/`DroppedFile` carry no position, so egui's pointer is stale then.
#[cfg(windows)]
fn os_cursor_pos(ctx: &egui::Context) -> Option<egui::Pos2> {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetCursorPos(p: *mut Point) -> i32;
    }
    let mut p = Point { x: 0, y: 0 };
    // SAFETY: documented Win32 call writing one POINT we own.
    if unsafe { GetCursorPos(&mut p) } == 0 {
        return None;
    }
    let (inner, native) = ctx.input(|i| (i.viewport().inner_rect, i.viewport().native_pixels_per_point));
    let inner = inner?;
    let native = native.unwrap_or(1.0).max(0.1);
    let zoom = ctx.pixels_per_point() / native;
    // Screen pixels -> screen points -> window points.
    let screen = egui::pos2(p.x as f32 / native, p.y as f32 / native);
    Some(egui::pos2((screen.x - inner.min.x) / zoom, (screen.y - inner.min.y) / zoom))
}

#[cfg(not(windows))]
fn os_cursor_pos(_ctx: &egui::Context) -> Option<egui::Pos2> {
    None
}

/// Opens Explorer with the file selected.
pub fn reveal(p: &Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer.exe").arg(format!("/select,{}", p.display())).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = p;
    }
}

impl eframe::App for AiFramesApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.now = ctx.input(|i| i.time);
        self.frames += 1;
        if !self.waker_set {
            let c = ctx.clone();
            self.services.hub.set_waker(move || c.request_repaint());
            let c = ctx.clone();
            self.services.jobs.set_waker(move || c.request_repaint());
            self.waker_set = true;
        }
        self.collect_notices();
        if self.services.hub.take_focus_request() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        self.handle_screenshots(&ctx);
        self.run_script(&ctx);
        self.poll_imports();
        if self.ai.poll() {
            ctx.request_repaint();
        }
        let jobs = self.poll_jobs();
        self.handle_dropped_files(&ctx);
        self.handle_close_request(&ctx);
        self.update_focus(&ctx);
        self.sync_docs();
        self.watch_files();
        self.sweep_media();
        self.note_interaction(&ctx);
        if !self.startup_imports.is_empty() {
            if let Some(id) = self.active_id() {
                let files = std::mem::take(&mut self.startup_imports);
                self.import(id, files, None);
            }
        }
        if !ctx.input(|i| i.pointer.primary_down()) && !self.viewer.dragging() {
            self.engine.set_scrubbing(false);
        }

        // Engine first: the panels draw the playhead of this frame.
        let active = self.active();
        let mut playhead = 0.0;
        if let Some((id, _)) = &active {
            self.pre_frame(*id);
            if self.viewer.display_px > 0.0 {
                let scale = self.viewer.quality.scale(self.viewer.display_px, self.engine.snapshot().project.settings.width);
                self.engine.set_scale(scale);
            }
            playhead = self.engine.update();
            self.previews.set_playing(self.engine.playing());
            if let Some(d) = self.docs.get_mut(id) {
                d.playhead = playhead;
            }
        }

        let mut actions: Vec<UiAction> = Vec::new();
        let mut cmds: Vec<Cmd> = Vec::new();
        let mut commands: Vec<Command> = Vec::new();
        let mut ai_run = false;
        let mut ai_reveal: Option<Id> = None;
        if active.is_some() && self.frames > 10 {
            if let Some(p) = self.knobs.ai_prompt.take() {
                self.ai.prompt = p;
                if let Some(m) = self.knobs.ai_model.take() {
                    self.ai.model = m;
                }
                self.side_tab = SideTab::Ai;
                ai_run = true;
            }
        }
        let mut dialog_out = Vec::new();
        let mut export_req = None;

        // A menu or popup open at the start of the frame owns Escape and the arrows.
        let popup_before = egui::Popup::is_any_open(&ctx);
        // Menu bar and tabs.
        let doc_list: Vec<(u64, String, bool)> = self
            .services
            .hub
            .list()
            .into_iter()
            .map(|(id, d)| {
                let g = lock(&d);
                (id, g.title(), g.doc.dirty())
            })
            .collect();
        let active_id = active.as_ref().map(|(id, _)| *id);
        let (undo, redo) = active.as_ref().map(|(_, d)| {
            let g = lock(d);
            (g.doc.undo_label().map(str::to_string), g.doc.redo_label().map(str::to_string))
        }).unwrap_or((None, None));
        let (snapping, tool, has_sel) = active_id.and_then(|id| self.docs.get(&id)).map(|d| (d.w.timeline.snapping, d.w.timeline.tool, !d.selection.is_empty())).unwrap_or((true, Default::default(), false));
        // Clip/Timeline menu items that depend on the selection.
        let sel_now: Vec<Id> = active_id.and_then(|id| self.docs.get(&id)).map(|d| d.selection.clone()).unwrap_or_default();
        let (match_length, fixed_length, gap_track, captions) = active
            .as_ref()
            .map(|(_, d)| {
                let g = lock(d);
                let p = &g.doc.project;
                let m = edits::match_length_ops(p, &sel_now).map(|(l, _)| l);
                let f = !edits::fixed_length_ops(p, &sel_now).is_empty();
                let t = edits::gap_track(p, &sel_now).map(|(t, _, n)| (track_label(p, &t), n >= 2));
                let c = (!p.transcripts.is_empty()).then(|| aiframes_ui::edits::caption_counts(p));
                (m, f, t, c)
            })
            .unwrap_or((None, false, None, None));
        egui::Panel::top("aif-menu").frame(theme::bar_frame()).show(ui, |ui| {
            let info = MenuInfo {
                has_doc: active.is_some(),
                undo,
                redo,
                recent: &self.recent,
                has_selection: has_sel,
                has_clipboard: self.clipboard.is_some(),
                snapping,
                tool,
                show_bin: self.prefs.layout.show_bin,
                show_side: self.prefs.layout.show_side,
                show_timeline: self.prefs.layout.show_timeline,
                safe: self.viewer.safe,
                zoom: self.viewer.zoom,
                quality: self.viewer.quality,
                looping: self.engine.looping,
                scrub_audio: self.engine.scrub_audio,
                follow_ai: self.prefs.follow_ai,
                follow_ai_playhead: self.prefs.follow_ai_playhead,
                port: self.port,
                match_length,
                fixed_length,
                gap_track,
                captions,
            };
            cmds.extend(panels::menu_bar(ui, &info));
        });
        if !doc_list.is_empty() {
            egui::Panel::top("aif-tabs").frame(egui::Frame::NONE.fill(theme::palette().bg_app).inner_margin(egui::Margin { left: 6, right: 6, top: 3, bottom: 0 })).show(ui, |ui| {
                cmds.extend(panels::tab_strip(ui, &doc_list, active_id));
            });
        }
        // Another program changed the open file while this window has unsaved changes.
        let mut conflict_answer: Option<(u64, Option<bool>)> = None;
        if let Some(id) = active_id {
            let has = self.docs.get(&id).is_some_and(|d| d.watch.conflict.is_some());
            if has {
                let title = doc_list.iter().find(|(d, _, _)| *d == id).map(|(_, t, _)| t.clone()).unwrap_or_default();
                let pal = theme::palette();
                egui::Panel::top("aif-ext-banner").frame(egui::Frame::NONE.fill(egui::Color32::from_rgb(0x4a, 0x3a, 0x12)).inner_margin(egui::Margin::symmetric(10, 5))).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("⚠").color(pal.warn));
                        ui.label(egui::RichText::new(format!("{title} was changed by another program, and this window has unsaved changes.")).color(pal.text));
                        if ui.button("Reload").on_hover_text("Use the version on disk (your version stays one Ctrl+Z away)").clicked() {
                            conflict_answer = Some((id, Some(true)));
                        }
                        if ui.button("Keep mine").on_hover_text("Keep editing your version; saving replaces the file on disk").clicked() {
                            conflict_answer = Some((id, Some(false)));
                        }
                        if ui.button("Compare").clicked() {
                            conflict_answer = Some((id, None));
                        }
                    });
                });
            }
        }

        // Status bar.
        let snap = self.engine.snapshot();
        let rev = active.as_ref().map(|(_, d)| lock(d).doc.revision()).unwrap_or(0);
        egui::Panel::bottom("aif-status").exact_size(22.0).frame(theme::bar_frame()).show(ui, |ui| {
            let info = StatusInfo {
                revision: rev,
                width: snap.project.settings.width,
                height: snap.project.settings.height,
                fps: snap.project.settings.fps,
                duration: snap.duration(),
                playhead,
                stats: &self.engine.stats,
                playing: self.engine.playing(),
                jobs: &jobs,
                port: self.port,
                notice: self.notice.as_ref().map(|(n, _)| n.as_str()),
                status: &self.status,
                last_export: self.last_export.as_ref(),
                ai_running: self.ai.running(),
            };
            cmds.extend(panels::status_bar(ui, &info));
        });

        match &active {
            None => {
                egui::CentralPanel::default().frame(egui::Frame::NONE.fill(theme::palette().bg_app)).show(ui, |ui| {
                    cmds.extend(panels::empty_state(ui, &self.recent));
                });
            }
            Some((id, doc_ref)) => {
                let id = *id;
                let now = self.now;
                // Layers under the playhead (for click-to-select and handles).
                let hover_viewer = ctx.input(|i| i.pointer.latest_pos()).is_some_and(|p| self.viewer_rect.contains(p));
                let need_layers = hover_viewer || self.docs.get(&id).is_some_and(|d| !d.selection.is_empty());
                let layers = if need_layers { self.layers_for(&snap, playhead) } else { Vec::new() };
                let shown = self.engine.shown();
                let warnings = self.engine.stats.warnings.clone();
                let rate = self.engine.rate();
                let playing = self.engine.playing();
                let render_scale = self.engine.stats.scale;
                let looping = self.engine.looping;
                let (volume, muted) = (self.engine.volume, self.engine.muted);
                let mut g = lock(doc_ref);
                let pd: &mut ProjectDoc = &mut g;
                let Some(dui) = self.docs.get_mut(&id) else { return };
                self.previews.set_dir(pd.doc.project_dir());
                let flash = dui.flash(now);
                let DocUi { w, selection, in_point, out_point, flash_ids, .. } = dui;
                let history = pd.doc.history();
                let cx = EditCtx {
                    project: &pd.doc.project,
                    resolved: pd.doc.resolved(),
                    playhead,
                    playing,
                    selection,
                    preview: &self.previews,
                    ai_highlight: flash_ids,
                    ai_flash: flash,
                    history,
                    revision: pd.doc.revision(),
                    can_undo: pd.doc.can_undo(),
                    can_redo: pd.doc.can_redo(),
                    in_point: *in_point,
                    out_point: *out_point,
                };
                let pal = theme::palette();
                let layout = &mut self.prefs.layout;
                if layout.show_timeline {
                    let r = egui::Panel::bottom("aif-timeline").resizable(true).default_size(layout.timeline_height).size_range(140.0..=1200.0).frame(theme::panel_frame().fill(pal.panel_dark)).show(ui, |ui| {
                        ui.set_min_size(ui.available_size());
                        actions.extend(w.timeline.ui(ui, &cx));
                    });
                    layout.timeline_height = r.response.rect.height();
                    self.timeline_rect = r.response.rect;
                } else {
                    self.timeline_rect = egui::Rect::NOTHING;
                }
                if layout.show_bin {
                    let r = egui::Panel::left("aif-bin").resizable(true).default_size(layout.bin_width).size_range(160.0..=900.0).frame(theme::panel_frame()).show(ui, |ui| {
                        theme::panel_header(ui, "MEDIA");
                        ui.set_min_size(ui.available_size());
                        actions.extend(w.bin.ui(ui, &cx));
                    });
                    layout.bin_width = r.response.rect.width();
                }
                if layout.show_side {
                    let r = egui::Panel::right("aif-side").resizable(true).default_size(layout.side_width).size_range(220.0..=900.0).frame(theme::panel_frame()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for t in SideTab::ALL {
                                let pending = pd.doc.project.suggestions.len();
                                let label = if t == SideTab::Ai && self.ai.running() {
                                    "AI ●".to_string()
                                } else if t == SideTab::Review && pending > 0 {
                                    format!("Review {pending}")
                                } else {
                                    t.label().to_string()
                                };
                                let text = if self.side_tab == t { egui::RichText::new(label).strong().color(pal.text) } else { egui::RichText::new(label).color(pal.text_dim) };
                                if ui.selectable_label(self.side_tab == t, text).clicked() {
                                    self.side_tab = t;
                                }
                            }
                        });
                        ui.separator();
                        ui.set_min_size(ui.available_size());
                        match self.side_tab {
                            SideTab::Inspector => actions.extend(w.inspector.ui(ui, &cx)),
                            SideTab::Effects => actions.extend(w.effects.ui(ui, &cx)),
                            SideTab::Transcript => actions.extend(w.transcript.ui(ui, &cx)),
                            SideTab::History => actions.extend(w.history.ui(ui, &cx)),
                            SideTab::Review => actions.extend(w.review.ui(ui, &cx)),
                            SideTab::Ai => {
                                let rcx = RunContext {
                                    port: self.port,
                                    project_id: id,
                                    project_path: pd.doc.path.clone(),
                                    title: pd.title(),
                                    playhead,
                                    timecode: timecode::timecode(playhead, pd.doc.project.settings.fps),
                                    selection: selection.clone(),
                                    in_point: *in_point,
                                    out_point: *out_point,
                                    duration: pd.doc.resolved().duration,
                                };
                                let r = self.ai.ui(ui, &rcx, &self.exe, &mut self.prefs.follow_ai, &mut self.prefs.follow_ai_playhead);
                                if r.run {
                                    ai_run = true;
                                }
                                if let Some(x) = r.reveal {
                                    ai_reveal = Some(x);
                                }
                            }
                        }
                    });
                    layout.side_width = r.response.rect.width();
                }
                let vcx = ViewerCx {
                    project: &pd.doc.project,
                    resolved: pd.doc.resolved(),
                    playhead,
                    playing,
                    rate,
                    selection,
                    shown,
                    layers: &layers,
                    in_point: *in_point,
                    out_point: *out_point,
                    looping,
                    ai_flash: flash,
                    render_scale,
                    buffering: self.engine.stats.buffering,
                    warnings: &warnings,
                    volume,
                    muted,
                };
                let mut vout = Default::default();
                let r = egui::CentralPanel::default().frame(egui::Frame::NONE.fill(pal.bg_app).inner_margin(egui::Margin::same(4))).show(ui, |ui| {
                    vout = self.viewer.ui(ui, &vcx);
                });
                self.viewer_rect = r.response.rect;
                let vout: crate::viewer::ViewerOut = vout;
                actions.extend(vout.actions);
                commands.extend(vout.commands);
                if let Some((v, m)) = vout.volume {
                    self.engine.set_volume(v, m);
                }
                // Live state for the AI.
                pd.live.playhead = playhead;
                pd.live.playing = playing;
                pd.live.selection = selection.clone();
                pd.live.in_point = *in_point;
                pd.live.out_point = *out_point;
                // Dialogs that need the project.
                let title = pd.title();
                let videos = dirs_videos();
                let ecx = ExportCx {
                    settings: &pd.doc.project.settings,
                    project_path: pd.doc.path.as_deref(),
                    name: &title,
                    in_out: match (*in_point, *out_point) {
                        (Some(a), Some(b)) if b > a => Some([a, b]),
                        _ => None,
                    },
                    duration: pd.doc.resolved().duration,
                    fallback_dir: &videos,
                };
                export_req = self.export.show(&ctx, &ecx);
                let memory = memory_line(&self.services.pool);
                let dcx = DialogCx { project: Some(&pd.doc.project), port: self.port, exe: &self.exe, memory, auto_cache_mb: auto_cache_mb() };
                dialog_out = self.dialogs.show(&ctx, &dcx);
                drop(g);
                // Live drag preview in the viewer.
                match (vout.preview, vout.preview_done) {
                    (Some(lp), false) => {
                        if self.live_preview.as_ref() != Some(&lp) {
                            self.apply_live_preview(id, &lp);
                            self.live_preview = Some(lp);
                        }
                    }
                    (_, true) => {
                        self.live_preview = None;
                        if let Some(s) = self.docs.get(&id).and_then(|d| d.snap.clone()) {
                            self.engine.set_snapshot(s);
                        }
                    }
                    _ => {}
                }
            }
        }
        if active.is_none() {
            let dcx = DialogCx { project: None, port: self.port, exe: &self.exe, memory: memory_line(&self.services.pool), auto_cache_mb: auto_cache_mb() };
            dialog_out = self.dialogs.show(&ctx, &dcx);
        }

        // Toasts and the drop overlay.
        panels::toasts(&ctx, &mut self.toasts, self.now);
        let hovering = ctx.input(|i| i.raw.hovered_files.len());
        if hovering > 0 {
            let pos = ctx.input(|i| i.pointer.latest_pos());
            let pos = if self.script.is_some() { pos } else { os_cursor_pos(&ctx).or(pos) };
            let over = pos.filter(|p| self.timeline_rect.contains(*p)).map(|_| self.timeline_rect);
            // No pointer events arrive while the OS drags files: keep repainting to follow it.
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
            panels::drop_overlay(&ctx, hovering, over);
        }

        // Keyboard (after the widgets, which consume the keys they handle).
        if !popup_before {
            commands.extend(self.global_keys(&ctx));
        }
        cmds.extend(self.clipboard_events(&ctx));

        // Process.
        if let Some(id) = active_id {
            for a in actions {
                self.handle_action(id, a, &ctx);
            }
        }
        // AI edits to bring into view (after the timeline laid itself out this frame).
        if let Some((id, ids, at, latest)) = self.pending_follow.clone() {
            if self.now - at >= 1.2 {
                self.pending_follow = None;
                if Some(id) == active_id {
                    self.follow_ai(id, ids, latest);
                }
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
            }
        }
        if let (Some(x), Some(id)) = (ai_reveal, active_id) {
            self.show_item(id, &x);
        }
        if let Some((id, answer)) = conflict_answer {
            match answer {
                Some(reload) => self.resolve_conflict(id, reload),
                None => {
                    if let Some(d) = self.docs.get_mut(&id) {
                        d.watch.comparing = true;
                    }
                }
            }
        }
        if let Some(id) = active_id {
            self.track_view(id);
            self.draw_ai_chip(&ctx, id);
            self.draw_compare(&ctx, id);
        }
        self.summarize_external(&ctx);
        for c in commands {
            self.run(c, &ctx);
        }
        for c in cmds {
            self.cmd(c, &ctx);
        }
        if ai_run {
            if let Some((id, d)) = self.active() {
                let rcx = {
                    let g = lock(&d);
                    let dui = self.docs.get(&id);
                    RunContext {
                        port: self.port,
                        project_id: id,
                        project_path: g.doc.path.clone(),
                        title: g.title(),
                        playhead,
                        timecode: timecode::timecode(playhead, g.doc.project.settings.fps),
                        selection: dui.map(|d| d.selection.clone()).unwrap_or_default(),
                        in_point: dui.and_then(|d| d.in_point),
                        out_point: dui.and_then(|d| d.out_point),
                        duration: g.doc.resolved().duration,
                    }
                };
                self.ai.start(&rcx);
            }
        }
        if let (Some(req), Some(id)) = (export_req, active_id) {
            match req {
                ExportRequest::Start { settings, out } => self.start_export(id, settings, out),
            }
        }
        for o in dialog_out {
            match o {
                DialogOut::Apply { label, ops } => {
                    if let Some(id) = active_id {
                        self.apply(id, &label, ops);
                    }
                }
                DialogOut::Prefs(p) => {
                    self.services.pool.set_cache_bytes(cache_bytes(&p));
                    self.services.pool.set_readahead_bytes(p.readahead_mb as usize * (1 << 20));
                    if let Some(h) = aiframes_media::HwAccel::parse(&p.hw_decode) {
                        self.services.pool.set_hwaccel(h);
                    }
                    self.viewer.quality = Quality::from_id(&p.quality);
                    let layout = self.prefs.layout.clone();
                    self.prefs = Prefs { layout, ..p };
                    self.prefs.save();
                }
                DialogOut::SaveChanges { doc, then, save } => match save {
                    None => self.exit_queue = None,
                    Some(save) => {
                        let ok = !save || self.save(doc, false);
                        if ok {
                            self.close_doc(doc);
                            if let AfterSave::Exit = then {
                                if let Some(q) = &mut self.exit_queue {
                                    q.retain(|d| *d != doc);
                                }
                            }
                        } else {
                            self.exit_queue = None;
                        }
                    }
                },
                DialogOut::Recover { doc, autosave, yes } => self.recover(doc, &autosave, yes),
                DialogOut::Speed { speed, keep_duration } => {
                    if let Some(id) = active_id {
                        let sel = self.docs.get(&id).map(|d| d.selection.clone()).unwrap_or_default();
                        let ops = edits::speed_ops(&sel, speed, keep_duration);
                        self.apply(id, &format!("Speed {speed}x"), ops);
                    }
                }
            }
        }

        self.autosave();
        self.remember_window(&ctx);
        self.update_title(&ctx);
        self.perf_log();
        self.fonts.end_frame(&ctx);
        // Keep animating what needs it.
        let flashing = active_id.and_then(|id| self.docs.get(&id)).is_some_and(|d| d.flash(self.now) > 0.0);
        if flashing || !self.toasts.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }
        if self.ai.running() || jobs.iter().any(|j| j.status == JobStatus::Running) || !self.imports.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        if let Some(s) = &mut self.script {
            if let Some(f) = s.inject(ctx, raw) {
                self.script_action = Some(f);
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if !self.exit_confirmed {
            self.on_exit_save();
        }
        self.ai.stop();
    }
}

/// "V1 Main": a track's id (upper case) and name.
fn track_label(p: &aiframes_core::Project, id: &str) -> String {
    match p.tracks.iter().find(|t| t.id == id) {
        Some(t) if !t.name.is_empty() => format!("{} {}", t.id.to_uppercase(), t.name),
        _ => id.to_uppercase(),
    }
}

/// Follow AI keeps a timeline view the person set this recently (seconds).
const VIEW_KEEP_SECS: f64 = 120.0;
/// An AI session from elsewhere counts as finished after this long without edits (seconds).
const EXT_QUIET_SECS: f64 = 20.0;

/// "◆ AI edited 12 items from 00:12 ▸": AI edits the view did not (fully) follow.
#[derive(Clone)]
struct AiChip {
    doc: u64,
    /// Where the edits start.
    t: f64,
    count: usize,
    ids: Vec<Id>,
    /// Shown since (refreshed while the person is busy).
    at: f64,
}

/// An AI edit from another MCP session: (group key, label, the project before it, ids it touched).
type ExtEdit = (String, String, Option<Arc<Snapshot>>, Vec<Id>);

/// AI edits by one MCP session (not the AI panel's run), until it goes quiet.
struct ExtBurst {
    doc: u64,
    /// "session:<id>", or "author:<name>" for session-less edits.
    key: String,
    /// "Claude Code (stdio #2)".
    label: String,
    /// The project before the first edit.
    before: Arc<Snapshot>,
    touched: Vec<Id>,
    last: f64,
    edits: usize,
}

/// Default folder for exports of unsaved projects.
fn dirs_videos() -> PathBuf {
    std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Videos")).filter(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir)
}
