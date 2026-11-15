//! `aiframes demo <dir>`: sample media generated with ffmpeg's lavfi sources plus a project that
//! exercises many features (magnetic chain, anchored titles with reveals, a shape with a draw-on
//! animation, an image with clip-local scale keys, transitions, effects, a voice with a transcript and
//! word-anchored karaoke captions, ducked music, chapter/word/todo markers). Other crates use it for
//! testing; it is deterministic and validates without errors.

use crate::actions;
use crate::tools::project_from_args;
use aiframes_core::{Document, Op, Severity, Transcript, Word};
use aiframes_media::MediaPool;
use serde_json::{json, Value as J};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What [`create`] made.
#[derive(Clone, Debug)]
pub struct Demo {
    /// The `.aifr` file.
    pub project: PathBuf,
    /// Generated media files.
    pub media: Vec<PathBuf>,
    pub revision: u64,
    pub duration: f64,
    pub clips: usize,
    /// Validation errors (normally none).
    pub errors: Vec<String>,
    pub warnings: usize,
}

/// The words of the demo voice: four 2-second phrases starting at 0.3, 3.3, 6.3 and 9.3 s (source),
/// each word 0.45 s long every 0.5 s.
pub const DEMO_PHRASES: [&str; 4] = ["Welcome to the demo", "every edit is undoable", "captions follow the voice", "thanks for watching today"];

pub fn demo_transcript() -> Transcript {
    let mut words = Vec::new();
    for (k, phrase) in DEMO_PHRASES.iter().enumerate() {
        for (j, w) in phrase.split_whitespace().enumerate() {
            let start = k as f64 * 3.0 + 0.3 + j as f64 * 0.5;
            let text = if j == 3 { format!("{w}.") } else { w.to_string() };
            words.push(Word { text, start, end: start + 0.45, conf: 1.0 });
        }
    }
    Transcript { language: "en".into(), words }
}

fn ffmpeg_cmd() -> Result<Command, String> {
    let t = aiframes_media::tools().map_err(|e| e.to_string())?;
    let mut c = Command::new(&t.ffmpeg);
    c.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    Ok(c)
}

