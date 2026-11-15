//! Projects, structure, editing, history, validation and references.

use super::*;
use crate::actions;
use aiframes_core::describe::clip_line;
use aiframes_core::{
    describe_with, source_span, timeline_json, AnchorTarget, Color, DescribeOptions, HistoryEntry, HistoryKind, MediaKind, Settings, Source, Time,
    Track, TrackKind, EFFECTS, OPS, TRANSITIONS,
};
use std::path::PathBuf;

// ---------------------------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------------------------

pub(crate) fn list_projects(s: &Arc<Services>) -> ToolResult {
    let active = s.hub.active().map(|(id, _)| id);
    let list: Vec<J> = s
        .hub
        .list()
        .into_iter()
        .map(|(id, d)| {
            let g = suite_hub::lock(&d);
            let st = &g.doc.project.settings;
            json!({
                "project": id,
                "title": g.title(),
                "name": g.doc.project.name,
                "path": g.doc.path.as_ref().map(|p| p.to_string_lossy().to_string()),
                "size": format!("{}x{} {} fps", st.width, st.height, num(st.fps)),
                "duration": t3(g.doc.resolved().duration),
                "clips": g.doc.project.clips.len(),
                "revision": g.doc.revision(),
                "dirty": g.doc.dirty(),
                "active": Some(id) == active,
            })
        })
        .collect();
    Ok(json!({"projects": list, "window": s.hub.has_ui()}))
}

/// Frame size and rate of a settings preset.
pub(crate) fn preset(name: &str) -> Option<(u32, u32, f64)> {
    Some(match name.trim().to_ascii_lowercase().replace([' ', '-', '_'], "").as_str() {
        "youtube" | "1080p" | "hd" | "fullhd" | "landscape" | "16:9" => (1920, 1080, 30.0),
        "4k" | "uhd" | "2160p" | "youtube4k" => (3840, 2160, 30.0),
        "720p" => (1280, 720, 30.0),
        "shorts" | "vertical" | "reels" | "tiktok" | "portrait" | "9:16" | "youtubeshorts" => (1080, 1920, 30.0),
        "square" | "instagram" | "1:1" => (1080, 1080, 30.0),
        _ => return None,
    })
}

pub(crate) const PRESETS: &str = "youtube (1920x1080), 4k (3840x2160), 720p, shorts (1080x1920), square (1080x1080)";

fn track(id: &str, kind: TrackKind, role: &str, name: &str) -> Track {
    Track { id: id.into(), name: name.into(), kind, role: role.into(), muted: false, hidden: false, locked: false, solo: false, volume: 1.0 }
}

/// A new project from tool/CLI arguments: `preset`, `width`/`height`/`fps`, `name`, `notes`, `tracks`.
pub(crate) fn project_from_args(args: &J) -> Result<Project, String> {
    let (mut w, mut h, mut fps) = (1920, 1080, 30.0);
    if let Some(p) = arg_str(args, "preset") {
        (w, h, fps) = preset(p).ok_or_else(|| format!("unknown preset '{p}'. Presets: {PRESETS}"))?;
    }
    if let Some(v) = arg_int(args, "width", 16, 16384)? {
        w = v as u32;
    }
    if let Some(v) = arg_int(args, "height", 16, 16384)? {
        h = v as u32;
    }
    if let Some(v) = arg_f64(args, "fps")? {
        fps = v;
    }
    if !(16..=16384).contains(&w) || !(16..=16384).contains(&h) {
        return Err(format!("frame size {w}x{h} is out of range (16–16384 px)"));
    }
    if !(1.0..=240.0).contains(&fps) {
        return Err(format!("fps {fps} is out of range (1–240)"));
    }
    let mut p = Project { name: arg_str(args, "name").unwrap_or("Untitled").to_string(), ..Default::default() };
    p.settings = Settings { width: w, height: h, fps, ..Settings::default() };
    if let Some(bg) = arg_str(args, "background") {
        p.settings.background = Color::parse(bg).ok_or_else(|| format!("invalid background colour '{bg}' (use \"#RRGGBB\")"))?;
    }
    if let Some(n) = arg_str(args, "notes") {
        p.notes = n.to_string();
    }
    if arg_bool(args, "tracks", true)? {
        p.tracks = vec![
            track("v1", TrackKind::Video, "main", "Main"),
            track("v2", TrackKind::Video, "overlay", "Overlay"),
            track("a1", TrackKind::Audio, "voice", "Voice"),
            track("a2", TrackKind::Audio, "music", "Music"),
        ];
    }
    Ok(p)
}

fn aifr_path(p: &str, base: Option<&Path>) -> PathBuf {
    let mut pb = abs_path(p, base);
    if pb.extension().is_none_or(|e| !e.eq_ignore_ascii_case("aifr")) {
        let name = pb.file_name().map(|n| format!("{}.aifr", n.to_string_lossy())).unwrap_or_else(|| "project.aifr".into());
        pb.set_file_name(name);
    }
    pb
}

