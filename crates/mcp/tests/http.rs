//! The live path: a "window" and the AI share one `Services`; the AI talks Streamable HTTP.

mod common;

use aiframes_core::{Document, Project};
use aiframes_mcp::ProjectDoc;
use common::*;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use suite_mcp::client;

#[test]
fn http_endpoint_serves_the_windows_projects() {
    let s = services();
    let wakes = Arc::new(AtomicUsize::new(0));
    let w = wakes.clone();
    s.hub.set_waker(move || {
        w.fetch_add(1, Ordering::Relaxed);
    });
    // The GUI opened a project.
    let id = s.hub.add(ProjectDoc::new(Document::new(Project { name: "Live".into(), ..Default::default() })));
    let port = aiframes_mcp::start_http_on(s.clone(), 0, 1, false).expect("bind");
    assert!(client::health(port, "aiframes"));

    let t = Duration::from_secs(30);
    let r = client::call_tool(port, "list_projects", json!({}), t).unwrap();
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("\"Live\""), "{text}");
    assert!(text.contains("\"window\": true"), "a waker = an attached window: {text}");

    let before = wakes.load(Ordering::Relaxed);
    let r = client::call_tool(port, "edit", json!({"ops": [{"op": "add_track", "kind": "video", "role": "main"}, {"op": "add_clip", "clip": {"track": "v1", "text": "From the AI", "duration": 2}}]}), t).unwrap();
    assert_eq!(r["isError"], false, "{r}");
    assert!(wakes.load(Ordering::Relaxed) > before, "the window was woken");
    // The window sees the AI's edit, its attribution and the notice for the AI log.
    let d = s.hub.get(id).unwrap();
    {
        let g = suite_hub::lock(&d);
        assert_eq!(g.doc.project.clips.len(), 1);
        assert_eq!(g.doc.history().last().unwrap().author, "ai");
        assert_eq!(g.live.ai_changed_revision, 1);
        assert!(!g.live.ai_changed.is_empty());
    }
    let notices = s.hub.take_notices();
    assert!(notices.iter().any(|n| n.starts_with("AI: ")), "{notices:?}");

    // Live requests reach the window.
    client::call_tool(port, "seek", json!({"time": 1.5}), t).unwrap();
    assert_eq!(suite_hub::lock(&d).live.seek_request, Some(1.5));

    // The person edits; the AI sees it.
    {
        let mut g = suite_hub::lock(&d);
        let op = aiframes_core::Op::from_json(&json!({"op": "set_project", "notes": "punchy"})).unwrap();
        g.doc.apply("user", "Notes", vec![op]).unwrap();
    }
    let r = client::call_tool(port, "changes_since", json!({"revision": 1}), t).unwrap();
    assert!(r["content"][0]["text"].as_str().unwrap().contains("\"user\""), "{r}");
}
