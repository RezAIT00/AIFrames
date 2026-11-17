//! Logic shared by the GUI and the AI tools, so the person and the AI get identical behaviour:
//! importing media, transcription and export jobs, silence removal, captions placement, chapters,
//! ducking and beat markers.

use crate::util::{secs, t3, timestamp};
use crate::{Services, AI_AUTHOR};
use aiframes_core::document::portable_path;
use aiframes_core::ops::IdGen;
use aiframes_core::{
    local_time_of_source, source_span, AnchorTarget, Document, ExportSettings, Id, MarkerKind, MediaAsset, MediaInfo, MediaKind, Op, Project, Resolved, Source,
    Time, TrackKind,
};
use aiframes_media::{MediaPool, TranscribeOptions, VideoCodec};
use serde_json::{json, Value as J};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Builds an op from JSON (internal ops are always well-formed; the error is kept for safety).
pub fn op(v: J) -> Result<Op, String> {
    Op::from_json(&v).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------------------------

pub const VIDEO_EXTS: &[&str] = &["mp4", "mov", "mkv", "webm", "avi", "m4v", "mts", "m2ts", "ts", "mpg", "mpeg", "wmv", "flv", "3gp", "mxf", "ogv"];
pub const AUDIO_EXTS: &[&str] = &["wav", "mp3", "m4a", "aac", "flac", "ogg", "opus", "wma", "aif", "aiff", "caf"];
pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "jfif", "webp", "bmp", "tif", "tiff", "gif", "tga", "avif", "heic", "heif", "exr", "qoi", "psd"];

/// True when the file extension is a known video, audio or image type.
pub fn is_media_file(p: &Path) -> bool {
    let e = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    VIDEO_EXTS.contains(&e.as_str()) || AUDIO_EXTS.contains(&e.as_str()) || IMAGE_EXTS.contains(&e.as_str())
}