pub(crate) fn new_project(s: &Arc<Services>, args: &J) -> ToolResult {
    let p = project_from_args(args)?;
    let target = match arg_str(args, "path") {
        Some(path) => {
            let pb = aifr_path(path, None);
            if pb.exists() && !arg_bool(args, "overwrite", false)? {
                return Err(format!("{} already exists; open it with open_project, choose another path, or pass overwrite: true", pb.display()));
            }
            check_save_as(s, u64::MAX, None, &pb, true)?;
            Some(pb)
        }
        None => None,
    };
    let id = s.add_project(p);
    let d = s.hub.get(id).ok_or("internal error: project vanished")?;
    let mut g = suite_hub::lock(&d);
    if let Some(pb) = &target {
        if let Err(e) = g.doc.save_as(pb) {
            drop(g);
            s.hub.remove(id);
            return Err(e.to_string());
        }
    }
    let title = g.title();
    let st = g.doc.project.settings.clone();
    let tracks: Vec<J> = g.doc.project.tracks.iter().map(|t| json!({"id": t.id, "kind": t.kind, "role": t.role})).collect();
    drop(g);
    s.hub.notify(format!("AI: created project {title}"));
    let mut res = json!({"project": id, "title": title, "settings": st, "tracks": tracks});
    if let Some(pb) = target {
        res["path"] = json!(pb.to_string_lossy());
    } else {
        res["note"] = json!("Not saved yet: call save_project with a .aifr path.");
    }
    Ok(res)
}

pub(crate) fn open_project(s: &Arc<Services>, args: &J) -> ToolResult {
    let path = abs_path(req_str(args, "path")?, None);
    let show = arg_bool(args, "show", true)?;
    if s.hub.find_path(&path).is_none() && !path.exists() {
        return Err(format!("file not found: {}", path.display()));
    }
    // show:false never changes which project the person is looking at (also when already open).
    let (id, was_open) = s.open_project_with(&path, show)?;
    let already = was_open.then_some(id);
    if show {
        s.hub.request_focus();
    }
    let d = s.hub.get(id).ok_or("internal error: project vanished")?;
    let mut reloaded = None;
    if was_open && arg_bool(args, "reload", false)? {
        // Take the file's version (written by another program) as one undoable step.
        let mut g = suite_hub::lock(&d);
        let rc = g.doc.reload_from_disk(&author_of(args), "Reload from disk").map_err(|e| e.to_string())?;
        g.mark_ai_changed_by(touched_ids(&rc), rc.revision, session_of(args).as_ref());
        reloaded = Some(receipt_json(&rc, false));
        drop(g);
        s.hub.notify(format!("{}: reloaded {} from disk", who(&author_of(args)), path.display()));
        s.hub.wake();
    }
    let g = suite_hub::lock(&d);
    let f = g.doc.validate();
    let (e, w, _) = counts(&f);
    let title = g.title();
    let res = json!({
        "project": id,
        "title": title,
        "path": path.to_string_lossy(),
        "already_open": already.is_some(),
        "name": g.doc.project.name,
        "duration": t3(g.doc.resolved().duration),
        "clips": g.doc.project.clips.len(),
        "media": g.doc.project.media.len(),
        "revision": g.doc.revision(),
        "open_errors": e,
        "open_warnings": w,
        "next": "describe_project shows the whole edit",
    });
    let mut res = res;
    if let Some(r) = reloaded {
        res["reloaded"] = r;
    } else if g.doc.disk_changed() {
        res["disk_changed"] = json!(true);
        res["note"] = json!("the file on disk changed since this window opened it: open_project with reload:true loads that version");
    }
    drop(g);
    if already.is_none() {
        s.hub.notify(format!("AI: opened {title}"));
    }
    Ok(res)
}

/// Checks a Save As target: never another open project's file (or its history/autosave) or anyone's
/// media; an existing file other than this project's own needs `overwrite`.
fn check_save_as(s: &Services, own: u64, own_path: Option<&Path>, target: &Path, overwrite: bool) -> Result<(), String> {
    use crate::actions::{autosave_path, path_key};
    let key = path_key(target);
    for (id, d) in s.hub.list() {
        let g = suite_hub::lock(&d);
        if id != own {
            if let Some(pf) = &g.doc.path {
                if [pf.clone(), aiframes_core::document::history_path(pf), autosave_path(pf)].iter().any(|f| path_key(f) == key) {
                    return Err(format!("{} belongs to the open project {} (id {id}); refusing to save another project over it. Choose another path.", target.display(), g.title()));
                }
            }
        }
        if let Some(m) = g.doc.project.media.iter().find(|m| path_key(&g.doc.media_path(m)) == key) {
            return Err(format!("{} is media {} of {}; refusing to save a project over it.", target.display(), m.id, g.title()));
        }
    }
    let is_own = own_path.is_some_and(|p| path_key(p) == key);
    if target.exists() && !is_own && !overwrite {
        return Err(format!("{} already exists; pass overwrite: true to replace it, or choose another path", target.display()));
    }
    Ok(())
}

