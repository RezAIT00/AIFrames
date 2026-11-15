//! JSON-RPC 2.0 / MCP message handling, generic over an app [`Backend`].

use crate::AppSpec;
use serde_json::{json, Value as J};
use std::path::Path;
use std::sync::Arc;

pub const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
pub const LATEST_VERSION: &str = "2025-06-18";

/// Key a tool result uses to return a base64 PNG (sent as MCP image content).
pub const IMAGE_KEY: &str = "__image_png_base64";

/// A headless document that can be handed over to a GUI window by `launch_gui`.
pub struct Handoff {
    pub id: u64,
    pub title: String,
    /// Saved file with no unsaved changes (reopened by path); otherwise it is saved to a temp file.
    pub clean_path: Option<std::path::PathBuf>,
}

/// What an app provides to the MCP framework.
pub trait Backend: Send + Sync + 'static {
    fn spec(&self) -> &'static AppSpec;
    /// The `instructions` string returned by initialize. `mode` describes live/headless operation.
    fn instructions(&self, mode: &str) -> String;
    /// Tool definitions (`{"name","title","description","inputSchema",...}`).
    fn tools(&self, include_launch: bool) -> Vec<J>;
    /// Run a tool. Errors become `isError: true` results.
    fn call(&self, name: &str, args: &J) -> Result<J, String>;
    /// Run a tool for one client session: `session` is the MCP session id (HTTP `Mcp-Session-Id`, or
    /// the id a stdio server uses for its client), `None` when the transport has none. Backends that
    /// keep per-client state (e.g. "the document this client is working on") override this; the
    /// default ignores the session.
    fn call_in(&self, session: Option<&str>, name: &str, args: &J) -> Result<J, String> {
        let _ = session;
        self.call(name, args)
    }
    /// The client of a session introduced itself (initialize `clientInfo`: `name`, `title?`,
    /// `version`), e.g. to label its edits. Default: ignored.
    fn client_info(&self, session: Option<&str>, info: &J) {
        let _ = (session, info);
    }
    /// How long a stdio proxy should wait for a window to answer this call, for tools that can
    /// legitimately take longer than the default (300 s, or the call's `wait_seconds` + a margin).
    /// Default: no opinion.
    fn proxy_timeout(&self, name: &str, args: &J) -> Option<std::time::Duration> {
        let _ = (name, args);
        None
    }
    fn resources(&self) -> Vec<J> {
        Vec::new()
    }
    fn read_resource(&self, uri: &str) -> Result<J, String> {
        Err(format!("unknown resource '{uri}'"))
    }
    fn document_count(&self) -> usize;
    /// Headless servers have no activity log: drop queued notices.
    fn discard_notices(&self) {}
    /// Documents to move into a GUI window.
    fn handoff_list(&self) -> Vec<Handoff>;
    /// Save document `id` (with unsaved changes) to `path` so a window can open it.
    fn handoff_save(&self, id: u64, path: &Path) -> Result<(), String>;
    /// File extension used for hand-off temp files ("xlsx", "pptx").
    fn handoff_ext(&self) -> &'static str;
    fn handoff_remove(&self, id: u64);
}