/// Expands folders (recursively, up to 4 levels, sorted by name) into media files. Explicit files are
/// kept even with unknown extensions (probe decides). Returns the files and what was skipped and why.
pub fn scan_paths(paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<(PathBuf, String)>) {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        entries.sort_by_key(|p| p.to_string_lossy().to_lowercase());
        for p in entries {
            let hidden = p.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(false);
            if hidden {
                continue;
            }
            if p.is_dir() {
                if depth < 4 {
                    walk(&p, depth + 1, out);
                }
            } else if is_media_file(&p) {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for p in paths {
        if p.is_dir() {
            let before = files.len();
            walk(p, 0, &mut files);
            if files.len() == before {
                skipped.push((p.clone(), "folder contains no media files".to_string()));
            }
        } else if p.is_file() {
            files.push(p.clone());
        } else {
            skipped.push((p.clone(), "not found".to_string()));
        }
    }
    (files, skipped)
}

/// A media file accepted for import.
#[derive(Clone, Debug)]
pub struct ImportedAsset {
    pub id: Id,
    pub path: PathBuf,
    pub kind: MediaKind,
    pub info: MediaInfo,
}

/// What an import would do: `add_media` ops (one per new file), the assets, and skipped paths.
#[derive(Clone, Debug, Default)]
pub struct ImportPlan {
    pub ops: Vec<Op>,
    pub assets: Vec<ImportedAsset>,
    pub skipped: Vec<(PathBuf, String)>,
}

/// A comparable key for a path (canonical when it or its folder exists; case-insensitive on Windows).
pub fn path_key(p: &Path) -> String {
    let canon = std::fs::canonicalize(p).ok().or_else(|| {
        let parent = p.parent().filter(|d| !d.as_os_str().is_empty())?;
        Some(std::fs::canonicalize(parent).ok()?.join(p.file_name()?))
    });
    let s = canon.unwrap_or_else(|| p.to_path_buf()).to_string_lossy().replace('\\', "/");
    let s = s.strip_prefix("//?/").map(str::to_string).unwrap_or(s);
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

// ---------------------------------------------------------------------------------------------
// File safety
// ---------------------------------------------------------------------------------------------

/// The autosave file the app keeps next to a project: `<project>.aifr.autosave`.
pub fn autosave_path(project_file: &Path) -> PathBuf {
    let name = project_file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    project_file.with_file_name(format!("{name}.autosave"))
}

/// Why writing `path` (an export, a rendered image) would destroy something AIFrames owns or uses,
/// or `None` when it is safe: an open project's file, its history or autosave, any AIFrames project
/// file, or a media file of any open project.
pub fn protected_target(s: &Services, path: &Path) -> Option<String> {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    if name.ends_with(".aifr") || name.ends_with(".aifr.autosave") || name.ends_with(".aifr.history.jsonl") {
        return Some(format!("{} is an AIFrames project file (or its history/autosave)", path.display()));
    }
    let key = path_key(path);
    for (_, d) in s.hub.list() {
        let g = suite_hub::lock(&d);
        if let Some(pf) = &g.doc.path {
            for f in [pf.clone(), aiframes_core::document::history_path(pf), autosave_path(pf)] {
                if path_key(&f) == key {
                    return Some(format!("{} belongs to the open project {}", path.display(), g.title()));
                }
            }
        }
        for m in &g.doc.project.media {
            if path_key(&g.doc.media_path(m)) == key {
                return Some(format!("{} is media {} of the project {}", path.display(), m.id, g.title()));
            }
        }
    }
    None
}

/// Refuses protected targets (see [`protected_target`]) and, unless `overwrite`, existing files.
pub fn check_output(s: &Services, path: &Path, overwrite: bool, what: &str) -> Result<(), String> {
    if let Some(why) = protected_target(s, path) {
        return Err(format!("refusing to write the {what} there: {why}. Choose another path."));
    }
    if path.is_dir() {
        return Err(format!("{} is a folder; give a file path for the {what}", path.display()));
    }
    if path.exists() && !overwrite {
        return Err(format!("{} already exists; pass overwrite: true to replace it, or choose another path", path.display()));
    }
    Ok(())
}

/// Probes `paths` (folders are scanned for media) and plans `add_media` ops with fresh ids and paths
/// relative to the project folder when possible. Files already in the project are skipped.
pub fn import_plan(doc: &Document, pool: &MediaPool, paths: &[PathBuf]) -> Result<ImportPlan, String> {
    import_plan_for(&doc.project, doc.path.as_deref(), pool, paths)
}

/// [`import_plan`] for a project (a copy, so probing need not hold the document lock) saved at
/// `project_file` (`None` = never saved: media paths stay absolute).
pub fn import_plan_for(project: &Project, project_file: Option<&Path>, pool: &MediaPool, paths: &[PathBuf]) -> Result<ImportPlan, String> {
    if paths.is_empty() {
        return Err("no files given to import".into());
    }
    let dir = match project_file.and_then(|p| p.parent()) {
        Some(d) if !d.as_os_str().is_empty() => Some(d.to_path_buf()),
        _ => None,
    };
    let base = dir.clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let (files, mut skipped) = scan_paths(paths);
    let existing: HashMap<String, &MediaAsset> = project.media.iter().map(|m| (path_key(&crate::util::abs_path(&m.path, Some(&base))), m)).collect();
    let mut seen: HashSet<String> = existing.keys().cloned().collect();
    let mut todo = Vec::new();
    let mut refreshed = Vec::new();
    for f in files {
        let abs = std::fs::canonicalize(&f).map(|p| strip_verbatim(&p)).unwrap_or(f.clone());
        let key = path_key(&abs);
        if !seen.insert(key.clone()) {
            // Already imported: if the file was replaced since (a re-rendered still, a new cut of a
            // video), refresh the stored probe info so fit, duration and audio follow the new file.
            if let (Some(m), Ok((_, info))) = (existing.get(&key), pool.probe(&abs)) {
                if media_info_changed(&m.info, &info) {
                    refreshed.push((m.id.clone(), m.path.clone(), info));
                    skipped.push((f, "already in the project (the file changed: its info was refreshed)".into()));
                    continue;
                }
            }
            skipped.push((f, "already in the project".into()));
            continue;
        }
        todo.push(abs);
    }
    // Probe in parallel (ffprobe takes ~50–100 ms per file).
    type Probed = (PathBuf, Result<(MediaKind, MediaInfo), String>);
    let probed: Vec<Probed> = std::thread::scope(|sc| {
        let chunk = todo.len().div_ceil(8).max(1);
        let handles: Vec<_> = todo
            .chunks(chunk)
            .map(|part| sc.spawn(move || part.iter().map(|p| (p.clone(), pool.probe(p).map_err(|e| e.to_string()))).collect::<Vec<_>>()))
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    let mut ids = IdGen::new(project);
    let mut plan = ImportPlan { skipped, ..Default::default() };
    for (id, path, info) in refreshed {
        plan.ops.push(op(json!({"op": "relink_media", "asset": id, "path": path, "info": info}))?);
    }
    for (path, r) in probed {
        match r {
            Ok((kind, info)) => {
                let id = ids.fresh("m");
                let stored = match &dir {
                    Some(d) => portable_path(&path, d),
                    None => path.to_string_lossy().replace('\\', "/"),
                };
                plan.ops.push(op(json!({"op": "add_media", "id": id, "path": stored, "kind": kind, "info": info}))?);
                plan.assets.push(ImportedAsset { id, path, kind, info });
            }
            Err(e) => plan.skipped.push((path, e)),
        }
    }
    if plan.ops.is_empty() && plan.skipped.iter().any(|(_, why)| !why.starts_with("already in the project")) {
        let why: Vec<String> = plan.skipped.iter().map(|(p, w)| format!("{}: {w}", p.display())).collect();
        return Err(format!("nothing could be imported: {}", why.join("; ")));
    }
    Ok(plan)
}

/// Whether a re-probe of a file differs from what the project stored (size, length, streams, rate).
fn media_info_changed(old: &MediaInfo, new: &MediaInfo) -> bool {
    old.width != new.width
        || old.height != new.height
        || (old.duration - new.duration).abs() > 1e-3
        || old.has_video != new.has_video
        || old.has_audio != new.has_audio
        || (old.fps - new.fps).abs() > 1e-3
}

fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

/// Probes `paths` (folders are scanned for media) and returns `add_media` ops for them, with paths
/// relative to the project folder when possible. Skips files already in the project.
pub fn import_ops(doc: &Document, pool: &MediaPool, paths: &[PathBuf]) -> Result<Vec<Op>, String> {
    import_plan(doc, pool, paths).map(|p| p.ops)
}

/// Ops that append the imported assets magnetically to the end of a track: pictures to `track` (or
/// the main video track), sound-only files to `track` if it is an audio track (or the first audio
/// track). Missing tracks are created. Returns the ops and the new clip ids.
pub fn append_ops(p: &Project, assets: &[ImportedAsset], track: Option<&str>) -> Result<(Vec<Op>, Vec<Id>), String> {
    let mut ids = IdGen::new(p);
    for a in assets {
        ids.claim(&a.id).map_err(|e| e.message)?;
    }
    let mut ops = Vec::new();
    let mut clips = Vec::new();
    let given = match track {
        Some(t) => Some(p.tracks.iter().find(|x| x.id == t).ok_or_else(|| format!("unknown track '{t}'"))?),
        None => None,
    };
    let mut video: Option<Id> = given.filter(|t| t.kind == TrackKind::Video).map(|t| t.id.clone()).or_else(|| main_video_track(p));
    let mut audio: Option<Id> =
        given.filter(|t| t.kind == TrackKind::Audio).map(|t| t.id.clone()).or_else(|| p.tracks.iter().find(|t| t.kind == TrackKind::Audio).map(|t| t.id.clone()));
    for a in assets {
        let picture = a.kind != MediaKind::Audio;
        let slot = if picture { &mut video } else { &mut audio };
        let t = match slot {
            Some(t) => t.clone(),
            None => {
                let (prefix, kind, role, name) = if picture { ("v", "video", "main", "Main") } else { ("a", "audio", "voice", "Audio") };
                let id = ids.fresh(prefix);
                ops.push(op(json!({"op": "add_track", "id": id, "kind": kind, "role": role, "name": name}))?);
                *slot = Some(id.clone());
                id
            }
        };
        let cid = ids.fresh("c");
        ops.push(op(json!({"op": "append_clip", "track": t, "clip": {"id": cid, "asset": a.id}}))?);
        clips.push(cid);
    }
    Ok((ops, clips))
}

/// The track a picture edit goes to by default: role "main", else the bottom video track.
pub fn main_video_track(p: &Project) -> Option<Id> {
    p.tracks
        .iter()
        .find(|t| t.kind == TrackKind::Video && t.role == "main")
        .or_else(|| p.tracks.iter().find(|t| t.kind == TrackKind::Video))
        .map(|t| t.id.clone())
}

// ---------------------------------------------------------------------------------------------
// Jobs: transcription and export
// ---------------------------------------------------------------------------------------------

/// Starts a Whisper transcription job for `asset` of hub document `doc`; when it finishes the
/// transcript is stored with a `set_transcript` op authored by `author`. Returns the job id.
pub fn start_transcribe(s: &Arc<Services>, doc: u64, asset: &str, author: &str) -> Result<u64, String> {
    start_transcribe_with(s, doc, asset, author, TranscribeOptions::default())
}

/// [`start_transcribe`] with explicit options (model, language, prompt, range). Progress and cancel
/// are wired to the job.
pub fn start_transcribe_with(s: &Arc<Services>, doc: u64, asset: &str, author: &str, opts: TranscribeOptions) -> Result<u64, String> {
    start_transcribe_by(s, doc, asset, author, opts, None)
}

/// [`start_transcribe_with`] recording the MCP session that asked for it on the stored transcript
/// ([`crate::LiveState::ai_sessions`]).
pub fn start_transcribe_by(s: &Arc<Services>, doc: u64, asset: &str, author: &str, mut opts: TranscribeOptions, session: Option<crate::AiSession>) -> Result<u64, String> {
    let d = s.hub.get(doc).ok_or_else(|| format!("project {doc} is not open"))?;
    let (path, name, kind, has_audio) = {
        let g = suite_hub::lock(&d);
        let m = g.doc.project.media.iter().find(|m| m.id == asset).ok_or_else(|| unknown_asset(&g.doc.project, asset))?;
        (g.doc.media_path(m), crate::util::file_name(&m.path).to_string(), m.kind, m.info.has_audio)
    };
    if kind == MediaKind::Image || !has_audio {
        return Err(format!("{asset} ({name}) has no audio to transcribe"));
    }
    if !path.exists() {
        return Err(format!("the file of {asset} is missing: {} (relink it first)", path.display()));
    }
    let services = s.clone();
    let asset = asset.to_string();
    let author = author.to_string();
    let label = format!("Transcribe {name}");
    let id = s.jobs.spawn("transcribe", &label, Some(doc), move |ctx| {
        let c2 = ctx.clone();
        opts.progress = Some(Arc::new(move |f: f32| c2.progress(f * 0.98, "transcribing")));
        opts.cancel = Some(ctx.cancel_flag());
        ctx.progress(0.0, "starting whisper");
        let started = std::time::Instant::now();
        let t = aiframes_media::transcribe(&path, &opts).map_err(|e| e.to_string())?;
        let words = t.words.len();
        let language = t.language.clone();
        let d = services.hub.get(doc).ok_or("the project was closed before the transcription finished")?;
        let (receipt, forced) = {
            let mut g = suite_hub::lock(&d);
            // Checked under the lock: nothing may change between this and the apply.
            if ctx.cancelled() {
                return Err("cancelled; the transcript was not stored".into());
            }
            let now = g.doc.project.media.iter().find(|m| m.id == asset).map(|m| g.doc.media_path(m));
            match now {
                None => return Err(format!("{asset} was removed from the project while it was being transcribed; the transcript was not stored")),
                Some(p) if path_key(&p) != path_key(&path) => {
                    return Err(format!(
                        "{asset} was relinked to {} while {} was being transcribed; the transcript was not stored (transcribe again)",
                        p.display(),
                        path.display()
                    ))
                }
                Some(_) => {}
            }
            let set = |force: bool| op(json!({"op": "set_transcript", "asset": asset, "transcript": t, "force": force}));
            let label = format!("Transcribe {name}");
            let r = match g.doc.apply(&author, &label, vec![set(false)?]) {
                Ok(r) => (r, false),
                // A new transcript can break word anchors into the old one; keep the work and report it.
                Err(e) if e.message.contains("word anchors") => (g.doc.apply(&author, &label, vec![set(true)?]).map_err(|e| e.to_string())?, true),
                Err(e) => return Err(e.to_string()),
            };
            if author != "user" {
                let rev = r.0.revision;
                g.mark_ai_changed_by(vec![asset.clone()], rev, session.as_ref());
            }
            r
        };
        let who = if author == AI_AUTHOR { "AI".to_string() } else { author.clone() };
        services.hub.notify(format!("{who}: transcribed {name} ({words} words)"));
        services.hub.wake();
        let mut res = json!({"asset": asset, "file": name, "words": words, "language": language, "revision": receipt.revision, "seconds": t3(started.elapsed().as_secs_f64())});
        if forced || !receipt.warnings.is_empty() {
            res["warnings"] = json!(receipt.warnings);
        }
        Ok(res)
    });
    Ok(id)
}

/// Where an export goes by default: the export settings' path (relative to the project folder),
/// else `<project folder>/<project file stem>.<ext>`. `None` for a never-saved project without one.
pub fn default_export_path(doc: &Document, settings: &ExportSettings) -> Option<PathBuf> {
    if let Some(p) = &settings.path {
        return Some(crate::util::abs_path(p, Some(&doc.project_dir())));
    }
    let path = doc.path.as_ref()?;
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "export".into());
    let ext = export_extension(&settings.codec);
    let name = if ext.is_empty() { format!("{stem}_frames") } else { format!("{stem}.{ext}") };
    Some(doc.project_dir().join(name))
}

/// File extension for an export codec ("" = a folder of PNGs).
pub fn export_extension(codec: &str) -> &'static str {
    if codec.eq_ignore_ascii_case("wav") {
        return "wav";
    }
    VideoCodec::parse(codec).map(VideoCodec::extension).unwrap_or("mp4")
}

/// Starts an export job for hub document `doc`. Returns the job id.
pub fn start_export(s: &Arc<Services>, doc: u64, settings: ExportSettings, out: PathBuf) -> Result<u64, String> {
    let d = s.hub.get(doc).ok_or_else(|| format!("project {doc} is not open"))?;
    let input = suite_hub::lock(&d).render_input();
    if input.resolved.duration <= 0.0 && settings.range.is_none() {
        return Err("the timeline is empty; add clips before exporting".into());
    }
    // Never render over the project, its history/autosave, or media (of this or any open project).
    if let Some(why) = protected_target(s, &out) {
        return Err(format!("refusing to export over {}: {why}. Choose another path.", out.display()));
    }
    if out.is_dir() && !settings.codec.eq_ignore_ascii_case("png_sequence") {
        return Err(format!("{} is a folder; give a file path for the export", out.display()));
    }
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
    }
    let services = s.clone();
    let name = out.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let label = format!("Export {name}");
    let id = s.jobs.spawn("export", &label, Some(doc), move |ctx| {
        let mut last = 0.0f32;
        let mut progress = |p: aiframes_render::ExportProgress| {
            let f = match p.stage {
                "prepare" => 0.0,
                "done" => 1.0,
                _ if p.frames > 0 => (p.frame as f32 / p.frames as f32).clamp(0.0, 1.0) * 0.97,
                _ => last,
            };
            last = f.max(last);
            let msg = if p.frames > 0 {
                format!("{} frame {}/{} · {:.0} s left", p.stage, p.frame, p.frames, p.eta.max(0.0))
            } else {
                p.stage.to_string()
            };
            ctx.progress(last, &msg);
            !ctx.cancelled()
        };
        let report = aiframes_render::export(&input.dir, &input.project, services.pool.clone(), &settings, &out, &mut progress).map_err(|e| e.to_string())?;
        let mut res = json!({
            "path": report.path.to_string_lossy(),
            "duration": t3(report.duration),
            "frames": report.frames,
            "width": report.width,
            "height": report.height,
            "encoder": report.encoder,
            "seconds": t3(report.seconds),
        });
        if let Some(l) = report.loudness {
            res["loudness"] = json!({"integrated_lufs": crate::util::r(l.integrated_lufs, 1), "true_peak_db": crate::util::r(l.true_peak_db, 1), "lra": crate::util::r(l.lra, 1)});
        }
        if let Ok(m) = std::fs::metadata(&report.path) {
            res["bytes"] = json!(m.len());
        }
        let lufs = report.loudness.map(|l| format!(", {:.1} LUFS", l.integrated_lufs)).unwrap_or_default();
        services.hub.notify(format!("Exported {name} ({} s, {}{lufs})", secs(report.duration), report.encoder));
        Ok(res)
    });
    Ok(id)
}

fn unknown_asset(p: &Project, asset: &str) -> String {
    let ids: Vec<&str> = p.media.iter().map(|m| m.id.as_str()).collect();
    format!("unknown media asset '{asset}'.{} Media: {}", crate::util::did_you_mean(asset, ids.iter().copied()), if ids.is_empty() { "none".into() } else { ids.join(", ") })
}

// ---------------------------------------------------------------------------------------------
// Transcript placement
// ---------------------------------------------------------------------------------------------

/// Where a transcript word plays on the timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct WordPlace {
    pub clip: Id,
    /// Timeline seconds.
    pub start: f64,
    pub end: f64,
}

/// The media clips playing `asset` that are placed on the timeline (only `only_clip` when given),
/// earliest first, with their start and source range.
fn users_of<'a>(p: &'a Project, r: &Resolved, asset: &str, only_clip: Option<&str>) -> Vec<(&'a aiframes_core::Clip, f64, (f64, f64))> {
    let mut users: Vec<(&aiframes_core::Clip, f64, (f64, f64))> = p
        .clips
        .iter()
        .filter(|c| matches!(&c.source, Source::Media { asset: a, .. } if a == asset))
        .filter(|c| only_clip.is_none_or(|o| o == c.id))
        .filter_map(|c| Some((c, r.span(&c.id)?.start, source_span(c)?)))
        .collect();
    users.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.id.cmp(&b.0.id)));
    users
}

