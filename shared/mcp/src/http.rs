//! Streamable-HTTP MCP endpoint for a running app window (JSON responses, localhost only).

use crate::instance;
use crate::protocol::{Backend, Server};
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tiny_http::{Header, Method, Request, Response};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);
const MAX_BODY: u64 = 64 * 1024 * 1024;

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn new_session_id() -> String {
    let n = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut h: u64 = 0xcbf29ce484222325;
    for b in format!("{t}-{n}-{}", std::process::id()).bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}{n:08x}")
}

fn host_of(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    if let Some(rest) = s.strip_prefix('[') {
        return format!("[{}]", rest.split(']').next().unwrap_or(""));
    }
    s.split(':').next().unwrap_or("").to_string()
}

fn is_local_host(h: &str) -> bool {
    matches!(h, "localhost" | "127.0.0.1" | "[::1]")
}

/// DNS-rebinding protection: browser Origins must be localhost.
pub fn origin_allowed(origin: &str) -> bool {
    let o = origin.trim().to_ascii_lowercase();
    let rest = match o.strip_prefix("http://").or_else(|| o.strip_prefix("https://")) {
        Some(r) => r,
        None => return false,
    };
    is_local_host(&host_of(rest.split('/').next().unwrap_or("")))
}

fn get_header<'a>(req: &'a Request, name: &'static str) -> Option<&'a str> {
    req.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str())
}

fn json_response(status: u16, body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body).with_status_code(status).with_header(header("Content-Type", "application/json"))
}