pub(crate) fn save_project(s: &Arc<Services>, args: &J) -> ToolResult {
    let (id, d) = find_doc(s, args)?;
    let target = match arg_str(args, "path") {
        Some(p) => {
            let own = suite_hub::lock(&d).doc.path.clone();
            let base = own.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf);
            let pb = aifr_path(p, base.as_deref());
            check_save_as(s, id, own.as_deref(), &pb, arg_bool(args, "overwrite", false)?)?;
            Some(pb)
        }
        None => None,
    };
    // Never overwrite what another program (or another window) wrote since we opened/saved it, unless
    // asked explicitly (core refuses with DiskChanged; `overwrite_external` uses the force save).
    let force = arg_bool(args, "overwrite_external", false)?;
    let mut g = suite_hub::lock(&d);
    let refused = |e: aiframes_core::DocError, file: &Path| -> String {
        if e.kind == aiframes_core::document::DocErrorKind::DiskChanged {
            changed_on_disk(e.path.as_deref().unwrap_or(file), id)
        } else {
            e.to_string()
        }
    };
    let path = match target {
        Some(pb) => {
            let r = if force { g.doc.save_as_force(&pb) } else { g.doc.save_as(&pb) };
            r.map_err(|e| refused(e, &pb))?;
            pb
        }
        None => {
            let Some(file) = g.doc.path.clone() else {
                return Err("this project has never been saved; pass 'path' (e.g. C:\\Users\\me\\Videos\\edit.aifr)".into());
            };
            let r = if force { g.doc.save_force() } else { g.doc.save() };
            r.map_err(|e| refused(e, &file))?;
            file
        }
    };
    let title = g.title();
    let rev = g.doc.revision();
    drop(g);
    s.hub.notify(format!("AI: saved {title}"));
    Ok(json!({"saved": path.to_string_lossy(), "revision": rev}))
}

/// The refusal when the project file changed on disk since this document last read or wrote it.
fn changed_on_disk(file: &Path, id: u64) -> String {
    format!(
        "{} was changed by another program since it was opened (or last saved here); saving would overwrite that. Check what changed: open_project {{\"path\": \"{}\", \"reload\": true}} loads the file's version (your unsaved edits stay undoable), or after checking call save_project {{\"project\": {id}, \"overwrite_external\": true}} to replace it with this version.",
        file.display(),
        file.to_string_lossy().replace('\\', "/")
    )
}

pub(crate) fn close_project(s: &Arc<Services>, args: &J) -> ToolResult {
    let (id, d) = find_doc(s, args)?;
    let (dirty, title) = {
        let g = suite_hub::lock(&d);
        (g.doc.dirty(), g.title())
    };
    if arg_bool(args, "save", false)? {
        save_project(s, &json!({"project": id, "path": args.get("path"), "overwrite": args.get("overwrite"), "overwrite_external": args.get("overwrite_external")}))?;
    } else if dirty && !arg_bool(args, "discard", false)? {
        return Err(format!("'{title}' has unsaved changes. Pass save:true to save first, or discard:true to close without saving."));
    }
    s.hub.remove(id);
    s.hub.notify(format!("AI: closed {title}"));
    Ok(json!({"closed": title}))
}

// ---------------------------------------------------------------------------------------------
// Structure
// ---------------------------------------------------------------------------------------------

/// The text description as lines (readable inside JSON).
pub(crate) fn describe_lines(p: &Project, r: &Resolved, o: &DescribeOptions) -> Vec<String> {
    let lines: Vec<String> = describe_with(p, r, o).lines().map(str::to_string).collect();
    match o.range {
        Some(range) if o.media => media_in_range(lines, p, r, range),
        _ => lines,
    }
}

/// With a time range, the media list only names the media the clips in that range use.
fn media_in_range(lines: Vec<String>, p: &Project, r: &Resolved, (a, b): (f64, f64)) -> Vec<String> {
    let used: HashSet<&str> = p
        .clips
        .iter()
        .filter(|c| r.span(&c.id).is_some_and(|s| s.end >= a && s.start <= b))
        .filter_map(|c| match &c.source {
            Source::Media { asset, .. } => Some(asset.as_str()),
            _ => None,
        })
        .collect();
    let Some(head) = lines.iter().position(|l| l.starts_with("Media (")) else { return lines };
    let mut end = head + 1;
    let mut kept = Vec::new();
    let mut keep = false;
    let mut total = 0;
    while end < lines.len() && lines[end].starts_with("  ") {
        let l = &lines[end];
        if !l.starts_with("    ") {
            total += 1;
            keep = l.split_whitespace().next().is_some_and(|id| used.contains(id));
        }
        if keep {
            kept.push(l.clone());
        }
        end += 1;
    }
    let shown = kept.iter().filter(|l| !l.starts_with("    ")).count();
    let mut out: Vec<String> = lines[..head].to_vec();
    out.push(if shown == 0 { format!("Media used in this range: none (of {total})") } else { format!("Media used in this range ({shown} of {total}):") });
    out.extend(kept);
    out.extend(lines[end..].iter().cloned());
    out
}

fn range_args(args: &J) -> Result<Option<(f64, f64)>, String> {
    let from = arg_f64(args, "from")?;
    let to = arg_f64(args, "to")?;
    Ok(match (from, to) {
        (None, None) => None,
        (a, b) => Some((a.unwrap_or(0.0), b.unwrap_or(f64::MAX))),
    })
}

