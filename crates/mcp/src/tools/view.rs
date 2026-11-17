//! Seeing the edit (rendered frames, contact sheets, filmstrips around cuts) and the live window
//! (playhead, play/pause/seek, selection, screenshots).

use super::*;
use crate::RenderInput;
use aiframes_core::{source_time, MediaKind, Source, TrackKind};
use aiframes_render::{encode_png, FrameOptions, Image};
use std::path::PathBuf;
use std::time::Duration;
use suite_mcp::{base64, IMAGE_KEY};

const MAX_FRAMES: usize = 60;

/// What is on screen at `t`, top layer first.
pub(crate) fn on_screen(p: &Project, r: &Resolved, t: f64) -> Vec<J> {
    let video: HashSet<&str> = p.tracks.iter().filter(|tr| tr.kind == TrackKind::Video && !tr.hidden).map(|tr| tr.id.as_str()).collect();
    let mut v: Vec<J> = r
        .clips_at(p, t)
        .into_iter()
        .filter(|c| c.enabled && video.contains(c.track.as_str()))
        .map(|c| {
            let local = r.span(&c.id).map(|s| t - s.start).unwrap_or(0.0);
            let what = match &c.source {
                Source::Media { asset, .. } => {
                    let file = p.media.iter().find(|m| &m.id == asset).map(|m| file_name(&m.path).to_string()).unwrap_or_else(|| asset.clone());
                    match source_time(c, local) {
                        Some(st) => format!("{file} @ {} s", secs(st)),
                        None => file,
                    }
                }
                Source::Text(tx) => format!("text {:?}", short(&tx.text, 40)),
                Source::Solid { color } => format!("solid {}", color.to_hex()),
                Source::Shape(sh) => format!("shape {:?}", sh.kind).to_lowercase(),
                Source::Adjustment => "adjustment layer".into(),
            };
            json!({"clip": c.id, "track": c.track, "what": what})
        })
        .collect();
    v.reverse();
    v
}

fn default_scale(p: &Project) -> f32 {
    let m = p.settings.width.max(p.settings.height).max(1) as f32;
    (768.0 / m).min(1.0)
}

fn scale_arg(args: &J, p: &Project) -> Result<f32, String> {
    match arg_f64(args, "scale")? {
        None => Ok(default_scale(p)),
        Some(v) if v > 0.0 && v <= 1.0 => Ok(v as f32),
        Some(v) => Err(format!("'scale' must be > 0 and ≤ 1 (1 = full project resolution; got {v})")),
    }
}

/// Times from an argument that may be a single time or a list (numbers, "1,2.5", clip/marker ids);
/// more than `max` is an error (checked before anything is rendered or allocated per time).
fn times_arg(p: &Project, r: &Resolved, v: &J, max: usize) -> Result<Vec<f64>, String> {
    let n = match v {
        J::Array(a) => a.len(),
        J::String(s) => s.split(',').count(),
        _ => 1,
    };
    if n > max {
        return Err(format!("{n} times requested; the maximum here is {max}"));
    }
    let items: Vec<J> = match v {
        J::Array(a) => a.clone(),
        J::String(s) if s.contains(',') => s.split(',').map(|x| json!(x.trim())).collect(),
        other => vec![other.clone()],
    };
    items.iter().map(|x| time_ref(p, r, x)).collect()
}

/// What the renderer will substitute at these times (offline media → placeholder, missing fonts →
/// fallback, unknown effects → skipped), so a frame that looks wrong is explained.
fn render_warnings(s: &Services, inp: &RenderInput, times: &[f64]) -> Vec<String> {
    let (p, r) = (&inp.project, &inp.resolved);
    let mut out: Vec<String> = Vec::new();
    let mut add = |w: String| {
        if !out.contains(&w) && out.len() < 20 {
            out.push(w);
        }
    };
    let mut seen = HashSet::new();
    for &t in times {
        for c in r.clips_at(p, t) {
            if !c.enabled || !seen.insert(c.id.clone()) {
                continue;
            }
            match &c.source {
                Source::Media { asset, .. } => match p.media.iter().find(|m| &m.id == asset) {
                    None => add(format!("clip {} uses media {asset}, which is not in the project (drawn as a placeholder)", c.id)),
                    Some(m) => {
                        let path = abs_path(&m.path, Some(&inp.dir));
                        if !path.exists() {
                            add(format!("clip {}: media {} is offline ({}); it is drawn as a placeholder — relink it with relink_media", c.id, m.id, path.display()));
                        }
                    }
                },
                Source::Text(tx) if !s.renderer.has_font(&inp.dir, &tx.font) => {
                    add(format!("clip {}: font {:?} is not installed; a fallback font is used", c.id, tx.font));
                }
                _ => {}
            }
            for e in &c.effects {
                if aiframes_core::effect_spec(&e.kind).is_none() {
                    add(format!("clip {}: unknown effect {:?} is skipped", c.id, e.kind));
                }
            }
        }
    }
    out
}

fn merge_warnings(meta: &mut J, mut w: Vec<String>, more: Vec<String>) {
    for x in more {
        if !w.contains(&x) {
            w.push(x);
        }
    }
    if !w.is_empty() {
        meta["warnings"] = json!(w);
    }
}