fn handle_request<B: Backend>(server: &Server<B>, port: u16, mut req: Request) {
    if let Some(origin) = get_header(&req, "Origin") {
        if !origin_allowed(origin) {
            let _ = req.respond(json_response(403, r#"{"error":"forbidden origin"}"#.into()));
            return;
        }
    }
    if let Some(host) = get_header(&req, "Host") {
        if !is_local_host(&host_of(host)) {
            let _ = req.respond(json_response(403, r#"{"error":"forbidden host"}"#.into()));
            return;
        }
    }
    let path = req.url().split('?').next().unwrap_or("").to_string();
    let method = req.method().clone();
    let spec = server.spec();
    match (method, path.as_str()) {
        (Method::Get, "/health") | (Method::Get, "/") => {
            let body = serde_json::json!({
                "app": spec.id,
                "version": spec.version,
                "pid": std::process::id(),
                "port": port,
                "mcp": format!("http://127.0.0.1:{port}/mcp"),
                "documents": server.backend.document_count(),
            });
            let _ = req.respond(json_response(200, body.to_string()));
        }
        (Method::Post, "/mcp") => {
            let mut body = String::new();
            if req.as_reader().take(MAX_BODY).read_to_string(&mut body).is_err() {
                let _ = req.respond(json_response(400, r#"{"error":"unreadable body"}"#.into()));
                return;
            }
            let is_init = body.contains("\"initialize\"")
                && serde_json::from_str::<serde_json::Value>(&body).ok().is_some_and(|v| v.get("method").and_then(|m| m.as_str()) == Some("initialize"));
            // Clients echo the session id they got from initialize; backends may keep per-client state.
            // An initialize gets its new id up front, so the backend sees the session it starts.
            let given = get_header(&req, "Mcp-Session-Id").map(str::trim).filter(|s| !s.is_empty() && s.len() <= 128).map(str::to_string);
            let new_id = (is_init && given.is_none()).then(new_session_id);
            let session = given.or_else(|| new_id.clone());
            // A stdio proxy forwards its client's name (it handled initialize itself).
            if let (Some(s), Some(name)) = (session.as_deref(), get_header(&req, "Mcp-Client-Name")) {
                let name: String = name.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').take(80).collect();
                if !name.trim().is_empty() {
                    server.backend.client_info(Some(s), &serde_json::json!({"name": name.trim()}));
                }
            }
            match server.handle_text_in(session.as_deref(), &body) {
                Some(out) => {
                    let mut resp = json_response(200, out);
                    if is_init {
                        let id = session.clone().unwrap_or_else(new_session_id);
                        resp = resp.with_header(header("Mcp-Session-Id", &id));
                    }
                    let _ = req.respond(resp);
                }
                None => {
                    let _ = req.respond(Response::empty(202));
                }
            }
        }
        (Method::Get, "/mcp") => {
            let _ = req.respond(Response::from_string("SSE streams are not offered; POST JSON-RPC messages to this endpoint.").with_status_code(405).with_header(header("Allow", "POST, DELETE")));
        }
        (Method::Delete, "/mcp") => {
            let _ = req.respond(Response::empty(200));
        }
        (Method::Options, _) => {
            let _ = req.respond(Response::empty(204).with_header(header("Allow", "GET, POST, DELETE, OPTIONS")));
        }
        _ => {
            let _ = req.respond(json_response(404, r#"{"error":"not found"}"#.into()));
        }
    }
}

/// Bind the first free port in `first..first+attempts` (port 0 = any free port) and serve MCP for
/// `backend` on a background thread. Optionally advertise it in the instance file.
pub fn start_http_on<B: Backend>(backend: Arc<B>, first: u16, attempts: u16, advertise: bool) -> std::io::Result<u16> {
    let spec = backend.spec();
    let mut last_err = None;
    for k in 0..attempts.max(1) {
        let port = if first == 0 { 0 } else { first.saturating_add(k) };
        match tiny_http::Server::http(("127.0.0.1", port)) {
            Ok(srv) => {
                let bound = srv.server_addr().to_ip().map(|a| a.port()).unwrap_or(port);
                let server = Arc::new(Server::new(backend, true));
                let srv = Arc::new(srv);
                std::thread::Builder::new().name(format!("{}-mcp-http", spec.id)).spawn(move || {
                    for req in srv.incoming_requests() {
                        let server = server.clone();
                        let _ = std::thread::Builder::new().name("mcp-req".into()).spawn(move || handle_request(&server, bound, req));
                    }
                })?;
                if advertise {
                    if let Err(e) = instance::write_instance(spec, bound) {
                        eprintln!("{}: could not write MCP instance file: {e}", spec.id);
                    }
                }
                return Ok(bound);
            }
            Err(e) => last_err = Some(e.to_string()),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, last_err.unwrap_or_else(|| "no free port".into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn origins() {
        assert!(origin_allowed("http://localhost:3000"));
        assert!(origin_allowed("http://127.0.0.1"));
        assert!(origin_allowed("https://[::1]:8080"));
        assert!(!origin_allowed("http://evil.com"));
        assert!(!origin_allowed("http://localhost.evil.com"));
        assert!(!origin_allowed("null"));
    }

    /// A backend that reports the session it was called in (and one that only implements `call`).
    struct Echo(bool, std::sync::Mutex<Vec<(Option<String>, String)>>);
    fn echo(sessions: bool) -> Arc<Echo> {
        Arc::new(Echo(sessions, std::sync::Mutex::new(Vec::new())))
    }
    static SPEC: crate::AppSpec = crate::AppSpec {
        id: "echo",
        name: "Echo",
        title: "Echo",
        version: "0",
        default_port: 0,
        doc_noun: "doc",
        open_tool: "open",
        save_tool: "save",
    };
    impl Backend for Echo {
        fn spec(&self) -> &'static crate::AppSpec {
            &SPEC
        }
        fn instructions(&self, _: &str) -> String {
            String::new()
        }
        fn tools(&self, _: bool) -> Vec<serde_json::Value> {
            Vec::new()
        }
        fn call(&self, _: &str, _: &serde_json::Value) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({"session": null}))
        }
        fn call_in(&self, session: Option<&str>, name: &str, args: &serde_json::Value) -> Result<serde_json::Value, String> {
            if name == "slow" {
                std::thread::sleep(std::time::Duration::from_secs_f64(args["sleep"].as_f64().unwrap_or(0.0)));
            }
            if self.0 {
                Ok(serde_json::json!({ "session": session }))
            } else {
                self.call(name, args)
            }
        }
        fn document_count(&self) -> usize {
            0
        }
        fn handoff_list(&self) -> Vec<crate::Handoff> {
            Vec::new()
        }
        fn handoff_save(&self, _: u64, _: &std::path::Path) -> Result<(), String> {
            Ok(())
        }
        fn handoff_ext(&self) -> &'static str {
            "x"
        }
        fn handoff_remove(&self, _: u64) {}
        fn client_info(&self, session: Option<&str>, info: &serde_json::Value) {
            self.1.lock().unwrap().push((session.map(String::from), info["name"].as_str().unwrap_or("").to_string()));
        }
    }

    #[test]
    fn sessions_reach_the_backend() {
        use std::time::Duration;
        let port = start_http_on(echo(true), 0, 1, false).unwrap();
        let t = Duration::from_secs(10);
        let with = crate::client::call_tool_with(port, "x", serde_json::json!({}), t, &[("Mcp-Session-Id", "abc")]).unwrap();
        assert_eq!(with["structuredContent"]["session"], "abc");
        let without = crate::client::call_tool(port, "x", serde_json::json!({}), t).unwrap();
        assert!(without["structuredContent"]["session"].is_null());
        // stdio: the headless server passes its own session id.
        let mut s = crate::stdio::StdioServer::headless(echo(true));
        let out = s.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"x","arguments":{}}}"#).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["result"]["structuredContent"]["session"], s.session_id());
        assert!(s.session_id().starts_with("stdio-"));
        // Backends that only implement `call` keep working.
        let port = start_http_on(echo(false), 0, 1, false).unwrap();
        let r = crate::client::call_tool_with(port, "x", serde_json::json!({}), t, &[("Mcp-Session-Id", "abc")]).unwrap();
        assert!(r["structuredContent"]["session"].is_null());
    }

    #[test]
    fn initialize_tells_the_backend_who_the_client_is() {
        use std::time::Duration;
        let b = echo(true);
        let port = start_http_on(b.clone(), 0, 1, false).unwrap();
        let init = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "claude-code", "version": "2.0"}}});
        let r = crate::client::request(port, "POST", "/mcp", Some(&init.to_string()), &[], Duration::from_secs(10)).unwrap();
        let sid = r.header("Mcp-Session-Id").unwrap().to_string();
        assert_eq!(b.1.lock().unwrap().last().cloned(), Some((Some(sid), "claude-code".to_string())), "the session it starts");
        // stdio: its own session, locally and (as Mcp-Client-Name) to a window.
        let mut s = crate::stdio::StdioServer::headless(b.clone());
        s.handle_line(&init.to_string()).unwrap();
        assert_eq!(b.1.lock().unwrap().last().cloned(), Some((Some(s.session_id().to_string()), "claude-code".to_string())));
        let r = crate::client::call_tool_with(port, "x", serde_json::json!({}), Duration::from_secs(10), &[("Mcp-Session-Id", "p1"), ("Mcp-Client-Name", "Claude Code")]).unwrap();
        assert_eq!(r["structuredContent"]["session"], "p1");
        assert_eq!(b.1.lock().unwrap().last().cloned(), Some((Some("p1".to_string()), "Claude Code".to_string())));
    }

    /// Proxy tests share the echo app's instance folder (ECHO_INSTANCE_DIR, so no real app is
    /// affected) and run one at a time.
    fn proxy_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("suite-mcp-echo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(SPEC.env("INSTANCE_DIR"), &dir);
        g
    }

    fn call_line(s: &mut crate::stdio::StdioServer<Echo>, name: &str, args: serde_json::Value) -> serde_json::Value {
        let msg = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}});
        serde_json::from_str::<serde_json::Value>(&s.handle_line(&msg.to_string()).unwrap()).unwrap()["result"].clone()
    }

    #[test]
    fn a_slow_call_does_not_disconnect_a_live_window() {
        let _g = proxy_lock();
        start_http_on(echo(true), 0, 1, true).unwrap();
        let mut s = crate::stdio::StdioServer::new(echo(false));
        assert!(s.is_proxying());
        s.set_proxy_timeout(std::time::Duration::from_secs(1));
        // Longer than the proxy waits: an error that says the window is alive, and it stays connected.
        let r = call_line(&mut s, "slow", serde_json::json!({"sleep": 3}));
        assert_eq!(r["isError"], true, "{r}");
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("stays connected") && !text.contains("headless"), "{text}");
        assert!(s.is_proxying(), "still proxying to the live window");
        let r = call_line(&mut s, "fast", serde_json::json!({}));
        assert_eq!(r["structuredContent"]["session"], s.session_id(), "the next call reaches the window");
        // A call that asks to wait longer gets that long (+ margin).
        let r = call_line(&mut s, "slow", serde_json::json!({"sleep": 2, "wait_seconds": 2}));
        assert_eq!(r["isError"], false, "{r}");
    }

    #[test]
    fn the_proxy_goes_headless_only_when_the_window_is_gone() {
        use std::io::{Read as _, Write as _};
        let _g = proxy_lock();
        // A window that answers its health check, then dies on the first tool call.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut st) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = st.read(&mut buf).unwrap_or(0);
                if String::from_utf8_lossy(&buf[..n]).starts_with("GET /health") {
                    let body = serde_json::json!({"app": "echo", "pid": 1, "port": port}).to_string();
                    let _ = write!(st, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                } else {
                    break; // crash: the connection and the listener close
                }
            }
        });
        crate::instance::write_instance(&SPEC, port).unwrap();
        let mut s = crate::stdio::StdioServer::new(echo(false));
        assert_eq!(s.proxy_port(), Some(port));
        let r = call_line(&mut s, "x", serde_json::json!({}));
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().contains("Switched to headless mode"), "{r}");
        assert!(!s.is_proxying());
    }

    #[test]
    fn stdio_proxy_sends_its_session_to_the_window() {
        let _g = proxy_lock();
        let port = start_http_on(echo(true), 0, 1, true).unwrap();
        let mut s = crate::stdio::StdioServer::new(echo(false));
        assert_eq!(s.proxy_port(), Some(port), "found the advertised window");
        let out = s.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"x","arguments":{}}}"#).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["result"]["structuredContent"]["session"], s.session_id(), "the window saw the stdio client's session");
    }
}
