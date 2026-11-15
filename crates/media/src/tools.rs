//! Discovery of the external executables (ffmpeg, ffprobe, whisper) and the child-process
//! plumbing shared by every module: no console windows on Windows, kill-on-drop guards, stderr
//! tails for error messages.

use crate::error::{MediaError, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

/// The external executables the media layer runs as child processes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// openai-whisper CLI; `None` when not installed (transcription then reports how to install it).
    pub whisper: Option<PathBuf>,
}

static TOOLS: OnceLock<Tools> = OnceLock::new();

/// Finds ffmpeg, ffprobe and (optionally) whisper. Successful discovery is cached for the life of
/// the process; a failure is not cached, so installing ffmpeg and retrying works without a restart.
///
/// Search order: `AIFRAMES_FFMPEG` / `AIFRAMES_FFPROBE` / `AIFRAMES_WHISPER` (a file or the
/// folder containing it), then `PATH`, then common install locations. ffprobe is also looked for
/// next to ffmpeg.
pub fn tools() -> Result<&'static Tools> {
    if let Some(t) = TOOLS.get() {
        return Ok(t);
    }
    let t = discover()?;
    Ok(TOOLS.get_or_init(|| t))
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Resolves an env override that may name the executable itself or its folder.
fn from_env(var: &str, name: &str) -> Option<std::result::Result<PathBuf, String>> {
    let v = std::env::var_os(var)?;
    if v.is_empty() {
        return None;
    }
    let p = PathBuf::from(v);
    if p.is_file() {
        return Some(Ok(p));
    }
    let inside = p.join(exe(name));
    if inside.is_file() {
        return Some(Ok(inside));
    }
    Some(Err(format!("{var} is set to {} but no {} exists there", p.display(), exe(name))))
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let file = exe(name);
    std::env::split_paths(&path).map(|d| d.join(&file)).find(|p| p.is_file())
}

fn common_locations(name: &str) -> Vec<PathBuf> {
    let mut v = Vec::new();
    let file = exe(name);
    if cfg!(windows) {
        if let Some(l) = std::env::var_os("LOCALAPPDATA") {
            v.push(PathBuf::from(&l).join("Microsoft").join("WinGet").join("Links").join(&file));
        }
        if let Some(u) = std::env::var_os("USERPROFILE") {
            v.push(PathBuf::from(&u).join("scoop").join("shims").join(&file));
        }
        v.push(PathBuf::from(r"C:\ProgramData\chocolatey\bin").join(&file));
        v.push(PathBuf::from(r"C:\ffmpeg\bin").join(&file));
        if let Some(pf) = std::env::var_os("ProgramFiles") {
            v.push(PathBuf::from(&pf).join("ffmpeg").join("bin").join(&file));
        }
    } else {
        v.push(PathBuf::from("/usr/local/bin").join(&file));
        v.push(PathBuf::from("/opt/homebrew/bin").join(&file));
        v.push(PathBuf::from("/usr/bin").join(&file));
    }
    v
}

const FFMPEG_HELP: &str = "Install ffmpeg (for example `winget install Gyan.FFmpeg`) so that ffmpeg.exe is on PATH, \
or set AIFRAMES_FFMPEG to the full path of ffmpeg.exe.";

fn discover() -> Result<Tools> {
    let ffmpeg = match from_env("AIFRAMES_FFMPEG", "ffmpeg") {
        Some(Ok(p)) => p,
        Some(Err(why)) => return Err(MediaError::ToolMissing { tool: "ffmpeg", help: format!("{why}. {FFMPEG_HELP}") }),
        None => on_path("ffmpeg")
            .or_else(|| common_locations("ffmpeg").into_iter().find(|p| p.is_file()))
            .ok_or_else(|| MediaError::ToolMissing { tool: "ffmpeg", help: FFMPEG_HELP.to_string() })?,
    };
    let ffprobe = match from_env("AIFRAMES_FFPROBE", "ffprobe") {
        Some(Ok(p)) => p,
        Some(Err(why)) => return Err(MediaError::ToolMissing { tool: "ffprobe", help: why }),
        None => ffmpeg
            .parent()
            .map(|d| d.join(exe("ffprobe")))
            .filter(|p| p.is_file())
            .or_else(|| on_path("ffprobe"))
            .or_else(|| common_locations("ffprobe").into_iter().find(|p| p.is_file()))
            .ok_or_else(|| MediaError::ToolMissing {
                tool: "ffprobe",
                help: format!(
                    "ffmpeg was found at {} but ffprobe was not. Install a full ffmpeg build (it ships ffprobe) or set AIFRAMES_FFPROBE.",
                    ffmpeg.display()
                ),
            })?,
    };
    Ok(Tools { ffmpeg, ffprobe, whisper: find_whisper() })
}