/// For every word of `asset`'s transcript: every place it plays on the timeline, through the clips
/// using that media (only `only_clip` when given), earliest first. Places at the same timeline time
/// (a picture and its linked detached sound) are listed once. Empty = cut out.
pub fn word_places_all(p: &Project, r: &Resolved, asset: &str, only_clip: Option<&str>) -> Vec<Vec<WordPlace>> {
    let Some(t) = p.transcripts.get(asset) else { return Vec::new() };
    let users = users_of(p, r, asset, only_clip);
    t.words
        .iter()
        .map(|w| {
            let mut v: Vec<WordPlace> = Vec::new();
            for (c, start, (lo, hi)) in &users {
                if w.start < lo - 1e-6 || w.start >= hi - 1e-6 {
                    continue;
                }
                let Some(a) = local_time_of_source(c, w.start) else { continue };
                let b = local_time_of_source(c, w.end.min(*hi)).unwrap_or(c.duration).max(a);
                let place = WordPlace { clip: c.id.clone(), start: start + a, end: start + b };
                if !v.iter().any(|x| (x.start - place.start).abs() < 1e-6) {
                    v.push(place);
                }
            }
            v
        })
        .collect()
}

/// For every word of `asset`'s transcript: where it first plays on the timeline (see
/// [`word_places_all`] for every place). `None` = cut out.
pub fn word_places(p: &Project, r: &Resolved, asset: &str, only_clip: Option<&str>) -> Vec<Option<WordPlace>> {
    word_places_all(p, r, asset, only_clip).into_iter().map(|v| v.into_iter().next()).collect()
}

