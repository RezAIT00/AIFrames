//! The single error type of the media layer. Every message names the file involved and, when a
//! child process failed, what it said, so a person or an AI can act on it.

use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub enum MediaError {
    /// A required executable (ffmpeg, ffprobe, whisper) could not be found.
    ToolMissing { tool: &'static str, help: String },
    /// The media file does not exist.
    NotFound { path: PathBuf },
    /// A filesystem operation failed.
    Io { path: PathBuf, action: String, message: String },
    /// ffmpeg / ffprobe / whisper ran but failed. `stderr` is the tail of what it printed.
    Tool { tool: &'static str, path: Option<PathBuf>, action: String, code: Option<i32>, stderr: String },
    /// The file exists but cannot be used for this purpose (no video stream, corrupt image...).
    Unsupported { path: PathBuf, message: String },
    /// The caller passed something invalid (odd size, empty frame, bad option...).
    InvalidInput(String),
    /// The operation was cancelled by the caller.
    Cancelled,
}

pub(crate) type Result<T, E = MediaError> = std::result::Result<T, E>;

impl MediaError {
    pub(crate) fn io(path: &Path, action: impl Into<String>, err: &std::io::Error) -> Self {
        if err.kind() == std::io::ErrorKind::NotFound && !path.as_os_str().is_empty() && !path.exists() {
            return MediaError::NotFound { path: path.to_path_buf() };
        }
        MediaError::Io { path: path.to_path_buf(), action: action.into(), message: err.to_string() }
    }

    pub(crate) fn unsupported(path: &Path, message: impl Into<String>) -> Self {
        MediaError::Unsupported { path: path.to_path_buf(), message: message.into() }
    }

    pub(crate) fn tool(
        tool: &'static str,
        path: Option<&Path>,
        action: impl Into<String>,
        code: Option<i32>,
        stderr: impl Into<String>,
    ) -> Self {
        MediaError::Tool { tool, path: path.map(Path::to_path_buf), action: action.into(), code, stderr: stderr.into() }
    }

    /// The file this error is about, if any.
    pub fn path(&self) -> Option<&Path> {
        match self {
            MediaError::NotFound { path } | MediaError::Io { path, .. } | MediaError::Unsupported { path, .. } => Some(path),
            MediaError::Tool { path, .. } => path.as_deref(),
            _ => None,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, MediaError::Cancelled)
    }
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaError::ToolMissing { tool, help } => write!(f, "{tool} was not found. {help}"),
            MediaError::NotFound { path } => write!(
                f,
                "media file not found: {} (was it moved, renamed or deleted? relink the asset to its new location)",
                path.display()
            ),
            MediaError::Io { path, action, message } => write!(f, "could not {action} {}: {message}", path.display()),
            MediaError::Tool { tool, path, action, code, stderr } => {
                write!(f, "{tool} failed to {action}")?;
                if let Some(p) = path {
                    write!(f, " ({})", p.display())?;
                }
                if let Some(c) = code {
                    write!(f, " [exit code {c}]")?;
                }
                let s = stderr.trim();
                if s.is_empty() {
                    write!(f, "; it printed nothing")
                } else {
                    write!(f, ": {s}")
                }
            }
            MediaError::Unsupported { path, message } => write!(f, "{}: {message}", path.display()),
            MediaError::InvalidInput(m) => write!(f, "invalid input: {m}"),
            MediaError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for MediaError {}
