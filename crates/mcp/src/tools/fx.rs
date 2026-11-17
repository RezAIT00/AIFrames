//! (Added) `preview_effects`: what effects and transitions would look like on a clip, without editing —
//! the AI's side of the effects browser's thumbnails and hover previews (same scratch-copy render).

use super::*;
use crate::fx_preview::{self as fxp, Scratch};
use aiframes_core::{effect_spec, transition_spec, EFFECTS, TRANSITIONS};
use aiframes_render::{encode_png, Image};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use suite_mcp::{base64, IMAGE_KEY};

/// Most tiles in one sheet (every effect and transition fits).
const MAX_TILES: usize = 80;

/// `"all"`, a list of kinds, or a comma-separated string; checked against the registry.
fn kinds_arg(args: &J, key: &str, valid: &[&str]) -> Result<Option<Vec<String>>, String> {
    let list: Vec<String> = match args.get(key) {
        None | Some(J::Null) => return Ok(None),
        Some(J::String(s)) if s.trim().eq_ignore_ascii_case("all") => return Ok(Some(valid.iter().map(|k| k.to_string()).collect())),
        Some(J::String(s)) => s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        Some(J::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_string).ok_or_else(|| format!("'{key}' lists kind names (strings), not {v}"))).collect::<Result<_, _>>()?,
        Some(J::Bool(false)) => return Ok(Some(Vec::new())),
        Some(other) => return Err(format!("'{key}' is \"all\" or a list of kinds, not {other}")),
    };
    for k in &list {
        if !valid.contains(&k.as_str()) {
            return Err(format!("unknown {} '{k}'.{} See list_{key}.", key.trim_end_matches('s'), did_you_mean(k, valid.iter().copied())));
        }
    }
    Ok(Some(list))
}

struct Tile {
    label: String,
    what: &'static str,
    kind: String,
    /// The project this tile renders (the document's or the sample's) and its folder.
    sample: bool,
    ops: Vec<Op>,
    at: f64,
}

