//! `<app> mcp`: newline-delimited JSON-RPC over stdin/stdout.
//!
//! If a window of the app is running, tool calls are proxied to it so the AI edits what the user
//! sees; otherwise documents live in an in-process headless backend. `launch_gui` starts a window
//! and moves headless documents into it.

use crate::protocol::{headless_mode, live_mode, rpc_error, tool_result, Backend, Server};
use crate::{client, instance};
use serde_json::{json, Value as J};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PROXY_TIMEOUT: Duration = Duration::from_secs(300);
/// Added to the wait a call asks for (`wait_seconds`, ...) before the proxy stops waiting for it.
const WAIT_MARGIN: Duration = Duration::from_secs(60);
/// The longest the proxy waits for one call.
const MAX_PROXY_TIMEOUT: Duration = Duration::from_secs(3600);
/// Arguments (seconds) by which a tool call asks to wait for a result.
const WAIT_ARGS: &[&str] = &["wait_seconds", "wait", "timeout_seconds"];

pub struct StdioServer<B: Backend> {
    local: Server<B>,
    proxy: Option<u16>,
    /// How long a proxied call may take at least (a call's own wait and the backend's
    /// [`Backend::proxy_timeout`] can make it longer).
    proxy_timeout: Duration,
    last_probe: Instant,
    auto_probe: bool,
    /// This client's session id: sent to the window as `Mcp-Session-Id` when proxying and passed to
    /// the headless backend, so per-client state (e.g. the current document) survives both.
    session: String,
    /// The client's name from initialize `clientInfo` (sent to the window as `Mcp-Client-Name`).
    client_name: Option<String>,
}

impl<B: Backend> StdioServer<B> {
    /// `headless` is the backend used when no window is running.
    pub fn new(headless: Arc<B>) -> Self {
        let spec = headless.spec();
        let mut local = Server::new(headless, false);
        local.launch_tool = true;
        StdioServer { local, proxy: instance::live_instance_port(spec), proxy_timeout: PROXY_TIMEOUT, last_probe: Instant::now(), auto_probe: true, session: new_session(), client_name: None }
    }

    /// Headless-only server (never proxies). Used by tests.
    pub fn headless(backend: Arc<B>) -> Self {
        let mut local = Server::new(backend, false);
        local.launch_tool = true;
        StdioServer { local, proxy: None, proxy_timeout: PROXY_TIMEOUT, last_probe: Instant::now(), auto_probe: false, session: new_session(), client_name: None }
    }

    pub fn is_proxying(&self) -> bool {
        self.proxy.is_some()
    }

    pub fn proxy_port(&self) -> Option<u16> {
        self.proxy
    }

    /// The session id this server uses for its client (headless calls and `Mcp-Session-Id` when proxying).
    pub fn session_id(&self) -> &str {
        &self.session
    }

    /// The shortest time a proxied call may take before the proxy stops waiting (default 300 s).
    pub fn set_proxy_timeout(&mut self, t: Duration) {
        self.proxy_timeout = t;
    }

    /// How long to wait for the window to answer this call: at least the base timeout, the call's
    /// own requested wait plus a margin, and what the backend says the tool may need.
    fn call_timeout(&self, msg: &J) -> Duration {
        let name = msg.pointer("/params/name").and_then(|n| n.as_str()).unwrap_or("");
        let args = msg.pointer("/params/arguments").cloned().unwrap_or(J::Null);
        let wait = WAIT_ARGS
            .iter()
            .filter_map(|k| args.get(*k))
            .filter_map(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
            .filter(|w| w.is_finite() && *w > 0.0)
            .fold(0.0f64, f64::max)
            .min(MAX_PROXY_TIMEOUT.as_secs_f64());
        let mut t = self.proxy_timeout;
        if wait > 0.0 {
            t = t.max(Duration::from_secs_f64(wait) + WAIT_MARGIN);
        }
        if let Some(b) = self.local.backend.proxy_timeout(name, &args) {
            t = t.max(b);
        }
        t.min(MAX_PROXY_TIMEOUT)
    }

    /// Whether the window still answers (`/health`, a few tries): a slow call is not a dead window.
    fn window_alive(&self, port: u16) -> bool {
        let id = self.spec().id;
        for i in 0..3 {
            if client::health(port, id) {
                return true;
            }
            if i < 2 {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        false
    }

    /// Headers identifying this client to the window it proxies to.
    fn headers(&self) -> Vec<(&str, &str)> {
        let mut h = vec![("Mcp-Session-Id", self.session.as_str())];
        if let Some(n) = &self.client_name {
            h.push(("Mcp-Client-Name", n.as_str()));
        }
        h
    }

    fn spec(&self) -> &'static crate::AppSpec {
        self.local.spec()
    }

    /// Re-check for a window when we have no headless documents of our own.
    fn maybe_probe(&mut self) {
        if self.auto_probe && self.proxy.is_none() && self.local.backend.document_count() == 0 && self.last_probe.elapsed() > Duration::from_secs(2) {
            self.last_probe = Instant::now();
            self.proxy = instance::live_instance_port(self.spec());
        }
    }

    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let parsed: J = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Some(rpc_error(J::Null, -32700, &format!("Parse error: {e}")).to_string()),
        };
        match parsed {
            J::Array(items) => {
                let out: Vec<J> = items.iter().filter_map(|m| self.handle(m)).collect();
                (!out.is_empty()).then(|| J::Array(out).to_string())
            }
            m => self.handle(&m).map(|r| r.to_string()),
        }
    }

