//! Word-level transcription with the openai-whisper CLI (`--word_timestamps True`).

use crate::error::{MediaError, Result};
use crate::probe::probe_full;
use crate::tools::{ff_path, ffmpeg_bg, lock, run_capture, spawn_error, tools, whisper_help, ChildGuard, Priority, StderrPump};
use crate::util::TempDir;
use aiframes_core::model::{Transcript, Word};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Progress callback: fraction done, 0..1.
pub type ProgressFn = Arc<dyn Fn(f32) + Send + Sync>;

#[derive(Clone)]
pub struct TranscribeOptions {
    /// Whisper model name (`turbo`, `large-v3`, `medium.en`, ...).
    pub model: String,
    /// Spoken language code (`en`, `de`...); `None` = detect.
    pub language: Option<String>,
    /// `cuda` / `cpu`; `None` = CUDA with automatic fallback to the CPU.
    pub device: Option<String>,
    /// Names and jargon to bias recognition (whisper's `--initial_prompt`).
    pub prompt: Option<String>,
    /// Only transcribe this `[start, end]` of the source (seconds); word times stay in source seconds.
    pub range: Option<[f64; 2]>,
    pub progress: Option<ProgressFn>,
    /// Set to `true` to stop; `transcribe` then returns `MediaError::Cancelled`.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for TranscribeOptions {
    fn default() -> Self {
        TranscribeOptions { model: "turbo".into(), language: None, device: None, prompt: None, range: None, progress: None, cancel: None }
    }
}

impl std::fmt::Debug for TranscribeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscribeOptions")
            .field("model", &self.model)
            .field("language", &self.language)
            .field("device", &self.device)
            .field("prompt", &self.prompt)
            .field("range", &self.range)
            .field("progress", &self.progress.is_some())
            .field("cancel", &self.cancel.is_some())
            .finish()
    }
}

/// Transcribes the file's speech into words with source-second timestamps.
pub fn transcribe(path: &Path, opts: &TranscribeOptions) -> Result<Transcript> {
    let t = tools()?;
    let whisper = t.whisper.clone().ok_or_else(|| MediaError::ToolMissing { tool: "whisper", help: whisper_help() })?;
    let p = probe_full(path)?;
    let Some(ai) = p.audio_index.filter(|_| p.info.has_audio) else {
        return Err(MediaError::unsupported(path, "has no audio stream to transcribe"));
    };
    let cancelled = || opts.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));
    let report = |f: f32| {
        if let Some(cb) = &opts.progress {
            cb(f.clamp(0.0, 1.0));
        }
    };

    // 16 kHz mono WAV aligned to source time (or to the range start).
    let tmp = TempDir::new("aiframes-whisper")?;
    let wav = tmp.0.join("audio.wav");
    let (offset, len) = match opts.range {
        Some([a, b]) if b > a && a.is_finite() && b.is_finite() => (a.max(0.0), Some(b - a.max(0.0))),
        _ => (0.0, None),
    };
    let mut c = ffmpeg_bg("error")?;
    if offset > 0.0 {
        c.args(["-ss", &format!("{offset:.6}")]);
    }
    c.arg("-i").arg(ff_path(path));
    if let Some(l) = len {
        c.args(["-t", &format!("{l:.6}")]);
    }
    c.args(["-map", &format!("0:{ai}"), "-vn", "-sn", "-dn", "-af", "aresample=16000:async=1:first_pts=0", "-ac", "1"]);
    c.args(["-c:a", "pcm_s16le", "-y"]).arg(&wav);
    run_capture(c, "ffmpeg", Some(path), "extract the audio for transcription")?;
    if cancelled() {
        return Err(MediaError::Cancelled);
    }
    report(0.02);

    let devices: Vec<String> = match &opts.device {
        Some(d) => vec![d.clone()],
        None => vec!["cuda".into(), "cpu".into()],
    };
    let mut last_err = None;
    for (i, device) in devices.iter().enumerate() {
        match run_whisper(&whisper, &t.ffmpeg, &wav, &tmp.0, device, opts, &cancelled, &report) {
            Ok(json) => {
                let tr = parse_whisper_json(&json, offset)
                    .ok_or_else(|| MediaError::tool("whisper", Some(path), "transcribe", None, "it wrote JSON without segments"))?;
                report(1.0);
                return Ok(tr);
            }
            Err(MediaError::Cancelled) => return Err(MediaError::Cancelled),
            Err(e) => {
                let gpu_problem = match &e {
                    MediaError::Tool { stderr, .. } => {
                        let s = stderr.to_ascii_lowercase();
                        s.contains("cuda") || s.contains("gpu") || s.contains("nvidia") || s.contains("out of memory")
                    }
                    _ => false,
                };
                last_err = Some(e);
                if !(gpu_problem && i + 1 < devices.len()) {
                    break;
                }
            }
        }
    }
    Err(match last_err {
        Some(MediaError::Tool { tool, action, code, stderr, .. }) => MediaError::Tool { tool, path: Some(path.to_path_buf()), action, code, stderr },
        Some(e) => e,
        None => MediaError::tool("whisper", Some(path), "transcribe", None, ""),
    })
}