/// A PNG output path: relative paths are resolved against the project folder (else the current
/// directory), ".png" is added when missing, and project files/media are never overwritten.
fn png_target(s: &Services, base: &std::path::Path, path: &str) -> Result<PathBuf, String> {
    let mut f = abs_path(path, Some(base));
    match f.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()) {
        None => {
            f.set_extension("png");
        }
        Some(e) if e == "png" => {}
        Some(e) => return Err(format!("images are written as PNG; use a .png path (got .{e})")),
    }
    if let Some(why) = crate::actions::protected_target(s, &f) {
        return Err(format!("refusing to write the image there: {why}"));
    }
    Ok(f)
}

fn clamp_time(p: &Project, r: &Resolved, t: f64) -> f64 {
    t.clamp(0.0, (r.duration - frame_dur(p)).max(0.0))
}

/// The labels the renderer prints under each frame of a grid (`mm:ss.ff`, ff = frame within the
/// second), so the AI can match a frame to its time in seconds.
fn add_labels(meta: &mut J, p: &Project, times: &[f64]) {
    meta["labels"] = json!(times.iter().map(|t| aiframes_render::timecode(*t, p.settings.fps)).collect::<Vec<_>>());
    meta["label_format"] = json!("frames are labelled mm:ss.ff (ff = frame within the second); `times` gives the same frames in seconds, in reading order");
}

fn image_result(img: &Image, mut meta: J) -> J {
    meta[IMAGE_KEY] = json!(base64(&encode_png(img)));
    meta["image"] = json!(format!("{}x{} PNG", img.width, img.height));
    meta
}

fn render_err(e: aiframes_render::RenderError) -> String {
    format!("render failed: {e}")
}

fn input_of(s: &Arc<Services>, args: &J) -> Result<(u64, RenderInput, f64), String> {
    with_doc(s, args, |id, g| Ok((id, g.render_input(), g.live.playhead)))
}

pub(crate) fn snapshot(s: &Arc<Services>, args: &J) -> ToolResult {
    if let Some(pv) = preview(s, args)? {
        return preview_snapshot(s, args, pv);
    }
    let (id, inp, playhead) = input_of(s, args)?;
    let (p, r) = (&inp.project, &inp.resolved);
    let at = args.get("at");
    let mut times = match at {
        Some(v) if !v.is_null() => times_arg(p, r, v, 16).map_err(|e| format!("{e} (use contact_sheet for more)"))?,
        _ => vec![playhead],
    };
    if times.is_empty() {
        return Err("'at' is empty".into());
    }
    for t in &mut times {
        *t = clamp_time(p, r, *t);
    }
    let o = FrameOptions { scale: scale_arg(args, p)?, guides: arg_bool(args, "guides", false)?, draft: arg_bool(args, "draft", false)? };
    let save_dir = match arg_str(args, "save_dir") {
        Some(d) => Some(abs_path(d, Some(&inp.dir))),
        None => None,
    };
    let mut meta = json!({"project": id, "times": times.iter().map(|t| t3(*t)).collect::<Vec<_>>()});
    meta["on_screen"] = if times.len() == 1 { J::Array(on_screen(p, r, times[0])) } else { J::Array(times.iter().map(|&t| json!({"time": t3(t), "clips": on_screen(p, r, t)})).collect()) };
    if !arg_bool(args, "image", true)? && save_dir.is_none() {
        // Only what is on screen (and what would render as a substitute): nothing is rendered.
        merge_warnings(&mut meta, render_warnings(s, &inp, &times), Vec::new());
        meta["note"] = json!("image:false: no picture rendered (pass save_dir to write PNG files)");
        return Ok(meta);
    }
    // Frames are rendered one by one (with the renderer's warnings) when there is one or they are
    // saved; a grid of several is rendered by the contact sheet.
    let mut warnings = Vec::new();
    let mut frames = Vec::new();
    if times.len() == 1 || save_dir.is_some() {
        for &t in &times {
            let (img, w) = s.renderer.frame_checked(&inp.dir, p, r, t, &o).map_err(render_err)?;
            frames.push(img);
            for x in w {
                if !warnings.contains(&x) {
                    warnings.push(x);
                }
            }
        }
    }
    if let Some(dir) = &save_dir {
        let mut files = Vec::new();
        let mut targets = Vec::new();
        for t in &times {
            let f = dir.join(format!("frame_{:08.3}s.png", t));
            if let Some(why) = crate::actions::protected_target(s, &f) {
                return Err(format!("refusing to write {}: {why}", f.display()));
            }
            targets.push(f);
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        for (f, img) in targets.iter().zip(&frames) {
            std::fs::write(f, encode_png(img)).map_err(|e| format!("cannot write {}: {e}", f.display()))?;
            files.push(f.to_string_lossy().to_string());
        }
        meta["files"] = json!(files);
        if !arg_bool(args, "image", true)? {
            merge_warnings(&mut meta, warnings, Vec::new());
            return Ok(meta);
        }
    }
    let img = if times.len() == 1 {
        frames.pop().unwrap_or_else(|| Image::new(1, 1))
    } else {
        let tw = ((p.settings.width as f32) * o.scale).round().max(16.0) as u32;
        add_labels(&mut meta, p, &times);
        aiframes_render::contact_sheet(&s.renderer, &inp.dir, p, r, &times, (times.len() as u32).min(4), tw).map_err(render_err)?
    };
    let extra = if times.len() > 1 { render_warnings(s, &inp, &times) } else { Vec::new() };
    merge_warnings(&mut meta, warnings, extra);
    meta["size"] = json!([img.width, img.height]);
    Ok(image_result(&img, meta))
}

fn write_png(f: &std::path::Path, img: &Image) -> Result<PathBuf, String> {
    let f = f.to_path_buf();
    if let Some(parent) = f.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
    }
    std::fs::write(&f, encode_png(img)).map_err(|e| format!("cannot write {}: {e}", f.display()))?;
    Ok(f)
}