fn find_whisper() -> Option<PathBuf> {
    match from_env("AIFRAMES_WHISPER", "whisper") {
        Some(Ok(p)) => return Some(p),
        Some(Err(_)) => return None, // transcribe() explains the bad override
        None => {}
    }
    on_path("whisper")
}

/// Why whisper is unavailable, with install instructions.
pub(crate) fn whisper_help() -> String {
    let mut s = String::new();
    if let Some(Err(why)) = from_env("AIFRAMES_WHISPER", "whisper") {
        s.push_str(&why);
        s.push_str(". ");
    }
    s.push_str(
        "Install openai-whisper (`pip install -U openai-whisper`) so that whisper.exe is on PATH, or set AIFRAMES_WHISPER \
         to the full path of whisper.exe.",
    );
    s
}

impl Tools {
    /// Whether this ffmpeg build has the named encoder (`libx264`, `h264_nvenc`, `libsvtav1`...).
    pub fn has_encoder(&self, name: &str) -> bool {
        encoders(&self.ffmpeg).contains(name)
    }

    /// Whether the named hardware encoder can actually open a session on this machine (an NVENC
    /// encoder is listed by every full build, but needs an NVIDIA GPU and driver). Cached.
    pub fn hardware_encoder_works(&self, name: &str) -> bool {
        static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(v) = lock(cache).get(name) {
            return *v;
        }
        let ok = self.has_encoder(name) && {
            let mut c = command(&self.ffmpeg);
            c.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-f", "lavfi", "-i"])
                .arg("color=c=black:s=256x144:r=30:d=0.2")
                .args(["-c:v", name, "-f", "null", "-"])
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            c.status().map(|s| s.success()).unwrap_or(false)
        };
        lock(cache).insert(name.to_string(), ok);
        ok
    }
}

fn encoders(ffmpeg: &Path) -> &'static HashSet<String> {
    static ENC: OnceLock<HashSet<String>> = OnceLock::new();
    ENC.get_or_init(|| {
        let mut set = HashSet::new();
        let mut c = command(ffmpeg);
        c.args(["-hide_banner", "-nostdin", "-encoders"]).stderr(Stdio::null());
        if let Ok(out) = c.output() {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut past_header = false;
            for line in text.lines() {
                if line.trim_start().starts_with("------") {
                    past_header = true;
                    continue;
                }
                if !past_header {
                    continue;
                }
                let mut it = line.split_whitespace();
                if let (Some(_flags), Some(name)) = (it.next(), it.next()) {
                    set.insert(name.to_string());
                }
            }
        }
        set
    })
}

// ---------------------------------------------------------------------------------------------
// Child processes
// ---------------------------------------------------------------------------------------------

/// Locks a mutex, recovering from poisoning (a panicked holder must not take the pool down).
pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// How urgent a child process is. Background work (whole-file audio decodes, thumbnails,
/// analysis, transcription) runs at below-normal priority on Windows so heavy projects never
/// starve playback and export, which stay at normal priority. `AIFRAMES_BACKGROUND_PRIORITY=normal`
/// turns the lowering off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Priority {
    Normal,
    Background,
}

fn lower_background() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| !std::env::var("AIFRAMES_BACKGROUND_PRIORITY").is_ok_and(|v| v.trim().eq_ignore_ascii_case("normal")))
}

/// A `Command` that never opens a console window and has no stdin.
pub(crate) fn command(program: &Path) -> Command {
    command_at(program, Priority::Normal)
}