/// The command that runs whisper. A `whisper` launcher inside a Python environment (`…\Scripts\whisper.exe`
/// or `…/bin/whisper` next to that environment's python) is run as `python -m whisper` instead: the
/// launcher stubs that pip and uv write break when the environment is moved or its base Python is
/// updated ("uv trampoline failed to canonicalize script path"), while the module keeps working.
pub(crate) fn whisper_command(whisper: &Path) -> std::process::Command {
    let python = whisper.parent().map(|d| d.join(if cfg!(windows) { "python.exe" } else { "python" })).filter(|p| p.is_file());
    let stem = whisper.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    match python {
        Some(py) if stem == "whisper" => {
            let mut c = crate::tools::command_at(&py, Priority::Background);
            c.args(["-m", "whisper"]);
            c
        }
        _ => crate::tools::command_at(whisper, Priority::Background),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_whisper(
    whisper: &Path,
    ffmpeg_exe: &Path,
    wav: &Path,
    out_dir: &Path,
    device: &str,
    opts: &TranscribeOptions,
    cancelled: &dyn Fn() -> bool,
    report: &dyn Fn(f32),
) -> Result<Value> {
    let mut c = whisper_command(whisper);
    c.arg(wav);
    c.args(["--model", if opts.model.trim().is_empty() { "turbo" } else { opts.model.trim() }]);
    c.args(["--device", device, "--output_dir"]).arg(out_dir);
    c.args(["--output_format", "json", "--word_timestamps", "True", "--verbose", "False"]);
    c.args(["--fp16", if device == "cpu" { "False" } else { "True" }]);
    if let Some(l) = opts.language.as_deref().filter(|l| !l.trim().is_empty()) {
        c.args(["--language", l.trim()]);
    }
    if let Some(pr) = opts.prompt.as_deref().filter(|l| !l.trim().is_empty()) {
        c.args(["--initial_prompt", pr]);
    }
    // whisper loads audio by running `ffmpeg`; make sure it finds ours. Force UTF-8 output.
    let mut path_var = std::ffi::OsString::new();
    if let Some(dir) = ffmpeg_exe.parent() {
        path_var.push(dir.as_os_str());
        path_var.push(if cfg!(windows) { ";" } else { ":" });
    }
    path_var.push(std::env::var_os("PATH").unwrap_or_default());
    c.env("PATH", path_var).env("PYTHONIOENCODING", "utf-8").env("PYTHONUTF8", "1").env("TQDM_MININTERVAL", "0.25");
    c.stdout(Stdio::null()).stderr(Stdio::piped());
    let child = c.spawn().map_err(|e| match spawn_error("whisper", e) {
        MediaError::ToolMissing { tool, help } => MediaError::ToolMissing { tool, help: format!("{help} {}", whisper_help()) },
        other => other,
    })?;
    let mut guard = ChildGuard::new(child);
    let progress = Arc::new(Mutex::new(None::<f32>));
    let p2 = progress.clone();
    let pump = StderrPump::start(guard.child().stderr.take().expect("stderr"), move |line| {
        if let Some(f) = parse_tqdm(line) {
            *lock(&p2) = Some(f);
            return true;
        }
        false
    });
    let status = loop {
        if cancelled() {
            guard.kill();
            return Err(MediaError::Cancelled);
        }
        if let Some(f) = lock(&progress).take() {
            report(0.05 + 0.93 * f);
        }
        match guard.child().try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(MediaError::io(wav, "wait for whisper", &e)),
        }
    };
    let tail = pump.finish();
    let json_path: PathBuf = out_dir.join(format!("{}.json", wav.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()));
    if !status.success() || !json_path.is_file() {
        return Err(MediaError::tool("whisper", None, format!("transcribe on {device}"), status.code(), tail.summary()));
    }
    let bytes = std::fs::read(&json_path).map_err(|e| MediaError::io(&json_path, "read the whisper output", &e))?;
    serde_json::from_slice(&bytes).map_err(|e| MediaError::tool("whisper", None, "transcribe", None, format!("unreadable JSON output: {e}")))
}

/// Fraction from a tqdm progress line ("  45%|####      | 1234/2742 [00:03<00:04, 345frames/s]").
fn parse_tqdm(line: &str) -> Option<f32> {
    let bar = line.find('|')?;
    let after = &line[bar + 1..];
    let close = after.find('|')?;
    let counts = after[close + 1..].split_whitespace().next()?;
    let (a, b) = counts.split_once('/')?;
    let (a, b): (f32, f32) = (a.parse().ok()?, b.parse().ok()?);
    (b > 0.0).then(|| (a / b).clamp(0.0, 1.0))
}

/// Words from whisper's JSON (segments[].words[]), shifted by `offset` seconds.
pub(crate) fn parse_whisper_json(v: &Value, offset: f64) -> Option<Transcript> {
    let segments = v.get("segments")?.as_array()?;
    let language = v.get("language").and_then(Value::as_str).unwrap_or("").to_string();
    let mut words: Vec<Word> = Vec::new();
    for seg in segments {
        let seg_words = seg.get("words").and_then(Value::as_array);
        match seg_words {
            Some(ws) if !ws.is_empty() => {
                for w in ws {
                    let text = w.get("word").and_then(Value::as_str).unwrap_or("").trim().to_string();
                    let (Some(s), Some(e)) = (w.get("start").and_then(Value::as_f64), w.get("end").and_then(Value::as_f64)) else {
                        continue;
                    };
                    if text.is_empty() {
                        continue;
                    }
                    let conf = w.get("probability").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.0, 1.0) as f32;
                    words.push(Word { text, start: s + offset, end: e + offset, conf });
                }
            }
            _ => {
                // No word timings: spread the segment's words evenly over it.
                let text = seg.get("text").and_then(Value::as_str).unwrap_or("");
                let (Some(s), Some(e)) = (seg.get("start").and_then(Value::as_f64), seg.get("end").and_then(Value::as_f64)) else {
                    continue;
                };
                let toks: Vec<&str> = text.split_whitespace().collect();
                let step = if toks.is_empty() { 0.0 } else { (e - s).max(0.0) / toks.len() as f64 };
                for (i, tok) in toks.iter().enumerate() {
                    let a = s + i as f64 * step + offset;
                    words.push(Word { text: tok.to_string(), start: a, end: a + step, conf: 0.5 });
                }
            }
        }
    }
    // Keep times ordered and non-negative-length.
    let mut last = f64::NEG_INFINITY;
    for w in &mut words {
        if w.start < last {
            w.start = last;
        }
        if w.end < w.start {
            w.end = w.start;
        }
        last = w.start;
    }
    Some(Transcript { language, words })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tqdm_lines() {
        assert_eq!(parse_tqdm(" 45%|####5     | 1234/2742 [00:03<00:04, 345.67frames/s]").map(|f| (f * 1000.0).round()), Some(450.0));
        assert_eq!(parse_tqdm("100%|##########| 10/10 [00:01<00:00]"), Some(1.0));
        assert_eq!(parse_tqdm("Detected language: English"), None);
    }

    #[test]
    fn whisper_json() {
        let v = json!({
            "language": "en",
            "segments": [
                {"start": 0.0, "end": 1.0, "text": " Hello world.", "words": [
                    {"word": " Hello", "start": 0.1, "end": 0.4, "probability": 0.9},
                    {"word": " world.", "start": 0.45, "end": 0.9, "probability": 0.8}
                ]},
                {"start": 2.0, "end": 3.0, "text": " no words here"}
            ]
        });
        let t = parse_whisper_json(&v, 10.0).unwrap();
        assert_eq!(t.language, "en");
        assert_eq!(t.words.len(), 5);
        assert_eq!(t.words[0].text, "Hello");
        assert!((t.words[0].start - 10.1).abs() < 1e-9);
        assert_eq!(t.words[1].text, "world.");
        assert!((t.words[2].start - 12.0).abs() < 1e-9);
        assert_eq!(t.words[4].text, "here");
    }
}

#[cfg(test)]
mod launcher_tests {
    use super::whisper_command;

    #[test]
    fn a_venv_launcher_runs_as_python_module() {
        let dir = std::env::temp_dir().join(format!("aiframes-whisper-launcher-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = |n: &str| dir.join(if cfg!(windows) { format!("{n}.exe") } else { n.to_string() });
        std::fs::write(exe("whisper"), b"").unwrap();
        // No python next to it: run the launcher itself.
        let c = whisper_command(&exe("whisper"));
        assert_eq!(std::path::Path::new(c.get_program()), exe("whisper"));
        // Inside a Python environment: python -m whisper (launcher stubs break when the environment moves).
        std::fs::write(exe("python"), b"").unwrap();
        let c = whisper_command(&exe("whisper"));
        assert_eq!(std::path::Path::new(c.get_program()), exe("python"));
        assert_eq!(c.get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>(), vec!["-m", "whisper"]);
        // Some other name (a wrapper the person pointed AIFRAMES_WHISPER at) is run as given.
        std::fs::write(exe("my-whisper"), b"").unwrap();
        assert_eq!(std::path::Path::new(whisper_command(&exe("my-whisper")).get_program()), exe("my-whisper"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