    pub fn handle(&mut self, msg: &J) -> Option<J> {
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let spec = self.spec();
        match method {
            "initialize" => {
                let id = msg.get("id").cloned()?;
                let mode = if self.proxy.is_some() { live_mode(spec) } else { headless_mode(spec) };
                let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                if let Some(ci) = params.get("clientInfo").filter(|c| c.is_object()) {
                    self.local.backend.client_info(Some(&self.session), ci);
                    let name = ci.get("title").or_else(|| ci.get("name")).and_then(|n| n.as_str()).unwrap_or("");
                    let name: String = name.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').take(80).collect();
                    self.client_name = (!name.trim().is_empty()).then(|| name.trim().to_string());
                }
                Some(json!({"jsonrpc": "2.0", "id": id, "result": self.local.initialize_result(&params, &mode)}))
            }
            "tools/call" => {
                let id = msg.get("id").cloned()?;
                let name = msg.pointer("/params/name").and_then(|n| n.as_str()).unwrap_or("");
                if name == "launch_gui" {
                    return Some(json!({"jsonrpc": "2.0", "id": id, "result": self.launch_gui()}));
                }
                self.maybe_probe();
                if let Some(port) = self.proxy {
                    let timeout = self.call_timeout(msg);
                    match client::post_mcp_with(port, msg, timeout, &self.headers()) {
                        Ok(Some(resp)) => return Some(resp),
                        Ok(None) => return None,
                        Err(e) if self.window_alive(port) => {
                            // A slow call (or a lost connection) is not a dead window: stay connected.
                            let text = format!(
                                "The {app} window did not answer this {name} call within {secs} s ({e}), but the window is running and stays connected: the call may still be finishing there. Check its result before repeating it (e.g. the jobs tool for long work), and pass a smaller wait_seconds to poll instead of waiting long.",
                                app = spec.name,
                                secs = timeout.as_secs()
                            );
                            return Some(json!({"jsonrpc": "2.0", "id": id, "result": tool_result(Err(text))}));
                        }
                        Err(e) => {
                            self.proxy = None;
                            self.last_probe = Instant::now();
                            let text = format!(
                                "The {name} window stopped responding ({e}) and no longer answers its health check. Switched to headless mode: {noun}s that were open in the window are not available here. Call launch_gui to open {name} again, or {open} to work on files headlessly.",
                                name = spec.name,
                                noun = spec.doc_noun,
                                open = spec.open_tool
                            );
                            return Some(json!({"jsonrpc": "2.0", "id": id, "result": tool_result(Err(text))}));
                        }
                    }
                }
                self.local.handle_in(Some(&self.session), msg)
            }
            "resources/list" | "resources/read" => {
                if let Some(port) = self.proxy {
                    if let Ok(Some(resp)) = client::post_mcp_with(port, msg, self.proxy_timeout, &self.headers()) {
                        return Some(resp);
                    }
                }
                self.local.handle_in(Some(&self.session), msg)
            }
            _ => self.local.handle_in(Some(&self.session), msg),
        }
    }