pub(crate) fn describe_project(s: &Arc<Services>, args: &J) -> ToolResult {
    let o = DescribeOptions {
        range: range_args(args)?,
        max_clips_per_track: arg_int_clamped(args, "max_clips", 1, 100_000)?.map(|v| v as usize),
        media: arg_bool(args, "media", true)?,
        notes: arg_bool(args, "notes", true)?,
    };
    let has_ui = s.hub.has_ui();
    let jobs = s.jobs.running();
    with_doc(s, args, |id, g| {
        let f = g.doc.validate();
        let (e, w, i) = counts(&f);
        let top: Vec<String> = f.iter().filter(|f| f.severity != Severity::Info).take(8).map(|f| format!("{:?} {}: {}", f.severity, f.code, f.message).to_lowercase_first()).collect();
        let mut res = json!({
            "project": id,
            "title": g.title(),
            "revision": g.doc.revision(),
            "dirty": g.doc.dirty(),
            "description": describe_lines(&g.doc.project, g.doc.resolved(), &o),
            "findings": {"errors": e, "warnings": w, "info": i},
        });
        if let Some(p) = &g.doc.path {
            res["path"] = json!(p.to_string_lossy());
        }
        if !top.is_empty() {
            res["findings"]["top"] = json!(top);
            res["findings"]["next"] = json!("validate lists every finding with its fix and fix_ops");
        }
        let mine: Vec<J> = jobs.iter().filter(|j| j.doc == Some(id)).map(|j| j.to_json()).collect();
        if !mine.is_empty() {
            res["jobs"] = J::Array(mine);
        }
        if has_ui {
            let sel = &g.live.selection;
            res["live"] = json!({"playhead": t3(g.live.playhead), "playing": g.live.playing, "selection": sel});
        }
        Ok(res)
    })
}

trait LowerFirst {
    fn to_lowercase_first(self) -> String;
}
impl LowerFirst for String {
    fn to_lowercase_first(self) -> String {
        let mut c = self.chars();
        match c.next() {
            Some(f) => f.to_lowercase().chain(c).collect(),
            None => self,
        }
    }
}

pub(crate) fn get_timeline(s: &Arc<Services>, args: &J) -> ToolResult {
    let range = range_args(args)?;
    with_doc(s, args, |id, g| {
        let mut j = timeline_json(&g.doc.project, g.doc.resolved());
        if let Some((a, b)) = range {
            if let Some(tracks) = j.get_mut("tracks").and_then(J::as_array_mut) {
                for t in tracks {
                    if let Some(clips) = t.get_mut("clips").and_then(J::as_array_mut) {
                        clips.retain(|c| match (c["start"].as_f64(), c["end"].as_f64()) {
                            (Some(s), Some(e)) => e >= a && s <= b,
                            _ => true,
                        });
                    }
                }
            }
            if let Some(m) = j.get_mut("markers").and_then(J::as_array_mut) {
                m.retain(|m| m["time"].as_f64().is_none_or(|t| t >= a && t <= b));
            }
            j["range"] = json!([a, if b == f64::MAX { J::Null } else { json!(b) }]);
        }
        j["project"] = json!(id);
        j["revision"] = json!(g.doc.revision());
        Ok(j)
    })
}

pub(crate) fn get_clip(s: &Arc<Services>, args: &J) -> ToolResult {
    let id = req_str(args, "clip")?;
    with_doc(s, args, |_, g| {
        let p = &g.doc.project;
        let r = g.doc.resolved();
        let c = clip_of(p, id)?;
        let mut j = json!({"clip": c, "line": clip_line(p, r, c, true), "param_paths": aiframes_core::param_paths(c)});
        match r.span(id) {
            Some(sp) => j["span"] = span_j(sp.start, sp.end),
            None => j["error"] = json!(r.error_for(id).map(|e| e.message.clone()).unwrap_or_else(|| "not placed".into())),
        }
        if let Some((a, b)) = source_span(c) {
            j["source_range"] = span_j(a, b);
        }
        if let Source::Media { asset, .. } = &c.source {
            if let Some(m) = p.media.iter().find(|m| &m.id == asset) {
                j["media"] = json!({"id": m.id, "file": file_name(&m.path), "kind": m.kind, "duration": t3(m.info.duration), "has_audio": m.info.has_audio});
            }
            if let (Some(t), Some((a, b))) = (p.transcripts.get(asset), source_span(c)) {
                let inside: Vec<usize> = t.words.iter().enumerate().filter(|(_, w)| w.start >= a - 1e-6 && w.start < b - 1e-6).map(|(i, _)| i).collect();
                if let (Some(&f), Some(&l)) = (inside.first(), inside.last()) {
                    let text = aiframes_core::join_words(&t.words[f..=l].iter().take(80).map(|w| w.text.as_str()).collect::<Vec<_>>());
                    let mut words = json!({"from": f, "to": l, "text": text});
                    if l - f + 1 > 80 {
                        words["truncated"] = json!(true);
                    }
                    j["words"] = words;
                }
            }
        }
        let trans: Vec<J> = p
            .transitions
            .iter()
            .filter(|t| t.from == id || t.to == id)
            .map(|t| json!({"id": t.id, "kind": t.kind, "duration": t.duration, "from": t.from, "to": t.to}))
            .collect();
        if !trans.is_empty() {
            j["transitions"] = J::Array(trans);
        }
        let depends = |t: &Time| match t {
            Time::Anchored(a) => match &a.to {
                AnchorTarget::After(x) | AnchorTarget::With(x) | AnchorTarget::Before(x) => x == id,
                AnchorTarget::Word(w) => w.clip == id,
                AnchorTarget::Marker(_) => false,
            },
            Time::At(_) => false,
        };
        let deps: Vec<&str> =
            p.clips.iter().filter(|o| depends(&o.start)).map(|o| o.id.as_str()).chain(p.markers.iter().filter(|m| depends(&m.time)).map(|m| m.id.as_str())).collect();
        if !deps.is_empty() {
            j["anchored_to_it"] = json!(deps);
        }
        Ok(j)
    })
}