pub(crate) fn contact_sheet(s: &Arc<Services>, args: &J) -> ToolResult {
    if let Some(pv) = preview(s, args)? {
        return preview_contact_sheet(s, args, pv);
    }
    let (id, inp, _) = input_of(s, args)?;
    let (p, r) = (&inp.project, &inp.resolved);
    if r.duration <= 0.0 {
        return Err("the timeline is empty".into());
    }
    let from = arg_f64(args, "from")?.unwrap_or(0.0).max(0.0);
    let to = arg_f64(args, "to")?.unwrap_or(r.duration).min(r.duration);
    if to <= from {
        return Err(format!("empty range {from}–{to}"));
    }
    // Every way of choosing frames is bounded by MAX_FRAMES before anything is allocated per frame.
    let mut times: Vec<f64> = if let Some(v) = args.get("times").filter(|v| !v.is_null()) {
        times_arg(p, r, v, MAX_FRAMES)?
    } else if let Some(per) = args.get("per_clip").filter(|v| !v.is_null() && **v != J::Bool(false)) {
        let track = match per {
            J::String(t) => t.clone(),
            _ => crate::actions::main_video_track(p).ok_or("the project has no video track")?,
        };
        let v: Vec<f64> = r.on_track(p, &track).into_iter().filter(|(_, sp)| sp.end > from && sp.start < to).map(|(_, sp)| sp.start + sp.duration() / 2.0).collect();
        if v.is_empty() {
            return Err(format!("track {track} has no clips in {}–{} s", secs(from), secs(to)));
        }
        if v.len() > MAX_FRAMES {
            return Err(format!("track {track} has {} clips in {}–{} s; the maximum is {MAX_FRAMES} frames (narrow from/to)", v.len(), secs(from), secs(to)));
        }
        v
    } else if let Some(every) = arg_f64(args, "every")? {
        if every <= 0.0 {
            return Err("'every' must be > 0 seconds".into());
        }
        let n = ((to - from) / every).ceil();
        if n > MAX_FRAMES as f64 {
            return Err(format!("every {} s gives {n} frames over {}–{} s; the maximum is {MAX_FRAMES} (raise 'every' or narrow from/to)", num(every), secs(from), secs(to)));
        }
        let mut v = Vec::new();
        let mut t = from + every / 2.0;
        while t < to && v.len() <= MAX_FRAMES {
            v.push(t);
            t += every;
        }
        v
    } else {
        let n = arg_int(args, "count", 1, MAX_FRAMES as u64)?.unwrap_or(12) as usize;
        (0..n).map(|i| from + (i as f64 + 0.5) * (to - from) / n as f64).collect()
    };
    if times.len() > MAX_FRAMES {
        return Err(format!("{} frames requested; the maximum is {MAX_FRAMES} (raise 'every' or lower 'count')", times.len()));
    }
    for t in &mut times {
        *t = clamp_time(p, r, *t);
    }
    let cols = arg_int_clamped(args, "cols", 1, 12)?.unwrap_or(4) as u32;
    let width = arg_int_clamped(args, "width", 64, 1920)?.unwrap_or(320) as u32;
    let target = match arg_str(args, "save_path") {
        Some(path) => Some(png_target(s, &inp.dir, path)?),
        None => None,
    };
    let img = aiframes_render::contact_sheet(&s.renderer, &inp.dir, p, r, &times, cols, width).map_err(render_err)?;
    let mut meta = json!({"project": id, "times": times.iter().map(|t| t3(*t)).collect::<Vec<_>>(), "cols": cols, "size": [img.width, img.height]});
    add_labels(&mut meta, p, &times);
    merge_warnings(&mut meta, render_warnings(s, &inp, &times), Vec::new());
    if let Some(f) = target {
        meta["file"] = json!(write_png(&f, &img)?.to_string_lossy());
        if !arg_bool(args, "image", true)? {
            return Ok(meta);
        }
    }
    Ok(image_result(&img, meta))
}