    fn launch_gui(&mut self) -> J {
        let spec = self.spec();
        if let Some(p) = self.proxy {
            if client::health(p, spec.id) {
                return tool_result(Ok(json!({"status": "already_connected", "port": p})));
            }
        }
        let port = match instance::live_instance_port(spec) {
            Some(p) => p,
            None => {
                if let Err(e) = spawn_gui(spec) {
                    return tool_result(Err(format!("could not start {}: {e}", spec.name)));
                }
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    if let Some(p) = instance::live_instance_port(spec) {
                        break p;
                    }
                    if Instant::now() > deadline {
                        return tool_result(Err(format!("{} was started but its window did not come up within 30 seconds", spec.name)));
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        };
        let (moved, failed) = self.migrate(port);
        self.proxy = Some(port);
        let mut res = json!({"status": "connected", "port": port});
        res[format!("moved_{}s", spec.doc_noun)] = json!(moved);
        if !failed.is_empty() {
            res["not_moved"] = json!(failed);
        }
        tool_result(Ok(res))
    }

    /// Move headless documents into the GUI (reopen saved files, hand off unsaved ones via a temp file).
    fn migrate(&mut self, port: u16) -> (Vec<J>, Vec<J>) {
        let spec = self.spec();
        let backend = self.local.backend.clone();
        let session = self.session.clone();
        let mut moved = Vec::new();
        let mut failed = Vec::new();
        for h in backend.handoff_list() {
            let (path, note) = match &h.clean_path {
                Some(p) => (Ok(p.clone()), None),
                None => {
                    let dir = std::env::temp_dir().join(format!("{}-handoff-{}", spec.id, std::process::id()));
                    let _ = std::fs::create_dir_all(&dir);
                    let stem: String = PathBuf::from(&h.title)
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "Document".into())
                        .chars()
                        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' { c } else { '_' })
                        .collect();
                    let p = dir.join(format!("{stem}.{}", backend.handoff_ext()));
                    let r = backend.handoff_save(h.id, &p).map(|_| p);
                    (r, Some(format!("unsaved changes were handed over through a temporary file; save it to a real location with {}(path)", spec.save_tool)))
                }
            };
            let result = path.and_then(|p| {
                let r = client::call_tool_with(port, spec.open_tool, json!({"path": p.to_string_lossy(), "show": true}), PROXY_TIMEOUT, &[("Mcp-Session-Id", &session)])?;
                if r.get("isError").and_then(|v| v.as_bool()).unwrap_or(false) {
                    Err(r.pointer("/content/0/text").and_then(|t| t.as_str()).unwrap_or("open failed").to_string())
                } else {
                    Ok(p)
                }
            });
            match result {
                Ok(p) => {
                    backend.handoff_remove(h.id);
                    let mut j = json!({"title": h.title, "path": p.to_string_lossy()});
                    if let Some(n) = note {
                        j["note"] = json!(n);
                    }
                    moved.push(j);
                }
                Err(e) => failed.push(json!({"title": h.title, "error": e})),
            }
        }
        (moved, failed)
    }
}

/// A process-unique session id for a stdio client (`stdio-<pid>-<n>-<hash>`).
fn new_session() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("stdio-{}-{n}-{:x}", std::process::id(), (t as u64) ^ (n << 48))
}

fn spawn_gui(spec: &crate::AppSpec) -> Result<(), String> {
    let exe = match std::env::var(spec.env("GUI_EXE")) {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => std::env::current_exe().map_err(|e| e.to_string())?,
    };
    let make = |flags: u32| {
        let mut cmd = std::process::Command::new(&exe);
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(flags);
        }
        #[cfg(not(windows))]
        let _ = flags;
        cmd
    };
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    match make(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Ok(_) => Ok(()),
        Err(_) => make(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP).spawn().map(|_| ()).map_err(|e| format!("{}: {e}", exe.display())),
    }
}

/// Run a stdio server until stdin closes. Logs go to stderr only.
pub fn run_stdio<B: Backend>(headless: Arc<B>) {
    let spec = headless.spec();
    let mut server = StdioServer::new(headless);
    match server.proxy {
        Some(p) => eprintln!("{} mcp: connected to the running {} window on port {p}", spec.id, spec.name),
        None => eprintln!("{} mcp: no {} window running; working headlessly", spec.id, spec.name),
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // Some clients (e.g. .NET) prefix the stream with a UTF-8 byte-order mark.
        let t = line.trim().trim_start_matches('\u{feff}').trim();
        if t.is_empty() {
            continue;
        }
        if let Some(out) = server.handle_line(t) {
            if writeln!(stdout, "{out}").and_then(|_| stdout.flush()).is_err() {
                break;
            }
        }
    }
}
