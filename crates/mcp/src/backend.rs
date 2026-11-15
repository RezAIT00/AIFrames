//! [`AiframesBackend`]: AIFrames' tools, instructions and resources for the suite MCP framework.

use crate::{schema, tools, Services, SPEC};
use aiframes_core::document::canonical_json;
use aiframes_core::DescribeOptions;
use serde_json::{json, Value as J};
use std::path::Path;
use std::sync::Arc;
use suite_mcp::{AppSpec, Handoff};

/// The AI tools over the shared [`Services`] (the GUI's in a window, a private one when headless).
pub struct AiframesBackend {
    pub s: Arc<Services>,
    /// The project each MCP client session works on.
    sessions: tools::Sessions,
}

impl AiframesBackend {
    pub fn new(s: Arc<Services>) -> Arc<Self> {
        Arc::new(AiframesBackend { s, sessions: tools::Sessions::default() })
    }

    /// (Addition) The project a client session works on (what tools without `project` act on).
    pub fn session_project(&self, session: &str) -> Option<u64> {
        self.sessions.project(session)
    }

    /// (Addition) A session's id and human label (as recorded on its edits).
    pub fn session_info(&self, session: &str) -> crate::AiSession {
        self.sessions.info(session)
    }
}

impl suite_mcp::Backend for AiframesBackend {
    fn spec(&self) -> &'static AppSpec {
        &SPEC
    }

    fn instructions(&self, mode: &str) -> String {
        schema::instructions(mode)
    }

    fn tools(&self, include_launch: bool) -> Vec<J> {
        schema::definitions(include_launch)
    }

    fn call(&self, name: &str, args: &J) -> Result<J, String> {
        tools::call(&self.s, name, args)
    }

    fn call_in(&self, session: Option<&str>, name: &str, args: &J) -> Result<J, String> {
        let session = session.map(|id| tools::Session { store: &self.sessions, id });
        tools::call_in(&self.s, session, name, args)
    }

    /// Tools that decode, render or analyse media can take minutes on long footage (their own waits are
    /// capped at 300 s): a stdio proxy gives them 15 minutes before treating the window as slow.
    fn proxy_timeout(&self, name: &str, _args: &J) -> Option<std::time::Duration> {
        const LONG: &[&str] = &[
            "import_media",
            "transcribe",
            "export",
            "export_status",
            "jobs",
            "detect_scenes",
            "detect_silence",
            "detect_beats",
            "remove_silences",
            "analyze_audio",
            "contact_sheet",
            "filmstrip",
            "snapshot",
            "import_transcript",
            "align_transcript",
            "generate_captions",
            "audit_joins",
        ];
        LONG.contains(&name).then(|| std::time::Duration::from_secs(900))
    }

    fn client_info(&self, session: Option<&str>, info: &J) {
        let name = info.get("title").or_else(|| info.get("name")).and_then(J::as_str).map(str::trim).unwrap_or("");
        if let (Some(s), false) = (session, name.is_empty()) {
            self.sessions.set_client(s, &crate::util::short(name, 60));
        }
    }

    fn resources(&self) -> Vec<J> {
        let mut v = vec![
            json!({"uri": "aiframes://guide", "name": "guide", "title": "How to edit with AIFrames", "description": "The work loop, anchors, clip-local time, text-based editing.", "mimeType": "text/markdown"}),
            json!({"uri": "aiframes://reference/ops", "name": "ops", "title": "Op reference", "description": "Every edit op with help and a JSON example.", "mimeType": "text/markdown"}),
            json!({"uri": "aiframes://reference/effects", "name": "effects", "title": "Effects and transitions", "description": "Every effect and transition with parameters, defaults and ranges.", "mimeType": "text/markdown"}),
            json!({"uri": "aiframes://reference/validation", "name": "validation", "title": "Validation codes", "description": "Every validation finding code and what it means.", "mimeType": "text/markdown"}),
            json!({"uri": "aiframes://reference/schema", "name": "schema", "title": "Project JSON schema", "description": "Every type, value form and enum of the project JSON with fields, defaults, units and examples.", "mimeType": "application/json"}),
        ];
        for (id, d) in self.s.hub.list() {
            let g = suite_hub::lock(&d);
            v.push(json!({
                "uri": format!("aiframes://project/{id}"),
                "name": g.title(),
                "title": g.title(),
                "description": format!("Open project ({} clips, {:.1} s) as text", g.doc.project.clips.len(), g.doc.resolved().duration),
                "mimeType": "text/plain",
            }));
        }
        v
    }

    fn read_resource(&self, uri: &str) -> Result<J, String> {
        let (mime, text) = match uri {
            "aiframes://guide" => ("text/markdown", schema::instructions("live or headlessly")),
            "aiframes://reference/ops" => ("text/markdown", schema::ops_reference()),
            "aiframes://reference/effects" => ("text/markdown", schema::effects_reference()),
            "aiframes://reference/validation" => ("text/markdown", schema::validation_reference()),
            "aiframes://reference/schema" => ("application/json", serde_json::to_string_pretty(&aiframes_core::schema_docs()).unwrap_or_default()),
            _ => {
                let id: u64 = uri.strip_prefix("aiframes://project/").and_then(|s| s.parse().ok()).ok_or_else(|| format!("unknown resource '{uri}'"))?;
                let d = self.s.hub.get(id).ok_or_else(|| format!("project {id} is not open"))?;
                let g = suite_hub::lock(&d);
                ("text/plain", tools::describe_lines(&g.doc.project, g.doc.resolved(), &DescribeOptions::default()).join("\n"))
            }
        };
        Ok(json!({"contents": [{"uri": uri, "mimeType": mime, "text": text}]}))
    }

    fn document_count(&self) -> usize {
        self.s.hub.list().len()
    }

    fn discard_notices(&self) {
        self.s.hub.take_notices();
    }

    fn handoff_list(&self) -> Vec<Handoff> {
        self.s
            .hub
            .list()
            .into_iter()
            .map(|(id, d)| {
                let g = suite_hub::lock(&d);
                Handoff { id, title: g.title(), clean_path: if g.doc.dirty() { None } else { g.doc.path.clone() } }
            })
            .collect()
    }

    /// Writes the project (with absolute media paths, so it opens from anywhere) for a window to open.
    fn handoff_save(&self, id: u64, path: &Path) -> Result<(), String> {
        let d = self.s.hub.get(id).ok_or("project is no longer open")?;
        let g = suite_hub::lock(&d);
        let mut p = g.doc.project.clone();
        for m in &mut p.media {
            m.path = g.doc.media_path(m).to_string_lossy().replace('\\', "/");
        }
        std::fs::write(path, canonical_json(&p)).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    fn handoff_ext(&self) -> &'static str {
        aiframes_core::PROJECT_EXT
    }

    fn handoff_remove(&self, id: u64) {
        self.s.hub.remove(id);
    }
}