pub(crate) fn list_media(s: &Arc<Services>, args: &J) -> ToolResult {
    with_doc(s, args, |id, g| {
        let p = &g.doc.project;
        let list: Vec<J> = p
            .media
            .iter()
            .map(|m| {
                let users: Vec<&str> = p.clips.iter().filter(|c| matches!(&c.source, Source::Media { asset, .. } if *asset == m.id)).map(|c| c.id.as_str()).collect();
                let mut j = json!({"id": m.id, "file": file_name(&m.path), "path": m.path, "kind": m.kind, "duration": t3(m.info.duration), "has_audio": m.info.has_audio, "used_by": users});
                if m.kind != MediaKind::Audio {
                    j["size"] = json!(format!("{}x{}", m.info.width, m.info.height));
                }
                if m.info.fps > 0.0 {
                    j["fps"] = json!(r(m.info.fps, 3));
                }
                if !m.name.is_empty() {
                    j["name"] = json!(m.name);
                }
                if !m.notes.is_empty() {
                    j["notes"] = json!(m.notes);
                }
                if !m.tags.is_empty() {
                    j["tags"] = json!(m.tags);
                }
                if let Some(t) = p.transcripts.get(&m.id) {
                    j["transcript_words"] = json!(t.words.len());
                }
                if !g.doc.media_path(m).exists() {
                    j["offline"] = json!(true);
                }
                j
            })
            .collect();
        Ok(json!({"project": id, "media": list}))
    })
}

pub(crate) fn import_media(s: &Arc<Services>, args: &J) -> ToolResult {
    let raw: Vec<String> = arg_strs(args, "paths")?.unwrap_or_default();
    if raw.is_empty() {
        return Err("pass 'paths' (files and/or folders to import)".into());
    }
    let place = arg_str(args, "place").unwrap_or("none").to_ascii_lowercase();
    if !matches!(place.as_str(), "none" | "append") {
        return Err(format!("'place' must be \"none\" or \"append\" (got {place:?})"));
    }
    let use_sidecars = arg_bool(args, "sidecars", true)?;
    let (doc_id, d) = find_doc(s, args)?;
    let (project, file) = {
        let g = suite_hub::lock(&d);
        (g.doc.project.clone(), g.doc.path.clone())
    };
    let base = file.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf);
    let paths: Vec<PathBuf> = raw.iter().map(|p| abs_path(p, base.as_deref())).collect();
    let plan = actions::import_plan_for(&project, file.as_deref(), &s.pool, &paths)?;
    let skipped: Vec<J> = plan.skipped.iter().map(|(p, why)| json!({"path": p.to_string_lossy(), "reason": why})).collect();
    if plan.ops.is_empty() {
        return Ok(json!({"imported": [], "skipped": skipped, "note": "nothing new to import"}));
    }
    let imported: Vec<J> = plan
        .assets
        .iter()
        .map(|a| {
            let mut j = json!({"id": a.id, "file": a.path.file_name().map(|n| n.to_string_lossy().to_string()), "kind": a.kind, "duration": t3(a.info.duration), "has_audio": a.info.has_audio});
            if a.kind != MediaKind::Audio {
                j["size"] = json!(format!("{}x{}", a.info.width, a.info.height));
            }
            j
        })
        .collect();
    let mut ops = plan.ops.clone();
    // Word timings that already exist next to the media (`<name>.words.json`, whisper JSON, SRT,
    // VTT) become the transcript in the same step: no transcription, exact spelling.
    let mut sidecars = Vec::new();
    let mut with_transcript = std::collections::HashSet::new();
    if use_sidecars {
        for a in plan.assets.iter().filter(|a| a.kind != MediaKind::Image && a.info.has_audio) {
            if let Some((p, imp)) = crate::transcripts::find_sidecar(&a.path) {
                sidecars.push(json!({"asset": a.id, "file": p.to_string_lossy(), "format": imp.format, "words": imp.transcript.words.len()}));
                ops.push(actions::op(json!({"op": "set_transcript", "asset": a.id, "transcript": imp.transcript}))?);
                with_transcript.insert(a.id.clone());
            }
        }
    }
    let mut clips = Vec::new();
    if place == "append" {
        let (more, c) = actions::append_ops(&project, &plan.assets, arg_str(args, "track"))?;
        ops.extend(more);
        clips = c;
    }
    let n = plan.assets.len();
    let label = if n == 1 { format!("Import {}", plan.assets[0].path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default()) } else { format!("Import {n} files") };
    let planned = arg_bool(args, "plan", false)?;
    let mut res = commit_with(s, args, &label, ops, |j| {
        j["imported"] = J::Array(imported);
        if !skipped.is_empty() {
            j["skipped"] = J::Array(skipped);
        }
        if !clips.is_empty() {
            j["clips"] = json!(clips);
        }
        if !sidecars.is_empty() {
            j["transcripts"] = J::Array(sidecars);
            j["transcripts_note"] = json!("word timings were imported from the sidecar files next to the media (sidecars: false skips them)");
        }
    })?;
    if arg_bool(args, "transcribe", false)? && !planned {
        let author = author_of(args);
        let mut jobs = Vec::new();
        let mut notes = Vec::new();
        for a in plan.assets.iter().filter(|a| a.kind != MediaKind::Image && a.info.has_audio) {
            if with_transcript.contains(&a.id) {
                notes.push(format!("{}: not transcribed, its sidecar transcript was imported", a.id));
                continue;
            }
            match actions::start_transcribe_by(s, doc_id, &a.id, &author, transcribe_options(args)?, session_of(args)) {
                Ok(j) => jobs.push(json!({"job": j, "asset": a.id})),
                Err(e) => notes.push(format!("{}: {e}", a.id)),
            }
        }
        res["transcribe_jobs"] = J::Array(jobs);
        if !notes.is_empty() {
            res["transcribe_notes"] = json!(notes);
        }
    }
    Ok(res)
}

