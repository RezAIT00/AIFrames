//! AIFrames MCP server, shared services and headless CLI (see ARCHITECTURE.md, "MCP and CLI").
//!
//! The GUI and the AI share one [`Services`]: the hub of open projects, the media cache, the
//! renderer and background jobs. Actions both sides need (import, transcribe, export, silence
//! removal, captions, chapters...) live in [`actions`] so the person and the AI get identical
//! behaviour.
//!
//! * [`AiframesBackend`] implements [`suite_mcp::Backend`]: the AI tools over the hub.
//! * [`start_http`] serves them to AIs for a running window (Streamable HTTP on 47675..).
//! * [`run_stdio`] is `aiframes mcp`: proxies to a running window, else works headlessly with a
//!   `launch_gui` hand-off.
//! * [`cli`] is the headless command line (`aiframes new|import|describe|...`), built on the same tools.
//!
//! Clients connect with e.g. `claude mcp add aiframes -- "C:\path\to\aiframes.exe" mcp` (stdio), or
//! directly to a running window at `http://127.0.0.1:47675/mcp`.

pub mod actions;
mod backend;
mod cli;
pub mod demo;
mod schema;
mod tools;
pub mod transcripts;
pub mod util;

use aiframes_core::{Document, Id, Project, Resolved};
use aiframes_media::MediaPool;
use aiframes_render::Renderer;
use serde_json::{json, Value as J};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use suite_mcp::AppSpec;

pub use backend::AiframesBackend;
pub use cli::{cli, cli_with, cli_with_window, is_cli_command, CLI_COMMANDS};
pub use schema::instructions;
pub use suite_hub;

pub static SPEC: AppSpec = AppSpec {
    id: "aiframes",
    name: "AIFrames",
    title: "AIFrames Video Editor",
    version: env!("CARGO_PKG_VERSION"),
    default_port: DEFAULT_PORT,
    doc_noun: "project",
    open_tool: "open_project",
    save_tool: "save_project",
};

/// First port tried by [`start_http`] (the next 20 are tried if busy).
pub const DEFAULT_PORT: u16 = 47675;

/// (Addition) The longest one tool call waits for a job (`wait_seconds` above it is capped): no call
/// blocks for many minutes; a job still running comes back with its id to follow with `jobs`.
pub const MAX_WAIT_SECONDS: f64 = 300.0;

/// Author name the AI tools use unless told otherwise.
pub const AI_AUTHOR: &str = "ai";

/// What the GUI and the AI share about an open project beyond the document itself.
#[derive(Clone, Debug, Default)]
pub struct LiveState {
    /// Written by the GUI every frame.
    pub playhead: f64,
    pub playing: bool,
    pub selection: Vec<Id>,
    pub in_point: Option<f64>,
    pub out_point: Option<f64>,
    /// Requests from the AI, consumed by the GUI.
    pub seek_request: Option<f64>,
    pub play_request: Option<bool>,
    pub select_request: Option<Vec<Id>>,
    /// (Addition) A side panel the AI wants the person to see ("review", "transcript", …), by its id.
    pub panel_request: Option<String>,
    /// Ids the AI changed most recently and the revision they belong to (the GUI flashes them).
    pub ai_changed: Vec<Id>,
    pub ai_changed_revision: u64,
    /// (Addition) With `play_request == Some(true)`: stop playback when the playhead reaches this
    /// timeline time (the AI's `play` tool with `to`). The GUI clears it when it stops.
    pub play_until: Option<f64>,
    /// (Addition) The MCP session of the latest AI edit (`ai_changed_revision`), `None` when the edit
    /// came without a session (CLI, session-less HTTP).
    pub ai_session: Option<AiSession>,
    /// (Addition) The session of every AI edit by revision (the latest [`AI_SESSIONS_KEPT`]), so the
    /// window can group a session's edits into one recap.
    pub ai_sessions: std::collections::BTreeMap<u64, AiSession>,
    /// (Addition) The person's Source monitor (written by the GUI every frame).
    pub source: SourceView,
    /// (Addition) A file the AI wants shown in the Source monitor, with marks (consumed by the GUI).
    pub source_request: Option<SourceView>,
}

