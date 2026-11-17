//! Preferences and panel layout, persisted in `%APPDATA%\AIFrames\prefs.json`; autosave files.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Decoded-frame cache budget in MB (when not automatic).
    pub cache_mb: u32,
    /// Size the frame cache from the installed RAM (1/64, 256..512 MB).
    pub cache_auto: bool,
    /// Memory for frames decoded ahead, shared by all decode sessions (MB).
    pub readahead_mb: u32,
    /// Scroll the timeline to what the AI changes.
    pub follow_ai: bool,
    /// Also seek to the latest AI change while the person is idle (off by default).
    pub follow_ai_playhead: bool,
    /// "auto" | "off" | "cuda" | "d3d11va".
    pub hw_decode: String,
    /// Seconds between autosaves of dirty projects (0 = off).
    pub autosave_secs: u32,
    pub import_dir: String,
    /// Preview quality ("auto", "full", "half", "quarter").
    pub quality: String,
    pub render_threads: u32,
    pub volume: f32,
    /// Short grains of sound while scrubbing the playhead.
    pub scrub_audio: bool,
    pub layout: Layout,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    pub bin_width: f32,
    pub side_width: f32,
    pub timeline_height: f32,
    pub show_bin: bool,
    pub show_side: bool,
    pub show_timeline: bool,
    /// (added) The Source monitor beside the program viewer.
    pub show_source: bool,
    /// (added) Its width (0 = half of the centre area).
    pub source_width: f32,
    pub side_tab: String,
    pub safe_guides: bool,
    pub window: Option<[f32; 4]>,
    pub maximized: bool,
}

impl Default for Layout {
    fn default() -> Self {
        Layout {
            bin_width: 270.0,
            side_width: 360.0,
            timeline_height: 330.0,
            show_bin: true,
            show_side: true,
            show_timeline: true,
            show_source: false,
            source_width: 0.0,
            side_tab: "inspector".into(),
            safe_guides: false,
            window: None,
            maximized: false,
        }
    }
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            cache_mb: 512,
            cache_auto: true,
            readahead_mb: (aiframes_media::DEFAULT_READAHEAD_BYTES >> 20) as u32,
            follow_ai: true,
            follow_ai_playhead: false,
            hw_decode: "auto".into(),
            autosave_secs: 60,
            import_dir: String::new(),
            quality: "auto".into(),
            render_threads: 3,
            volume: 1.0,
            scrub_audio: true,
            layout: Layout::default(),
        }
    }
}

pub fn app_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("AIFrames")
}

fn prefs_path() -> PathBuf {
    std::env::var_os("AIFRAMES_PREFS").map(PathBuf::from).unwrap_or_else(|| app_dir().join("prefs.json"))
}

/// Copies an unreadable preferences file aside (`prefs.unreadable.json`, or a timestamped name when an
/// older, different unreadable copy is already there) and returns where it went.
fn keep_unreadable(path: &std::path::Path, text: &str) -> Option<PathBuf> {
    let dir = path.parent()?;
    let first = dir.join("prefs.unreadable.json");
    let target = match std::fs::read_to_string(&first) {
        Ok(existing) if existing == text => return Some(first),
        Ok(_) => {
            let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            dir.join(format!("prefs.unreadable-{secs}.json"))
        }
        Err(_) => first,
    };
    std::fs::write(&target, text).ok().map(|_| target)
}

impl Prefs {
    pub fn load() -> Prefs {
        Self::load_reported().0
    }

    /// (added) [`Self::load`], plus a message for the person when the file existed but could not be read.
    /// Such a file is kept as `prefs.unreadable.json` before anything is saved over it, so a hand edit
    /// gone wrong never silently costs the person their settings.
    pub fn load_reported() -> (Prefs, Option<String>) {
        Self::load_from(&prefs_path())
    }

    fn load_from(path: &std::path::Path) -> (Prefs, Option<String>) {
        let Ok(text) = std::fs::read_to_string(path) else { return (Prefs::default().sanitized(), None) };
        // A byte-order mark (e.g. after editing the file in Notepad or PowerShell) is not a problem.
        let body = text.trim_start_matches('\u{feff}');
        if body.trim().is_empty() {
            return (Prefs::default().sanitized(), None);
        }
        match serde_json::from_str::<Prefs>(body) {
            Ok(p) => (p.sanitized(), None),
            Err(e) => {
                let kept = keep_unreadable(path, &text);
                let where_ = kept.map(|k| format!(" The file was kept as {}.", k.display())).unwrap_or_default();
                (Prefs::default().sanitized(), Some(format!("Your preferences file could not be read ({e}), so AIFrames started with default settings.{where_}")))
            }
        }
    }