pub(crate) fn filmstrip(s: &Arc<Services>, args: &J) -> ToolResult {
    if let Some(pv) = preview(s, args)? {
        return preview_filmstrip(s, args, pv);
    }
    let (id, inp, playhead) = input_of(s, args)?;
    let (p, r) = (&inp.project, &inp.resolved);
    let fd = frame_dur(p);
    let mut meta = json!({"project": id});
    let center = if let Some(c) = arg_str(args, "cut") {
        let clip = clip_of(p, c)?;
        let sp = r.span(&clip.id).ok_or_else(|| format!("{c} is not placed on the timeline"))?;
        let end = arg_str(args, "edge").is_some_and(|e| e.eq_ignore_ascii_case("end"));
        let t = if end { sp.end } else { sp.start };
        // The clips on either side of that edge on the same track, and a transition there.
        let others = r.on_track(p, &clip.track);
        let before = others.iter().find(|(o, s2)| o.id != clip.id && (s2.end - t).abs() < 1e-3).map(|(o, _)| o.id.clone());
        let after = others.iter().find(|(o, s2)| o.id != clip.id && (s2.start - t).abs() < 1e-3).map(|(o, _)| o.id.clone());
        let (out_id, in_id) = if end { (Some(clip.id.clone()), after) } else { (before, Some(clip.id.clone())) };
        meta["cut"] = json!({"time": t3(t), "outgoing": out_id, "incoming": in_id});
        if let Some(tr) = p.transitions.iter().find(|tr| Some(&tr.from) == out_id.as_ref() && Some(&tr.to) == in_id.as_ref()) {
            meta["transition"] = json!({"id": tr.id, "kind": tr.kind, "duration": tr.duration});
        }
        t
    } else if let Some(v) = args.get("at").filter(|v| !v.is_null()) {
        time_ref(p, r, v)?
    } else {
        playhead
    };
    let frames = arg_int_clamped(args, "frames", 2, 16)?.unwrap_or(6) as usize;
    let span = arg_f64(args, "span")?.unwrap_or(1.0).max(fd * frames as f64);
    let step = (span / frames as f64).max(fd);
    let nb = frames / 2;
    let mut times: Vec<f64> = (0..nb).map(|i| center - fd - step * (nb - 1 - i) as f64).collect();
    times.extend((0..frames - nb).map(|i| center + step * i as f64));
    for t in &mut times {
        *t = clamp_time(p, r, *t);
    }
    let width = arg_int_clamped(args, "width", 64, 960)?.unwrap_or(240) as u32;
    let target = match arg_str(args, "save_path") {
        Some(path) => Some(png_target(s, &inp.dir, path)?),
        None => None,
    };
    let img = aiframes_render::contact_sheet(&s.renderer, &inp.dir, p, r, &times, frames as u32, width).map_err(render_err)?;
    meta["times"] = json!(times.iter().map(|t| t3(*t)).collect::<Vec<_>>());
    add_labels(&mut meta, p, &times);
    merge_warnings(&mut meta, render_warnings(s, &inp, &times), Vec::new());
    meta["center"] = json!(t3(center));
    meta["size"] = json!([img.width, img.height]);
    if let Some(f) = target {
        meta["file"] = json!(write_png(&f, &img)?.to_string_lossy());
    }
    Ok(image_result(&img, meta))
}

// ---------------------------------------------------------------------------------------------
// Source media previews (never touch any project)
// ---------------------------------------------------------------------------------------------

/// A media file to look at directly: its frames are rendered through a scratch, in-memory project
/// (the clip at source time = timeline time), so the person's project is never changed.
pub(crate) struct Preview {
    inp: RenderInput,
    asset: Option<Id>,
    file: PathBuf,
    kind: MediaKind,
    duration: f64,
    /// Where relative output paths go (the project's folder, else the current directory) — never the
    /// media's own folder.
    out_dir: PathBuf,
}

impl Preview {
    fn meta(&self) -> J {
        let mut j = json!({"file": self.file.to_string_lossy(), "kind": self.kind, "duration": t3(self.duration), "times_are": "source seconds of the media (not timeline)"});
        if let Some(a) = &self.asset {
            j["asset"] = json!(a);
        }
        j
    }
    fn clamp(&self, t: f64) -> f64 {
        let fd = frame_dur(&self.inp.project);
        t.clamp(0.0, (self.duration - fd).max(0.0))
    }
}