/// (Addition) What the Source monitor shows: a media file, its playhead and in/out marks, all in
/// seconds of the file (source seconds, like `in_point`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceView {
    pub asset: Option<Id>,
    pub playhead: f64,
    pub in_point: Option<f64>,
    pub out_point: Option<f64>,
}

/// (Addition) How many revisions [`LiveState::ai_sessions`] remembers.
pub const AI_SESSIONS_KEPT: usize = 2000;

/// (Addition) An MCP client session that edited a project.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AiSession {
    /// The session id (stable for the session: `Mcp-Session-Id`, or the stdio server's id).
    pub id: String,
    /// A short human label: the client's name from `initialize` (title or name) and the transport
    /// with a per-window number, e.g. "Claude Code (stdio #2)"; "AI session #3 (HTTP)" when the
    /// client did not say who it is.
    pub label: String,
}

/// One open project in the hub.
pub struct ProjectDoc {
    pub doc: Document,
    pub live: LiveState,
    untitled: u32,
}

impl ProjectDoc {
    pub fn new(doc: Document) -> Self {
        ProjectDoc { doc, live: LiveState::default(), untitled: 0 }
    }

    /// Window/tab title: file name, or "Untitled N".
    pub fn title(&self) -> String {
        match &self.doc.path {
            Some(p) => p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            None => format!("Untitled {}", self.untitled.max(1)),
        }
    }

    /// (Addition) What the renderer needs, cloned so rendering can happen without holding the lock:
    /// the project, its resolution and the folder relative media paths resolve against.
    pub fn render_input(&self) -> RenderInput {
        RenderInput { project: self.doc.project.clone(), resolved: self.doc.resolved().clone(), dir: self.doc.project_dir() }
    }

    /// (Addition) Records that `author` (not the person) changed `ids` at `revision`, so the GUI can
    /// flash them. Called by every AI edit.
    pub fn mark_ai_changed(&mut self, ids: Vec<Id>, revision: u64) {
        self.mark_ai_changed_by(ids, revision, None);
    }

    /// (Addition) [`ProjectDoc::mark_ai_changed`] recording which MCP session made the edit
    /// ([`LiveState::ai_session`], [`LiveState::ai_sessions`]).
    pub fn mark_ai_changed_by(&mut self, ids: Vec<Id>, revision: u64, session: Option<&AiSession>) {
        self.live.ai_changed = ids;
        self.live.ai_changed_revision = revision;
        self.live.ai_session = session.cloned();
        if let Some(s) = session {
            self.live.ai_sessions.insert(revision, s.clone());
            while self.live.ai_sessions.len() > AI_SESSIONS_KEPT {
                self.live.ai_sessions.pop_first();
            }
        }
    }
}

/// (Addition) A detached copy of what rendering a project needs.
#[derive(Clone)]
pub struct RenderInput {
    pub project: Project,
    pub resolved: Resolved,
    pub dir: PathBuf,
}

impl suite_hub::HubDoc for ProjectDoc {
    fn path(&self) -> Option<&Path> {
        self.doc.path.as_deref()
    }
    fn set_untitled_index(&mut self, i: u32) {
        self.untitled = i;
    }
}

pub type Hub = suite_hub::Hub<ProjectDoc>;

/// (Addition) A shared handle to one open project.
pub type DocRef = suite_hub::DocRef<ProjectDoc>;

// ---------------------------------------------------------------------------------------------
// Jobs (export, transcription, analysis) shared by the GUI and the AI
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobStatus {
    /// "running" | "done" | "failed" | "cancelled".
    pub fn name(self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug)]