/// `command` at the given priority.
pub(crate) fn command_at(program: &Path, priority: Priority) -> Command {
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        let mut flags = CREATE_NO_WINDOW;
        if priority == Priority::Background && lower_background() {
            flags |= BELOW_NORMAL_PRIORITY_CLASS;
        }
        c.creation_flags(flags);
    }
    #[cfg(not(windows))]
    let _ = (priority, lower_background);
    c.stdin(Stdio::null());
    c
}

/// `ffmpeg -hide_banner -nostdin` with the given log level.
pub(crate) fn ffmpeg(loglevel: &str) -> Result<Command> {
    ffmpeg_at(loglevel, Priority::Normal)
}

/// `ffmpeg` for background work (below-normal priority on Windows).
pub(crate) fn ffmpeg_bg(loglevel: &str) -> Result<Command> {
    ffmpeg_at(loglevel, Priority::Background)
}

pub(crate) fn ffmpeg_at(loglevel: &str, priority: Priority) -> Result<Command> {
    let t = tools()?;
    let mut c = command_at(&t.ffmpeg, priority);
    c.args(["-hide_banner", "-nostdin", "-nostats", "-loglevel", loglevel]);
    Ok(c)
}

/// Kills (and reaps) the child when dropped unless it already exited.
pub(crate) struct ChildGuard(Option<Child>);

impl ChildGuard {
    pub fn new(c: Child) -> Self {
        ChildGuard(Some(c))
    }
    pub fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("child taken")
    }
    pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.child().wait()
    }
    pub fn kill(&mut self) {
        if let Some(c) = self.0.as_mut() {
            if let Ok(None) = c.try_wait() {
                let _ = c.kill();
            }
            let _ = c.wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Keeps the last lines of a child's stderr for error messages.
#[derive(Default)]
pub(crate) struct Tail {
    lines: VecDeque<String>,
}

const TAIL_LINES: usize = 24;
const TAIL_CHARS: usize = 3000;

impl Tail {
    pub fn push(&mut self, line: &str) {
        let l = line.trim_end();
        if l.trim().is_empty() {
            return;
        }
        if self.lines.len() == TAIL_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(l.to_string());
    }
    pub fn text(&self) -> String {
        let mut s = String::new();
        for l in &self.lines {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(l);
        }
        if s.len() > TAIL_CHARS {
            let mut cut = s.len() - TAIL_CHARS;
            while !s.is_char_boundary(cut) {
                cut += 1;
            }
            s = format!("...{}", &s[cut..]);
        }
        s
    }
    /// The most informative lines (errors first) for a one-paragraph message.
    pub fn summary(&self) -> String {
        let interesting: Vec<&String> = self
            .lines
            .iter()
            .filter(|l| {
                let x = l.to_ascii_lowercase();
                x.contains("error") || x.contains("invalid") || x.contains("failed") || x.contains("no such")
                    || x.contains("not ") || x.contains("unable") || x.contains("cannot") || x.contains("unknown")
            })
            .collect();
        if interesting.is_empty() {
            self.text()
        } else {
            interesting.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
        }
    }
}

/// Drains a child's stderr on a thread, keeping a tail and passing every line to `on_line`
/// (which returns `true` when it consumed the line, so it is not kept in the tail).
pub(crate) struct StderrPump {
    pub tail: Arc<Mutex<Tail>>,
    handle: Option<JoinHandle<()>>,
}

impl StderrPump {
    pub fn start(stderr: ChildStderr, on_line: impl FnMut(&str) -> bool + Send + 'static) -> Self {
        Self::start_with_close(stderr, on_line, || {})
    }

    /// Like `start`, calling `on_close` once stderr has closed (the child exited).
    pub fn start_with_close(
        stderr: ChildStderr,
        mut on_line: impl FnMut(&str) -> bool + Send + 'static,
        on_close: impl FnOnce() + Send + 'static,
    ) -> Self {
        let tail = Arc::new(Mutex::new(Tail::default()));
        let t2 = tail.clone();
        let handle = std::thread::Builder::new()
            .name("ffmpeg-stderr".into())
            .spawn(move || {
                let mut r = BufReader::with_capacity(64 * 1024, stderr);
                let mut buf = Vec::with_capacity(512);
                loop {
                    buf.clear();
                    match read_line_cr(&mut r, &mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let line = String::from_utf8_lossy(&buf);
                            if !on_line(&line) {
                                lock(&t2).push(&line);
                            }
                        }
                    }
                }
                on_close();
            })
            .ok();
        StderrPump { tail, handle }
    }

    /// Waits for the child's stderr to close and returns the tail.
    pub fn finish(mut self) -> Tail {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        std::mem::take(&mut *lock(&self.tail))
    }
}

/// Reads one line ending in `\n` or `\r` (progress bars redraw with `\r`).
fn read_line_cr<R: BufRead>(r: &mut R, out: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut total = 0;
    loop {
        let (done, used) = {
            let available = match r.fill_buf() {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if available.is_empty() {
                return Ok(total);
            }
            match available.iter().position(|&b| b == b'\n' || b == b'\r') {
                Some(i) => {
                    out.extend_from_slice(&available[..i]);
                    (true, i + 1)
                }
                None => {
                    out.extend_from_slice(available);
                    (false, available.len())
                }
            }
        };
        r.consume(used);
        total += used;
        if done {
            return Ok(total);
        }
    }
}

/// Runs a command to completion, capturing stdout; on failure returns a `Tool` error with the
/// stderr tail.
pub(crate) fn run_capture(
    mut cmd: Command,
    tool: &'static str,
    path: Option<&Path>,
    action: &str,
) -> Result<(Vec<u8>, String)> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn().map_err(|e| spawn_error(tool, e))?;
    let mut guard = ChildGuard::new(child);
    let stderr = guard.child().stderr.take();
    let mut stdout = guard.child().stdout.take();
    let err_thread = stderr.map(|mut s| {
        std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = s.read_to_end(&mut v);
            v
        })
    });
    let mut out = Vec::new();
    if let Some(s) = stdout.as_mut() {
        let _ = s.read_to_end(&mut out);
    }
    let status = guard.wait().map_err(|e| spawn_error(tool, e))?;
    let err_bytes = err_thread.and_then(|h| h.join().ok()).unwrap_or_default();
    let err = String::from_utf8_lossy(&err_bytes).into_owned();
    if !status.success() {
        let mut tail = Tail::default();
        for l in err.lines() {
            tail.push(l);
        }
        return Err(MediaError::tool(tool, path, action, status.code(), tail.summary()));
    }
    Ok((out, err))
}

