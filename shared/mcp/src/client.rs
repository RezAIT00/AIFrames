//! Minimal HTTP/1.1 client for talking to a local app window (no external dependencies).

use serde_json::{json, Value as J};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn decode_chunked(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(pos) = data.windows(2).position(|w| w == b"\r\n") {
        let size_str = String::from_utf8_lossy(&data[..pos]);
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("0").trim(), 16).unwrap_or(0);
        data = &data[pos + 2..];
        if size == 0 || data.len() < size {
            break;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[(size + 2).min(data.len())..];
    }
    out
}

/// Perform one request against `127.0.0.1:port` (connection closed after the response).
pub fn request(port: u16, method: &str, path: &str, body: Option<&str>, extra_headers: &[(&str, &str)], timeout: Duration) -> Result<HttpResponse, String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(800)).map_err(|e| format!("cannot connect on port {port}: {e}"))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
    let body = body.unwrap_or("");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nAccept: application/json, text/event-stream\r\n");
    if !body.is_empty() || method == "POST" {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
    }
    for (k, v) in extra_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).and_then(|_| stream.write_all(body.as_bytes())).map_err(|e| format!("write failed: {e}"))?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| format!("read failed: {e}"))?;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("malformed HTTP response")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut body_bytes = raw[split + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or("malformed status line")?;
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    let resp_tmp = HttpResponse { status, headers, body: String::new() };
    if resp_tmp.header("Transfer-Encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        body_bytes = decode_chunked(&body_bytes);
    } else if let Some(len) = resp_tmp.header("Content-Length").and_then(|v| v.parse::<usize>().ok()) {
        body_bytes.truncate(len);
    }
    Ok(HttpResponse { body: String::from_utf8_lossy(&body_bytes).to_string(), ..resp_tmp })
}

/// True if an MCP endpoint of app `app_id` answers on `port`.
pub fn health(port: u16, app_id: &str) -> bool {
    match request(port, "GET", "/health", None, &[], Duration::from_secs(2)) {
        Ok(r) if r.status == 200 => serde_json::from_str::<J>(&r.body).ok().and_then(|j| j.get("app").and_then(|a| a.as_str()).map(|a| a == app_id)).unwrap_or(false),
        _ => false,
    }
}

/// POST a JSON-RPC message to `/mcp` and return the parsed response (None for 202 Accepted).
pub fn post_mcp(port: u16, msg: &J, timeout: Duration) -> Result<Option<J>, String> {
    post_mcp_with(port, msg, timeout, &[])
}

/// [`post_mcp`] with extra request headers (e.g. `Mcp-Session-Id`).
pub fn post_mcp_with(port: u16, msg: &J, timeout: Duration, headers: &[(&str, &str)]) -> Result<Option<J>, String> {
    let r = request(port, "POST", "/mcp", Some(&msg.to_string()), headers, timeout)?;
    match r.status {
        202 | 204 => Ok(None),
        200 => {
            let body = r.body.trim();
            let json_text = if body.starts_with("event:") || body.starts_with("data:") {
                body.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim).collect::<Vec<_>>().join("")
            } else {
                body.to_string()
            };
            serde_json::from_str(&json_text).map(Some).map_err(|e| format!("invalid JSON from the app window: {e}"))
        }
        s => Err(format!("the app window returned HTTP {s}: {}", r.body)),
    }
}

/// Call a tool on a running window. Ok(result) is the CallToolResult object.
pub fn call_tool(port: u16, name: &str, args: J, timeout: Duration) -> Result<J, String> {
    call_tool_with(port, name, args, timeout, &[])
}

/// [`call_tool`] with extra request headers (e.g. `Mcp-Session-Id`).
pub fn call_tool_with(port: u16, name: &str, args: J, timeout: Duration, headers: &[(&str, &str)]) -> Result<J, String> {
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}});
    let resp = post_mcp_with(port, &msg, timeout, headers)?.ok_or("no response")?;
    if let Some(e) = resp.get("error") {
        return Err(e.get("message").and_then(|m| m.as_str()).unwrap_or("error").to_string());
    }
    resp.get("result").cloned().ok_or_else(|| "missing result".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunked() {
        assert_eq!(decode_chunked(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"), b"hello world");
    }
}