/// The preview target of a seeing tool: `asset` (a media id of the project) or `file` (any media
/// file); `None` = the timeline.
fn preview(s: &Arc<Services>, args: &J) -> Result<Option<Preview>, String> {
    let (asset, file) = (arg_str(args, "asset"), arg_str(args, "file"));
    if asset.is_none() && file.is_none() {
        return Ok(None);
    }
    if asset.is_some() && file.is_some() {
        return Err("pass 'asset' or 'file', not both".into());
    }
    let project = find_doc(s, args).ok();
    let out_dir = match &project {
        Some((_, d)) => suite_hub::lock(d).doc.project_dir(),
        None => std::env::current_dir().unwrap_or_default(),
    };
    let (path, kind, info, asset) = match asset {
        Some(a) => {
            let (_, d) = project.as_ref().ok_or(NO_DOC)?;
            let g = suite_hub::lock(d);
            let path = asset_path(&g.doc, a)?;
            let m = g.doc.project.media.iter().find(|m| m.id == a).ok_or_else(|| format!("unknown media asset '{a}'"))?;
            (path, m.kind, m.info.clone(), Some(a.to_string()))
        }
        None => {
            let p = abs_path(file.unwrap_or_default(), Some(&out_dir));
            let (kind, info) = s.pool.probe(&p).map_err(|e| e.to_string())?;
            (p, kind, info, None)
        }
    };
    if kind == MediaKind::Audio || (!info.has_video && kind == MediaKind::Video) {
        return Err(format!("{} has no picture (sound only): use analyze_audio / detect_silence / get_transcript to inspect it", path.display()));
    }
    let fps = if info.fps > 0.0 { info.fps } else { 30.0 };
    let duration = if kind == MediaKind::Image { 1.0 } else { info.duration.max(1.0 / fps) };
    let (w, h) = (info.width.max(16), info.height.max(16));
    let project: Project = serde_json::from_value(json!({
        "format": aiframes_core::FORMAT_VERSION,
        "name": "preview",
        "settings": {"width": w, "height": h, "fps": fps},
        "media": [{"id": "src", "path": path.to_string_lossy(), "kind": kind, "info": info}],
        "tracks": [{"id": "v1", "kind": "video"}],
        "clips": [{"id": "src", "track": "v1", "start": 0.0, "duration": duration, "source": {"type": "media", "asset": "src"}}],
    }))
    .map_err(|e| format!("internal error building the preview: {e}"))?;
    let resolved = aiframes_core::resolve(&project);
    let dir = path.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    Ok(Some(Preview { inp: RenderInput { project, resolved, dir }, asset, file: path, kind, duration: if kind == MediaKind::Image { 0.0 } else { duration }, out_dir }))
}

/// Source times from `at`/`times` (seconds: a number, a list, or "1,2.5").
fn source_times(v: &J, max: usize) -> Result<Vec<f64>, String> {
    let items: Vec<J> = match v {
        J::Array(a) => a.clone(),
        J::String(s) => s.split(',').map(|x| json!(x.trim())).collect(),
        other => vec![other.clone()],
    };
    if items.len() > max {
        return Err(format!("{} times requested; the maximum here is {max}", items.len()));
    }
    items.iter().map(|x| num_of(x).ok_or_else(|| format!("previewing media takes source seconds (numbers), got {x}"))).collect()
}

fn preview_snapshot(s: &Arc<Services>, args: &J, pv: Preview) -> ToolResult {
    let (p, r) = (&pv.inp.project, &pv.inp.resolved);
    let mut times = match args.get("at").filter(|v| !v.is_null()) {
        Some(v) => source_times(v, 16).map_err(|e| format!("{e} (use contact_sheet for more)"))?,
        None => vec![pv.duration / 2.0],
    };
    if times.is_empty() {
        return Err("'at' is empty".into());
    }
    for t in &mut times {
        *t = pv.clamp(*t);
    }
    let mut meta = pv.meta();
    meta["source_times"] = json!(times.iter().map(|t| t3(*t)).collect::<Vec<_>>());
    let save_dir = arg_str(args, "save_dir").map(|d| abs_path(d, Some(&pv.out_dir)));
    let want_image = arg_bool(args, "image", true)?;
    if !want_image && save_dir.is_none() {
        meta["note"] = json!("image:false: no picture rendered (pass save_dir to write PNG files)");
        return Ok(meta);
    }
    let o = FrameOptions { scale: scale_arg(args, p)?, guides: arg_bool(args, "guides", false)?, draft: arg_bool(args, "draft", false)? };
    let mut frames = Vec::new();
    let mut warnings = Vec::new();
    if times.len() == 1 || save_dir.is_some() {
        for &t in &times {
            let (img, w) = s.renderer.frame_checked(&pv.inp.dir, p, r, t, &o).map_err(render_err)?;
            frames.push(img);
            warnings.extend(w);
        }
    }
    if let Some(dir) = &save_dir {
        let mut files = Vec::new();
        for (t, img) in times.iter().zip(&frames) {
            let f = dir.join(format!("source_{:08.3}s.png", t));
            if let Some(why) = crate::actions::protected_target(s, &f) {
                return Err(format!("refusing to write {}: {why}", f.display()));
            }
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            std::fs::write(&f, encode_png(img)).map_err(|e| format!("cannot write {}: {e}", f.display()))?;
            files.push(f.to_string_lossy().to_string());
        }
        meta["files"] = json!(files);
        if !want_image {
            merge_warnings(&mut meta, warnings, Vec::new());
            return Ok(meta);
        }
    }
    let img = if times.len() == 1 {
        frames.pop().unwrap_or_else(|| Image::new(1, 1))
    } else {
        add_labels(&mut meta, p, &times);
        let tw = ((p.settings.width as f32) * o.scale).round().max(16.0) as u32;
        aiframes_render::contact_sheet(&s.renderer, &pv.inp.dir, p, r, &times, (times.len() as u32).min(4), tw).map_err(render_err)?
    };
    merge_warnings(&mut meta, warnings, Vec::new());
    meta["size"] = json!([img.width, img.height]);
    Ok(image_result(&img, meta))
}

