//! Tool implementations. Every edit goes through `Document::apply` (undoable, attributed), records
//! what the AI changed for the GUI, posts a notice to the AI log and wakes the window.

mod joins;
mod media;
mod project;
mod review;
mod view;

use crate::util::*;
use crate::{DocRef, ProjectDoc, Services, AI_AUTHOR};
use aiframes_core::{apply_batch, Clip, Finding, Id, Op, Project, Receipt, Resolved, Severity};
use serde_json::{json, Value as J};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

pub(crate) use project::{describe_lines, project_from_args};

pub(crate) fn time_help() -> &'static str {
    project::TIME_HELP
}

pub(crate) fn param_help() -> &'static str {
    project::PARAM_HELP
}

pub(crate) const NO_DOC: &str = "No project is open. Use new_project or open_project first (or list_projects).";

/// Tools that are not about one project (the `project` argument is not resolved for them).
const GLOBAL_TOOLS: &[&str] = &[
    "list_projects",
    "new_project",
    "open_project",
    "list_ops",
    "list_effects",
    "list_transitions",
    "export_status",
    "jobs",
    "cancel_job",
    "screenshot_ui",
    "launch_gui",
    "schema",
];

/// Per-client state: the project each MCP session works on (the one it last created, opened or
/// used). Tools without `project` act on it, not on whatever tab the window shows — another
/// program (or the person) may open or focus a different project at any time.
#[derive(Default)]
pub(crate) struct Sessions {
    map: std::sync::Mutex<std::collections::HashMap<String, (u64, std::time::Instant)>>,
    /// Per session: its number in this window (in order of first appearance) and the client's name.
    clients: std::sync::Mutex<(u32, std::collections::HashMap<String, Client>)>,
}

/// A session's number in this window and its client's name.
type Client = (u32, Option<String>);

/// The internal argument carrying the calling session to the tools (added after argument checking).
pub(crate) const SESSION_ARG: &str = "__session";

impl Sessions {
    const MAX: usize = 512;

    /// The client of `session` introduced itself (initialize `clientInfo` title or name).
    pub(crate) fn set_client(&self, session: &str, name: &str) {
        let mut c = suite_hub::lock(&self.clients);
        let n = match c.1.get(session) {
            Some((n, _)) => *n,
            None => {
                c.0 += 1;
                c.0
            }
        };
        c.1.insert(session.to_string(), (n, Some(name.to_string())));
    }

    /// The session's id and human label ("Claude Code (stdio #2)").
    pub(crate) fn info(&self, session: &str) -> crate::AiSession {
        let mut c = suite_hub::lock(&self.clients);
        let (n, client) = match c.1.get(session) {
            Some(v) => v.clone(),
            None => {
                c.0 += 1;
                let v = (c.0, None);
                if c.1.len() < Self::MAX * 4 {
                    c.1.insert(session.to_string(), v.clone());
                }
                v
            }
        };
        let transport = if session.starts_with("stdio-") { "stdio" } else { "HTTP" };
        let label = match client {
            Some(name) => format!("{name} ({transport} #{n})"),
            None => format!("AI session #{n} ({transport})"),
        };
        crate::AiSession { id: session.to_string(), label }
    }

    pub(crate) fn project(&self, session: &str) -> Option<u64> {
        suite_hub::lock(&self.map).get(session).map(|(id, _)| *id)
    }

    pub(crate) fn remember(&self, session: &str, id: u64) {
        let mut m = suite_hub::lock(&self.map);
        if m.len() >= Self::MAX && !m.contains_key(session) {
            if let Some(oldest) = m.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| k.clone()) {
                m.remove(&oldest);
            }
        }
        m.insert(session.to_string(), (id, std::time::Instant::now()));
    }
}

/// The session a call belongs to (none for the CLI and session-less HTTP clients).
#[derive(Clone, Copy)]
pub(crate) struct Session<'a> {
    pub store: &'a Sessions,
    pub id: &'a str,
}

/// Runs one tool without a session (the CLI, tests): `project` defaults to the active project.
pub(crate) fn call(s: &Arc<Services>, name: &str, args: &J) -> ToolResult {
    call_in(s, None, name, args)
}

