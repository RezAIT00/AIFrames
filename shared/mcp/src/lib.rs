//! Shared MCP server framework for the suite's apps.
//!
//! An app implements [`Backend`] (its tool schemas + tool dispatch over its document hub) and
//! describes itself with an [`AppSpec`]. The framework provides:
//! * [`protocol::Server`] – JSON-RPC 2.0 / MCP handling (initialize, tools, resources, batches)
//! * [`http::start_http_on`] – Streamable-HTTP endpoint (`/mcp`, `/health`) for a running GUI,
//!   localhost-only with Origin/Host checks, advertised via an instance file
//! * [`stdio::StdioServer`] / [`stdio::run_stdio`] – `<app> mcp`: proxies to a running window when
//!   there is one, otherwise headless, with a `launch_gui` hand-off
//! * [`forward_open`] – single-instance file opening

pub mod client;
pub mod http;
pub mod instance;
pub mod protocol;
pub mod stdio;

pub use protocol::{tool_result, Backend, Handoff, Server, IMAGE_KEY};

use std::path::PathBuf;
use std::time::Duration;

/// Static description of an app for the MCP framework.
#[derive(Clone, Copy, Debug)]
pub struct AppSpec {
    /// Lower-case id: `/health` "app" field, instance dir (`%LOCALAPPDATA%\<Name>`), env prefix.
    pub id: &'static str,
    /// Display name ("AIFrames").
    pub name: &'static str,
    /// serverInfo title ("AIFrames Video Editor").
    pub title: &'static str,
    pub version: &'static str,
    /// First HTTP port tried by the GUI (the next 20 are tried if busy).
    pub default_port: u16,
    /// What a document is called in messages ("workbook", "presentation").
    pub doc_noun: &'static str,
    /// Tool used to open a file in a running window (`open_workbook`, `open_presentation`).
    pub open_tool: &'static str,
    /// Tool used to save (`save_workbook`).
    pub save_tool: &'static str,
}

impl AppSpec {
    /// Environment variable name with the app prefix, e.g. `AIFRAMES_INSTANCE_DIR`.
    pub fn env(&self, suffix: &str) -> String {
        format!("{}_{suffix}", self.id.to_ascii_uppercase())
    }
}

/// Standard base64 (RFC 4648) encoding (for image tool results).
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// If a window of this app is already running, ask it to open `paths` and return true.
/// Returns false when no live instance is reachable (the caller should start its own window).
pub fn forward_open(spec: &AppSpec, paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }
    let Some(port) = instance::live_instance_port(spec) else { return false };
    for p in paths {
        let abs = std::fs::canonicalize(p).unwrap_or_else(|_| {
            if p.is_absolute() {
                p.clone()
            } else {
                std::env::current_dir().map(|c| c.join(p)).unwrap_or_else(|_| p.clone())
            }
        });
        let mut s = abs.to_string_lossy().to_string();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            s = stripped.to_string();
        }
        if client::call_tool(port, spec.open_tool, serde_json::json!({"path": s, "show": true}), Duration::from_secs(120)).is_err() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn b64() {
        assert_eq!(super::base64(b"hello"), "aGVsbG8=");
        assert_eq!(super::base64(b"hi"), "aGk=");
        assert_eq!(super::base64(b"abc"), "YWJj");
    }
}