fn preview_contact_sheet(s: &Arc<Services>, args: &J, pv: Preview) -> ToolResult {
    if args.get("per_clip").is_some_and(|v| !v.is_null() && *v != J::Bool(false)) {
        return Err("per_clip is for the timeline; for a media file use count, every or times (source seconds)".into());
    }
    let (p, r) = (&pv.inp.project, &pv.inp.resolved);
    let from = arg_f64(args, "from")?.unwrap_or(0.0).max(0.0);
    let to = arg_f64(args, "to")?.unwrap_or(pv.duration).min(pv.duration.max(from));
    let mut times: Vec<f64> = if let Some(v) = args.get("times").filter(|v| !v.is_null()) {
        source_times(v, MAX_FRAMES)?
    } else if pv.kind == MediaKind::Image {
        vec![0.0]
    } else if let Some(every) = arg_f64(args, "every")? {
        if every <= 0.0 {
            return Err("'every' must be > 0 seconds".into());
        }
        let n = ((to - from) / every).ceil();
        if n > MAX_FRAMES as f64 {
            return Err(format!("every {} s gives {n} frames over {}–{} s of the media; the maximum is {MAX_FRAMES} (raise 'every' or narrow from/to)", num(every), secs(from), secs(to)));
        }
        let mut v = Vec::new();
        let mut t = from + every / 2.0;
        while t < to && v.len() < MAX_FRAMES {
            v.push(t);
            t += every;
        }
        v
    } else {
        if to <= from {
            return Err(format!("empty range {from}–{to} (the media is {} s long)", secs(pv.duration)));
        }
        let n = arg_int(args, "count", 1, MAX_FRAMES as u64)?.unwrap_or(12) as usize;
        (0..n).map(|i| from + (i as f64 + 0.5) * (to - from) / n as f64).collect()
    };
    for t in &mut times {
        *t = pv.clamp(*t);
    }
    let cols = arg_int_clamped(args, "cols", 1, 12)?.unwrap_or(4) as u32;
    let width = arg_int_clamped(args, "width", 64, 1920)?.unwrap_or(320) as u32;
    let target = match arg_str(args, "save_path") {
        Some(path) => Some(png_target(s, &pv.out_dir, path)?),
        None => None,
    };
    let img = aiframes_render::contact_sheet(&s.renderer, &pv.inp.dir, p, r, &times, cols, width).map_err(render_err)?;
    let mut meta = pv.meta();
    meta["source_times"] = json!(times.iter().map(|t| t3(*t)).collect::<Vec<_>>());
    meta["cols"] = json!(cols);
    meta["size"] = json!([img.width, img.height]);
    add_labels(&mut meta, p, &times);
    if let Some(f) = target {
        meta["file_written"] = json!(write_png(&f, &img)?.to_string_lossy());
        if !arg_bool(args, "image", true)? {
            return Ok(meta);
        }
    }
    Ok(image_result(&img, meta))
}

fn preview_filmstrip(s: &Arc<Services>, args: &J, pv: Preview) -> ToolResult {
    if arg_str(args, "cut").is_some() || arg_str(args, "edge").is_some() {
        return Err("cut/edge are for the timeline; for a media file pass 'at' (source seconds)".into());
    }
    let (p, r) = (&pv.inp.project, &pv.inp.resolved);
    let fd = frame_dur(p);
    let center = match args.get("at").filter(|v| !v.is_null()) {
        Some(v) => num_of(v).ok_or("for a media file 'at' is source seconds (a number)")?,
        None => return Err("pass 'at' (source seconds of the media) for a filmstrip of a media file".into()),
    };
    let frames = arg_int_clamped(args, "frames", 2, 16)?.unwrap_or(6) as usize;
    let span = arg_f64(args, "span")?.unwrap_or(1.0).max(fd * frames as f64);
    let step = (span / frames as f64).max(fd);
    let nb = frames / 2;
    let mut times: Vec<f64> = (0..nb).map(|i| center - fd - step * (nb - 1 - i) as f64).collect();
    times.extend((0..frames - nb).map(|i| center + step * i as f64));
    for t in &mut times {
        *t = pv.clamp(*t);
    }
    let width = arg_int_clamped(args, "width", 64, 960)?.unwrap_or(240) as u32;
    let target = match arg_str(args, "save_path") {
        Some(path) => Some(png_target(s, &pv.out_dir, path)?),
        None => None,
    };
    let img = aiframes_render::contact_sheet(&s.renderer, &pv.inp.dir, p, r, &times, frames as u32, width).map_err(render_err)?;
    let mut meta = pv.meta();
    meta["source_times"] = json!(times.iter().map(|t| t3(*t)).collect::<Vec<_>>());
    meta["center"] = json!(t3(center));
    meta["size"] = json!([img.width, img.height]);
    add_labels(&mut meta, p, &times);
    if let Some(f) = target {
        meta["file_written"] = json!(write_png(&f, &img)?.to_string_lossy());
    }
    Ok(image_result(&img, meta))
}