pub struct JobInfo {
    pub id: u64,
    /// "export" | "transcribe" | "analyze" | ...
    pub kind: String,
    pub label: String,
    /// Hub document id the job belongs to, if any.
    pub doc: Option<u64>,
    /// 0..1
    pub progress: f32,
    pub status: JobStatus,
    /// Latest status line, or the error.
    pub message: String,
    pub result: Option<J>,
    /// (Addition) When the job started and finished.
    pub started: Instant,
    pub finished: Option<Instant>,
}

impl JobInfo {
    /// (Addition) Seconds since the job started (until it finished).
    pub fn elapsed(&self) -> f64 {
        self.finished.unwrap_or_else(Instant::now).duration_since(self.started).as_secs_f64()
    }

    /// (Addition) The job as JSON for tools and the CLI.
    pub fn to_json(&self) -> J {
        let mut j = json!({
            "job": self.id,
            "kind": self.kind,
            "label": self.label,
            "status": self.status.name(),
            "progress": (self.progress as f64 * 1000.0).round() / 1000.0,
            "elapsed_s": (self.elapsed() * 10.0).round() / 10.0,
        });
        if let Some(d) = self.doc {
            j["project"] = json!(d);
        }
        if !self.message.is_empty() {
            j[if self.status == JobStatus::Failed { "error" } else { "message" }] = json!(self.message);
        }
        if let Some(r) = &self.result {
            j["result"] = r.clone();
        }
        j
    }
}

/// Handed to a running job.
#[derive(Clone)]
pub struct JobCtx {
    pub id: u64,
    jobs: Arc<Jobs>,
    cancel: Arc<AtomicBool>,
}

impl JobCtx {
    pub fn progress(&self, fraction: f32, message: &str) {
        self.jobs.update(self.id, |j| {
            j.progress = fraction.clamp(0.0, 1.0);
            j.message = message.to_string();
        });
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }
}

type Waker = Arc<dyn Fn() + Send + Sync>;

/// Finished jobs kept for `jobs`/`export_status` (older ones are pruned; running jobs are never pruned).
pub const KEEP_FINISHED_JOBS: usize = 40;

#[derive(Default)]
pub struct Jobs {
    inner: Mutex<Vec<(JobInfo, Arc<AtomicBool>)>>,
    next: Mutex<u64>,
    waker: Mutex<Option<Waker>>,
}

impl Jobs {
    pub fn new() -> Arc<Self> {
        Arc::new(Jobs::default())
    }

    /// (Addition) Called after every job change (progress, completion) so a UI can repaint.
    pub fn set_waker(&self, f: impl Fn() + Send + Sync + 'static) {
        *suite_hub::lock(&self.waker) = Some(Arc::new(f));
    }

    fn wake(&self) {
        let w = suite_hub::lock(&self.waker).clone();
        if let Some(w) = w {
            w();
        }
    }