/// Runs one tool for a client session.
pub(crate) fn call_in(s: &Arc<Services>, session: Option<Session>, name: &str, args: &J) -> ToolResult {
    let empty = json!({});
    let args = if args.is_object() { args } else { &empty };
    check_args(name, args)?;
    // The calling session travels with the arguments (internal key) so every AI edit — including
    // ones finished later by a background job — records which session made it.
    let with_session;
    let args = match session {
        Some(x) => {
            let info = x.store.info(x.id);
            let mut a = args.clone();
            a[SESSION_ARG] = json!({"id": info.id, "label": info.label});
            with_session = a;
            &with_session
        }
        None => args,
    };
    // Resolve "which project" exactly once, per session: an explicit `project`, else the project this
    // session works on (if still open), else the window's active project. Tools that analyse media
    // without holding the project's lock look it up again later; pinning the id makes every later
    // lookup name the same project even if the active tab changes meanwhile.
    let pinned;
    let mut doc_id = None;
    let args = if GLOBAL_TOOLS.contains(&name) {
        args
    } else {
        let explicit = args.get("project").is_some_and(|v| !v.is_null() && v.as_str() != Some(""));
        let from_session = if explicit { None } else { session.and_then(|x| x.store.project(x.id)).filter(|id| s.hub.get(*id).is_some()) };
        let found = match from_session {
            Some(id) => Some(id),
            None => find_doc(s, args).ok().map(|(id, _)| id),
        };
        match found {
            Some(id) => {
                if let Some(x) = session {
                    x.store.remember(x.id, id);
                }
                doc_id = Some(id);
                let mut a = args.clone();
                a["project"] = json!(id);
                pinned = a;
                &pinned
            }
            None => args,
        }
    };
    let mut r = dispatch(s, name, args);
    if let Ok(v) = &mut r {
        if matches!(name, "new_project" | "open_project") {
            doc_id = v.get("project").and_then(J::as_u64);
            if let (Some(x), Some(id)) = (session, doc_id) {
                x.store.remember(x.id, id);
            }
        }
        if name == "list_projects" {
            let mine = session.and_then(|x| x.store.project(x.id));
            if let Some(list) = v.get_mut("projects").and_then(J::as_array_mut) {
                for p in list {
                    let is_mine = mine.is_some() && p["project"].as_u64() == mine;
                    p["session"] = json!(is_mine);
                }
            }
        }
        if let Some(id) = doc_id {
            annotate(s, v, id);
        }
    }
    r
}

/// Every project-scoped result names the project it acted on (id + name) and its revision.
fn annotate(s: &Services, v: &mut J, id: u64) {
    let Some(obj) = v.as_object_mut() else { return };
    let Some(d) = s.hub.get(id) else { return };
    let g = suite_hub::lock(&d);
    obj.entry("project").or_insert(json!(id));
    let name = if g.doc.project.name.is_empty() { g.title() } else { format!("{} ({})", g.doc.project.name, g.title()) };
    obj.entry("project_name").or_insert(json!(name));
    obj.entry("revision").or_insert(json!(g.doc.revision()));
}

/// The argument names every tool accepts (from its schema).
fn tool_args() -> &'static std::collections::HashMap<String, Vec<String>> {
    static ARGS: std::sync::OnceLock<std::collections::HashMap<String, Vec<String>>> = std::sync::OnceLock::new();
    ARGS.get_or_init(|| {
        crate::schema::definitions(true)
            .iter()
            .filter_map(|t| {
                let name = t["name"].as_str()?.to_string();
                let mut props: Vec<String> = t["inputSchema"]["properties"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
                props.sort();
                Some((name, props))
            })
            .collect()
    })
}