pub(crate) fn spawn_error(tool: &'static str, e: std::io::Error) -> MediaError {
    MediaError::ToolMissing { tool, help: format!("it could not be started ({e}). {FFMPEG_HELP}") }
}

/// A path as an ffmpeg argument. ffmpeg treats `name:...` as a protocol, so plain relative paths
/// that contain a colon are prefixed with `file:`.
pub(crate) fn ff_path(p: &Path) -> std::ffi::OsString {
    let s = p.as_os_str().to_os_string();
    let text = p.to_string_lossy();
    let looks_like_protocol = text.find(':').is_some_and(|i| i > 1) && !p.is_absolute();
    if looks_like_protocol {
        let mut o = std::ffi::OsString::from("file:");
        o.push(&s);
        o
    } else {
        s
    }
}

#[cfg(all(test, windows))]
mod priority_tests {
    use super::*;

    /// Priority class of a running process as Windows reports it ("Normal", "BelowNormal").
    fn priority_class(pid: u32) -> String {
        let out = command(Path::new("powershell"))
            .args(["-NoProfile", "-NonInteractive", "-Command", &format!("(Get-Process -Id {pid}).PriorityClass")])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn background_children_run_below_normal() {
        for (priority, want) in [(Priority::Background, "BelowNormal"), (Priority::Normal, "Normal")] {
            let mut c = ffmpeg_at("error", priority).unwrap();
            c.args(["-re", "-f", "lavfi", "-i", "anullsrc=r=8000", "-t", "20", "-f", "null", "-"]).stdout(Stdio::null()).stderr(Stdio::null());
            let mut child = ChildGuard::new(c.spawn().unwrap());
            let pid = child.child().id();
            assert_eq!(priority_class(pid), want, "{priority:?}");
            child.kill();
        }
    }
}