fn run_ffmpeg(args: &[&str]) -> Result<(), String> {
    let out = ffmpeg_cmd()?.args(args).output().map_err(|e| format!("cannot run ffmpeg: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("ffmpeg {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Generates `name` in `dir` with the first argument list that works (later ones are fallbacks for
/// ffmpeg builds without a source filter). Existing non-empty files are kept.
fn make(dir: &Path, name: &str, variants: &[Vec<String>]) -> Result<PathBuf, String> {
    let out = dir.join(name);
    if std::fs::metadata(&out).map(|m| m.len() > 0).unwrap_or(false) {
        return Ok(out);
    }
    let tmp = dir.join(format!(".tmp-{}-{name}", std::process::id()));
    let mut last = String::new();
    for v in variants {
        let mut args: Vec<&str> = v.iter().map(String::as_str).collect();
        let t = tmp.to_string_lossy().to_string();
        args.extend(["-f", format_of(name)]);
        args.push(&t);
        match run_ffmpeg(&args) {
            Ok(()) => {
                std::fs::rename(&tmp, &out).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
                return Ok(out);
            }
            Err(e) => last = e,
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Err(last)
}

fn format_of(name: &str) -> &'static str {
    match Path::new(name).extension().and_then(|e| e.to_str()) {
        Some("wav") => "wav",
        Some("png") => "image2",
        _ => "mp4",
    }
}

fn v(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

const X264: [&str; 8] = ["-c:v", "libx264", "-preset", "veryfast", "-crf", "26", "-pix_fmt", "yuv420p"];

fn video(src: &str, tone: Option<&str>) -> Vec<String> {
    let mut a = v(&["-f", "lavfi", "-i", src]);
    if let Some(t) = tone {
        a.extend(v(&["-f", "lavfi", "-i", t, "-af", "volume=0.05", "-c:a", "aac", "-b:a", "96k", "-shortest"]));
    }
    a.extend(v(&X264));
    a
}

/// Generates the demo media into `dir` (created if needed): bars.mp4 (6 s), pattern.mp4 (10 s),
/// gradient.mp4 (8 s, no sound), voice.wav (12 s, four tone "phrases"), music.wav (20 s at 120 BPM),
/// logo.png (400×400).
pub fn generate_media(dir: &Path) -> Result<Vec<PathBuf>, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let voice = "aevalsrc='0.45*sin(2*PI*(170+30*sin(2*PI*2*t))*t)*(0.55+0.45*sin(2*PI*4*t))*between(mod(t,3),0.3,2.3)':s=48000:d=12";
    let music = "aevalsrc='0.3*sin(2*PI*880*t)*exp(-25*mod(t,0.5))+0.1*sin(2*PI*110*t)+0.06*sin(2*PI*165*t)':s=48000:d=20";
    Ok(vec![
        make(dir, "bars.mp4", &[video("smptehdbars=s=1280x720:r=30:d=6", Some("sine=f=1000:r=48000:d=6")), video("testsrc=s=1280x720:r=30:d=6", Some("sine=f=1000:r=48000:d=6"))])?,
        make(dir, "pattern.mp4", &[video("testsrc2=s=1280x720:r=30:d=10", Some("sine=f=440:r=48000:d=10")), video("testsrc=s=1280x720:r=30:d=10", Some("sine=f=440:r=48000:d=10"))])?,
        make(dir, "gradient.mp4", &[video("gradients=s=1280x720:r=30:d=8:speed=0.02", None), video("rgbtestsrc=s=1280x720:r=30:d=8", None)])?,
        make(dir, "voice.wav", &[v(&["-f", "lavfi", "-i", voice, "-c:a", "pcm_s16le"])])?,
        make(dir, "music.wav", &[v(&["-f", "lavfi", "-i", music, "-c:a", "pcm_s16le"])])?,
        make(
            dir,
            "logo.png",
            &[
                v(&["-f", "lavfi", "-i", "gradients=s=400x400:c0=0xff3070:c1=0x3070ff:x0=0:y0=0:x1=400:y1=400:d=1", "-frames:v", "1", "-update", "1"]),
                v(&["-f", "lavfi", "-i", "color=c=0x3050ff:s=400x400:d=1", "-frames:v", "1", "-update", "1"]),
            ],
        )?,
    ])
}

fn ops(list: Vec<J>) -> Result<Vec<Op>, String> {
    list.into_iter().map(actions::op).collect()
}

fn apply(doc: &mut Document, label: &str, list: Vec<J>) -> Result<(), String> {
    doc.apply("demo", label, ops(list)?).map(|_| ()).map_err(|e| format!("demo step \"{label}\": {e}"))
}

/// Generates the media and the project `<dir>/demo.aifr` (overwritten). Media go to `<dir>/media`.
pub fn create(dir: &Path, pool: &MediaPool) -> Result<Demo, String> {
    let media = generate_media(&dir.join("media"))?;
    let mut doc = Document::new(project_from_args(&json!({
        "preset": "youtube",
        "name": "AIFrames demo",
        "notes": "Generated by `aiframes demo`: a 15 s piece exercising anchors, clip-local animation, transitions, effects, captions and ducking.\nStyle: clean, centred titles; captions at the bottom.",
    }))?);
    let path = dir.join("demo.aifr");
    let _ = std::fs::remove_file(aiframes_core::document::history_path(&path));
    doc.save_as(&path).map_err(|e| e.to_string())?;

    let plan = actions::import_plan(&doc, pool, &media)?;
    let id_of = |file: &str| -> Result<String, String> {
        plan.assets
            .iter()
            .find(|a| a.path.file_name().is_some_and(|n| n.to_string_lossy() == file))
            .map(|a| a.id.clone())
            .ok_or_else(|| format!("{file} was not imported"))
    };
    let (bars, pattern, grad, voice, music, logo) = (id_of("bars.mp4")?, id_of("pattern.mp4")?, id_of("gradient.mp4")?, id_of("voice.wav")?, id_of("music.wav")?, id_of("logo.png")?);
    doc.apply("demo", "Import demo media", plan.ops.clone()).map_err(|e| e.to_string())?;

    apply(
        &mut doc,
        "Rough cut",
        vec![
            json!({"op": "append_clip", "track": "v1", "clip": {"id": "intro", "name": "Bars", "asset": bars, "duration": 4}}),
            json!({"op": "append_clip", "track": "v1", "clip": {"id": "pattern", "name": "Test pattern", "asset": pattern, "in_point": 1.0, "duration": 6}}),
            json!({"op": "append_clip", "track": "v1", "clip": {"id": "outro", "name": "Outro", "asset": grad, "in_point": 0.5, "duration": 5}}),
        ],
    )?;
    apply(
        &mut doc,
        "Transitions",
        vec![
            json!({"op": "add_transition", "from": "intro", "to": "pattern", "kind": "crossfade", "duration": 0.8}),
            json!({"op": "add_transition", "from": "pattern", "to": "outro", "kind": "dip_black", "duration": 0.6}),
        ],
    )?;
    apply(
        &mut doc,
        "Grade",
        vec![
            json!({"op": "add_effect", "clip": "intro", "effect": {"kind": "blur", "params": {"radius": {"keys": [{"t": 0, "v": 14, "ease": "out"}, {"t": 1.2, "v": 0}]}}}}),
            json!({"op": "add_effect", "clip": "pattern", "effect": {"kind": "color", "params": {"saturation": 1.35, "contrast": 0.15}}}),
            json!({"op": "add_effect", "clip": "outro", "effect": {"kind": "vignette", "params": {"amount": 0.6}}}),
        ],
    )?;
    apply(
        &mut doc,
        "Titles and graphics",
        vec![
            json!({"op": "add_clip", "clip": {"id": "title", "track": "v2", "start": {"with": "intro", "offset": 0.5}, "duration": 3, "fade_out": 0.4,
                "source": {"type": "text", "text": "AIFrames Demo", "size": 120, "reveal": {"kind": "pop_words", "duration": 0.8}, "shadow": {"color": "#000000AA", "blur": 12, "x": 0, "y": 4}}}}),
            json!({"op": "add_clip", "clip": {"id": "subtitle", "track": "v2", "start": {"after": "title", "offset": 0.2}, "duration": 2, "transform": {"y": 120},
                "source": {"type": "text", "text": "anchors · clip-local keys · captions", "size": 52, "weight": 500, "reveal": {"kind": "typewriter", "duration": 1.0}}}}),
            json!({"op": "add_clip", "clip": {"id": "logo", "track": "v2", "asset": logo, "start": {"with": "outro", "offset": 0.5}, "duration": 4, "fade_in": 0.5,
                "transform": {"fit": "none", "y": -60, "scale": {"keys": [{"t": 0, "v": 0.7, "ease": "in_out"}, {"t": 4, "v": 0.85}]}}}}),
            json!({"op": "add_track", "id": "v3", "kind": "video", "role": "overlay", "name": "Graphics"}),
            json!({"op": "add_clip", "clip": {"id": "lower3", "track": "v3", "start": {"with": "pattern", "offset": 0.5}, "duration": 2.5, "fade_in": 0.3, "fade_out": 0.3,
                "transform": {"x": -420, "y": 340},
                "source": {"type": "text", "text": "testsrc2 · 1280×720", "size": 44, "weight": 600, "align": "left", "background": {"color": "#000000B0", "padding": 20, "radius": 12},
                    "reveal": {"kind": "slide_up", "duration": 0.5}}}}),
            json!({"op": "add_clip", "clip": {"id": "arrow", "track": "v3", "start": {"after": "lower3", "offset": 0.3}, "duration": 2.5, "transform": {"x": -200, "y": 160},
                "source": {"type": "shape", "kind": "arrow", "width": 320, "height": 120, "stroke": {"color": "#FFD400", "width": 12}, "draw": {"keys": [{"t": 0, "v": 0, "ease": "out"}, {"t": 0.6, "v": 1}]}}}}),
        ],
    )?;
    apply(
        &mut doc,
        "Voice and music",
        vec![
            json!({"op": "add_clip", "clip": {"id": "vo", "name": "Voice-over", "track": "a1", "asset": voice, "start": {"with": "intro", "offset": 0.5}}}),
            json!({"op": "add_clip", "clip": {"id": "music", "name": "Music", "track": "a2", "asset": music, "start": 0, "duration": 15, "volume": 0.6, "fade_in": 1, "fade_out": 2}}),
            json!({"op": "add_ducking", "target": "a2", "trigger": "a1", "amount_db": -12}),
        ],
    )?;
    apply(&mut doc, "Transcript", vec![json!({"op": "set_transcript", "asset": voice, "transcript": demo_transcript()})])?;
    apply(
        &mut doc,
        "Captions",
        vec![
            json!({"op": "add_track", "id": "v4", "kind": "video", "role": "captions", "name": "Captions"}),
            json!({"op": "generate_captions", "clips": ["vo"], "track": "v4", "max_words": 4}),
        ],
    )?;
    apply(
        &mut doc,
        "Markers",
        vec![
            json!({"op": "add_marker", "id": "ch_intro", "time": 0, "name": "Intro", "kind": "chapter"}),
            json!({"op": "add_marker", "id": "ch_pattern", "time": {"with": "pattern"}, "name": "Test pattern", "kind": "chapter"}),
            json!({"op": "add_marker", "id": "ch_outro", "time": {"with": "outro"}, "name": "Outro", "kind": "chapter"}),
            json!({"op": "add_marker", "id": "mk_word", "time": {"word": {"clip": "vo", "index": 4}}, "name": "\"every edit\""}),
            json!({"op": "add_marker", "id": "mk_todo", "time": 12.0, "name": "check the logo animation", "kind": "todo"}),
        ],
    )?;
    doc.save().map_err(|e| e.to_string())?;
    let findings = doc.validate();
    Ok(Demo {
        project: path,
        media,
        revision: doc.revision(),
        duration: doc.resolved().duration,
        clips: doc.project.clips.len(),
        errors: findings.iter().filter(|f| f.severity == Severity::Error).map(|f| format!("{}: {}", f.code, f.message)).collect(),
        warnings: findings.iter().filter(|f| f.severity == Severity::Warning).count(),
    })
}