/// Whisper options from tool arguments.
pub(crate) fn transcribe_options(args: &J) -> Result<aiframes_media::TranscribeOptions, String> {
    let mut o = aiframes_media::TranscribeOptions::default();
    if let Some(m) = arg_str(args, "model") {
        o.model = m.to_string();
    }
    o.language = arg_str(args, "language").map(String::from);
    o.prompt = arg_str(args, "prompt").map(String::from);
    Ok(o)
}

// ---------------------------------------------------------------------------------------------
// Editing and history
// ---------------------------------------------------------------------------------------------

pub(crate) fn edit(s: &Arc<Services>, args: &J) -> ToolResult {
    let ops = parse_ops(args.get("ops").unwrap_or(&J::Null))?;
    commit(s, args, "", ops)
}

pub(crate) fn undo_redo(s: &Arc<Services>, args: &J, undo: bool) -> ToolResult {
    let steps = arg_int_clamped(args, "steps", 1, 100)?.unwrap_or(1) as usize;
    let author = author_of(args);
    let word = if undo { "undo" } else { "redo" };
    let (id, d) = find_doc(s, args)?;
    let mut g = suite_hub::lock(&d);
    let mut done = Vec::new();
    let mut moved: Vec<J> = Vec::new();
    let mut touched: Vec<Id> = Vec::new();
    let mut by_others = Vec::new();
    for _ in 0..steps {
        let rc = if undo { g.doc.undo_by(&author) } else { g.doc.redo_by(&author) };
        let Some(rc) = rc else { break };
        let target = g.doc.history().last().and_then(|e| e.target);
        let original = target.and_then(|t| g.doc.history().iter().find(|e| e.revision == t && e.kind == HistoryKind::Edit)).map(|e| e.author.clone());
        if let Some(o) = &original {
            if o != &author {
                by_others.push(format!("{} (by {o})", rc.label));
            }
        }
        done.push(json!({"label": rc.label, "revision": rc.revision, "edit": target, "by": original}));
        if let Some(m) = receipt_json(&rc, false).get("moved") {
            moved.extend(m.as_array().cloned().unwrap_or_default());
        }
        touched.extend(touched_ids(&rc));
    }
    if done.is_empty() {
        return Err(format!("nothing to {word}"));
    }
    let rev = g.doc.revision();
    let f = g.doc.validate();
    let (e, w, _) = counts(&f);
    g.mark_ai_changed_by(touched, rev, session_of(args).as_ref());
    let label = done[0]["label"].as_str().unwrap_or("").to_string();
    let mut res = json!({
        "project": id,
        "revision": rev,
        "open_errors": e,
        "open_warnings": w,
        "can_undo": g.doc.can_undo(),
        "can_redo": g.doc.can_redo(),
    });
    res[if undo { "undone" } else { "redone" }] = J::Array(done);
    if !moved.is_empty() {
        res["moved"] = J::Array(moved.into_iter().take(40).collect());
    }
    if !by_others.is_empty() {
        res["note"] = json!(format!("this {word} affected edits made by someone else: {}", by_others.join("; ")));
    }
    drop(g);
    s.hub.notify(format!("{}: {label}", who(&author)));
    s.hub.wake();
    Ok(res)
}

fn entry_json(e: &HistoryEntry, ops: bool) -> J {
    let mut j = json!({"revision": e.revision, "author": e.author, "label": e.label, "time": e.time});
    if e.kind != HistoryKind::Edit {
        j["kind"] = json!(e.kind);
    }
    if let Some(t) = e.target {
        j["target"] = json!(t);
    }
    if !e.summary.is_empty() && e.summary != e.label {
        j["summary"] = json!(e.summary);
    }
    if ops && !e.ops.is_empty() {
        j["ops"] = json!(e.ops);
    }
    j
}

pub(crate) fn history(s: &Arc<Services>, args: &J) -> ToolResult {
    let limit = arg_int_clamped(args, "limit", 1, 100_000)?.unwrap_or(30) as usize;
    let since = arg_usize(args, "since")?.map(|v| v as u64);
    let author = arg_str(args, "by");
    let ops = arg_bool(args, "ops", false)?;
    with_doc(s, args, |id, g| {
        let h = g.doc.history();
        let sel: Vec<&HistoryEntry> = h.iter().filter(|e| since.is_none_or(|r| e.revision > r)).filter(|e| author.is_none_or(|a| e.author == a)).collect();
        let total = sel.len();
        let entries: Vec<J> = sel.iter().skip(total.saturating_sub(limit)).map(|e| entry_json(e, ops)).collect();
        Ok(json!({
            "project": id,
            "revision": g.doc.revision(),
            "can_undo": g.doc.can_undo(),
            "undo": g.doc.undo_label(),
            "can_redo": g.doc.can_redo(),
            "redo": g.doc.redo_label(),
            "entries": entries,
            "older_entries": total.saturating_sub(limit),
        }))
    })
}

