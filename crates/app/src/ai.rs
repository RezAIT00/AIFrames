//! The AI panel: runs Claude Code headless against this window's MCP endpoint and streams what it
//! does (assistant text, tool calls, results). Edits arrive live through MCP and flash in the
//! timeline; this panel is the person's view of the conversation.
//!
//! Command: `claude -p <prompt> --output-format stream-json --verbose --mcp-config <file>
//! --allowedTools mcp__aiframes__* [--strict-mcp-config] [--model m] --append-system-prompt <...>`
//! with the project folder as working directory.

use crate::theme;
use aiframes_core::Id;
use eframe::egui::{self, RichText};
use serde_json::Value as J;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub const SYSTEM_PROMPT: &str = "You are working inside AIFrames, a native video editor that the person has open and is watching right now. \
Use the `aiframes` MCP tools: they operate on the open project live, every edit appears immediately in the person's window \
and the person can undo it. Start with describe_project (and get_timeline when you need exact times). Look at frames with \
snapshot or contact_sheet before and after visual changes. Prefer anchored times (after/with a clip, at a marker or a \
spoken word) so the edit stays magnetic. Batch related ops into one edit call with a clear label. Do not save the project \
unless asked. Finish with one or two sentences on what you changed.";

/// What the panel knows about the window when a run starts.
#[derive(Clone, Debug, Default)]
pub struct RunContext {
    pub port: Option<u16>,
    /// Hub id of the project this window shows (the run is pinned to it).
    pub project_id: u64,
    pub project_path: Option<PathBuf>,
    pub title: String,
    pub playhead: f64,
    pub timecode: String,
    pub selection: Vec<Id>,
    pub in_point: Option<f64>,
    pub out_point: Option<f64>,
    pub duration: f64,
}

/// One parsed event of Claude Code's stream-json output.
#[derive(Clone, Debug, PartialEq)]
pub enum AiEvent {
    Init { model: String, mcp: Vec<(String, String)> },
    Text(String),
    ToolUse { name: String, summary: String },
    ToolResult { ok: bool, summary: String },
    /// Ids an edit reported as created / changed / removed (from the tool result's receipt).
    Receipt { created: Vec<String>, changed: Vec<String>, removed: Vec<String> },
    Done { ok: bool, text: String, cost: Option<f64>, turns: Option<u64>, ms: Option<u64> },
    Stderr(String),
    Exited(Option<i32>),
}

pub fn mcp_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/mcp")
}

/// The `--mcp-config` file contents.
pub fn mcp_config_json(port: u16) -> String {
    serde_json::json!({"mcpServers": {"aiframes": {"type": "http", "url": mcp_url(port)}}}).to_string()
}