pub(crate) fn preview_effects(s: &Arc<Services>, args: &J) -> ToolResult {
    let effect_kinds: Vec<&str> = EFFECTS.iter().map(|e| e.kind).collect();
    let transition_kinds: Vec<&str> = TRANSITIONS.iter().map(|t| t.kind).collect();
    let mut effects = kinds_arg(args, "effects", &effect_kinds)?;
    let mut transitions = kinds_arg(args, "transitions", &transition_kinds)?;
    if effects.is_none() && transitions.is_none() {
        effects = Some(effect_kinds.iter().map(|k| k.to_string()).collect());
        transitions = Some(transition_kinds.iter().map(|k| k.to_string()).collect());
    }
    let (effects, transitions) = (effects.unwrap_or_default(), transitions.unwrap_or_default());
    let original = arg_bool(args, "original", true)?;
    let n = effects.len() + transitions.len() + original as usize;
    if n == 0 {
        return Err("nothing to preview: pass effects and/or transitions (kinds or \"all\")".into());
    }
    if n > MAX_TILES {
        return Err(format!("{n} previews requested; at most {MAX_TILES} per sheet"));
    }
    let width = arg_int(args, "width", 64, 640)?.unwrap_or(240) as u32;
    let cols = arg_int(args, "cols", 1, 12)?.unwrap_or(6) as u32;

    // The document (when one is open) and the clip to preview on.
    let doc = find_doc(s, args).ok().map(|(id, d)| {
        let g = suite_hub::lock(&d);
        (id, g.render_input(), g.live.playhead, g.live.selection.clone())
    });
    let picture = |p: &Project, c: &Clip| {
        let shows = match &c.source {
            aiframes_core::Source::Media { asset, .. } => p.media.iter().any(|m| &m.id == asset && m.kind != aiframes_core::MediaKind::Audio),
            _ => true,
        };
        shows && p.tracks.iter().any(|t| t.id == c.track && t.kind == aiframes_core::TrackKind::Video)
    };
    let clip: Option<String> = match (arg_str(args, "clip"), &doc) {
        (Some(c), Some((_, inp, ..))) => {
            let cl = inp.project.clips.iter().find(|x| x.id == c).ok_or_else(|| format!("unknown clip '{c}'"))?;
            if !picture(&inp.project, cl) {
                return Err(format!("clip '{c}' has no picture (sound only); effects and transitions apply to picture clips"));
            }
            Some(c.to_string())
        }
        (Some(_), None) => return Err(NO_DOC.into()),
        (None, Some((_, inp, _, sel))) => sel.iter().find(|id| inp.project.clips.iter().any(|c| &c.id == *id && picture(&inp.project, c))).cloned(),
        (None, None) => None,
    };
    let settings = doc.as_ref().map(|(_, inp, ..)| inp.project.settings.clone()).unwrap_or_default();
    let cache = s.pool.cache_dir().to_path_buf();
    let sample = fxp::sample_project(&cache, settings.width, settings.height, settings.fps)?;
    let sample_r = aiframes_core::resolve(&sample);
    let mut notes: Vec<String> = Vec::new();

    let mut tiles: Vec<Tile> = Vec::new();
    let (base_at, base_label) = match (&clip, &doc) {
        (Some(c), Some((_, inp, playhead, _))) => {
            let sp = inp.resolved.span(c).ok_or_else(|| format!("clip '{c}' has no place on the timeline (unresolved anchor): fix it first (validate)"))?;
            let frame = 1.0 / if settings.fps > 0.0 { settings.fps } else { 30.0 };
            let t = arg_f64(args, "at")?.unwrap_or(*playhead).clamp(sp.start, (sp.end - frame).max(sp.start));
            (t, format!("clip {c} at {} s", secs(t)))
        }
        _ => {
            if arg_f64(args, "at")?.is_some() {
                notes.push("'at' is ignored on the sample picture (no clip given or selected)".into());
            }
            (fxp::SAMPLE_AT, "a sample picture (no picture clip given or selected)".to_string())
        }
    };
    let on_doc = clip.is_some();
    let target = clip.clone().unwrap_or_else(|| fxp::SAMPLE_A.to_string());
    if original {
        tiles.push(Tile { label: "Original".into(), what: "original", kind: String::new(), sample: !on_doc, ops: Vec::new(), at: base_at });
    }
    for k in &effects {
        let label = effect_spec(k).map(|e| e.label).unwrap_or(k).to_string();
        tiles.push(Tile { label, what: "effect", kind: k.clone(), sample: !on_doc, ops: vec![fxp::effect_op(&target, k)], at: base_at });
    }
    // Transitions go on the clip's cut; without one, on the sample's.
    let cut = match (&clip, &doc) {
        (Some(c), Some((_, inp, ..))) => fxp::cut_of(&inp.project, &inp.resolved, c),
        _ => None,
    };
    if !transitions.is_empty() && on_doc && cut.is_none() {
        notes.push(format!("clip {target} has no neighbour on its track (no cut): transitions are shown on the sample picture's cut"));
    }
    for k in &transitions {
        let label = transition_spec(k).map(|t| t.label).unwrap_or(k).to_string();
        let planned = match (&cut, &doc) {
            (Some((from, to)), Some((_, inp, ..))) => fxp::transition_ops(&inp.project, &inp.resolved, from, to, k).map(|(ops, at, d)| (ops, fxp::transition_still(at, d), false)),
            _ => None,
        };
        let (ops, at, sample) = match planned {
            Some(x) => x,
            None => {
                let (ops, at, d) = fxp::transition_ops(&sample, &sample_r, fxp::SAMPLE_A, fxp::SAMPLE_B, k).ok_or_else(|| format!("cannot place transition {k} on the sample"))?;
                (ops, fxp::transition_still(at, d), true)
            }
        };
        tiles.push(Tile { label, what: "transition", kind: k.clone(), sample, ops, at });
    }

    // Render every tile on scratch copies (in parallel; the document is never changed).
    let doc_input = doc.as_ref().map(|(_, inp, ..)| inp);
    let next = AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<Result<Image, String>>>> = tiles.iter().map(|_| std::sync::Mutex::new(None)).collect();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 4);
    let t0 = std::time::Instant::now();
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(t) = tiles.get(i) else { break };
                let r = (|| -> Result<Image, String> {
                    let (p, dir): (&Project, PathBuf) = match (t.sample, doc_input) {
                        (false, Some(inp)) => (&inp.project, inp.dir.clone()),
                        _ => (&sample, cache.clone()),
                    };
                    let sc: Scratch = fxp::scratch(p, &t.ops)?;
                    fxp::render(&s.renderer, &dir, &sc, t.at, width)
                })();
                *suite_hub::lock(&results[i]) = Some(r);
            });
        }
    });
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    let mut grid_tiles: Vec<(Image, String)> = Vec::new();
    let mut list = Vec::new();
    let mut failed = Vec::new();
    let (tw, th) = (width, ((width as f64) * settings.height.max(1) as f64 / settings.width.max(1) as f64).round().max(8.0) as u32);
    for (t, r) in tiles.iter().zip(results) {
        let r = r.into_inner().unwrap_or_else(|e| e.into_inner()).unwrap_or_else(|| Err("not rendered".into()));
        let mut entry = json!({"label": t.label, "what": t.what, "at": t3(t.at)});
        if !t.kind.is_empty() {
            entry["kind"] = json!(t.kind);
        }
        if t.sample && on_doc {
            entry["on"] = json!("sample");
        }
        match r {
            Ok(img) => grid_tiles.push((img, t.label.clone())),
            Err(e) => {
                entry["error"] = json!(e);
                failed.push(json!({"kind": t.kind, "error": e}));
                // A red tile keeps the grid in order and makes the failure visible.
                let mut red = Image::new(tw, th);
                for px in red.rgba.chunks_exact_mut(4) {
                    px.copy_from_slice(&[0x90, 0x10, 0x18, 0xFF]);
                }
                grid_tiles.push((red, format!("{} — FAILED", t.label)));
            }
        }
        list.push(entry);
    }
    let img = fxp::grid(&s.renderer, &grid_tiles, cols)?;
    let mut meta = json!({
        "on": base_label,
        "tiles": list,
        "render_ms": r(ms, 0),
        "note": "Nothing was changed: each tile is a copy of the edit with that one effect (default settings) or transition added. Apply one with edit add_effect {clip, effect} or add_transition {from, to, kind}.",
    });
    if !failed.is_empty() {
        meta["failed"] = json!(failed);
    }
    if !notes.is_empty() {
        meta["notes"] = json!(notes);
    }
    if let Some(path) = arg_str(args, "save_path") {
        let base = doc.as_ref().map(|(_, inp, ..)| inp.dir.clone()).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let mut f = abs_path(path, Some(&base));
        if f.extension().is_none() {
            f.set_extension("png");
        }
        if let Some(why) = crate::actions::protected_target(s, &f) {
            return Err(format!("refusing to write the image there: {why}"));
        }
        if let Some(parent) = f.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&f, encode_png(&img)).map_err(|e| format!("cannot write {}: {e}", f.display()))?;
        meta["file"] = json!(f.to_string_lossy());
    }
    meta["size"] = json!([img.width, img.height]);
    if arg_bool(args, "image", true)? {
        meta[IMAGE_KEY] = json!(base64(&encode_png(&img)));
        meta["image"] = json!(format!("{}x{} PNG", img.width, img.height));
    }
    Ok(meta)
}