    pub fn save(&self) {
        let p = prefs_path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(t) = serde_json::to_string_pretty(self) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, t).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }

    /// Clamps values from a hand-edited or older file.
    pub fn sanitized(mut self) -> Prefs {
        self.cache_mb = self.cache_mb.clamp(128, 64 * 1024);
        self.readahead_mb = self.readahead_mb.clamp(32, 4096);
        self.render_threads = self.render_threads.clamp(1, 8);
        self.volume = if self.volume.is_finite() { self.volume.clamp(0.0, 2.0) } else { 1.0 };
        if !["auto", "off", "cuda", "d3d11va"].contains(&self.hw_decode.as_str()) {
            self.hw_decode = "auto".into();
        }
        let l = &mut self.layout;
        l.bin_width = l.bin_width.clamp(160.0, 900.0);
        l.side_width = l.side_width.clamp(220.0, 900.0);
        l.timeline_height = l.timeline_height.clamp(140.0, 1200.0);
        self
    }
}

/// `<project>.aifr.autosave` next to the project file.
pub fn autosave_path(project: &Path) -> PathBuf {
    let name = project.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "project.aifr".into());
    project.with_file_name(format!("{name}.autosave"))
}

/// Autosave location of an unsaved project (one per hub document).
pub fn untitled_autosave_path(doc: u64) -> PathBuf {
    app_dir().join("autosave").join(format!("untitled-{}-{doc}.aifr.autosave", std::process::id()))
}

/// Writes `text` atomically (temp file + rename) so a crash never leaves half a file.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_file_name(format!(".{}.tmp", path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// An autosave newer than the project file whose content differs: worth offering to recover.
pub fn recoverable(project: &Path) -> Option<PathBuf> {
    let auto = autosave_path(project);
    let am = std::fs::metadata(&auto).ok()?.modified().ok()?;
    let pm = std::fs::metadata(project).ok().and_then(|m| m.modified().ok());
    if pm.is_some_and(|pm| pm >= am) {
        return None;
    }
    let a = std::fs::read_to_string(&auto).ok()?;
    let p = std::fs::read_to_string(project).unwrap_or_default();
    (a.trim() != p.trim()).then_some(auto)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_sanitize() {
        let p: Prefs = serde_json::from_str(r#"{"cache_mb": 1, "hw_decode": "nope", "layout": {"bin_width": 5000}}"#).unwrap();
        let p = p.sanitized();
        assert_eq!(p.cache_mb, 128);
        assert_eq!(p.hw_decode, "auto");
        assert_eq!(p.autosave_secs, 60, "missing fields take defaults");
        assert_eq!(p.layout.bin_width, 900.0);
        assert!(p.layout.show_timeline);
    }

    #[test]
    fn unreadable_prefs_are_kept_and_reported_and_a_bom_is_fine() {
        let dir = std::env::temp_dir().join(format!("aiframes-prefs-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prefs.json");
        // A byte-order mark before valid JSON loads normally.
        std::fs::write(&path, "\u{feff}{\"cache_mb\": 2048, \"layout\": {\"side_tab\": \"review\"}}").unwrap();
        let (p, msg) = Prefs::load_from(&path);
        assert!(msg.is_none(), "{msg:?}");
        assert_eq!((p.cache_mb, p.layout.side_tab.as_str()), (2048, "review"));
        // A broken hand edit: defaults, a message, and the person's file kept aside before any save.
        let broken = "{\"cache_mb\": 2048, \"layout\": {\"side_tab\": \"review\",}}";
        std::fs::write(&path, broken).unwrap();
        let (p, msg) = Prefs::load_from(&path);
        assert_eq!(p.cache_mb, Prefs::default().sanitized().cache_mb);
        let msg = msg.expect("reported");
        assert!(msg.contains("prefs.unreadable.json"), "{msg}");
        assert_eq!(std::fs::read_to_string(dir.join("prefs.unreadable.json")).unwrap(), broken);
        // Loading again (the app reads prefs twice at start) keeps the one copy.
        Prefs::load_from(&path);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        // A missing or empty file is simply a first run.
        assert!(Prefs::load_from(&dir.join("none.json")).1.is_none());
        std::fs::write(&path, "  ").unwrap();
        assert!(Prefs::load_from(&path).1.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn autosave_paths_and_recovery() {
        assert_eq!(autosave_path(Path::new("C:/v/edit.aifr")), PathBuf::from("C:/v/edit.aifr.autosave"));
        let dir = std::env::temp_dir().join(format!("aiframes-app-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let proj = dir.join("p.aifr");
        std::fs::write(&proj, "{\"format\":1}\n").unwrap();
        assert!(recoverable(&proj).is_none(), "no autosave");
        std::thread::sleep(std::time::Duration::from_millis(30));
        write_atomic(&autosave_path(&proj), "{\"format\":1,\"name\":\"x\"}\n").unwrap();
        assert_eq!(recoverable(&proj), Some(autosave_path(&proj)));
        // Same content: nothing to recover.
        write_atomic(&autosave_path(&proj), "{\"format\":1}\n").unwrap();
        assert!(recoverable(&proj).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