/// The prompt sent to Claude: context about the window, then the person's request.
pub fn compose_prompt(cx: &RunContext, request: &str, ids: &[Id]) -> String {
    let mut s = String::new();
    s.push_str("Context (the AIFrames window the person is watching):\n");
    match &cx.project_path {
        Some(p) => s.push_str(&format!("- Project: {} ({})\n", cx.title, p.display())),
        None => s.push_str(&format!("- Project: {} (not saved yet)\n", cx.title)),
    }
    s.push_str(&format!(
        "- Project id: {id}. Other projects may be open in other tabs: pass \"project\": {id} in every aiframes tool call so you only ever edit this one.\n",
        id = cx.project_id
    ));
    s.push_str(&format!("- Playhead: {:.3} s ({}) of {:.2} s\n", cx.playhead, cx.timecode, cx.duration));
    if !cx.selection.is_empty() {
        s.push_str(&format!("- Selected: {}\n", cx.selection.join(", ")));
    }
    if cx.in_point.is_some() || cx.out_point.is_some() {
        let f = |v: Option<f64>| v.map(|v| format!("{v:.3} s")).unwrap_or_else(|| "-".into());
        s.push_str(&format!("- In/out: {} .. {}\n", f(cx.in_point), f(cx.out_point)));
    }
    let extra: Vec<&Id> = ids.iter().filter(|i| !cx.selection.contains(i)).collect();
    if !extra.is_empty() {
        s.push_str(&format!("- About: {}\n", extra.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
    }
    s.push_str("\nRequest:\n");
    s.push_str(request.trim());
    s.push('\n');
    s
}

/// Command-line arguments for `claude`.
pub fn claude_args(prompt: &str, config: &Path, model: &str, strict: bool) -> Vec<String> {
    let mut a = vec![
        "-p".to_string(),
        prompt.to_string(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--mcp-config".into(),
        config.to_string_lossy().to_string(),
    ];
    if strict {
        a.push("--strict-mcp-config".into());
    }
    if !model.trim().is_empty() {
        a.push("--model".into());
        a.push(model.trim().to_string());
    }
    a.push("--append-system-prompt".into());
    a.push(SYSTEM_PROMPT.into());
    a.push("--allowedTools".into());
    a.push("mcp__aiframes__*".into());
    a
}

fn short_tool(name: &str) -> String {
    name.strip_prefix("mcp__aiframes__").unwrap_or(name).to_string()
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

/// A one-line summary of a tool call's input (op names for `edit`).
pub fn summarize_input(input: &J) -> String {
    if let Some(ops) = input.get("ops").and_then(J::as_array) {
        let parts: Vec<String> = ops
            .iter()
            .map(|o| {
                let name = o.get("op").and_then(J::as_str).unwrap_or("?");
                let target = ["clip", "track", "asset", "marker", "transition", "from"].iter().find_map(|k| o.get(*k).and_then(J::as_str));
                match target {
                    Some(t) => format!("{name} {t}"),
                    None => name.to_string(),
                }
            })
            .collect();
        let label = input.get("label").and_then(J::as_str).map(|l| format!("\"{l}\": ")).unwrap_or_default();
        return truncate(&format!("{label}{}", parts.join(", ")), 160);
    }
    match input {
        J::Object(m) if m.is_empty() => String::new(),
        J::Null => String::new(),
        other => truncate(&other.to_string(), 160),
    }
}

fn summarize_result(content: &J) -> String {
    match content {
        J::String(s) => truncate(s.lines().next().unwrap_or(""), 200),
        J::Array(items) => {
            let mut parts = Vec::new();
            for it in items {
                match it.get("type").and_then(J::as_str) {
                    Some("text") => parts.push(truncate(it.get("text").and_then(J::as_str).unwrap_or("").lines().next().unwrap_or(""), 200)),
                    Some("image") => parts.push("[image]".into()),
                    _ => {}
                }
            }
            parts.join(" ")
        }
        J::Null => String::new(),
        other => truncate(&other.to_string(), 200),
    }
}

/// The created/changed/removed ids of an edit's receipt (a tool result whose text is JSON).
fn receipt_of(content: &J) -> Option<AiEvent> {
    let text = match content {
        J::String(s) => s.clone(),
        J::Array(items) => items.iter().filter(|i| i.get("type").and_then(J::as_str) == Some("text")).filter_map(|i| i.get("text").and_then(J::as_str)).collect::<Vec<_>>().join(""),
        _ => return None,
    };
    let v: J = serde_json::from_str(text.trim()).ok()?;
    if v.get("plan").and_then(J::as_bool) == Some(true) || v.get("planned").is_some() {
        return None; // a dry run changed nothing
    }
    let ids = |k: &str| -> Vec<String> { v.get(k).and_then(J::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default() };
    let (created, changed, removed) = (ids("created"), ids("changed"), ids("removed"));
    (!created.is_empty() || !changed.is_empty() || !removed.is_empty()).then_some(AiEvent::Receipt { created, changed, removed })
}

/// What a run did in the end: (added, changed, removed, temporary = added and removed again).
pub fn net_changes(created: &[String], changed: &[String], removed: &[String]) -> [Vec<String>; 4] {
    let has = |v: &[String], x: &String| v.contains(x);
    let mut out: [Vec<String>; 4] = Default::default();
    let mut push = |i: usize, x: &String| {
        if !out[i].contains(x) {
            out[i].push(x.clone());
        }
    };
    for x in created {
        if has(removed, x) {
            push(3, x);
        } else {
            push(0, x);
        }
    }
    for x in changed {
        if !has(created, x) && !has(removed, x) {
            push(1, x);
        }
    }
    for x in removed {
        if !has(created, x) {
            push(2, x);
        }
    }
    out
}

/// Parses one line of `--output-format stream-json` output.
pub fn parse_line(line: &str) -> Vec<AiEvent> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_str::<J>(line) else {
        return vec![AiEvent::Stderr(truncate(line, 300))];
    };
    let mut out = Vec::new();
    match v.get("type").and_then(J::as_str) {
        Some("system") if v.get("subtype").and_then(J::as_str) == Some("init") => {
            let model = v.get("model").and_then(J::as_str).unwrap_or("").to_string();
            let mcp = v
                .get("mcp_servers")
                .and_then(J::as_array)
                .map(|a| {
                    a.iter()
                        .map(|s| (s.get("name").and_then(J::as_str).unwrap_or("?").to_string(), s.get("status").and_then(J::as_str).unwrap_or("?").to_string()))
                        .collect()
                })
                .unwrap_or_default();
            out.push(AiEvent::Init { model, mcp });
        }
        Some("assistant") => {
            for c in v.pointer("/message/content").and_then(J::as_array).into_iter().flatten() {
                match c.get("type").and_then(J::as_str) {
                    Some("text") => {
                        let t = c.get("text").and_then(J::as_str).unwrap_or("").trim().to_string();
                        if !t.is_empty() {
                            out.push(AiEvent::Text(t));
                        }
                    }
                    Some("tool_use") => out.push(AiEvent::ToolUse {
                        name: short_tool(c.get("name").and_then(J::as_str).unwrap_or("?")),
                        summary: summarize_input(c.get("input").unwrap_or(&J::Null)),
                    }),
                    _ => {}
                }
            }
        }
        Some("user") => {
            for c in v.pointer("/message/content").and_then(J::as_array).into_iter().flatten() {
                if c.get("type").and_then(J::as_str) == Some("tool_result") {
                    let ok = !c.get("is_error").and_then(J::as_bool).unwrap_or(false);
                    let content = c.get("content").unwrap_or(&J::Null);
                    out.push(AiEvent::ToolResult { ok, summary: summarize_result(content) });
                    if ok {
                        out.extend(receipt_of(content));
                    }
                }
            }
        }
        Some("result") => {
            let ok = v.get("subtype").and_then(J::as_str) == Some("success") && !v.get("is_error").and_then(J::as_bool).unwrap_or(false);
            out.push(AiEvent::Done {
                ok,
                text: v.get("result").and_then(J::as_str).unwrap_or("").to_string(),
                cost: v.get("total_cost_usd").and_then(J::as_f64),
                turns: v.get("num_turns").and_then(J::as_u64),
                ms: v.get("duration_ms").and_then(J::as_u64),
            });
        }
        _ => {}
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogKind {
    User,
    Assistant,
    Tool,
    ToolOk,
    ToolErr,
    Done,
    Error,
    Notice,
    System,
    Summary,
}

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub time: String,
    pub kind: LogKind,
    pub text: String,
    /// Clickable ids by group ("Added", "Changed", …) for run summaries.
    pub ids: Vec<(String, Vec<String>)>,
}

/// What the panel asks the app to do.
#[derive(Default)]
pub struct AiUi {
    pub run: bool,
    /// Show this clip/marker (select it and scroll the timeline to it).
    pub reveal: Option<String>,
}

struct Run {
    child: Arc<Mutex<Option<Child>>>,
    events: Arc<Mutex<Vec<AiEvent>>>,
    done: Arc<AtomicBool>,
    started: Instant,
    config: PathBuf,
}

/// Quick prompts offered under the prompt box.
pub const PRESETS: &[(&str, &str)] = &[
    ("Describe", "Describe this edit briefly: structure, pacing, anything that looks wrong."),
    ("Check", "Validate the project and fix the problems you find (explain each fix)."),
    ("Tighten", "Remove long silences and pauses from the voice/main track, keeping natural breaths."),
    ("Captions", "Transcribe the voice if needed and add readable word-synced captions on a captions track."),
    ("Polish", "Add tasteful transitions at the cuts and a short fade in/out at the start and end."),
];

pub struct AiPanel {
    pub prompt: String,
    pub ids: Vec<Id>,
    pub model: String,
    pub strict: bool,
    pub log: Vec<LogEntry>,
    run: Option<Run>,
    pub focus_prompt: bool,
    pub last_cost: Option<f64>,
    /// Ids the current run's edits created / changed / removed, in order.
    run_ids: [Vec<String>; 3],
}

impl Default for AiPanel {
    fn default() -> Self {
        AiPanel { prompt: String::new(), ids: Vec::new(), model: String::new(), strict: true, log: Vec::new(), run: None, focus_prompt: false, last_cost: None, run_ids: Default::default() }
    }
}

fn now_label() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

impl AiPanel {
    pub fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|r| !r.done.load(Ordering::Relaxed))
    }

    pub fn push(&mut self, kind: LogKind, text: impl Into<String>) {
        self.log.push(LogEntry { time: now_label(), kind, text: text.into(), ids: Vec::new() });
        if self.log.len() > 2000 {
            self.log.drain(0..200);
        }
    }

    /// Pre-fills the prompt (from an `AskAi` action).
    pub fn ask(&mut self, prompt: &str, ids: Vec<Id>) {
        self.prompt = prompt.to_string();
        self.ids = ids;
        self.focus_prompt = true;
    }

    pub fn start(&mut self, cx: &RunContext) {
        if self.running() || self.prompt.trim().is_empty() {
            return;
        }
        let Some(port) = cx.port else {
            self.push(LogKind::Error, "The MCP server is not running, so Claude cannot reach this window (see the status bar).");
            return;
        };
        let config = std::env::temp_dir().join(format!("aiframes-mcp-{}-{port}.json", std::process::id()));
        if let Err(e) = std::fs::write(&config, mcp_config_json(port)) {
            self.push(LogKind::Error, format!("Cannot write the MCP config {}: {e}", config.display()));
            return;
        }
        let prompt = compose_prompt(cx, &self.prompt, &self.ids);
        let args = claude_args(&prompt, &config, &self.model, self.strict);
        let cwd = cx.project_path.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf).filter(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir);
        let mut cmd = Command::new("claude");
        cmd.args(&args).current_dir(&cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.push(LogKind::Error, format!("Could not start Claude Code (`claude`): {e}. Install it and make sure it is on PATH."));
                return;
            }
        };
        self.push(LogKind::User, self.prompt.trim().to_string());
        let events = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let child = Arc::new(Mutex::new(Some(child)));
        if let Some(err) = stderr {
            let ev = events.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        lock(&ev).push(AiEvent::Stderr(truncate(&line, 300)));
                    }
                }
            });
        }
        {
            let ev = events.clone();
            let dn = done.clone();
            let ch = child.clone();
            std::thread::spawn(move || {
                if let Some(out) = stdout {
                    for line in BufReader::new(out).lines().map_while(Result::ok) {
                        let parsed = parse_line(&line);
                        if !parsed.is_empty() {
                            lock(&ev).extend(parsed);
                        }
                    }
                }
                let code = lock(&ch).as_mut().and_then(|c| c.wait().ok()).and_then(|s| s.code());
                lock(&ev).push(AiEvent::Exited(code));
                dn.store(true, Ordering::Relaxed);
            });
        }
        self.run = Some(Run { child, events, done, started: Instant::now(), config });
        self.run_ids = Default::default();
        self.prompt.clear();
        self.ids.clear();
    }

    pub fn stop(&mut self) {
        if let Some(r) = &self.run {
            if let Some(c) = lock(&r.child).as_mut() {
                let _ = c.kill();
            }
            self.push(LogKind::System, "Stopped.");
        }
    }

    /// Moves streamed events into the log. Returns true when something arrived.
    pub fn poll(&mut self) -> bool {
        let Some(run) = &self.run else { return false };
        let events: Vec<AiEvent> = std::mem::take(&mut *lock(&run.events));
        let finished = run.done.load(Ordering::Relaxed);
        let config = run.config.clone();
        let any = !events.is_empty();
        for e in events {
            match e {
                AiEvent::Init { model, mcp } => {
                    let servers: Vec<String> = mcp.iter().map(|(n, s)| format!("{n}: {s}")).collect();
                    self.push(LogKind::System, format!("Claude {model} connected ({})", if servers.is_empty() { "no MCP servers".into() } else { servers.join(", ") }));
                    if !mcp.iter().any(|(n, s)| n == "aiframes" && s == "connected") {
                        self.push(LogKind::Error, "The aiframes MCP server did not connect; check the endpoint in the status bar.");
                    }
                }
                AiEvent::Text(t) => self.push(LogKind::Assistant, t),
                AiEvent::ToolUse { name, summary } => self.push(LogKind::Tool, if summary.is_empty() { name } else { format!("{name}  {summary}") }),
                AiEvent::ToolResult { ok, summary } => self.push(if ok { LogKind::ToolOk } else { LogKind::ToolErr }, summary),
                AiEvent::Receipt { created, changed, removed } => {
                    for (i, v) in [created, changed, removed].into_iter().enumerate() {
                        for x in v {
                            if !self.run_ids[i].contains(&x) {
                                self.run_ids[i].push(x);
                            }
                        }
                    }
                }
                AiEvent::Done { ok, text: _, cost, turns, ms } => {
                    self.last_cost = cost;
                    let mut s = String::from(if ok { "Done" } else { "Finished with an error" });
                    if let Some(t) = turns {
                        s.push_str(&format!(" · {t} turns"));
                    }
                    if let Some(ms) = ms {
                        s.push_str(&format!(" · {:.1} s", ms as f64 / 1000.0));
                    }
                    if let Some(c) = cost {
                        s.push_str(&format!(" · ${c:.3}"));
                    }
                    self.push(if ok { LogKind::Done } else { LogKind::Error }, s);
                }
                AiEvent::Stderr(s) => self.push(LogKind::System, s),
                AiEvent::Exited(code) => {
                    if code.is_some_and(|c| c != 0) {
                        self.push(LogKind::Error, format!("claude exited with code {}", code.unwrap_or(-1)));
                    }
                }
            }
        }
        if finished {
            let _ = std::fs::remove_file(config);
            self.run = None;
            self.summarize_run();
        }
        any || finished
    }

    /// The run's net effect on the project, with clickable ids.
    fn summarize_run(&mut self) {
        let groups = net_changes(&self.run_ids[0], &self.run_ids[1], &self.run_ids[2]);
        self.run_ids = Default::default();
        let n: usize = groups.iter().map(Vec::len).sum();
        if n > 0 {
            self.push_summary(format!("This run touched {n} item(s):"), groups);
        }
    }

    /// A recap with clickable ids: `[added, changed, removed, temporary]` (empty groups left out).
    pub fn push_summary(&mut self, text: String, [added, changed, removed, temporary]: [Vec<String>; 4]) {
        let mut groups = Vec::new();
        for (label, v) in [("Added", added), ("Changed", changed), ("Removed", removed), ("Temporary (added and removed again)", temporary)] {
            if !v.is_empty() {
                groups.push((label.to_string(), v));
            }
        }
        self.log.push(LogEntry { time: now_label(), kind: LogKind::Summary, text, ids: groups });
    }

    pub fn elapsed(&self) -> Option<f64> {
        self.run.as_ref().filter(|r| !r.done.load(Ordering::Relaxed)).map(|r| r.started.elapsed().as_secs_f64())
    }

    /// The panel. Returns true when the user asked to run.
    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &RunContext, exe: &str, follow_ai: &mut bool, follow_playhead: &mut bool) -> AiUi {
        let mut run = false;
        let mut reveal = None;
        let pal = theme::palette();
        ui.horizontal(|ui| {
            match cx.port {
                Some(p) => {
                    ui.label(RichText::new("●").color(pal.ok));
                    ui.label(RichText::new(mcp_url(p)).monospace().small());
                    if ui.small_button("Copy").on_hover_text("Copy the MCP endpoint").clicked() {
                        ui.ctx().copy_text(mcp_url(p));
                    }
                }
                None => {
                    ui.label(RichText::new("● MCP server not running").color(pal.error));
                }
            }
        });
        ui.add_space(2.0);
        let id = egui::Id::unique("ai-prompt");
        let resp = ui.add(
            egui::TextEdit::multiline(&mut self.prompt)
                .id(id)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("Ask Claude to edit this video… (Ctrl+Enter to run)"),
        );
        if self.focus_prompt {
            resp.request_focus();
            self.focus_prompt = false;
        }
        if resp.has_focus() && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)) {
            run = true;
        }
        if !self.ids.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("About:").small().color(pal.text_dim));
                for i in &self.ids {
                    ui.label(RichText::new(i).small().monospace().color(pal.ai));
                }
                if ui.small_button("✕").clicked() {
                    self.ids.clear();
                }
            });
        } else if !cx.selection.is_empty() {
            ui.label(RichText::new(format!("Selection sent as context: {}", cx.selection.join(", "))).small().color(pal.text_dim));
        }
        // Wrapped: at the default 360 px side width the row does not fit on one line, and a plain
        // horizontal row would push the side panel wider.
        ui.horizontal_wrapped(|ui| {
            let running = self.running();
            if running {
                if ui.button(RichText::new("■ Stop").color(pal.error)).clicked() {
                    self.stop();
                }
                ui.spinner();
                if let Some(s) = self.elapsed() {
                    ui.label(RichText::new(format!("{s:.0}s")).small().color(pal.text_dim));
                }
            } else if ui.add_enabled(!self.prompt.trim().is_empty(), egui::Button::new(RichText::new("▶ Run").strong())).clicked() {
                run = true;
            }
            egui::ComboBox::from_id_salt("ai-model")
                .width(90.0)
                .selected_text(if self.model.is_empty() { "default".to_string() } else { self.model.clone() })
                .show_ui(ui, |ui| {
                    for m in ["", "fable", "opus", "sonnet", "haiku"] {
                        ui.selectable_value(&mut self.model, m.to_string(), if m.is_empty() { "default" } else { m });
                    }
                })
                .response
                .on_hover_text("Model for Claude Code");
            ui.checkbox(&mut self.strict, "only AIFrames tools").on_hover_text("--strict-mcp-config: ignore your other MCP servers (faster start)");
            ui.checkbox(follow_ai, "Follow AI").on_hover_text("Scroll the timeline to what the AI changes (unless you are working there yourself; your zoom is kept)");
            ui.checkbox(follow_playhead, "playhead").on_hover_text("Also move the playhead to the latest AI change when you are not busy (the viewer shows it)");
        });
        ui.horizontal_wrapped(|ui| {
            for (label, text) in PRESETS {
                if ui.small_button(*label).on_hover_text(*text).clicked() {
                    self.prompt = text.to_string();
                    self.focus_prompt = true;
                }
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Activity").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Clear").clicked() {
                    self.log.clear();
                }
            });
        });
        let avail = ui.available_height() - 64.0;
        egui::ScrollArea::vertical().id_salt("ai-log").max_height(avail.max(80.0)).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            if self.log.is_empty() {
                ui.label(RichText::new("Claude's steps and every AI edit to this project appear here. AI-changed clips flash in the timeline.").small().color(pal.text_dim));
            }
            for e in &self.log {
                let (icon, color) = match e.kind {
                    LogKind::User => ("›", pal.accent),
                    LogKind::Assistant => ("◆", pal.text),
                    LogKind::Tool => ("⚙", pal.ai),
                    LogKind::ToolOk => ("✓", pal.text_dim),
                    LogKind::ToolErr => ("✗", pal.error),
                    LogKind::Done => ("✔", pal.ok),
                    LogKind::Error => ("!", pal.error),
                    LogKind::Notice => ("•", pal.ai),
                    LogKind::System => ("·", pal.text_dim),
                    LogKind::Summary => ("Σ", pal.ai),
                };
                ui.horizontal_top(|ui| {
                    ui.label(RichText::new(&e.time).small().monospace().color(pal.text_faint));
                    ui.label(RichText::new(icon).color(color));
                    let text = RichText::new(&e.text).color(if matches!(e.kind, LogKind::ToolOk | LogKind::System) { pal.text_dim } else { pal.text });
                    let text = if e.kind == LogKind::Tool { text.monospace().size(11.5) } else { text };
                    ui.add(egui::Label::new(text).wrap());
                });
                for (label, ids) in &e.ids {
                    ui.horizontal_wrapped(|ui| {
                        ui.add_space(64.0);
                        ui.label(RichText::new(format!("{label}:")).small().color(pal.text_dim));
                        for id in ids.iter().take(60) {
                            let r = ui.add(egui::Button::new(RichText::new(id).small().monospace().color(pal.ai)).small().frame(false));
                            if r.on_hover_text("Select it and show it in the timeline").clicked() {
                                reveal = Some(id.clone());
                            }
                        }
                        if ids.len() > 60 {
                            ui.label(RichText::new(format!("+{} more", ids.len() - 60)).small().color(pal.text_dim));
                        }
                    });
                }
            }
        });
        ui.separator();
        ui.label(RichText::new("Use AIFrames from your own Claude Code session:").small().color(pal.text_dim));
        let line = connect_command(exe);
        // Button first (right to left) so the truncated line gets only the width that is left;
        // laid out the other way, the button overflows and the side panel grows every frame.
        let row = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
        ui.allocate_ui_with_layout(row, egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(line.clone());
            }
            ui.add(egui::Label::new(RichText::new(&line).monospace().small()).truncate()).on_hover_text(&line);
        });
        AiUi { run, reveal }
    }
}