/// Unknown arguments are errors with a near-miss suggestion (silently ignoring them once made an AI
/// put hours of footage on the person's timeline: `asset` was not an argument of contact_sheet).
fn check_args(name: &str, args: &J) -> Result<(), String> {
    let Some(valid) = tool_args().get(name) else { return Ok(()) };
    let Some(obj) = args.as_object() else { return Ok(()) };
    for k in obj.keys() {
        if !valid.iter().any(|v| v == k) {
            let hint = did_you_mean(k, valid.iter().map(String::as_str));
            let list = if valid.is_empty() { "none".to_string() } else { valid.join(", ") };
            return Err(format!("unknown argument '{k}' for {name}.{hint} Valid arguments: {list}."));
        }
    }
    Ok(())
}

fn dispatch(s: &Arc<Services>, name: &str, args: &J) -> ToolResult {
    match name {
        // projects
        "list_projects" => project::list_projects(s),
        "new_project" => project::new_project(s, args),
        "open_project" => project::open_project(s, args),
        "save_project" => project::save_project(s, args),
        "close_project" => project::close_project(s, args),
        "describe_project" => project::describe_project(s, args),
        "get_timeline" => project::get_timeline(s, args),
        "get_clip" => project::get_clip(s, args),
        "list_media" => project::list_media(s, args),
        "media_info" => view::media_info(s, args),
        "import_media" => project::import_media(s, args),
        "schema" => project::schema(args),
        // editing
        "edit" => project::edit(s, args),
        "undo" => project::undo_redo(s, args, true),
        "redo" => project::undo_redo(s, args, false),
        "history" => project::history(s, args),
        "changes_since" => project::changes_since(s, args),
        "validate" => project::validate_tool(s, args),
        "list_ops" => project::list_ops(args),
        "list_effects" => project::list_effects(args),
        "list_transitions" => project::list_transitions(),
        "chapters" => project::chapters(s, args),
        "auto_ducking" => project::auto_ducking(s, args),
        // seeing
        "snapshot" => view::snapshot(s, args),
        "contact_sheet" => view::contact_sheet(s, args),
        "filmstrip" => view::filmstrip(s, args),
        // transcript, analysis, jobs
        "transcribe" => media::transcribe(s, args),
        "get_transcript" => media::get_transcript(s, args),
        "import_transcript" => media::import_transcript(s, args),
        "align_transcript" => media::align_transcript(s, args),
        "search_transcript" => media::search_transcript(s, args),
        "delete_words" => media::delete_words(s, args),
        "audit_joins" => joins::audit_joins(s, args),
        "trim_to_words" => joins::trim_to_words(s, args),
        "export_otio" => joins::export_otio(s, args),
        "suggest" => review::suggest(s, args),
        "list_suggestions" => review::list_suggestions(s, args),
        "resolve_suggestion" => review::resolve_suggestion(s, args),
        "generate_captions" => media::generate_captions(s, args),
        "detect_scenes" => media::detect_scenes(s, args),
        "detect_silence" => media::detect_silence(s, args),
        "remove_silences" => media::remove_silences(s, args),
        "detect_beats" => media::detect_beats(s, args),
        "analyze_audio" => media::analyze_audio(s, args),
        "export" => media::export(s, args),
        "export_status" => media::export_status(s, args),
        "jobs" => media::jobs(s, args),
        "cancel_job" => media::cancel_job(s, args),
        // live window
        "get_state" => view::get_state(s, args),
        "play" => view::play(s, args, true),
        "pause" => view::play(s, args, false),
        "seek" => view::seek(s, args),
        "select" => view::select(s, args),
        "show_panel" => view::show_panel(s, args),
        "screenshot_ui" => view::screenshot_ui(s),
        "launch_gui" => Err("launch_gui is only available when AIFrames' MCP server runs over stdio (`aiframes mcp`). The AIFrames window is already running.".into()),
        other => {
            let names: Vec<String> = crate::schema::definitions(true).iter().filter_map(|t| t["name"].as_str().map(String::from)).collect();
            Err(format!("unknown tool '{other}'.{} Call tools/list to see the tools.", did_you_mean(other, names.iter().map(String::as_str))))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------------------------

/// Resolves the `project` argument (hub id, file name, stem or path; default: the active project).
pub(crate) fn find_doc(s: &Services, args: &J) -> Result<(u64, DocRef), String> {
    let hub = &s.hub;
    let docs = hub.list();
    if docs.is_empty() {
        return Err(NO_DOC.into());
    }
    let key = match args.get("project") {
        None | Some(J::Null) => return hub.active().ok_or_else(|| NO_DOC.to_string()),
        Some(J::Number(n)) => n.to_string(),
        Some(J::String(s)) if s.trim().is_empty() => return hub.active().ok_or_else(|| NO_DOC.to_string()),
        Some(J::String(s)) => s.trim().to_string(),
        Some(other) => return Err(format!("'project' must be an id, file name or path (got {other})")),
    };
    if let Ok(id) = key.parse::<u64>() {
        if let Some((i, d)) = docs.iter().find(|(i, _)| *i == id) {
            return Ok((*i, d.clone()));
        }
    }
    let lk = key.to_lowercase().replace('\\', "/");
    for (id, d) in &docs {
        let g = suite_hub::lock(d);
        let title = g.title().to_lowercase();
        let stem = Path::new(&title).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let path = g.doc.path.as_ref().map(|p| p.to_string_lossy().to_lowercase().replace('\\', "/")).unwrap_or_default();
        if title == lk || stem == lk || (!path.is_empty() && path == lk) || g.doc.project.name.to_lowercase() == lk {
            return Ok((*id, d.clone()));
        }
    }
    if let Some(id) = hub.find_path(Path::new(&key)) {
        if let Some(d) = hub.get(id) {
            return Ok((id, d));
        }
    }
    let names: Vec<String> = docs.iter().map(|(i, d)| format!("{} (id {i})", suite_hub::lock(d).title())).collect();
    Err(format!("no open project matches '{key}'. Open projects: {}", names.join(", ")))
}

/// Runs `f` on the project named by `args` (locked).
pub(crate) fn with_doc<T>(s: &Services, args: &J, f: impl FnOnce(u64, &mut ProjectDoc) -> Result<T, String>) -> Result<T, String> {
    let (id, d) = find_doc(s, args)?;
    let mut g = suite_hub::lock(&d);
    f(id, &mut g)
}

pub(crate) fn author_of(args: &J) -> String {
    arg_str(args, "author").unwrap_or(AI_AUTHOR).to_string()
}

/// "AI" for the default author, else "AI (name)".
pub(crate) fn who(author: &str) -> String {
    if author == AI_AUTHOR {
        "AI".into()
    } else {
        format!("AI ({author})")
    }
}

fn finding_key(f: &Finding) -> String {
    format!("{}|{}", f.code, f.ids.join(","))
}

pub(crate) fn finding_json(f: &Finding, with_ops: bool) -> J {
    let mut j = json!({"severity": f.severity, "code": f.code, "message": f.message});
    if let Some(at) = f.at {
        j["at"] = json!(t3(at));
    }
    if !f.ids.is_empty() {
        j["ids"] = json!(f.ids);
    }
    if let Some(fix) = &f.fix {
        j["fix"] = json!(fix);
    }
    if with_ops && !f.fix_ops.is_empty() {
        j["fix_ops"] = json!(f.fix_ops);
    }
    j
}

pub(crate) fn counts(fs: &[Finding]) -> (usize, usize, usize) {
    let c = |s: Severity| fs.iter().filter(|f| f.severity == s).count();
    (c(Severity::Error), c(Severity::Warning), c(Severity::Info))
}

fn moved_json(rc: &Receipt, limit: usize) -> J {
    let sp = |s: &Option<aiframes_core::Span>| s.map(|s| span_j(s.start, s.end)).unwrap_or(J::Null);
    let v: Vec<J> = rc.moved.iter().take(limit).map(|m| json!({"id": m.id, "from": sp(&m.before), "to": sp(&m.after)})).collect();
    J::Array(v)
}

fn push_unique(v: &mut Vec<Id>, ids: &[Id]) {
    for i in ids {
        if !v.contains(i) {
            v.push(i.clone());
        }
    }
}

/// The receipt as compact JSON for the AI. For a dry run, `revision` is the project's CURRENT
/// revision (the one the plan was computed against): a plan consumes no revision, so
/// `changes_since(revision)` after a plan still shows every later edit.
pub(crate) fn receipt_json(rc: &Receipt, planned: bool) -> J {
    let mut j = json!({"revision": rc.revision, "label": rc.label});
    if planned {
        j["revision"] = json!(rc.revision.saturating_sub(1));
        j["planned"] = json!(true);
        j["note"] = json!("Dry run: nothing changed (revision is the current one). Call again without plan to apply.");
    }
    let done: Vec<&str> = rc.outcomes.iter().map(|o| o.summary.as_str()).filter(|s| !s.is_empty()).collect();
    if !done.is_empty() {
        j["done"] = json!(done);
    }
    let (mut created, mut changed, mut removed) = (Vec::new(), Vec::new(), Vec::new());
    for o in &rc.outcomes {
        push_unique(&mut created, &o.created);
        push_unique(&mut changed, &o.changed);
        push_unique(&mut removed, &o.removed);
    }
    changed.retain(|c| !created.contains(c) && !removed.contains(c));
    created.retain(|c| !removed.contains(c));
    for (k, v) in [("created", created), ("changed", changed), ("removed", removed)] {
        if !v.is_empty() {
            j[k] = json!(v);
        }
    }
    if !rc.moved.is_empty() {
        j["moved"] = moved_json(rc, 40);
        if rc.moved.len() > 40 {
            j["moved_more"] = json!(rc.moved.len() - 40);
        }
    }
    if !rc.warnings.is_empty() {
        j["warnings"] = json!(rc.warnings);
    }
    // Structured results of ops that have them (set_spacing/close_gaps gaps, caption regeneration).
    let details: Vec<J> = rc.outcomes.iter().filter(|o| !o.details.is_null()).map(|o| o.details.clone()).collect();
    match details.len() {
        0 => {}
        1 => j["details"] = details.into_iter().next().unwrap_or(J::Null),
        _ => j["details"] = J::Array(details),
    }
    if !rc.new_errors.is_empty() {
        j["new_errors"] = json!(rc.new_errors);
    }
    j
}

/// Ids the GUI should flash after a receipt: created, changed and moved items (not removed ones).
pub(crate) fn touched_ids(rc: &Receipt) -> Vec<Id> {
    let mut v = Vec::new();
    for o in &rc.outcomes {
        push_unique(&mut v, &o.created);
        push_unique(&mut v, &o.changed);
    }
    let moved: Vec<Id> = rc.moved.iter().filter(|m| m.after.is_some()).map(|m| m.id.clone()).collect();
    push_unique(&mut v, &moved);
    v
}

/// Adds validation context to a receipt: findings the edit introduced and the open counts.
fn add_findings(j: &mut J, before: &[Finding], after: &[Finding], dur_before: f64, dur_after: f64) {
    let old: HashSet<String> = before.iter().map(finding_key).collect();
    let new: Vec<J> = after
        .iter()
        .filter(|f| f.severity != Severity::Info && !old.contains(&finding_key(f)))
        .take(10)
        .map(|f| finding_json(f, false))
        .collect();
    if !new.is_empty() {
        j["new_findings"] = J::Array(new);
    }
    let (e, w, _) = counts(after);
    j["open_errors"] = json!(e);
    j["open_warnings"] = json!(w);
    j["duration"] = json!(t3(dur_after));
    if (dur_after - dur_before).abs() > 1e-6 {
        j["duration_before"] = json!(t3(dur_before));
    }
}

/// Applies (or with `plan: true` dry-runs) `ops` on the project named by `args` as one undoable
/// edit by `author` (default "ai"), and returns the receipt with validation context.
pub(crate) fn commit(s: &Arc<Services>, args: &J, label: &str, ops: Vec<Op>) -> ToolResult {
    commit_with(s, args, label, ops, |_| {})
}

/// [`commit`] with a hook to add tool-specific fields to the result.
pub(crate) fn commit_with(s: &Arc<Services>, args: &J, label: &str, ops: Vec<Op>, extra: impl FnOnce(&mut J)) -> ToolResult {
    let plan = arg_bool(args, "plan", false)?;
    let author = author_of(args);
    let label = arg_str(args, "label").unwrap_or(label).to_string();
    let (id, d) = find_doc(s, args)?;
    let mut j;
    let notice;
    {
        let mut g = suite_hub::lock(&d);
        let dur_before = g.doc.resolved().duration;
        if plan {
            let (mut rc, planned) = g.doc.plan_project(&ops).map_err(|e| schema_hint(e.to_string()))?;
            // The same label and the same validation (incl. media_offline) the apply would use.
            if !label.trim().is_empty() {
                rc.label = label.clone();
            }
            let before = g.doc.validate();
            let mut after_doc = aiframes_core::Document::new(planned);
            after_doc.path = g.doc.path.clone();
            let after = after_doc.validate();
            j = receipt_json(&rc, true);
            add_findings(&mut j, &before, &after, dur_before, after_doc.resolved().duration);
            notice = None;
        } else {
            let before = g.doc.validate();
            let rc = g.doc.apply(&author, &label, ops).map_err(|e| schema_hint(e.to_string()))?;
            let after = g.doc.validate();
            j = receipt_json(&rc, false);
            add_findings(&mut j, &before, &after, dur_before, g.doc.resolved().duration);
            let session = session_of(args);
            g.mark_ai_changed_by(touched_ids(&rc), rc.revision, session.as_ref());
            if let Some(x) = &session {
                j["session"] = json!({"id": x.id, "label": x.label});
            }
            notice = Some(format!("{}: {}", who(&author), rc.label));
        }
    }
    j["project"] = json!(id);
    extra(&mut j);
    if let Some(n) = notice {
        s.hub.notify(n);
        s.hub.wake();
    }
    Ok(j)
}

/// Parses ops from a JSON array (or a single op, or a JSON string of either) with the index and the
/// near-miss suggestion of every invalid op.
pub(crate) fn parse_ops(v: &J) -> Result<Vec<Op>, String> {
    let owned;
    let v = match v {
        J::String(s) => {
            owned = serde_json::from_str::<J>(s).map_err(|e| format!("'ops' is a string but not valid JSON: {e}"))?;
            &owned
        }
        other => other,
    };
    let items: Vec<&J> = match v {
        J::Array(a) => a.iter().collect(),
        J::Object(_) => vec![v],
        J::Null => return Err("missing 'ops': an array of ops like [{\"op\":\"split_clip\",\"clip\":\"c3\",\"at\":12.4}] (list_ops shows every op)".into()),
        other => return Err(format!("'ops' must be an array of op objects (got {other})")),
    };
    let mut ops = Vec::new();
    let mut errs = Vec::new();
    for (i, item) in items.iter().enumerate() {
        match Op::from_json(item) {
            Ok(o) => ops.push(o),
            Err(e) => errs.push(schema_hint(format!("op #{i}: {}", e.message))),
        }
    }
    if !errs.is_empty() {
        return Err(format!("{} (nothing was changed)", errs.join(" | ")));
    }
    if ops.is_empty() {
        return Err("'ops' is empty".into());
    }
    Ok(ops)
}

/// A clip by id, with a suggestion when it does not exist.
pub(crate) fn clip_of<'a>(p: &'a Project, id: &str) -> Result<&'a Clip, String> {
    p.clips.iter().find(|c| c.id == id).ok_or_else(|| {
        let hint = did_you_mean(id, p.clips.iter().map(|c| c.id.as_str()));
        format!("unknown clip '{id}'.{hint} (describe_project lists the clips)")
    })
}

/// One frame's duration of the project.
pub(crate) fn frame_dur(p: &Project) -> f64 {
    1.0 / p.settings.fps.max(1.0)
}

/// A point in time from a tool argument: seconds (number or "12.5"), a clip id (its middle; `c3@start`,
/// `c3@end` for its first/last frame), or a marker id.
pub(crate) fn time_ref(p: &Project, r: &Resolved, v: &J) -> Result<f64, String> {
    if let Some(t) = num_of(v) {
        return Ok(t);
    }
    let s = v.as_str().map(str::trim).ok_or_else(|| format!("a time must be seconds, a clip id or a marker id (got {v})"))?;
    let (id, edge) = match s.split_once('@') {
        Some((a, b)) => (a.trim(), Some(b.trim().to_ascii_lowercase())),
        None => (s, None),
    };
    if let Some(sp) = r.span(id) {
        let fd = frame_dur(p);
        return Ok(match edge.as_deref() {
            Some("start") | Some("in") => sp.start,
            Some("end") | Some("out") => (sp.end - fd).max(sp.start),
            _ => sp.start + sp.duration() / 2.0,
        });
    }
    if let Some(t) = r.marker(id) {
        return Ok(t);
    }
    if p.media.iter().any(|m| m.id == id) {
        let at = edge.as_deref().unwrap_or("0");
        return Err(format!(
            "'{s}' names media {id}, not the timeline: to look at a media file pass asset (source seconds), e.g. {{\"asset\":\"{id}\",\"at\":{at}}} for snapshot or {{\"asset\":\"{id}\",\"from\":0,\"to\":60}} for contact_sheet"
        ));
    }
    if p.clips.iter().any(|c| c.id == id) {
        return Err(format!("clip '{id}' is not placed on the timeline (unresolved anchor); see validate"));
    }
    let hint = did_you_mean(id, p.clips.iter().map(|c| c.id.as_str()).chain(p.markers.iter().map(|m| m.id.as_str())));
    Err(format!("'{s}' is not a time, clip id or marker id.{hint}"))
}

/// Absolute media file of an asset.
pub(crate) fn asset_path(doc: &aiframes_core::Document, asset: &str) -> Result<std::path::PathBuf, String> {
    let m = doc.project.media.iter().find(|m| m.id == asset).ok_or_else(|| {
        let hint = did_you_mean(asset, doc.project.media.iter().map(|m| m.id.as_str()));
        format!("unknown media asset '{asset}'.{hint} (list_media lists them)")
    })?;
    let p = doc.media_path(m);
    if !p.exists() {
        return Err(format!("the file of {asset} is missing: {} (relink it with relink_media)", p.display()));
    }
    Ok(p)
}

/// `asset` from the arguments: `asset` directly, or the media of `clip`. Returns (asset, clip).
pub(crate) fn asset_arg(p: &Project, args: &J) -> Result<(Id, Option<Id>), String> {
    if let Some(c) = arg_str(args, "clip") {
        let clip = clip_of(p, c)?;
        return match &clip.source {
            aiframes_core::Source::Media { asset, .. } => Ok((asset.clone(), Some(clip.id.clone()))),
            _ => Err(format!("{c} is not a media clip (it has no source file)")),
        };
    }
    if let Some(a) = arg_str(args, "asset") {
        return Ok((a.to_string(), None));
    }
    Err("pass 'clip' (a media clip id) or 'asset' (a media id from list_media)".into())
}

/// Strict-JSON errors ("unknown field", "invalid type", "missing field") point to the `schema` tool,
/// naming the type when the message does ("expected struct TextBox").
pub(crate) fn schema_hint(e: String) -> String {
    let strict = ["unknown field", "invalid type", "missing field", "unknown variant", "expected struct", "expected one of"];
    if !strict.iter().any(|k| e.contains(k)) || e.contains("schema {") {
        return e;
    }
    let ty = e.split("struct ").nth(1).map(|rest| rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect::<String>()).filter(|t| !t.is_empty());
    match ty {
        Some(t) => format!("{e} — schema {{\"type\":\"{t}\"}} lists every field of {t}"),
        None => format!("{e} — the schema tool lists the exact fields of every type"),
    }
}

/// The calling MCP session (from the internal argument [`SESSION_ARG`]), if any.
pub(crate) fn session_of(args: &J) -> Option<crate::AiSession> {
    let s = args.get(SESSION_ARG)?;
    Some(crate::AiSession { id: s["id"].as_str()?.to_string(), label: s["label"].as_str().unwrap_or("").to_string() })
}
