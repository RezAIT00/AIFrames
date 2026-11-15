//! Headless command line: `aiframes new|import|describe|timeline|validate|apply|snapshot|contact-sheet|
//! render|transcribe|chapters|history|ops|demo|help`. Built on the same tools as the MCP server, so the
//! CLI, the GUI and the AI behave identically. Human-readable output by default, `--json` for machines.

use crate::util::{num, secs, short};
use crate::{tools, JobStatus, Services};
use serde_json::{json, Value as J};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

/// Every subcommand [`cli`] handles (and the help flags).
pub const CLI_COMMANDS: &[&str] =
    &["new", "import", "describe", "timeline", "validate", "apply", "snapshot", "contact-sheet", "render", "transcribe", "chapters", "history", "ops", "demo", "help", "--help", "-h"];

/// True when `cmd` is a CLI subcommand handled by [`cli`] (so the binary doesn't start the GUI).
pub fn is_cli_command(cmd: &str) -> bool {
    CLI_COMMANDS.contains(&cmd)
}

const USAGE: &str = "AIFrames — AI-first video editor

Usage:
  aiframes                              open the editor
  aiframes <project.aifr>               open a project in the editor
  aiframes mcp                          MCP server over stdio (proxies to a running window)

  aiframes new <path.aifr> [--preset youtube|4k|720p|shorts|square] [--name N] [--fps F] [--force]
  aiframes import <project> <files or folders...> [--append] [--track T] [--transcribe] [--language L]
  aiframes describe <project> [--from S --to S] [--json]
  aiframes timeline <project> [--json]
  aiframes validate <project> [--fix] [--severity error|warning|info] [--json]   (exit 1 if errors remain)
  aiframes apply <project> <ops.json | -> [--plan] [--author A] [--label L] [--json]
  aiframes snapshot <project> --at 1,2.5 [-o dir] [--scale 0.5] [--guides]
  aiframes contact-sheet <project> [--every 5 | --count 12] [--cols 4] [--width 320] [-o out.png]
  aiframes render <project> [-o out.mp4] [--preset youtube|1080p|4k|shorts|square|hevc|draft|prores|gif|wav] [--codec C]
                  [--quality Q] [--range a,b] [--size WxH] [--fit contain|cover] [--no-loudness] [--no-hw] [--force]
  aiframes transcribe <project> <asset or clip> [--language L] [--model turbo]
  aiframes chapters <project>           YouTube chapter list from chapter markers
  aiframes history <project> [--limit N]
  aiframes ops                          every edit op with an example
  aiframes demo <dir>                   sample media + a project using most features
  aiframes help

