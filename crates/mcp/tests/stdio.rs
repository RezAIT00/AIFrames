//! `aiframes mcp`-style JSON-RPC over the headless stdio server.

mod common;

use common::*;
use serde_json::{json, Value as J};

fn rpc(server: &mut suite_mcp::stdio::StdioServer<aiframes_mcp::AiframesBackend>, msg: J) -> J {
    let out = server.handle_line(&msg.to_string()).expect("a response");
    serde_json::from_str(&out).unwrap()
}

#[test]
fn json_rpc_lines() {
    let dir = scratch("stdio");
    let mut srv = aiframes_mcp::headless_stdio(services());
    let init = rpc(&mut srv, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}}));
    assert_eq!(init["result"]["serverInfo"]["name"], "aiframes");
    let instr = init["result"]["instructions"].as_str().unwrap();
    assert!(instr.contains("headlessly") && instr.contains("anchor") && instr.contains("CLIP-LOCAL"), "{instr}");
    assert!(srv.handle_line(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string()).is_none());

    let list = rpc(&mut srv, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for n in ["edit", "describe_project", "snapshot", "get_transcript", "remove_silences", "export", "launch_gui", "changes_since"] {
        assert!(names.contains(&n), "missing {n}");
    }
    for t in tools {
        assert!(t["description"].as_str().unwrap().len() > 20, "{} needs a description", t["name"]);
        assert_eq!(t["inputSchema"]["type"], "object");
    }
    let edit = tools.iter().find(|t| t["name"] == "edit").unwrap();
    assert!(edit["inputSchema"]["properties"]["plan"].is_object());

    let path = dir.join("rpc.aifr");
    let r = rpc(&mut srv, json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "new_project", "arguments": {"path": path_str(&path), "preset": "shorts"}}}));
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert_eq!(r["result"]["structuredContent"]["settings"]["height"], 1920);
    let r = rpc(&mut srv, json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "edit", "arguments": {"ops": [{"op": "add_clip", "clip": {"track": "v2", "text": "Hi", "duration": 2}}]}}}));
    assert_eq!(r["result"]["structuredContent"]["revision"], 1, "{r}");
    let r = rpc(&mut srv, json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "snapshot", "arguments": {"at": 1}}}));
    assert_eq!(r["result"]["content"][0]["type"], "image", "{r}");
    assert_eq!(r["result"]["content"][0]["mimeType"], "image/png");
    let r = rpc(&mut srv, json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "edit", "arguments": {"ops": [{"op": "trim_clp"}]}}}));
    assert_eq!(r["result"]["isError"], true);
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("trim_clip"), "{r}");

    let res = rpc(&mut srv, json!({"jsonrpc": "2.0", "id": 7, "method": "resources/list"}));
    let uris: Vec<&str> = res["result"]["resources"].as_array().unwrap().iter().map(|r| r["uri"].as_str().unwrap()).collect();
    assert!(uris.contains(&"aiframes://reference/ops") && uris.contains(&"aiframes://project/1"), "{uris:?}");
    let ops = rpc(&mut srv, json!({"jsonrpc": "2.0", "id": 8, "method": "resources/read", "params": {"uri": "aiframes://reference/ops"}}));
    assert!(ops["result"]["contents"][0]["text"].as_str().unwrap().contains("split_clip"));
    let proj = rpc(&mut srv, json!({"jsonrpc": "2.0", "id": 9, "method": "resources/read", "params": {"uri": "aiframes://project/1"}}));
    assert!(proj["result"]["contents"][0]["text"].as_str().unwrap().starts_with("Project"));

    // A batch of two requests in one line.
    let out = srv.handle_line(&json!([{"jsonrpc": "2.0", "id": 10, "method": "ping"}, {"jsonrpc": "2.0", "id": 11, "method": "tools/call", "params": {"name": "list_projects", "arguments": {}}}]).to_string()).unwrap();
    let v: J = serde_json::from_str(&out).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2);
}