/// What a media file is (an asset of the project, or any file), without importing it.
pub(crate) fn media_info(s: &Arc<Services>, args: &J) -> ToolResult {
    let (asset, file) = (arg_str(args, "asset"), arg_str(args, "file"));
    let project = find_doc(s, args).ok();
    match (asset, file) {
        (Some(a), None) => {
            let (_, d) = project.ok_or(NO_DOC)?;
            let g = suite_hub::lock(&d);
            let p = &g.doc.project;
            let m = p.media.iter().find(|m| m.id == a).ok_or_else(|| format!("unknown media asset '{a}'.{}", did_you_mean(a, p.media.iter().map(|m| m.id.as_str()))))?;
            let path = g.doc.media_path(m);
            let users: Vec<&str> = p.clips.iter().filter(|c| matches!(&c.source, Source::Media { asset, .. } if asset == a)).map(|c| c.id.as_str()).collect();
            let mut j = json!({"asset": a, "file": path.to_string_lossy(), "kind": m.kind, "info": m.info, "used_by": users, "offline": !path.exists()});
            streams_j(&mut j, &m.info);
            if m.info.audio_stream_count() > 1 {
                let plays: Vec<J> = p
                    .clips
                    .iter()
                    .filter_map(|c| match &c.source {
                        Source::Media { asset, stream, .. } if asset == a => Some(json!({"clip": c.id, "stream": stream, "muted": c.muted})),
                        _ => None,
                    })
                    .collect();
                j["clip_streams"] = J::Array(plays);
            }
            if let Some(t) = p.transcripts.get(a) {
                j["transcript_words"] = json!(t.words.len());
                if m.info.audio_stream_count() > 1 {
                    j["transcript_stream"] = json!(t.stream);
                }
            } else if let Some((sc, imp)) = crate::transcripts::find_sidecar(&path) {
                j["sidecar"] = json!({"file": sc.to_string_lossy(), "format": imp.format, "words": imp.transcript.words.len(), "next": "import_transcript stores it as the transcript"});
            }
            j["next"] = json!(preview_hint(m.kind, &json!({"asset": a})));
            Ok(j)
        }
        (None, Some(f)) => {
            let base = match &project {
                Some((_, d)) => suite_hub::lock(d).doc.project_dir(),
                None => std::env::current_dir().unwrap_or_default(),
            };
            let path = abs_path(f, Some(&base));
            let (kind, info) = s.pool.probe(&path).map_err(|e| e.to_string())?;
            let mut j = json!({"file": path.to_string_lossy(), "kind": kind, "info": info});
            streams_j(&mut j, &info);
            if let Some((sc, imp)) = crate::transcripts::find_sidecar(&path) {
                j["sidecar"] = json!({"file": sc.to_string_lossy(), "format": imp.format, "words": imp.transcript.words.len(), "note": "import_media imports it as the transcript"});
            }
            j["next"] = json!(preview_hint(kind, &json!({"file": path.to_string_lossy()})));
            Ok(j)
        }
        (Some(_), Some(_)) => Err("pass 'asset' or 'file', not both".into()),
        (None, None) => Err("pass 'asset' (a media id from list_media) or 'file' (a media file path)".into()),
    }
}

/// `audio_streams` (index, channels, rate, title, language, label) and a hint for files with several.
fn streams_j(j: &mut J, info: &aiframes_core::MediaInfo) {
    if info.audio_stream_count() == 0 {
        return;
    }
    j["audio_streams"] = aiframes_core::describe::audio_streams_json(info);
    if info.audio_stream_count() > 1 {
        j["streams_note"] = json!(format!(
            "{} sound streams. Placing the file (import_media place:\"append\", or add_clip then detach_streams) gives each its own linked audio clip on its own track; set_audio_stream picks the stream of one clip; transcribe/detect_silence/analyze_audio take stream (speech stream by default: {}).",
            info.audio_stream_count(),
            info.speech_stream()
        ));
    }
}

fn preview_hint(kind: MediaKind, target: &J) -> String {
    match kind {
        MediaKind::Audio => "sound only: analyze_audio / detect_silence / get_transcript inspect it".into(),
        _ => format!("contact_sheet {target} (+ from/to/count in source seconds) or snapshot {target} with at shows its frames without touching the timeline"),
    }
}

// ---------------------------------------------------------------------------------------------
// Live window
// ---------------------------------------------------------------------------------------------

const NO_WINDOW: &str = "No AIFrames window is attached (headless): requests are recorded but nothing plays. Call launch_gui to open the window so the person can watch.";

pub(crate) fn get_state(s: &Arc<Services>, args: &J) -> ToolResult {
    let has_ui = s.hub.has_ui();
    with_doc(s, args, |id, g| {
        let p = &g.doc.project;
        let r = g.doc.resolved();
        let l = &g.live;
        let selected: Vec<String> = l.selection.iter().filter_map(|c| p.clips.iter().find(|x| &x.id == c)).map(|c| aiframes_core::describe::clip_line(p, r, c, false)).collect();
        let mut j = json!({
            "project": id,
            "title": g.title(),
            "window": has_ui,
            "revision": g.doc.revision(),
            "playhead": t3(l.playhead),
            "playing": l.playing,
            "selection": l.selection,
            "selected": selected,
            "on_screen": on_screen(p, r, l.playhead),
            "duration": t3(r.duration),
        });
        if let Some(i) = l.in_point {
            j["in_point"] = json!(t3(i));
        }
        if let Some(o) = l.out_point {
            j["out_point"] = json!(t3(o));
        }
        if !has_ui {
            j["note"] = json!(NO_WINDOW);
        }
        Ok(j)
    })
}