/// Every timeline place where source times `times` of `asset` play (through `only_clip` when given),
/// as `(clip, timeline seconds)`, earliest first, one entry per distinct time. `margin` keeps away
/// from the clip edges (e.g. a scene cut exactly at an in-point is not a cut inside the clip).
pub fn source_places(p: &Project, r: &Resolved, asset: &str, only_clip: Option<&str>, times: &[f64], margin: f64) -> Vec<Vec<(Id, f64)>> {
    let users = users_of(p, r, asset, only_clip);
    times
        .iter()
        .map(|&st| {
            let mut v: Vec<(Id, f64)> = Vec::new();
            for (c, start, (lo, hi)) in &users {
                if st < lo + margin || st > hi - margin {
                    continue;
                }
                let Some(l) = local_time_of_source(c, st) else { continue };
                let t = start + l;
                if !v.iter().any(|(_, x)| (x - t).abs() < 1e-6) {
                    v.push((c.id.clone(), t));
                }
            }
            v
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Silence removal
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct SilenceOptions {
    /// Level (dBFS) below which audio counts as silent.
    pub noise_db: f64,
    /// Shortest silence (seconds, inside the clip) that is cut.
    pub min_len: f64,
    /// Silence kept on each side of a cut (seconds), so speech does not sound clipped.
    pub padding: f64,
}

impl Default for SilenceOptions {
    fn default() -> Self {
        SilenceOptions { noise_db: -35.0, min_len: 0.6, padding: 0.12 }
    }
}

/// `cut_source` ops, the cuts they make, and notes about skipped clips.
pub type SilencePlan = (Vec<Op>, Vec<SilenceCut>, Vec<String>);

/// One planned silence cut.
#[derive(Clone, Debug, PartialEq)]
pub struct SilenceCut {
    pub clip: Id,
    /// Source seconds removed.
    pub source: [f64; 2],
    /// Where that is on the timeline now (before any cut).
    pub timeline: [f64; 2],
}

/// Whether a media asset has sound worth analysing (a picture-only file is "all silence" to the
/// detector, and cutting its silences would delete the whole shot).
pub fn has_sound(p: &Project, asset: &str) -> bool {
    p.media.iter().any(|m| m.id == asset && m.kind != MediaKind::Image && m.info.has_audio)
}

/// The clip of a linked group (`clip` plus [`aiframes_core::linked_partners`]) that the others
/// follow: the one not anchored `with` another member. Editing it carries the whole group.
pub fn link_leader(p: &Project, clip: &str) -> Id {
    let mut group = aiframes_core::linked_partners(p, clip);
    group.push(clip.to_string());
    group
        .iter()
        .find(|id| {
            p.clips.iter().find(|c| &c.id == *id).is_some_and(|c| match &c.start {
                Time::Anchored(a) => !matches!(&a.to, AnchorTarget::With(x) if group.contains(x)),
                Time::At(_) => true,
            })
        })
        .cloned()
        .unwrap_or_else(|| clip.to_string())
}

/// `cut_source` ops removing the silences (per asset, in source seconds) inside `clips`, keeping
/// `padding` on each side. Cuts within a clip go from the end backwards so the clip's id always names
/// the piece that still contains the next cut.
///
/// * Clips whose media has no sound (pictures, b-roll without audio) are skipped with a note.
/// * Linked clips (a picture and its detached sound, [`aiframes_core::linked_partners`]) are cut once,
///   through their leader, so picture and sound stay in sync; a listed follower is cut through it.
/// * A clip listed twice (or together with its linked partner) is cut once.
pub fn silence_cut_ops(
    p: &Project,
    r: &Resolved,
    clips: &[Id],
    silences: &HashMap<Id, Vec<[f64; 2]>>,
    o: &SilenceOptions,
) -> Result<SilencePlan, String> {
    let mut ops = Vec::new();
    let mut cuts = Vec::new();
    let mut notes = Vec::new();
    let mut seen: HashSet<Id> = HashSet::new();
    let mut handled_by: HashMap<Id, Id> = HashMap::new();
    for id in clips {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(leader) = handled_by.get(id) {
            notes.push(format!("{id} is linked to {leader}; it is cut together with it"));
            continue;
        }
        let c = p.clips.iter().find(|c| &c.id == id).ok_or_else(|| format!("unknown clip '{id}'"))?;
        let Source::Media { asset, .. } = &c.source else {
            notes.push(format!("{id} is not a media clip; skipped"));
            continue;
        };
        if !has_sound(p, asset) {
            notes.push(format!("{id} plays {asset}, which has no sound; skipped (it is not silence, it is a picture)"));
            continue;
        }
        let leader_id = link_leader(p, id);
        let partners = aiframes_core::linked_partners(p, &leader_id);
        for x in partners.iter().chain(std::iter::once(&leader_id)) {
            handled_by.insert(x.clone(), leader_id.clone());
        }
        seen.insert(leader_id.clone());
        if leader_id != *id {
            notes.push(format!("{id} is the linked sound of {leader_id}; cut through {leader_id} so picture and sound stay in sync"));
        }
        let c = p.clips.iter().find(|c| c.id == leader_id).unwrap_or(c);
        if c.reverse {
            notes.push(format!("{} plays backwards; skipped", c.id));
            continue;
        }
        let (Some(span), Some((lo, hi))) = (r.span(&c.id), source_span(c)) else {
            notes.push(format!("{} is not placed on the timeline; skipped", c.id));
            continue;
        };
        let Some(sil) = silences.get(asset) else { continue };
        let mut mine = Vec::new();
        for s in sil {
            let (a0, b0) = (s[0].max(lo), s[1].min(hi));
            if b0 - a0 < o.min_len {
                continue;
            }
            let (a, b) = (a0 + o.padding, b0 - o.padding);
            if b - a < 0.05 {
                continue;
            }
            let (Some(la), Some(lb)) = (local_time_of_source(c, a), local_time_of_source(c, b)) else { continue };
            mine.push(SilenceCut { clip: c.id.clone(), source: [a, b], timeline: [span.start + la.min(lb), span.start + la.max(lb)] });
        }
        mine.sort_by(|x, y| y.source[0].total_cmp(&x.source[0]));
        for cut in &mine {
            ops.push(op(json!({"op": "cut_source", "clip": c.id, "from": t3(cut.source[0]), "to": t3(cut.source[1])}))?);
        }
        mine.reverse();
        cuts.extend(mine);
    }
    Ok((ops, cuts, notes))
}

// ---------------------------------------------------------------------------------------------
// Chapters, ducking, beats
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Chapter {
    pub time: f64,
    pub title: String,
    pub marker: Id,
}

/// YouTube chapters from the project's chapter markers: the list, the description text ("0:00 Intro"
/// per line) and the rules it breaks (first at 0:00, at least 3, each at least 10 s long).
pub fn chapters(p: &Project, r: &Resolved) -> (Vec<Chapter>, String, Vec<String>) {
    let mut list: Vec<Chapter> = p
        .markers
        .iter()
        .filter(|m| m.kind == MarkerKind::Chapter)
        .filter_map(|m| Some(Chapter { time: r.marker(&m.id)?, title: if m.name.is_empty() { m.id.clone() } else { m.name.clone() }, marker: m.id.clone() }))
        .collect();
    list.sort_by(|a, b| a.time.total_cmp(&b.time));
    let long = r.duration >= 3600.0;
    let text: Vec<String> = list.iter().map(|c| format!("{} {}", timestamp(c.time, long), c.title)).collect();
    let mut problems = Vec::new();
    let unresolved = p.markers.iter().filter(|m| m.kind == MarkerKind::Chapter && r.marker(&m.id).is_none()).count();
    if unresolved > 0 {
        problems.push(format!("{unresolved} chapter marker(s) cannot be placed (unresolved anchor); see validate"));
    }
    if list.is_empty() {
        problems.push("no chapter markers; add markers with kind \"chapter\" (the first at 0 s)".into());
    } else {
        if list[0].time >= 1.0 {
            problems.push(format!("the first chapter must start at 0:00 (it starts at {}); add a chapter marker at 0 s", timestamp(list[0].time, long)));
        }
        if list.len() < 3 {
            problems.push(format!("YouTube needs at least 3 chapters (there are {})", list.len()));
        }
        for w in list.windows(2) {
            if w[1].time - w[0].time < 10.0 {
                problems.push(format!("chapter {:?} is only {} s long; chapters must be at least 10 s", w[0].title, secs(w[1].time - w[0].time)));
            }
        }
        if let Some(last) = list.last() {
            if r.duration - last.time < 10.0 && r.duration > last.time {
                problems.push(format!("the last chapter {:?} is only {} s long; chapters must be at least 10 s", last.title, secs(r.duration - last.time)));
            }
            if last.time >= r.duration && r.duration > 0.0 {
                problems.push(format!("chapter {:?} starts after the end of the video", last.title));
            }
        }
    }
    (list, text.join("\n"), problems)
}

/// Ops that make the music track duck under the voice: `add_ducking` (or `set_ducking` when that
/// pair already has a rule and `amount_db` is given). Tracks default to the roles "music" and "voice".
pub fn auto_ducking_ops(p: &Project, target: Option<&str>, trigger: Option<&str>, amount_db: Option<f64>) -> Result<Vec<Op>, String> {
    let find_role = |role: &str| p.tracks.iter().find(|t| t.role == role).map(|t| t.id.clone());
    let target = match target {
        Some(t) => t.to_string(),
        None => find_role("music").ok_or("no track has the role \"music\"; pass 'target' (the track to lower)")?,
    };
    let trigger = match trigger {
        Some(t) => t.to_string(),
        None => find_role("voice").ok_or("no track has the role \"voice\"; pass 'trigger' (the track whose sound lowers the target)")?,
    };
    for t in [&target, &trigger] {
        if !p.tracks.iter().any(|x| &x.id == t) {
            return Err(format!("unknown track '{t}'"));
        }
    }
    if let Some(d) = p.ducking.iter().find(|d| d.target == target && d.trigger == trigger) {
        return match amount_db {
            Some(a) => Ok(vec![op(json!({"op": "set_ducking", "ducking": d.id, "amount_db": a}))?]),
            None => Err(format!("{target} already ducks under {trigger} ({}, {} dB); pass amount_db to change it", d.id, d.amount_db)),
        };
    }
    let mut v = json!({"op": "add_ducking", "target": target, "trigger": trigger});
    if let Some(a) = amount_db {
        v["amount_db"] = json!(a);
    }
    Ok(vec![op(v)?])
}

/// `add_marker` ops of kind "beat" at timeline `times` (every `every`-th beat, numbered from 1).
pub fn beat_marker_ops(times: &[f64], every: usize) -> Result<Vec<Op>, String> {
    let every = every.max(1);
    times
        .iter()
        .enumerate()
        .filter(|(i, _)| i % every == 0)
        .map(|(i, t)| op(json!({"op": "add_marker", "time": t3(*t), "kind": "beat", "name": format!("beat {}", i + 1)})))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Source monitor: three-point edits (insert / overwrite)
// ---------------------------------------------------------------------------------------------

/// How a Source-monitor range goes into the timeline (Premiere's Insert `,` and Overwrite `.`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceEdit {
    /// Splits a clip under the time and moves later clips on that track right.
    Insert,
    /// Replaces whatever is under the new clip on its track; nothing moves.
    Overwrite,
}

impl SourceEdit {
    pub fn parse(s: &str) -> Option<SourceEdit> {
        match s.trim().to_ascii_lowercase().as_str() {
            "insert" => Some(SourceEdit::Insert),
            "overwrite" => Some(SourceEdit::Overwrite),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            SourceEdit::Insert => "Insert",
            SourceEdit::Overwrite => "Overwrite",
        }
    }
}

/// The source range `[in, out)` (seconds of the file) a source edit uses: the marks, else the
/// whole file. Stills have no range (`None`: the clip gets the default still length unless both
/// marks give one). Errors when the range is empty.
pub fn source_range(m: &MediaAsset, in_point: Option<f64>, out_point: Option<f64>) -> Result<Option<(f64, f64)>, String> {
    let fin = |v: Option<f64>, what: &str| -> Result<Option<f64>, String> {
        match v {
            Some(x) if !x.is_finite() => Err(format!("{what} must be a number of seconds")),
            other => Ok(other),
        }
    };
    let (a, b) = (fin(in_point, "in")?, fin(out_point, "out")?);
    if m.kind == MediaKind::Image {
        return match (a, b) {
            (Some(a), Some(b)) if b - a > 1e-3 => Ok(Some((0.0, b - a))),
            (Some(_), Some(_)) => Err("the out mark must be after the in mark".into()),
            _ => Ok(None),
        };
    }
    let dur = m.info.duration;
    let end = if dur > 0.0 { dur } else { f64::INFINITY };
    let a = a.unwrap_or(0.0).clamp(0.0, if end.is_finite() { end } else { f64::MAX });
    let b = match b {
        Some(b) => b.min(end),
        None if end.is_finite() => end,
        None => return Err(format!("{} has no known duration: set an out mark", m.id)),
    };
    if b - a < 1e-3 {
        return Err(format!("the source range {a:.3}–{b:.3} s of {} is empty: the out mark must be after the in mark", m.id));
    }
    Ok(Some((a, b)))
}

/// Ops for a Source-monitor edit: `asset` from source second `in_point` to `out_point` (default:
/// the whole file) placed at timeline second `at` on `track` (default: the main video track for
/// pictures, the first unlocked audio track for sound; created when missing). One undoable step for
/// the person and the AI alike: `insert_clip` (ripple) or `add_clip` with `overwrite`.
pub fn source_edit_ops(p: &Project, asset: &str, in_point: Option<f64>, out_point: Option<f64>, at: f64, track: Option<&str>, mode: SourceEdit) -> Result<Vec<Op>, String> {
    let m = p.media.iter().find(|m| m.id == asset).ok_or_else(|| format!("unknown media asset '{asset}'"))?;
    if !at.is_finite() {
        return Err("the timeline time must be a number of seconds".into());
    }
    let at = at.max(0.0);
    let range = source_range(m, in_point, out_point)?;
    let picture = m.kind != MediaKind::Audio;
    let want = if picture { TrackKind::Video } else { TrackKind::Audio };
    let mut ops = Vec::new();
    let track = match track {
        Some(t) => {
            let tr = p.tracks.iter().find(|x| x.id == t).ok_or_else(|| format!("unknown track '{t}'"))?;
            if tr.locked {
                return Err(format!("track {t} is locked: unlock it or pick another track"));
            }
            if tr.kind != want {
                let (what, need) = if picture { ("picture", "a video track") } else { ("sound-only", "an audio track") };
                return Err(format!("{} is {what} media: put it on {need} (track {t} is not one)", m.id));
            }
            t.to_string()
        }
        None => {
            let found = if picture {
                main_video_track(p).filter(|id| p.tracks.iter().any(|t| &t.id == id && !t.locked)).or_else(|| p.tracks.iter().find(|t| t.kind == TrackKind::Video && !t.locked).map(|t| t.id.clone()))
            } else {
                p.tracks.iter().find(|t| t.kind == TrackKind::Audio && !t.locked).map(|t| t.id.clone())
            };
            match found {
                Some(t) => t,
                None => {
                    let id = IdGen::new(p).fresh(if picture { "v" } else { "a" });
                    let (kind, role, name) = if picture { ("video", "main", "Main") } else { ("audio", "voice", "Audio") };
                    ops.push(op(json!({"op": "add_track", "id": id, "kind": kind, "role": role, "name": name}))?);
                    id
                }
            }
        }
    };
    let mut clip = json!({"asset": asset});
    if let Some((a, b)) = range {
        if m.kind != MediaKind::Image {
            clip["in_point"] = json!(a);
        }
        clip["duration"] = json!(b - a);
    }
    match mode {
        SourceEdit::Insert => ops.push(op(json!({"op": "insert_clip", "track": track, "at": at, "clip": clip}))?),
        SourceEdit::Overwrite => {
            clip["track"] = json!(track);
            clip["start"] = json!(at);
            ops.push(op(json!({"op": "add_clip", "clip": clip, "overwrite": true}))?);
        }
    }
    Ok(ops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiframes_core::{resolve, Marker, Transcript, Word};

    fn project() -> Project {
        serde_json::from_value(json!({
            "format": 1,
            "tracks": [{"id": "v1", "kind": "video", "role": "main"}, {"id": "a1", "kind": "audio", "role": "voice"}, {"id": "a2", "kind": "audio", "role": "music"}],
            "media": [{"id": "m1", "path": "voice.wav", "kind": "audio", "info": {"duration": 20.0, "has_audio": true}}],
            "clips": [{"id": "c1", "track": "a1", "start": 2.0, "duration": 10.0, "source": {"type": "media", "asset": "m1", "in_point": 5.0}}]
        }))
        .unwrap()
    }

    #[test]
    fn words_map_through_the_clip() {
        let mut p = project();
        let words = vec![
            Word { text: "early".into(), start: 1.0, end: 1.5, conf: 1.0 },
            Word { text: " hello".into(), start: 6.0, end: 6.4, conf: 1.0 },
            Word { text: "late".into(), start: 16.0, end: 16.5, conf: 1.0 },
        ];
        p.transcripts.insert("m1".into(), Transcript { language: "en".into(), words });
        let r = resolve(&p);
        let w = word_places(&p, &r, "m1", None);
        assert_eq!(w[0], None, "before the in-point");
        let w1 = w[1].clone().unwrap();
        assert_eq!(w1.clip, "c1");
        assert!((w1.start - 3.0).abs() < 1e-9 && (w1.end - 3.4).abs() < 1e-9, "{w1:?}");
        assert_eq!(w[2], None, "after the out-point");
    }

    #[test]
    fn silence_cuts_go_backwards_with_padding() {
        let p = project();
        let r = resolve(&p);
        let mut sil = HashMap::new();
        sil.insert("m1".to_string(), vec![[0.0, 6.0], [8.0, 9.0], [10.0, 10.3], [14.0, 20.0]]);
        let o = SilenceOptions { noise_db: -35.0, min_len: 0.5, padding: 0.1 };
        let (ops, cuts, _) = silence_cut_ops(&p, &r, &["c1".to_string()], &sil, &o).unwrap();
        // [5,6] (clipped to the in-point), [8,9], [14,15] (clipped to the out-point); [10,10.3] too short.
        assert_eq!(cuts.len(), 3);
        let close = |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).abs() < 1e-9 && (a[1] - b[1]).abs() < 1e-9;
        assert!(close(cuts[0].source, [5.1, 5.9]), "{:?}", cuts[0]);
        assert!(close(cuts[0].timeline, [2.1, 2.9]), "{:?}", cuts[0]);
        let froms: Vec<f64> = ops.iter().map(|o| if let Op::CutSource { from, .. } = o { *from } else { 0.0 }).collect();
        assert_eq!(froms, vec![14.1, 8.1, 5.1], "last cut first");
        let mut q = p.clone();
        aiframes_core::apply_batch(&mut q, &ops).unwrap();
        let rq = resolve(&q);
        assert!((rq.duration - (r.duration - 0.8 - 0.8 - 0.8)).abs() < 1e-6, "{}", rq.duration);
    }

    #[test]
    fn chapter_rules() {
        let mut p = project();
        let r = resolve(&p);
        assert!(!chapters(&p, &r).2.is_empty());
        for (i, t) in [0.0, 4.0, 30.0].iter().enumerate() {
            p.markers.push(Marker { id: format!("mk{i}"), time: Time::At(*t), name: format!("Part {i}"), kind: MarkerKind::Chapter, color: None, note: String::new() });
        }
        let r = resolve(&p);
        let (list, text, problems) = chapters(&p, &r);
        assert_eq!(list.len(), 3);
        assert_eq!(text, "0:00 Part 0\n0:04 Part 1\n0:30 Part 2");
        assert!(problems.iter().any(|m| m.contains("Part 0") && m.contains("10 s")), "{problems:?}");
    }

    #[test]
    fn ducking_and_beats() {
        let p = project();
        let ops = auto_ducking_ops(&p, None, None, Some(-18.0)).unwrap();
        assert!(matches!(&ops[0], Op::AddDucking { target, trigger, amount_db: Some(a), .. } if target == "a2" && trigger == "a1" && *a == -18.0));
        let ops = beat_marker_ops(&[0.5, 1.0, 1.5, 2.0, 2.5], 2).unwrap();
        assert_eq!(ops.len(), 3);
    }

    #[test]
    fn source_edits_insert_and_overwrite() {
        let mut p: Project = serde_json::from_value(json!({
            "format": 1,
            "settings": {"width": 1920, "height": 1080, "fps": 30.0},
            "tracks": [{"id": "v1", "kind": "video", "role": "main"}, {"id": "a1", "kind": "audio"}],
            "media": [
                {"id": "mv", "path": "a.mp4", "kind": "video", "info": {"duration": 20.0, "width": 1920, "height": 1080, "fps": 30.0, "has_video": true, "has_audio": true}},
                {"id": "ms", "path": "s.png", "kind": "image", "info": {"width": 800, "height": 600}},
                {"id": "ma", "path": "a.wav", "kind": "audio", "info": {"duration": 9.0, "has_audio": true}}
            ],
            "clips": [
                {"id": "c1", "track": "v1", "start": 0.0, "duration": 4.0, "source": {"type": "media", "asset": "mv", "in_point": 0.0}},
                {"id": "c2", "track": "v1", "start": {"after": "c1"}, "duration": 4.0, "source": {"type": "media", "asset": "mv", "in_point": 10.0}}
            ]
        }))
        .unwrap();
        // Insert 2 s (source 5..7) at 2.0: c1 splits, everything after moves right by 2 s.
        let ops = source_edit_ops(&p, "mv", Some(5.0), Some(7.0), 2.0, None, SourceEdit::Insert).unwrap();
        let mut q = p.clone();
        aiframes_core::apply_batch(&mut q, &ops).unwrap();
        let r = resolve(&q);
        assert!((r.duration - 10.0).abs() < 1e-6, "{}", r.duration);
        let new = q.clips.iter().find(|c| matches!(&c.source, Source::Media { in_point, .. } if (*in_point - 5.0).abs() < 1e-9)).expect("the inserted clip");
        let sp = r.span(&new.id).unwrap();
        assert!((sp.start - 2.0).abs() < 1e-6 && (sp.end - 4.0).abs() < 1e-6, "{sp:?}");
        assert!((r.span("c2").unwrap().start - 6.0).abs() < 1e-6, "later clips moved right");
        // Overwrite the same range at 2.0: nothing moves, the length stays 8 s.
        let ops = source_edit_ops(&p, "mv", Some(5.0), Some(7.0), 2.0, None, SourceEdit::Overwrite).unwrap();
        let mut q = p.clone();
        aiframes_core::apply_batch(&mut q, &ops).unwrap();
        let r = resolve(&q);
        assert!((r.duration - 8.0).abs() < 1e-6, "{}", r.duration);
        assert!((r.span("c2").unwrap().start - 4.0).abs() < 1e-6);
        // No marks: the whole file; sound-only media goes to an audio track.
        let ops = source_edit_ops(&p, "ma", None, None, 1.0, None, SourceEdit::Overwrite).unwrap();
        let mut q = p.clone();
        aiframes_core::apply_batch(&mut q, &ops).unwrap();
        let c = q.clips.last().unwrap();
        assert_eq!(c.track, "a1");
        assert!((c.duration - 9.0).abs() < 1e-9);
        // Stills: no range unless both marks; never an in_point.
        let ops = source_edit_ops(&p, "ms", None, None, 8.0, None, SourceEdit::Insert).unwrap();
        aiframes_core::apply_batch(&mut p.clone(), &ops).unwrap();
        let ops = source_edit_ops(&p, "ms", Some(1.0), Some(3.5), 8.0, None, SourceEdit::Overwrite).unwrap();
        let mut q = p.clone();
        aiframes_core::apply_batch(&mut q, &ops).unwrap();
        assert!((q.clips.last().unwrap().duration - 2.5).abs() < 1e-9);
        // Loud failures.
        assert!(source_edit_ops(&p, "mv", Some(7.0), Some(5.0), 0.0, None, SourceEdit::Insert).unwrap_err().contains("after the in mark"));
        assert!(source_edit_ops(&p, "zz", None, None, 0.0, None, SourceEdit::Insert).unwrap_err().contains("unknown media"));
        assert!(source_edit_ops(&p, "ma", None, None, 0.0, Some("v1"), SourceEdit::Insert).unwrap_err().contains("audio track"));
        p.tracks[0].locked = true;
        assert!(source_edit_ops(&p, "mv", None, None, 0.0, Some("v1"), SourceEdit::Insert).unwrap_err().contains("locked"));
        // The main track is locked: the edit goes to a new video track.
        let ops = source_edit_ops(&p, "mv", None, Some(3.0), 0.0, None, SourceEdit::Overwrite).unwrap();
        assert!(matches!(&ops[0], Op::AddTrack { kind: TrackKind::Video, .. }), "{ops:?}");
        aiframes_core::apply_batch(&mut p, &ops).unwrap();
        assert_eq!(SourceEdit::parse(" Overwrite "), Some(SourceEdit::Overwrite));
        assert_eq!(SourceEdit::parse("lift"), None);
    }
}