pub fn rpc_result(id: J, result: J) -> J {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn rpc_error(id: J, code: i64, message: &str) -> J {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Wrap a tool outcome as an MCP CallToolResult.
pub fn tool_result(r: Result<J, String>) -> J {
    match r {
        Ok(mut v) if v.get(IMAGE_KEY).is_some() => {
            let data = v.as_object_mut().and_then(|o| o.remove(IMAGE_KEY)).unwrap_or(J::Null);
            let text = serde_json::to_string(&v).unwrap_or_default();
            json!({"content": [{"type": "image", "data": data, "mimeType": "image/png"}, {"type": "text", "text": text}], "isError": false})
        }
        Ok(v) => {
            let text = {
                let pretty = serde_json::to_string_pretty(&v).unwrap_or_default();
                if pretty.len() > 20_000 {
                    serde_json::to_string(&v).unwrap_or_default()
                } else {
                    pretty
                }
            };
            let structured = if v.is_object() { v } else { json!({"result": v}) };
            json!({"content": [{"type": "text", "text": text}], "structuredContent": structured, "isError": false})
        }
        Err(e) => json!({"content": [{"type": "text", "text": format!("Error: {e}")}], "isError": true}),
    }
}

/// An MCP server bound to an app backend.
pub struct Server<B: Backend> {
    pub backend: Arc<B>,
    /// True when serving a live GUI (notices are kept for its activity log).
    pub live: bool,
    /// Whether to advertise the stdio-only `launch_gui` tool.
    pub launch_tool: bool,
}

pub fn live_mode(spec: &AppSpec) -> String {
    format!("live in the running {} window — the user sees every change as you make it", spec.name)
}

pub fn headless_mode(spec: &AppSpec) -> String {
    format!("headlessly for now (no {} window is open; call launch_gui to open one so the user can watch)", spec.name)
}

impl<B: Backend> Server<B> {
    pub fn new(backend: Arc<B>, live: bool) -> Self {
        Server { backend, live, launch_tool: false }
    }

    pub fn spec(&self) -> &'static AppSpec {
        self.backend.spec()
    }

    pub fn initialize_result(&self, params: &J, mode: &str) -> J {
        let requested = params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or(LATEST_VERSION);
        let version = if SUPPORTED_VERSIONS.contains(&requested) { requested } else { LATEST_VERSION };
        let spec = self.spec();
        json!({
            "protocolVersion": version,
            "capabilities": {
                "tools": {"listChanged": false},
                "resources": {"listChanged": false, "subscribe": false},
            },
            "serverInfo": {"name": spec.id, "title": spec.title, "version": spec.version},
            "instructions": self.backend.instructions(mode),
        })
    }

    fn mode(&self) -> String {
        if self.live {
            live_mode(self.spec())
        } else {
            headless_mode(self.spec())
        }
    }

    /// Execute one tool call and return a CallToolResult.
    pub fn call_tool(&self, name: &str, args: &J) -> J {
        self.call_tool_in(None, name, args)
    }

    /// [`Server::call_tool`] for a client session (see [`Backend::call_in`]).
    pub fn call_tool_in(&self, session: Option<&str>, name: &str, args: &J) -> J {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.backend.call_in(session, name, args)))
            .unwrap_or_else(|_| Err(format!("internal error while running '{name}'")));
        if !self.live {
            self.backend.discard_notices();
        }
        tool_result(r)
    }

    /// Handle one JSON-RPC message. Returns None for notifications and client responses.
    pub fn handle(&self, msg: &J) -> Option<J> {
        self.handle_in(None, msg)
    }

    /// [`Server::handle`] for a client session (see [`Backend::call_in`]).
    pub fn handle_in(&self, session: Option<&str>, msg: &J) -> Option<J> {
        let id = msg.get("id").cloned();
        let Some(method) = msg.get("method").and_then(|m| m.as_str()) else {
            if msg.get("result").is_some() || msg.get("error").is_some() {
                return None;
            }
            return Some(rpc_error(id.unwrap_or(J::Null), -32600, "Invalid Request"));
        };
        if method.starts_with("notifications/") {
            return None;
        }
        let id = id?;
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        let res: Result<J, (i64, String)> = match method {
            "initialize" => {
                if let Some(ci) = params.get("clientInfo").filter(|c| c.is_object()) {
                    self.backend.client_info(session, ci);
                }
                Ok(self.initialize_result(&params, &self.mode()))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.backend.tools(self.launch_tool)})),
            "tools/call" => match params.get("name").and_then(|n| n.as_str()) {
                Some(name) => Ok(self.call_tool_in(session, name, params.get("arguments").unwrap_or(&J::Null))),
                None => Err((-32602, "tools/call requires 'name'".into())),
            },
            "resources/list" => Ok(json!({"resources": self.backend.resources()})),
            "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
            "resources/read" => match params.get("uri").and_then(|u| u.as_str()) {
                Some(uri) => self.backend.read_resource(uri).map_err(|e| (-32002, e)),
                None => Err((-32602, "resources/read requires 'uri'".into())),
            },
            "prompts/list" => Ok(json!({"prompts": []})),
            "logging/setLevel" => Ok(json!({})),
            "completion/complete" => Ok(json!({"completion": {"values": [], "hasMore": false}})),
            other => Err((-32601, format!("Method not found: {other}"))),
        };
        Some(match res {
            Ok(r) => rpc_result(id, r),
            Err((c, m)) => rpc_error(id, c, &m),
        })
    }

    /// Handle a request body which may be a single message or a batch.
    pub fn handle_text(&self, body: &str) -> Option<String> {
        self.handle_text_in(None, body)
    }

    /// [`Server::handle_text`] for a client session (see [`Backend::call_in`]).
    pub fn handle_text_in(&self, session: Option<&str>, body: &str) -> Option<String> {
        let parsed: J = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(e) => return Some(rpc_error(J::Null, -32700, &format!("Parse error: {e}")).to_string()),
        };
        match parsed {
            J::Array(items) => {
                let out: Vec<J> = items.iter().filter_map(|m| self.handle_in(session, m)).collect();
                (!out.is_empty()).then(|| J::Array(out).to_string())
            }
            m => self.handle_in(session, &m).map(|r| r.to_string()),
        }
    }
}