Edits made by the CLI are saved to the project and recorded in its history (author \"cli\").
When the AIFrames window has the project open, commands run in the window (the person sees them;
edits stay unsaved there unless --save is given) so the window never overwrites them.
";

const VALUE_FLAGS: &[&str] = &[
    "preset", "name", "fps", "width", "height", "author", "label", "at", "o", "out", "output", "scale", "every", "count", "cols", "codec", "quality", "range",
    "language", "model", "prompt", "from", "to", "track", "severity", "limit", "size", "fit",
];

struct Args {
    pos: Vec<String>,
    flags: HashMap<String, Option<String>>,
}

impl Args {
    fn parse(args: &[String]) -> Args {
        let mut pos = Vec::new();
        let mut flags = HashMap::new();
        let mut it = args.iter().peekable();
        while let Some(a) = it.next() {
            let stripped = a.strip_prefix("--").or_else(|| a.strip_prefix('-').filter(|s| !s.is_empty() && s.parse::<f64>().is_err()));
            match stripped {
                Some(f) if !f.is_empty() => {
                    if let Some((k, v)) = f.split_once('=') {
                        flags.insert(k.to_string(), Some(v.to_string()));
                    } else if VALUE_FLAGS.contains(&f) {
                        flags.insert(f.to_string(), it.next().cloned());
                    } else {
                        flags.insert(f.to_string(), None);
                    }
                }
                _ => pos.push(a.clone()),
            }
        }
        Args { pos, flags }
    }
    fn has(&self, f: &str) -> bool {
        self.flags.contains_key(f)
    }
    fn val(&self, f: &str) -> Option<&str> {
        self.flags.get(f).and_then(|v| v.as_deref())
    }
    fn out(&self) -> Option<&str> {
        self.val("o").or_else(|| self.val("out")).or_else(|| self.val("output"))
    }
    fn num(&self, f: &str) -> Result<Option<f64>, String> {
        match self.val(f) {
            None => Ok(None),
            Some(v) => v.trim().parse::<f64>().map(Some).map_err(|_| format!("--{f} must be a number (got {v:?})")),
        }
    }
}

enum Fail {
    Usage(String),
    Error(String),
}

impl From<String> for Fail {
    fn from(e: String) -> Self {
        Fail::Error(e)
    }
}

type Res = Result<i32, Fail>;

/// Headless CLI (`aiframes new|import|describe|validate|apply|snapshot|contact-sheet|render|transcribe ...`).
/// `args` excludes the program name. Returns the process exit code.
///
/// When a running AIFrames window has the project open, the command runs in that window (through its
/// MCP endpoint) so edits land in the document the person sees instead of being overwritten by the
/// window's next save; reads see the live state.
pub fn cli(args: &[String]) -> i32 {
    cli_with(Services::new(), args, &mut std::io::stdout(), &mut std::io::stderr())
}

/// (Addition) [`cli`] with explicit services and output streams (for tests and embedding). Finds a
/// running window like `aiframes mcp` does.
pub fn cli_with(s: Arc<Services>, args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    cli_with_window(s, crate::live_instance_port(), args, out, err)
}

/// (Addition) [`cli_with`] with the window to route to given explicitly (`None` = no window).
pub fn cli_with_window(s: Arc<Services>, window: Option<u16>, args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let Some(cmd) = args.first().map(String::as_str) else {
        let _ = write!(out, "{USAGE}");
        return 2;
    };
    let a = Args::parse(&args[1..]);
    let e = Env { s, window, route: std::cell::Cell::new(None), edited: std::cell::Cell::new(None) };
    let r = match cmd {
        "help" | "--help" | "-h" => {
            let _ = write!(out, "{USAGE}");
            Ok(0)
        }
        "new" => cmd_new(&e, &a, out),
        "import" => cmd_import(&e, &a, out, err),
        "describe" => cmd_describe(&e, &a, out, false),
        "timeline" => cmd_describe(&e, &a, out, true),
        "validate" => cmd_validate(&e, &a, out),
        "apply" => cmd_apply(&e, &a, out),
        "snapshot" => cmd_snapshot(&e, &a, out),
        "contact-sheet" => cmd_contact_sheet(&e, &a, out),
        "render" => cmd_render(&e, &a, out, err),
        "transcribe" => cmd_transcribe(&e, &a, out, err),
        "chapters" => cmd_chapters(&e, &a, out),
        "history" => cmd_history(&e, &a, out),
        "ops" => cmd_ops(&e, &a, out),
        "demo" => cmd_demo(&e, &a, out),
        other => Err(Fail::Usage(format!("unknown command '{other}'"))),
    };
    // Only commands that changed the window's document say where the change went.
    if !a.has("json") {
        match e.edited.get() {
            Some(false) => {
                let _ = writeln!(err, "(ran in the open AIFrames window: the edit is in the window's document, unsaved — save there, or pass --save)");
            }
            Some(true) => {
                let _ = writeln!(err, "(ran in the open AIFrames window and saved there)");
            }
            None => {}
        }
    }
    match r {
        Ok(code) => code,
        Err(Fail::Usage(m)) => {
            let _ = writeln!(err, "aiframes {cmd}: {m}\n");
            let _ = write!(err, "{USAGE}");
            2
        }
        Err(Fail::Error(m)) => {
            let _ = writeln!(err, "aiframes {cmd}: {m}");
            1
        }
    }
}

/// A path argument as the shell user meant it: relative to the current directory (the tools would
/// otherwise resolve relative paths against the project's folder).
fn cwd_path(p: &str) -> String {
    crate::util::abs_path(p, None).to_string_lossy().to_string()
}

/// Where a command runs: this process, or the window that has the project open.
struct Env {
    s: Arc<Services>,
    /// A running AIFrames window (its MCP port), if any.
    window: Option<u16>,
    /// Set by [`open`] when the project is open in that window: every call goes there.
    route: std::cell::Cell<Option<u16>>,
    /// Set when a command changed the window's document: `Some(true)` when it was also saved there.
    edited: std::cell::Cell<Option<bool>>,
}

/// A job as the CLI follows it (local or in the window).
struct JobOut {
    status: String,
    message: String,
    result: J,
}

const WINDOW_TIMEOUT: Duration = Duration::from_secs(900);

/// Calls a tool in the window; Ok = its structured result.
fn remote(port: u16, name: &str, args: J) -> Result<J, String> {
    let r = suite_mcp::client::call_tool(port, name, args, WINDOW_TIMEOUT).map_err(|e| format!("the AIFrames window did not answer: {e}"))?;
    let text = r["content"].as_array().and_then(|c| c.iter().find(|x| x["type"] == "text")).and_then(|x| x["text"].as_str()).unwrap_or("").to_string();
    if r["isError"].as_bool().unwrap_or(false) {
        return Err(text.strip_prefix("Error: ").unwrap_or(&text).to_string());
    }
    match r.get("structuredContent") {
        Some(v) => Ok(v.clone()),
        None => serde_json::from_str(&text).map_err(|e| format!("unexpected answer from the window: {e}")),
    }
}

impl Env {
    fn call(&self, name: &str, args: J) -> Result<J, String> {
        match self.route.get() {
            Some(port) => remote(port, name, args),
            None => tools::call(&self.s, name, &args),
        }
    }

    /// The window's project id for `path`, if the window has it open.
    fn window_project(&self, path: &str) -> Option<(u16, u64)> {
        let port = self.window?;
        let list = remote(port, "list_projects", json!({})).ok()?;
        let key = crate::actions::path_key(std::path::Path::new(path));
        list["projects"].as_array()?.iter().find_map(|p| {
            let pp = p["path"].as_str()?;
            (crate::actions::path_key(std::path::Path::new(pp)) == key).then(|| (port, p["project"].as_u64().unwrap_or(0)))
        })
    }

    /// Saves after an edit: locally always; in the window only with `--save` (the person's document
    /// may hold their own unsaved work).
    fn save(&self, id: u64, a: &Args) -> Result<(), String> {
        if self.route.get().is_some() {
            if !a.has("save") {
                self.edited.set(Some(false));
                return Ok(());
            }
            self.call("save_project", json!({"project": id}))?;
            self.edited.set(Some(true));
            return Ok(());
        }
        self.call("save_project", json!({"project": id})).map(|_| ())
    }

    fn job(&self, job: u64) -> Result<JobOut, String> {
        match self.route.get() {
            Some(port) => {
                let j = remote(port, "jobs", json!({"job": job, "wait_seconds": 0.5}))?;
                let status = j["status"].as_str().unwrap_or("failed").to_string();
                let mut message = j["error"].as_str().or_else(|| j["message"].as_str()).unwrap_or("").to_string();
                if status == "running" {
                    message = format!("[{:>3.0}%] {} {}", j["progress"].as_f64().unwrap_or(0.0) * 100.0, j["label"].as_str().unwrap_or(""), message);
                }
                Ok(JobOut { status, message, result: j["result"].clone() })
            }
            None => {
                let j = self.s.jobs.get(job).ok_or("the job disappeared")?;
                let mut message = j.message.clone();
                if j.status == JobStatus::Running {
                    message = format!("[{:>3.0}%] {} {}", j.progress * 100.0, j.label, j.message);
                }
                Ok(JobOut { status: j.status.name().to_string(), message, result: j.result.unwrap_or(J::Null) })
            }
        }
    }

    /// Follows a job to its end, printing progress on `err`.
    fn follow_job(&self, job: u64, err: &mut dyn Write) -> Result<JobOut, String> {
        let mut last = String::new();
        loop {
            let j = self.job(job)?;
            if j.status != "running" {
                if !last.is_empty() {
                    let _ = writeln!(err);
                }
                return Ok(j);
            }
            if j.message != last {
                let _ = write!(err, "\r{:<90}", j.message);
                let _ = err.flush();
                last = j.message.clone();
            }
            if self.route.get().is_none() {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn json_out(out: &mut dyn Write, v: &J) {
    let _ = writeln!(out, "{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

/// The project named by the first positional argument: the window's copy when the window has it open
/// (every later call is routed there), else opened in this process. Returns its id.
fn open(e: &Env, a: &Args) -> Result<u64, Fail> {
    let p = a.pos.first().ok_or_else(|| Fail::Usage("missing <project> (a .aifr file)".into()))?;
    let path = cwd_path(p);
    if let Some((port, id)) = e.window_project(&path) {
        e.route.set(Some(port));
        return Ok(id);
    }
    let r = e.call("open_project", json!({"path": path}))?;
    Ok(r["project"].as_u64().unwrap_or(1))
}

fn cmd_new(e: &Env, a: &Args, out: &mut dyn Write) -> Res {
    let path = a.pos.first().ok_or_else(|| Fail::Usage("missing <path.aifr>".into()))?;
    let abs = cwd_path(path);
    if e.window_project(&abs).is_some() {
        return Err(Fail::Error(format!("{abs} is open in the AIFrames window; close it there before creating a new project over it")));
    }
    let mut args = json!({"path": abs, "overwrite": a.has("force")});
    for k in ["preset", "name"] {
        if let Some(v) = a.val(k) {
            args[k] = json!(v);
        }
    }
    for k in ["fps", "width", "height"] {
        if let Some(v) = a.num(k)? {
            args[k] = json!(v);
        }
    }
    let r = e.call("new_project", args)?;
    if a.has("json") {
        json_out(out, &r);
    } else {
        let st = &r["settings"];
        let tracks: Vec<String> = r["tracks"].as_array().cloned().unwrap_or_default().iter().map(|t| format!("{} {}", t["id"].as_str().unwrap_or(""), t["role"].as_str().unwrap_or(""))).collect();
        let _ = writeln!(out, "Created {} ({}x{}, {} fps; tracks: {})", r["path"].as_str().unwrap_or(path), st["width"], st["height"], st["fps"], tracks.join(", "));
    }
    Ok(0)
}

fn cmd_import(e: &Env, a: &Args, out: &mut dyn Write, err: &mut dyn Write) -> Res {
    if a.pos.len() < 2 {
        return Err(Fail::Usage("usage: aiframes import <project> <files or folders...>".into()));
    }
    let id = open(e, a)?;
    let mut args = json!({"project": id, "paths": a.pos[1..].iter().map(|p| cwd_path(p)).collect::<Vec<_>>(), "place": if a.has("append") { "append" } else { "none" }, "author": a.val("author").unwrap_or("cli"), "transcribe": a.has("transcribe")});
    for k in ["track", "language", "model"] {
        if let Some(v) = a.val(k) {
            args[k] = json!(v);
        }
    }
    let r = e.call("import_media", args)?;
    let mut failed = false;
    if let Some(jobs) = r["transcribe_jobs"].as_array() {
        for j in jobs {
            if let Some(job) = j["job"].as_u64() {
                let info = e.follow_job(job, err)?;
                if info.status != "done" {
                    failed = true;
                    let _ = writeln!(err, "transcription of {} failed: {}", j["asset"].as_str().unwrap_or("?"), info.message);
                }
            }
        }
    }
    if r["imported"].as_array().is_some_and(|v| !v.is_empty()) {
        e.save(id, a)?;
    }
    if a.has("json") {
        json_out(out, &r);
    } else {
        for m in r["imported"].as_array().cloned().unwrap_or_default() {
            let _ = writeln!(out, "+ {} {} ({}, {} s)", m["id"].as_str().unwrap_or(""), m["file"].as_str().unwrap_or(""), m["kind"].as_str().unwrap_or(""), m["duration"]);
        }
        for m in r["skipped"].as_array().cloned().unwrap_or_default() {
            let _ = writeln!(out, "  skipped {}: {}", m["path"].as_str().unwrap_or(""), m["reason"].as_str().unwrap_or(""));
        }
        for t in r["transcripts"].as_array().cloned().unwrap_or_default() {
            let _ = writeln!(out, "  transcript for {} from {} ({} words, {})", t["asset"].as_str().unwrap_or(""), t["file"].as_str().unwrap_or(""), t["words"], t["format"].as_str().unwrap_or(""));
        }
        for n in r["transcribe_notes"].as_array().cloned().unwrap_or_default() {
            let _ = writeln!(out, "  {}", n.as_str().unwrap_or(""));
        }
        if let Some(c) = r["clips"].as_array() {
            let _ = writeln!(out, "Appended clips: {}", c.iter().filter_map(J::as_str).collect::<Vec<_>>().join(", "));
        }
        if let Some(n) = r["note"].as_str() {
            let _ = writeln!(out, "{n}");
        }
    }
    Ok(if failed { 1 } else { 0 })
}

fn cmd_describe(e: &Env, a: &Args, out: &mut dyn Write, timeline: bool) -> Res {
    let id = open(e, a)?;
    if timeline && a.has("json") {
        json_out(out, &e.call("get_timeline", json!({"project": id}))?);
        return Ok(0);
    }
    let mut args = json!({"project": id});
    if timeline {
        args["media"] = json!(false);
        args["notes"] = json!(false);
    }
    for k in ["from", "to"] {
        if let Some(v) = a.num(k)? {
            args[k] = json!(v);
        }
    }
    let r = e.call("describe_project", args)?;
    if a.has("json") {
        json_out(out, &r);
        return Ok(0);
    }
    for l in r["description"].as_array().cloned().unwrap_or_default() {
        let _ = writeln!(out, "{}", l.as_str().unwrap_or(""));
    }
    if !timeline {
        let f = &r["findings"];
        let _ = writeln!(out, "Findings: {} error(s), {} warning(s), {} info (aiframes validate for details)", f["errors"], f["warnings"], f["info"]);
    }
    Ok(0)
}

fn print_findings(out: &mut dyn Write, r: &J) {
    for f in r["findings"].as_array().cloned().unwrap_or_default() {
        let at = f["at"].as_f64().map(|t| format!(" @{}", secs(t))).unwrap_or_default();
        let _ = writeln!(out, "{:<7} {}{at}: {}", f["severity"].as_str().unwrap_or(""), f["code"].as_str().unwrap_or(""), f["message"].as_str().unwrap_or(""));
        if let Some(fix) = f["fix"].as_str() {
            let auto = if f["fix_ops"].is_array() { " (auto-fixable: --fix)" } else { "" };
            let _ = writeln!(out, "        fix: {fix}{auto}");
        }
    }
    let _ = writeln!(out, "{} error(s), {} warning(s), {} info", r["errors"], r["warnings"], r["info"]);
}

fn cmd_validate(e: &Env, a: &Args, out: &mut dyn Write) -> Res {
    let id = open(e, a)?;
    let mut args = json!({"project": id, "apply_fixes": a.has("fix"), "author": a.val("author").unwrap_or("cli")});
    if let Some(sev) = a.val("severity") {
        args["severity"] = json!(sev);
    }
    let r = e.call("validate", args)?;
    let applied = r["fix"]["fixed_findings"].as_u64().unwrap_or(0);
    if applied > 0 {
        e.save(id, a)?;
    }
    if a.has("json") {
        json_out(out, &r);
    } else {
        if a.has("fix") {
            let _ = writeln!(out, "Applied fixes for {applied} finding(s){}", if applied > 0 { " and saved" } else { "" });
        }
        print_findings(out, &r);
    }
    Ok(if r["errors"].as_u64().unwrap_or(0) > 0 { 1 } else { 0 })
}

fn print_receipt(out: &mut dyn Write, r: &J) {
    let planned = r["planned"].as_bool().unwrap_or(false);
    let _ = writeln!(out, "{} r{}: {}", if planned { "Plan (nothing changed)" } else { "Applied" }, r["revision"], r["label"].as_str().unwrap_or(""));
    for d in r["done"].as_array().cloned().unwrap_or_default() {
        let _ = writeln!(out, "  - {}", d.as_str().unwrap_or(""));
    }
    let sp = |v: &J| match v.as_array() {
        Some(x) if x.len() == 2 => format!("{}–{}", secs(x[0].as_f64().unwrap_or(0.0)), secs(x[1].as_f64().unwrap_or(0.0))),
        _ => "—".into(),
    };
    for m in r["moved"].as_array().cloned().unwrap_or_default() {
        let _ = writeln!(out, "  moved {}: {} → {}", m["id"].as_str().unwrap_or(""), sp(&m["from"]), sp(&m["to"]));
    }
    for w in r["warnings"].as_array().cloned().unwrap_or_default() {
        let _ = writeln!(out, "  warning: {}", w.as_str().unwrap_or(""));
    }
    for f in r["new_findings"].as_array().cloned().unwrap_or_default() {
        let _ = writeln!(out, "  new {}: {} {}", f["severity"].as_str().unwrap_or(""), f["code"].as_str().unwrap_or(""), f["message"].as_str().unwrap_or(""));
    }
    let _ = writeln!(out, "  open: {} error(s), {} warning(s); duration {} s", r["open_errors"], r["open_warnings"], r["duration"]);
}

fn cmd_apply(e: &Env, a: &Args, out: &mut dyn Write) -> Res {
    let src = a.pos.get(1).ok_or_else(|| Fail::Usage("usage: aiframes apply <project> <ops.json | ->".into()))?;
    let text = if src == "-" {
        let mut t = String::new();
        std::io::stdin().read_to_string(&mut t).map_err(|e| format!("cannot read stdin: {e}"))?;
        t
    } else {
        std::fs::read_to_string(src).map_err(|e| format!("cannot read {src}: {e}"))?
    };
    let v: J = serde_json::from_str(&text).map_err(|e| format!("{src} is not valid JSON: {e}"))?;
    let ops = match v.get("ops") {
        Some(o) => o.clone(),
        None => v,
    };
    let id = open(e, a)?;
    let mut args = json!({"project": id, "ops": ops, "plan": a.has("plan"), "author": a.val("author").unwrap_or("cli")});
    if let Some(l) = a.val("label") {
        args["label"] = json!(l);
    }
    let r = e.call("edit", args)?;
    if !a.has("plan") {
        e.save(id, a)?;
    }
    if a.has("json") {
        json_out(out, &r);
    } else {
        print_receipt(out, &r);
    }
    Ok(0)
}

fn cmd_snapshot(e: &Env, a: &Args, out: &mut dyn Write) -> Res {
    let id = open(e, a)?;
    let at = a.val("at").ok_or_else(|| Fail::Usage("missing --at (times in seconds, e.g. --at 1,2.5)".into()))?;
    let mut args = json!({"project": id, "at": at, "save_dir": cwd_path(a.out().unwrap_or(".")), "image": false, "guides": a.has("guides")});
    if let Some(sc) = a.num("scale")? {
        args["scale"] = json!(sc);
    }
    let r = e.call("snapshot", args)?;
    if a.has("json") {
        json_out(out, &r);
    } else {
        for f in r["files"].as_array().cloned().unwrap_or_default() {
            let _ = writeln!(out, "{}", f.as_str().unwrap_or(""));
        }
    }
    Ok(0)
}

fn cmd_contact_sheet(e: &Env, a: &Args, out: &mut dyn Write) -> Res {
    let id = open(e, a)?;
    let path = a.out().unwrap_or("contact_sheet.png");
    let mut args = json!({"project": id, "save_path": cwd_path(path), "image": false});
    for k in ["every", "count", "cols", "width", "from", "to"] {
        if let Some(v) = a.num(k)? {
            args[k] = json!(v);
        }
    }
    let r = e.call("contact_sheet", args)?;
    if a.has("json") {
        json_out(out, &r);
    } else {
        let _ = writeln!(out, "{} ({} frames)", r["file"].as_str().unwrap_or(path), r["times"].as_array().map(|v| v.len()).unwrap_or(0));
    }
    Ok(0)
}

fn cmd_render(e: &Env, a: &Args, out: &mut dyn Write, err: &mut dyn Write) -> Res {
    let id = open(e, a)?;
    let mut args = json!({"project": id, "author": "cli"});
    if let Some(o) = a.out() {
        args["path"] = json!(cwd_path(o));
    }
    for k in ["preset", "codec", "quality", "range", "fit"] {
        if let Some(v) = a.val(k) {
            args[k] = json!(v);
        }
    }
    if let Some(size) = a.val("size") {
        let (w, h) = size.split_once(['x', 'X']).ok_or_else(|| Fail::Usage(format!("--size must be WIDTHxHEIGHT, e.g. 1080x1920 (got {size:?})")))?;
        let n = |v: &str| v.trim().parse::<u32>().map_err(|_| Fail::Usage(format!("--size must be WIDTHxHEIGHT, e.g. 1080x1920 (got {size:?})")));
        args["width"] = json!(n(w)?);
        args["height"] = json!(n(h)?);
    }
    if a.has("no-loudness") {
        args["loudness_lufs"] = J::Null;
    }
    if a.has("no-hw") {
        args["hardware"] = json!(false);
    }
    if a.has("force") || a.has("y") {
        args["overwrite"] = json!(true);
    }
    let r = e.call("export", args)?;
    let job = r["job"].as_u64().ok_or_else(|| "export did not start".to_string())?;
    let _ = writeln!(err, "Rendering {}", r["path"].as_str().unwrap_or(""));
    let j = e.follow_job(job, err)?;
    match j.status.as_str() {
        "done" => {
            let rep = j.result.clone();
            if a.has("json") {
                json_out(out, &rep);
            } else {
                let l = &rep["loudness"];
                let lufs = l["integrated_lufs"].as_f64().map(|v| format!(", {} LUFS", num(v))).unwrap_or_default();
                let _ = writeln!(
                    out,
                    "{} — {} s, {}x{}, {} frames, {}{lufs}, rendered in {} s",
                    rep["path"].as_str().unwrap_or(""),
                    rep["duration"],
                    rep["width"],
                    rep["height"],
                    rep["frames"],
                    rep["encoder"].as_str().unwrap_or(""),
                    rep["seconds"]
                );
            }
            Ok(0)
        }
        _ => Err(Fail::Error(format!("render {}: {}", j.status, j.message))),
    }
}

fn cmd_transcribe(e: &Env, a: &Args, out: &mut dyn Write, err: &mut dyn Write) -> Res {
    let target = a.pos.get(1).ok_or_else(|| Fail::Usage("usage: aiframes transcribe <project> <asset or clip>".into()))?;
    let id = open(e, a)?;
    let is_clip = e.call("get_clip", json!({"project": id, "clip": target})).is_ok();
    let mut args = json!({"project": id, "author": a.val("author").unwrap_or("cli")});
    args[if is_clip { "clip" } else { "asset" }] = json!(target);
    for k in ["language", "model", "prompt"] {
        if let Some(v) = a.val(k) {
            args[k] = json!(v);
        }
    }
    let r = e.call("transcribe", args)?;
    let job = r["job"].as_u64().ok_or_else(|| "transcription did not start".to_string())?;
    let j = e.follow_job(job, err)?;
    if j.status != "done" {
        return Err(Fail::Error(format!("transcription {}: {}", j.status, j.message)));
    }
    e.save(id, a)?;
    let rep = j.result;
    if a.has("json") {
        json_out(out, &rep);
    } else {
        let _ = writeln!(out, "Transcribed {} ({}): {} words, language {}", rep["asset"].as_str().unwrap_or(""), rep["file"].as_str().unwrap_or(""), rep["words"], rep["language"].as_str().unwrap_or("?"));
    }
    Ok(0)
}

fn cmd_chapters(e: &Env, a: &Args, out: &mut dyn Write) -> Res {
    let id = open(e, a)?;
    let r = e.call("chapters", json!({"project": id}))?;
    if a.has("json") {
        json_out(out, &r);
    } else {
        let _ = writeln!(out, "{}", r["text"].as_str().unwrap_or(""));
        for p in r["problems"].as_array().cloned().unwrap_or_default() {
            let _ = writeln!(out, "! {}", p.as_str().unwrap_or(""));
        }
    }
    Ok(if r["ok"].as_bool().unwrap_or(false) { 0 } else { 1 })
}

fn cmd_history(e: &Env, a: &Args, out: &mut dyn Write) -> Res {
    let id = open(e, a)?;
    let r = e.call("history", json!({"project": id, "limit": a.num("limit")?.unwrap_or(30.0)}))?;
    if a.has("json") {
        json_out(out, &r);
    } else {
        for e in r["entries"].as_array().cloned().unwrap_or_default() {
            let _ = writeln!(out, "r{:<4} {:<8} {}  {}", e["revision"], e["author"].as_str().unwrap_or(""), e["time"].as_str().unwrap_or(""), e["label"].as_str().unwrap_or(""));
        }
    }
    Ok(0)
}

fn cmd_ops(e: &Env, a: &Args, out: &mut dyn Write) -> Res {
    let r = e.call("list_ops", json!({}))?;
    if a.has("json") {
        json_out(out, &r);
        return Ok(0);
    }
    let mut group = String::new();
    for o in r["ops"].as_array().cloned().unwrap_or_default() {
        let g = o["group"].as_str().unwrap_or("").to_string();
        if g != group {
            let _ = writeln!(out, "\n{g}:");
            group = g;
        }
        let _ = writeln!(out, "  {:<18} {}", o["op"].as_str().unwrap_or(""), o["help"].as_str().unwrap_or(""));
        let _ = writeln!(out, "  {:<18} {}", "", short(&o["example"].to_string(), 140));
    }
    let _ = writeln!(out, "\n{}\n{}", r["time"].as_str().unwrap_or(""), r["params"].as_str().unwrap_or(""));
    Ok(0)
}

fn cmd_demo(e: &Env, a: &Args, out: &mut dyn Write) -> Res {
    let dir = a.pos.first().ok_or_else(|| Fail::Usage("missing <dir> (where to write the demo)".into()))?;
    let dir = crate::util::abs_path(dir, None);
    let d = crate::demo::create(&dir, &e.s.pool)?;
    if a.has("json") {
        json_out(
            out,
            &json!({"project": d.project.to_string_lossy(), "media": d.media.iter().map(|m| m.to_string_lossy().to_string()).collect::<Vec<_>>(), "revision": d.revision, "duration": d.duration, "clips": d.clips, "errors": d.errors, "warnings": d.warnings}),
        );
    } else {
        let _ = writeln!(out, "Demo project: {}", d.project.display());
        let _ = writeln!(out, "  {} clips, {} s, {} media files, revision {}, {} error(s), {} warning(s)", d.clips, secs(d.duration), d.media.len(), d.revision, d.errors.len(), d.warnings);
        for e in &d.errors {
            let _ = writeln!(out, "  error: {e}");
        }
        let _ = writeln!(out, "Try: aiframes describe \"{}\"", d.project.display());
    }
    Ok(if d.errors.is_empty() { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_parse() {
        let a = Args::parse(&["p.aifr".into(), "--at".into(), "1,2".into(), "-o".into(), "out".into(), "--json".into(), "--scale=0.5".into(), "-3".into()]);
        assert_eq!(a.pos, vec!["p.aifr".to_string(), "-3".to_string()]);
        assert_eq!(a.val("at"), Some("1,2"));
        assert_eq!(a.out(), Some("out"));
        assert!(a.has("json"));
        assert_eq!(a.num("scale").unwrap(), Some(0.5));
    }

    #[test]
    fn commands_are_in_sync() {
        for c in CLI_COMMANDS {
            assert!(is_cli_command(c));
            assert!(c.starts_with('-') || *c == "help" || USAGE.contains(&format!("aiframes {c}")), "{c} missing from usage");
        }
        assert!(!is_cli_command("mcp"));
        assert!(!is_cli_command("project.aifr"));
    }
}