pub(crate) fn play(s: &Arc<Services>, args: &J, start: bool) -> ToolResult {
    let has_ui = s.hub.has_ui();
    let res = with_doc(s, args, |id, g| {
        let (p, r) = (&g.doc.project, g.doc.resolved());
        let from = match args.get("from").filter(|v| !v.is_null()) {
            Some(v) => Some(clamp_time(p, r, time_ref(p, r, v)?)),
            None => None,
        };
        let until = match args.get("to").filter(|v| !v.is_null()) {
            Some(v) => Some(time_ref(p, r, v)?),
            None => None,
        };
        // The shared state changes now (get_state right after sees it); the window acts on the requests.
        if let Some(t) = from {
            g.live.seek_request = Some(t);
            g.live.playhead = t;
        }
        g.live.playing = start;
        g.live.play_request = Some(start);
        g.live.play_until = if start { until } else { None };
        let mut j = json!({"project": id, "playing": start, "from": from.map(t3).unwrap_or(t3(g.live.playhead))});
        if let Some(u) = until {
            j["until"] = json!(t3(u));
        }
        if !has_ui {
            j["note"] = json!(NO_WINDOW);
        }
        Ok(j)
    })?;
    s.hub.wake();
    Ok(res)
}

pub(crate) fn seek(s: &Arc<Services>, args: &J) -> ToolResult {
    let has_ui = s.hub.has_ui();
    let res = with_doc(s, args, |id, g| {
        let (p, r) = (&g.doc.project, g.doc.resolved());
        let v = ["time", "clip", "marker"].iter().find_map(|k| args.get(*k).filter(|v| !v.is_null())).ok_or("pass 'time' (seconds, a clip id or a marker id)")?;
        let t = if let (Some(c), None) = (arg_str(args, "clip"), args.get("time")) {
            r.span(clip_of(p, c)?.id.as_str()).map(|sp| sp.start).ok_or_else(|| format!("{c} is not placed on the timeline"))?
        } else {
            time_ref(p, r, v)?
        };
        let t = clamp_time(p, r, t);
        g.live.seek_request = Some(t);
        g.live.playhead = t;
        let mut j = json!({"project": id, "playhead": t3(t), "on_screen": on_screen(p, r, t)});
        if !has_ui {
            j["note"] = json!(NO_WINDOW);
        }
        Ok(j)
    })?;
    s.hub.wake();
    Ok(res)
}

pub(crate) fn select(s: &Arc<Services>, args: &J) -> ToolResult {
    let has_ui = s.hub.has_ui();
    let ids = arg_strs(args, "clips")?.unwrap_or_default();
    let res = with_doc(s, args, |id, g| {
        for c in &ids {
            clip_of(&g.doc.project, c)?;
        }
        if arg_bool(args, "seek", false)? {
            if let Some(sp) = ids.first().and_then(|c| g.doc.resolved().span(c)) {
                g.live.seek_request = Some(sp.start);
                g.live.playhead = sp.start;
            }
        }
        // Applied to the shared state at once (get_state sees it); the window also gets the request.
        g.live.select_request = Some(ids.clone());
        g.live.selection = ids.clone();
        let mut j = json!({"project": id, "selected": ids});
        if !has_ui {
            j["note"] = json!(NO_WINDOW);
        }
        Ok(j)
    })?;
    s.hub.wake();
    Ok(res)
}

pub(crate) fn screenshot_ui(s: &Arc<Services>) -> ToolResult {
    if !s.hub.has_ui() {
        return Err("screenshots need the AIFrames window; call launch_gui first (snapshot renders frames without it)".into());
    }
    let rx = s.hub.request_screenshot();
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(png)) => Ok(json!({IMAGE_KEY: base64(&png), "note": "PNG of the AIFrames window as the person currently sees it."})),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("the AIFrames window did not answer the screenshot request (minimized or screen locked?). snapshot works without the window.".into()),
    }
}

/// (Addition) The window's side panels, by id.
pub(crate) const PANELS: &[&str] = &["inspector", "effects", "transcript", "history", "review", "ai"];

/// (Addition) Brings a side panel forward in the window (e.g. "review" after suggesting edits).
pub(crate) fn show_panel(s: &Arc<Services>, args: &J) -> ToolResult {
    let panel = req_str(args, "panel")?.trim().to_ascii_lowercase();
    if !PANELS.contains(&panel.as_str()) {
        return Err(format!("unknown panel '{panel}': one of {}", PANELS.join(", ")));
    }
    request_panel(s, args, &panel)
}

/// Asks the window to show `panel` for the project the call names.
pub(crate) fn request_panel(s: &Arc<Services>, args: &J, panel: &str) -> ToolResult {
    let has_ui = s.hub.has_ui();
    let res = with_doc(s, args, |id, g| {
        g.live.panel_request = Some(panel.to_string());
        let mut j = json!({"project": id, "panel": panel});
        if !has_ui {
            j["note"] = json!(NO_WINDOW);
        }
        Ok(j)
    })?;
    s.hub.wake();
    Ok(res)
}