pub(crate) fn changes_since(s: &Arc<Services>, args: &J) -> ToolResult {
    let since = arg_usize(args, "revision")?.ok_or("missing 'revision' (the revision you last saw, from an edit receipt or describe_project)")? as u64;
    let exclude = arg_str(args, "exclude_author");
    let ops = arg_bool(args, "ops", true)?;
    with_doc(s, args, |id, g| {
        let all = g.doc.changes_since(since);
        let entries: Vec<J> = all.iter().filter(|e| exclude.is_none_or(|x| e.author != x)).map(|e| entry_json(e, ops)).collect();
        let mut authors = serde_json::Map::new();
        for e in &all {
            let n = authors.get(&e.author).and_then(J::as_u64).unwrap_or(0) + 1;
            authors.insert(e.author.clone(), json!(n));
        }
        let mut res = json!({"project": id, "since": since, "revision": g.doc.revision(), "entries": entries, "by_author": authors});
        if all.is_empty() {
            res["note"] = json!("nothing changed since that revision");
        }
        Ok(res)
    })
}

pub(crate) fn validate_tool(s: &Arc<Services>, args: &J) -> ToolResult {
    let min = match arg_str(args, "severity").unwrap_or("info").to_ascii_lowercase().as_str() {
        "error" | "errors" => Severity::Error,
        "warning" | "warnings" => Severity::Warning,
        "info" | "all" => Severity::Info,
        other => return Err(format!("'severity' must be error, warning or info (got {other:?})")),
    };
    let codes = arg_strs(args, "codes")?;
    let limit = arg_int_clamped(args, "limit", 1, 10_000)?.unwrap_or(60) as usize;
    let fix = arg_bool(args, "apply_fixes", false)?;
    let keep = |f: &Finding| f.severity <= min && codes.as_ref().is_none_or(|c| c.iter().any(|x| x == f.code));
    let mut fixed = None;
    if fix {
        // Accept every finding's fix_ops that still apply on top of the earlier ones.
        let (ops, n, skipped) = with_doc(s, args, |_, g| {
            let mut work = g.doc.project.clone();
            let mut ops = Vec::new();
            let mut n = 0;
            let mut skipped = Vec::new();
            for f in g.doc.validate().iter().filter(|f| keep(f) && !f.fix_ops.is_empty()) {
                let mut trial = work.clone();
                match apply_batch(&mut trial, &f.fix_ops) {
                    Ok(_) => {
                        work = trial;
                        ops.extend(f.fix_ops.iter().cloned());
                        n += 1;
                    }
                    Err(e) => skipped.push(format!("{} ({}): {e}", f.code, f.ids.join(","))),
                }
            }
            Ok((ops, n, skipped))
        })?;
        if ops.is_empty() {
            fixed = Some(json!({"applied": 0, "note": "no finding has machine-applicable fix_ops", "skipped": skipped}));
        } else {
            let mut rc = commit(s, args, &format!("Fix {n} validation finding(s)"), ops)?;
            rc["fixed_findings"] = json!(n);
            if !skipped.is_empty() {
                rc["skipped"] = json!(skipped);
            }
            fixed = Some(rc);
        }
    }
    with_doc(s, args, |id, g| {
        let all = g.doc.validate();
        let (e, w, i) = counts(&all);
        let sel: Vec<&Finding> = all.iter().filter(|f| keep(f)).collect();
        let list: Vec<J> = sel.iter().take(limit).map(|f| finding_json(f, true)).collect();
        let mut res = json!({"project": id, "revision": g.doc.revision(), "errors": e, "warnings": w, "info": i, "findings": list});
        if sel.len() > limit {
            res["more"] = json!(sel.len() - limit);
        }
        if sel.is_empty() {
            res["note"] = json!("no findings at this level: the project is clean");
        }
        if let Some(f) = fixed {
            res["fix"] = f;
        }
        Ok(res)
    })
}

// ---------------------------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------------------------

pub(crate) const TIME_HELP: &str = "Time (clip start, marker time): seconds (12.5) or an anchor {\"after\":\"c3\"}, {\"after\":\"c3\",\"offset\":0.4}, {\"with\":\"c3\",\"offset\":1.2}, {\"before\":\"c7\"}, {\"marker\":\"mk1\"}, {\"word\":{\"clip\":\"c3\",\"index\":17},\"offset\":-0.1} (\"end\":true = the word's end). Prefer anchors: they keep relationships when things move.";
pub(crate) const PARAM_HELP: &str = "Animatable numbers (opacity, volume, speed, transform.*, effect params, source.draw) are a constant or {\"keys\":[{\"t\":0,\"v\":1},{\"t\":2,\"v\":1.2,\"ease\":\"in_out\"}]} with t in CLIP-LOCAL seconds (0 = the clip's first frame). Eases: linear, hold, in, out, in_out, back_out, elastic, {\"bezier\":[x1,y1,x2,y2]}.";