/// `claude mcp add` line for using this AIFrames from another Claude Code session.
pub fn connect_command(exe: &str) -> String {
    format!("claude mcp add aiframes -- \"{exe}\" mcp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_and_args() {
        let c = mcp_config_json(47675);
        let v: J = serde_json::from_str(&c).unwrap();
        assert_eq!(v["mcpServers"]["aiframes"]["type"], "http");
        assert_eq!(v["mcpServers"]["aiframes"]["url"], "http://127.0.0.1:47675/mcp");
        let a = claude_args("hello", Path::new("C:/t/cfg.json"), "sonnet", true);
        assert_eq!(&a[..2], &["-p".to_string(), "hello".to_string()]);
        let pos = |s: &str| a.iter().position(|x| x == s).unwrap();
        assert_eq!(a[pos("--output-format") + 1], "stream-json");
        assert!(a.contains(&"--verbose".to_string()));
        assert_eq!(a[pos("--mcp-config") + 1], "C:/t/cfg.json");
        assert_eq!(a[pos("--allowedTools") + 1], "mcp__aiframes__*");
        assert_eq!(a[pos("--model") + 1], "sonnet");
        assert!(a.contains(&"--strict-mcp-config".to_string()));
        // The variadic --allowedTools comes last so it cannot swallow the prompt.
        assert_eq!(a.last().unwrap(), "mcp__aiframes__*");
        let b = claude_args("x", Path::new("c.json"), "", false);
        assert!(!b.contains(&"--model".to_string()) && !b.contains(&"--strict-mcp-config".to_string()));
    }

    #[test]
    fn prompt_has_context() {
        let cx = RunContext {
            port: Some(1),
            project_id: 7,
            project_path: Some(PathBuf::from("C:/v/edit.aifr")),
            title: "edit.aifr".into(),
            playhead: 12.5,
            timecode: "00:00:12:15".into(),
            selection: vec!["c3".into()],
            in_point: Some(2.0),
            out_point: None,
            duration: 60.0,
        };
        let p = compose_prompt(&cx, "  make it punchier ", &["c3".into(), "c9".into()]);
        assert!(p.contains("edit.aifr"));
        assert!(p.contains("12.500 s (00:00:12:15)"));
        assert!(p.contains("Selected: c3"));
        assert!(p.contains("About: c9"));
        assert!(p.contains("In/out: 2.000 s .. -"));
        assert!(p.contains("Project id: 7") && p.contains("\"project\": 7"));
        assert!(p.ends_with("make it punchier\n"));
    }

    #[test]
    fn parses_stream_json() {
        let init = r#"{"type":"system","subtype":"init","model":"claude-x","mcp_servers":[{"name":"aiframes","status":"connected"}],"tools":[]}"#;
        assert_eq!(parse_line(init), vec![AiEvent::Init { model: "claude-x".into(), mcp: vec![("aiframes".into(), "connected".into())] }]);
        let a = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looking."},{"type":"tool_use","id":"t1","name":"mcp__aiframes__edit","input":{"label":"Tighten","ops":[{"op":"trim_clip","clip":"c3","edge":"end","delta":-1},{"op":"add_marker","time":3}]}}]}}"#;
        let ev = parse_line(a);
        assert_eq!(ev[0], AiEvent::Text("Looking.".into()));
        assert_eq!(ev[1], AiEvent::ToolUse { name: "edit".into(), summary: "\"Tighten\": trim_clip c3, add_marker".into() });
        let u = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"ok: revision 4\nmore"},{"type":"image","source":{}}]}]}}"#;
        assert_eq!(parse_line(u), vec![AiEvent::ToolResult { ok: true, summary: "ok: revision 4 [image]".into() }]);
        let e = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":"unknown clip c99","is_error":true}]}}"#;
        assert_eq!(parse_line(e), vec![AiEvent::ToolResult { ok: false, summary: "unknown clip c99".into() }]);
        let r = r#"{"type":"result","subtype":"success","is_error":false,"result":"Trimmed.","total_cost_usd":0.0123,"num_turns":3,"duration_ms":4200}"#;
        assert_eq!(parse_line(r), vec![AiEvent::Done { ok: true, text: "Trimmed.".into(), cost: Some(0.0123), turns: Some(3), ms: Some(4200) }]);
        assert!(parse_line("").is_empty());
        assert!(matches!(parse_line("not json")[0], AiEvent::Stderr(_)));
        assert!(parse_line(r#"{"type":"stream_event"}"#).is_empty());
    }

    #[test]
    fn receipts_and_net_changes() {
        let u = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"{\"changed\":[\"c3\"],\"created\":[\"c9\",\"mk1\"],\"revision\":4}"}]}]}}"#;
        let ev = parse_line(u);
        assert!(matches!(&ev[0], AiEvent::ToolResult { ok: true, .. }));
        assert_eq!(ev[1], AiEvent::Receipt { created: vec!["c9".into(), "mk1".into()], changed: vec!["c3".into()], removed: vec![] });
        let plan = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"{\"plan\":true,\"changed\":[\"c3\"]}"}]}}"#;
        assert_eq!(parse_line(plan).len(), 1, "a dry run is not a change");
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let [added, changed, removed, temp] = net_changes(&s(&["c9", "p1", "p2"]), &s(&["c3", "c9", "p1"]), &s(&["p1", "p2", "c4"]));
        assert_eq!(added, s(&["c9"]));
        assert_eq!(changed, s(&["c3"]));
        assert_eq!(removed, s(&["c4"]));
        assert_eq!(temp, s(&["p1", "p2"]), "probe clips the AI added and removed again");
    }

    #[test]
    fn summaries() {
        assert_eq!(summarize_input(&serde_json::json!({})), "");
        assert_eq!(summarize_input(&serde_json::json!({"at":[1,2]})), r#"{"at":[1,2]}"#);
        let long = "x".repeat(500);
        assert!(summarize_input(&serde_json::json!({ "t": long })).chars().count() <= 161);
        assert_eq!(connect_command("C:\\a\\aiframes.exe"), "claude mcp add aiframes -- \"C:\\a\\aiframes.exe\" mcp");
    }
}