    /// Runs `f` on a background thread and returns the job id.
    pub fn spawn(
        self: &Arc<Self>,
        kind: &str,
        label: &str,
        doc: Option<u64>,
        f: impl FnOnce(JobCtx) -> Result<J, String> + Send + 'static,
    ) -> u64 {
        let id = {
            let mut n = suite_hub::lock(&self.next);
            *n += 1;
            *n
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let info = JobInfo {
            id,
            kind: kind.into(),
            label: label.into(),
            doc,
            progress: 0.0,
            status: JobStatus::Running,
            message: String::new(),
            result: None,
            started: Instant::now(),
            finished: None,
        };
        {
            let mut inner = suite_hub::lock(&self.inner);
            inner.push((info, cancel.clone()));
            prune(&mut inner);
        }
        let ctx = JobCtx { id, jobs: self.clone(), cancel: cancel.clone() };
        let jobs = self.clone();
        let spawned = std::thread::Builder::new().name(format!("aiframes-job-{id}")).spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(ctx)))
                .unwrap_or_else(|_| Err("internal error: the job crashed".to_string()));
            jobs.update(id, |j| {
                j.finished = Some(Instant::now());
                match r {
                    Ok(v) => {
                        j.status = JobStatus::Done;
                        j.progress = 1.0;
                        j.message.clear();
                        j.result = Some(v);
                    }
                    Err(e) if cancel.load(Ordering::Relaxed) => {
                        j.status = JobStatus::Cancelled;
                        j.message = e;
                    }
                    Err(e) => {
                        j.status = JobStatus::Failed;
                        j.message = e;
                    }
                }
            });
        });
        if let Err(e) = spawned {
            self.update(id, |j| {
                j.status = JobStatus::Failed;
                j.finished = Some(Instant::now());
                j.message = format!("could not start a worker thread: {e}");
            });
        }
        self.wake();
        id
    }

    /// Every job, oldest first.
    pub fn list(&self) -> Vec<JobInfo> {
        suite_hub::lock(&self.inner).iter().map(|(j, _)| j.clone()).collect()
    }

    pub fn get(&self, id: u64) -> Option<JobInfo> {
        suite_hub::lock(&self.inner).iter().find(|(j, _)| j.id == id).map(|(j, _)| j.clone())
    }

    /// Asks a running job to stop. False if there is no such job.
    pub fn cancel(&self, id: u64) -> bool {
        match suite_hub::lock(&self.inner).iter().find(|(j, _)| j.id == id) {
            Some((_, c)) => {
                c.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// (Addition) Blocks until the job is no longer running or `timeout` passes; returns its state.
    pub fn wait(&self, id: u64, timeout: Duration) -> Option<JobInfo> {
        let deadline = Instant::now() + timeout;
        loop {
            let j = self.get(id)?;
            if j.status != JobStatus::Running || Instant::now() >= deadline {
                return Some(j);
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    /// (Addition) The most recent job of `kind` (optionally for document `doc`).
    pub fn latest(&self, kind: &str, doc: Option<u64>) -> Option<JobInfo> {
        suite_hub::lock(&self.inner).iter().rev().find(|(j, _)| j.kind == kind && (doc.is_none() || j.doc == doc)).map(|(j, _)| j.clone())
    }

    /// (Addition) Running jobs.
    pub fn running(&self) -> Vec<JobInfo> {
        suite_hub::lock(&self.inner).iter().filter(|(j, _)| j.status == JobStatus::Running).map(|(j, _)| j.clone()).collect()
    }

    fn update(&self, id: u64, f: impl FnOnce(&mut JobInfo)) {
        if let Some((j, _)) = suite_hub::lock(&self.inner).iter_mut().find(|(j, _)| j.id == id) {
            f(j);
        }
        self.wake();
    }
}

/// Drops the oldest finished jobs beyond [`KEEP_FINISHED_JOBS`].
fn prune(v: &mut Vec<(JobInfo, Arc<AtomicBool>)>) {
    let finished = v.iter().filter(|(j, _)| j.status != JobStatus::Running).count();
    let mut excess = finished.saturating_sub(KEEP_FINISHED_JOBS);
    if excess == 0 {
        return;
    }
    v.retain(|(j, _)| {
        if excess > 0 && j.status != JobStatus::Running {
            excess -= 1;
            false
        } else {
            true
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------------------------

/// Shared by the GUI and the MCP backend in one process.
pub struct Services {
    pub hub: Arc<Hub>,
    pub pool: Arc<MediaPool>,
    pub renderer: Arc<Renderer>,
    pub jobs: Arc<Jobs>,
}

impl Services {
    pub fn new() -> Arc<Self> {
        Self::with_pool(MediaPool::new(1 << 30))
    }

    /// (Addition) Services over an existing media pool (e.g. one with a private cache folder in tests).
    pub fn with_pool(pool: Arc<MediaPool>) -> Arc<Self> {
        let hub = Hub::new();
        let jobs = Jobs::new();
        let h = hub.clone();
        jobs.set_waker(move || h.wake());
        Arc::new(Services { hub, renderer: Arc::new(Renderer::new(pool.clone())), pool, jobs })
    }

    /// (Addition) Opens a project file into the hub (or activates it if it is already open).
    /// Returns the hub id.
    pub fn open_project(&self, path: &Path) -> Result<u64, String> {
        self.open_project_with(path, true).map(|(id, _)| id)
    }

    /// (Addition) Opens a project file into the hub. `activate: false` leaves the person's active
    /// project as it is (also when the file is already open). Returns the hub id and whether it was
    /// already open.
    pub fn open_project_with(&self, path: &Path, activate: bool) -> Result<(u64, bool), String> {
        if let Some(id) = self.hub.find_path(path) {
            if activate {
                self.hub.set_active(id);
            }
            return Ok((id, true));
        }
        let doc = Document::open(path).map_err(|e| e.to_string())?;
        let prev = self.hub.active().map(|(i, _)| i);
        let id = self.hub.add(ProjectDoc::new(doc));
        if !activate {
            if let Some(p) = prev {
                self.hub.set_active(p);
            }
        }
        Ok((id, false))
    }

    /// (Addition) Adds a new unsaved project to the hub and makes it active. Returns the hub id.
    pub fn add_project(&self, project: Project) -> u64 {
        self.hub.add(ProjectDoc::new(Document::new(project)))
    }
}

// ---------------------------------------------------------------------------------------------
// Entry points used by the `aiframes` binary
// ---------------------------------------------------------------------------------------------

/// Starts the Streamable-HTTP MCP endpoint for a running GUI. Returns the port.
pub fn start_http(s: Arc<Services>) -> std::io::Result<u16> {
    start_http_on(s, DEFAULT_PORT, 21, true)
}

/// (Addition) Serves MCP on the first free port from `first` (0 = any free port), optionally
/// advertising it in the instance file other `aiframes mcp` processes look for.
pub fn start_http_on(s: Arc<Services>, first: u16, attempts: u16, advertise: bool) -> std::io::Result<u16> {
    suite_mcp::http::start_http_on(AiframesBackend::new(s), first, attempts, advertise)
}

/// (Addition) Removes this process's instance file (call when the GUI exits).
pub fn shutdown() {
    suite_mcp::instance::remove_instance_if_ours(&SPEC);
}

/// (Addition) If an AIFrames window is already running, asks it to open `paths` and returns true.
pub fn try_forward_open(paths: &[PathBuf]) -> bool {
    suite_mcp::forward_open(&SPEC, paths)
}

/// (Addition) Port of a running AIFrames window, if any.
pub fn live_instance_port() -> Option<u16> {
    suite_mcp::instance::live_instance_port(&SPEC)
}

/// `aiframes mcp`: stdio server that proxies to a running window or works headlessly.
pub fn run_stdio() {
    ensure_gui_exe();
    suite_mcp::stdio::run_stdio(AiframesBackend::new(Services::new()))
}

/// (Addition) Headless stdio server (never proxies to a window). Used by tests.
pub fn headless_stdio(s: Arc<Services>) -> suite_mcp::stdio::StdioServer<AiframesBackend> {
    suite_mcp::stdio::StdioServer::headless(AiframesBackend::new(s))
}

/// `launch_gui` starts `AIFRAMES_GUI_EXE`, else the current executable (which is `aiframes.exe` when
/// running as `aiframes mcp`). When this process is something else (a wrapper or test binary) and an
/// `aiframes` executable sits next to it, point the framework at that one.
fn ensure_gui_exe() {
    let var = SPEC.env("GUI_EXE");
    if std::env::var_os(&var).is_some_and(|v| !v.is_empty()) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let stem = exe.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    if stem == "aiframes" {
        return;
    }
    let sibling = exe.with_file_name(if cfg!(windows) { "aiframes.exe" } else { "aiframes" });
    if sibling.is_file() {
        std::env::set_var(var, sibling);
    }
}