pub(crate) fn list_ops(args: &J) -> ToolResult {
    let group = arg_str(args, "group");
    let name = arg_str(args, "op");
    let list: Vec<J> = OPS
        .iter()
        .filter(|o| group.is_none_or(|g| o.group == g) && name.is_none_or(|n| o.name == n))
        .map(|o| json!({"op": o.name, "group": o.group, "help": o.help, "example": serde_json::from_str::<J>(o.example).unwrap_or(J::Null)}))
        .collect();
    if list.is_empty() {
        if let Some(n) = name {
            return Err(format!("unknown op '{n}'.{}", did_you_mean(n, OPS.iter().map(|o| o.name))));
        }
        let mut groups: Vec<&str> = OPS.iter().map(|o| o.group).collect();
        groups.dedup();
        return Err(format!("no ops in group '{}'. Groups: {}", group.unwrap_or(""), groups.join(", ")));
    }
    let mut res = json!({"ops": list, "time": TIME_HELP, "params": PARAM_HELP, "usage": "pass ops to the edit tool as one array (atomic, one undo step); plan:true is a dry run"});
    if group.is_none() && name.is_none() || matches!(name, Some("set_param" | "add_keyframe" | "remove_keyframe")) || group == Some("clips") {
        res["clip_params"] = json!(aiframes_core::CLIP_PARAMS);
    }
    if group.is_none() && name.is_none() || group == Some("audio") || name.is_some_and(|n| n.ends_with("ducking")) {
        res["ducking_params"] = json!(aiframes_core::DUCKING_PARAMS);
    }
    Ok(res)
}

pub(crate) fn list_effects(args: &J) -> ToolResult {
    let cat = arg_str(args, "category");
    let kind = arg_str(args, "kind");
    let list: Vec<J> = EFFECTS.iter().filter(|e| cat.is_none_or(|c| e.category == c) && kind.is_none_or(|k| e.kind == k)).map(|e| json!(e)).collect();
    if list.is_empty() {
        if let Some(k) = kind {
            return Err(format!("unknown effect '{k}'.{}", did_you_mean(k, EFFECTS.iter().map(|e| e.kind))));
        }
    }
    Ok(json!({"effects": list, "usage": "add with {\"op\":\"add_effect\",\"clip\":\"c3\",\"effect\":{\"kind\":\"blur\",\"params\":{\"radius\":8}}}; params may be animated with clip-local keys"}))
}

/// The JSON shape of project types (core's `schema_docs`): the index, one type / value form / enum,
/// or everything (`type: "all"`). Names are matched loosely: `TextSource`, `text_source` and `text`
/// all find the text source; `TextBox` finds `text_box`.
pub(crate) fn schema(args: &J) -> ToolResult {
    let docs = aiframes_core::schema_docs();
    let Some(want) = arg_str(args, "type") else {
        let list = |k: &str| -> Vec<J> {
            docs[k].as_object().map(|o| o.iter().map(|(n, v)| json!({"name": n, "about": v["about"].as_str().unwrap_or("")})).collect()).unwrap_or_default()
        };
        let enums: Vec<&String> = docs["enums"].as_object().map(|o| o.keys().collect()).unwrap_or_default();
        return Ok(json!({
            "about": docs["about"],
            "types": list("types"),
            "forms": list("forms"),
            "enums": enums,
            "next": "schema {\"type\": \"clip\"} (or a form like \"time\", \"param\", \"until\", or an enum) gives every field with its type, default, units and an example; \"all\" gives everything",
        }));
    };
    if want.eq_ignore_ascii_case("all") {
        return Ok(docs);
    }
    // CamelCase → snake_case, then the model's Rust names → the docs' names.
    let mut snake = String::new();
    for (i, ch) in want.trim().chars().enumerate() {
        if ch.is_uppercase() && i > 0 && !snake.ends_with('_') {
            snake.push('_');
        }
        snake.extend(ch.to_lowercase());
    }
    let key = match snake.as_str() {
        "text_source" => "text",
        "shape_source" => "shape",
        "media_asset" => "media",
        "export_settings" => "export",
        "anchor" | "anchor_target" => "time",
        "keys" | "keyframes" => "param",
        "colour" => "color",
        "word_highlight" => "highlight",
        "text_reveal" => "reveal",
        other => other,
    }
    .to_string();
    for (section, label) in [("types", "type"), ("forms", "form"), ("enums", "enum")] {
        if let Some(v) = docs[section].get(&key) {
            let mut out = json!({"name": key, "kind": label});
            out[if section == "enums" { "values" } else { "doc" }] = v.clone();
            if section == "types" {
                out["note"] = json!("fields marked required must be present; optional ones may be left out (their default applies)");
            }
            return Ok(out);
        }
    }
    let names: Vec<String> = ["types", "forms", "enums"].iter().flat_map(|s| docs[*s].as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default()).collect();
    Err(format!("unknown schema type '{want}'.{} Known: {}", did_you_mean(&key, names.iter().map(String::as_str)), names.join(", ")))
}

pub(crate) fn list_transitions() -> ToolResult {
    Ok(json!({"transitions": TRANSITIONS, "usage": "{\"op\":\"add_transition\",\"from\":\"c3\",\"to\":\"c4\",\"kind\":\"crossfade\",\"duration\":0.5} — the clips must touch on one track; it is centred on the cut"}))
}

pub(crate) fn chapters(s: &Arc<Services>, args: &J) -> ToolResult {
    with_doc(s, args, |id, g| {
        let (list, text, problems) = actions::chapters(&g.doc.project, g.doc.resolved());
        let ch: Vec<J> = list.iter().map(|c| json!({"time": t3(c.time), "title": c.title, "marker": c.marker})).collect();
        Ok(json!({"project": id, "text": text, "chapters": ch, "ok": problems.is_empty(), "problems": problems}))
    })
}

pub(crate) fn auto_ducking(s: &Arc<Services>, args: &J) -> ToolResult {
    let ops = with_doc(s, args, |_, g| actions::auto_ducking_ops(&g.doc.project, arg_str(args, "target"), arg_str(args, "trigger"), arg_f64(args, "amount_db")?))?;
    commit(s, args, "", ops)
}
