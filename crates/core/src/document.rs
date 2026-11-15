//! An open project: canonical file IO, an attributed history, snapshot undo/redo, dry-run plans and
//! receipts that list every clip whose resolved position changed.
//!
//! Files: `<name>.aifr` is canonical pretty JSON (2-space indent, defaults omitted, trailing newline),
//! written atomically (temp file + rename). History entries are appended to
//! `<name>.aifr.history.jsonl` (one JSON object per line) on save and loaded on open, so revision
//! numbers continue across sessions and `changes_since` works for an AI coming back to a project.

use crate::model::*;
use crate::ops::{apply_batch_as, EditError, Op, OpOutcome};
use crate::resolve::{resolve, Resolved, Span};
use crate::validate::{validate, Finding, Severity};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

/// A clip whose resolved span changed (`None` = did not exist / not placeable).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Moved {
    pub id: Id,
    pub before: Option<Span>,
    pub after: Option<Span>,
}

/// What an edit (or undo/redo, or a dry-run plan) did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    /// The document revision after the change (for `plan`: the revision it would get).
    pub revision: u64,
    pub author: String,
    pub label: String,
    pub outcomes: Vec<OpOutcome>,
    /// Every clip whose resolved span changed, created or removed, sorted by time.
    pub moved: Vec<Moved>,
    pub warnings: Vec<String>,
    /// Resolve errors that did not exist before (normally empty: ops refuse to create them).
    pub new_errors: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HistoryKind {
    #[default]
    Edit,
    Undo,
    Redo,
}

fn is_edit(k: &HistoryKind) -> bool {
    *k == HistoryKind::Edit
}

/// One line of history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub revision: u64,
    /// "user", "ai", or any name.
    pub author: String,
    pub label: String,
    /// RFC 3339 UTC, e.g. "2026-11-08T21:41:00Z".
    pub time: String,
    #[serde(default, skip_serializing_if = "is_edit")]
    pub kind: HistoryKind,
    /// For undo/redo: the revision of the edit that was undone/redone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ops: Vec<Op>,
    /// The ops' summaries, joined with "; ".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocErrorKind {
    Io,
    Parse,
    /// `save` on a document that was never saved (use `save_as`).
    NoPath,
    /// Written by a newer AIFrames.
    Format,
    /// `save` refused: another program changed the project file since this document opened, saved or
    /// reloaded it. Reload it ([`Document::reload_from_disk`]) or overwrite it on purpose
    /// ([`Document::save_force`]).
    DiskChanged,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DocError {
    pub kind: DocErrorKind,
    pub path: Option<PathBuf>,
    pub message: String,
}

impl DocError {
    fn io(path: &Path, what: &str, e: std::io::Error) -> Self {
        DocError { kind: DocErrorKind::Io, path: Some(path.to_path_buf()), message: format!("{what} {}: {e}", path.display()) }
    }
}

impl fmt::Display for DocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DocError {}

struct Step {
    project: Project,
    revision: u64,
    label: String,
}

/// What the file system says about a file's contents: modification time and size. Two stamps of the
/// same file differ when another program wrote it (see [`Document::disk_changed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    pub modified: Option<std::time::SystemTime>,
    pub len: u64,
}

impl FileStamp {
    /// The stamp of the file at `path` now (`None` if it does not exist or cannot be read).
    pub fn of(path: &Path) -> Option<FileStamp> {
        let m = std::fs::metadata(path).ok()?;
        Some(FileStamp { modified: m.modified().ok(), len: m.len() })
    }
}

/// The canonical file text of a project: pretty JSON, 2-space indent, defaults omitted, newline-terminated.
///
/// JSON cannot hold NaN or infinity (they would be written as `null` and the file would not open
/// again); [`try_canonical_json`] refuses such projects, and ops never produce them.
pub fn canonical_json(p: &Project) -> String {
    let mut s = serde_json::to_string_pretty(p).unwrap_or_else(|_| "{}".into());
    s.push('\n');
    s
}

/// [`canonical_json`], refusing projects that contain a number that is not finite.
pub fn try_canonical_json(p: &Project) -> Result<String, DocError> {
    if let Some(what) = crate::ops::prims::first_non_finite(p) {
        return Err(DocError {
            kind: DocErrorKind::Format,
            path: None,
            message: format!("cannot save: {what} is not a finite number (JSON has no NaN/infinity, so the file could not be opened again). Fix that value first."),
        });
    }
    Ok(canonical_json(p))
}

static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The history file next to a project file: `<project>.aifr.history.jsonl`.
pub fn history_path(project_path: &Path) -> PathBuf {
    let name = project_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "project.aifr".into());
    project_path.with_file_name(format!("{name}.history.jsonl"))
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Rough in-memory size of a project snapshot (for the undo budget).
fn estimate_bytes(p: &Project) -> usize {
    let words: usize = p.transcripts.values().map(|t| t.words.len()).sum();
    2048 + p.clips.len() * 700 + words * 64 + p.media.len() * 300 + p.markers.len() * 120 + p.notes.len()
}

pub struct Document {
    pub project: Project,
    /// The `.aifr` file, once saved/opened.
    pub path: Option<PathBuf>,
    resolved: Resolved,
    history: Vec<HistoryEntry>,
    undo: VecDeque<Step>,
    redo: Vec<Step>,
    revision: u64,
    saved_revision: u64,
    /// Number of history entries already written to the history file.
    persisted: usize,
    /// Most undo steps kept (default 300).
    pub undo_limit: usize,
    /// Approximate memory budget for undo snapshots (default 512 MB); the oldest steps are dropped first.
    pub undo_budget_bytes: usize,
    /// Problems found while opening (e.g. unreadable history lines); the document still works.
    load_warnings: Vec<String>,
    /// The project file's stamp when this document last read or wrote it.
    stamp: Option<FileStamp>,
}

impl Document {
    pub fn new(project: Project) -> Self {
        let resolved = resolve(&project);
        Document {
            project,
            path: None,
            resolved,
            history: Vec::new(),
            undo: VecDeque::new(),
            redo: Vec::new(),
            revision: 0,
            saved_revision: 0,
            persisted: 0,
            undo_limit: 300,
            undo_budget_bytes: 512 << 20,
            load_warnings: Vec::new(),
            stamp: None,
        }
    }

    /// Problems noticed by [`Document::open`] that did not stop it (unreadable history lines).
    pub fn load_warnings(&self) -> &[String] {
        &self.load_warnings
    }

    /// Opens a project file (and its history file, if any).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DocError> {
        let path = path.as_ref();
        let stamp = FileStamp::of(path);
        let text = std::fs::read_to_string(path).map_err(|e| DocError::io(path, "cannot read project", e))?;
        let project = parse_project(&text).map_err(|mut e| {
            e.path = Some(path.to_path_buf());
            e.message = format!("{}: {}", path.display(), e.message);
            e
        })?;
        let mut doc = Document::new(project);
        doc.path = Some(absolute(path));
        doc.stamp = stamp;
        if let Ok(h) = std::fs::read_to_string(history_path(path)) {
            let lines: Vec<&str> = h.lines().filter(|l| !l.trim().is_empty()).collect();
            let mut bad = Vec::new();
            for (i, l) in lines.iter().enumerate() {
                match serde_json::from_str::<HistoryEntry>(l) {
                    Ok(e) => doc.history.push(e),
                    Err(e) => bad.push(format!("line {}: {e}", i + 1)),
                }
            }
            // Each line is one revision, so new revisions continue after the highest one seen and after
            // the number of lines (unreadable lines may have held the latest revisions).
            doc.revision = doc.history.iter().map(|e| e.revision).max().unwrap_or(0).max(lines.len() as u64);
            doc.persisted = doc.history.len();
            if !bad.is_empty() {
                doc.load_warnings.push(format!(
                    "{} history line(s) in {} could not be read and were skipped (they stay in the file): {}",
                    bad.len(),
                    history_path(path).display(),
                    crate::util::join_limited(&bad, 3)
                ));
            }
        }
        doc.saved_revision = doc.revision;
        Ok(doc)
    }

    /// Saves to `path` (error if the document has none: use `save_as`).
    ///
    /// Refused with [`DocErrorKind::DiskChanged`] when another program changed the file since this
    /// document opened, saved or reloaded it ([`Document::disk_changed`]), so its changes are never
    /// overwritten silently: reload ([`Document::reload_from_disk`]) or overwrite on purpose
    /// ([`Document::save_force`]). A file that was deleted meanwhile is simply written again.
    pub fn save(&mut self) -> Result<(), DocError> {
        let path = self.save_path()?;
        self.check_disk(&path)?;
        self.write(&path, false)
    }

    /// [`Document::save`] even when the file changed on disk (overwrites the other program's version;
    /// its history entries are kept in the history file).
    pub fn save_force(&mut self) -> Result<(), DocError> {
        let path = self.save_path()?;
        self.write(&path, false)
    }

    fn save_path(&self) -> Result<PathBuf, DocError> {
        self.path.clone().ok_or_else(|| DocError { kind: DocErrorKind::NoPath, path: None, message: "the project has never been saved; use save_as with a .aifr path".into() })
    }

    /// Refuses to overwrite a project file another program changed since we last read or wrote it.
    fn check_disk(&self, path: &Path) -> Result<(), DocError> {
        let now = FileStamp::of(path);
        if now.is_none() || now == self.stamp {
            return Ok(());
        }
        Err(DocError {
            kind: DocErrorKind::DiskChanged,
            path: Some(path.to_path_buf()),
            message: format!(
                "{} was changed on disk by another program since it was opened or saved here; saving would overwrite those changes. Reload it first (its changes become an undoable revision), or save with force to overwrite them.",
                path.display()
            ),
        })
    }

    /// Saves to a new path. Relative media paths are rebased so they still point at the same files;
    /// media under (or near) the new folder become relative. The full history is written next to it.
    /// Saving to the document's own path follows the rule of [`Document::save`] (refused when the
    /// file changed on disk; see [`Document::save_as_force`]).
    pub fn save_as(&mut self, path: impl AsRef<Path>) -> Result<(), DocError> {
        let path = absolute(path.as_ref());
        if self.path.as_deref() == Some(path.as_path()) {
            self.check_disk(&path)?;
        }
        self.save_as_unchecked(path)
    }

    /// [`Document::save_as`] that overwrites the document's own file even when it changed on disk.
    pub fn save_as_force(&mut self, path: impl AsRef<Path>) -> Result<(), DocError> {
        self.save_as_unchecked(absolute(path.as_ref()))
    }

    fn save_as_unchecked(&mut self, path: PathBuf) -> Result<(), DocError> {
        let new_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let old_dir = self.project_dir();
        let rebase = |p: &mut Project| {
            for m in &mut p.media {
                let abs = resolve_media(&old_dir, &m.path);
                m.path = portable_path(&abs, &new_dir);
            }
        };
        rebase(&mut self.project);
        // Undo/redo snapshots must point at the same files from the new location too.
        for s in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            rebase(&mut s.project);
        }
        let same = self.path.as_deref() == Some(path.as_path());
        self.path = Some(path.clone());
        self.write(&path, !same)
    }

    fn write(&mut self, path: &Path, fresh_history: bool) -> Result<(), DocError> {
        // Another program may have appended to the history file since we read it: take its entries in
        // and number ours after them, so revision numbers in the file never repeat.
        if !fresh_history {
            self.sync_history(&history_path(path));
        }
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() && !dir.exists() {
                std::fs::create_dir_all(dir).map_err(|e| DocError::io(dir, "cannot create folder", e))?;
            }
        }
        let text = try_canonical_json(&self.project).map_err(|mut e| {
            e.path = Some(path.to_path_buf());
            e
        })?;
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "project.aifr".into());
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let n = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_file_name(format!(".{name}.tmp-{}-{n}-{nanos}", std::process::id()));
        let res = (|| -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, path)
        })();
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            return Err(DocError::io(path, "cannot write project", e));
        }
        self.stamp = FileStamp::of(path);
        // History: append what is new (or write it all for a new location).
        let hp = history_path(path);
        let from = if fresh_history { 0 } else { self.persisted };
        if from < self.history.len() || fresh_history {
            let mut lines = String::new();
            for e in &self.history[from..] {
                if let Ok(l) = serde_json::to_string(e) {
                    lines.push_str(&l);
                    lines.push('\n');
                }
            }
            let res = if fresh_history {
                std::fs::write(&hp, lines.as_bytes())
            } else {
                std::fs::OpenOptions::new().create(true).append(true).open(&hp).and_then(|mut f| f.write_all(lines.as_bytes()))
            };
            res.map_err(|e| DocError::io(&hp, "cannot write history", e))?;
            self.persisted = self.history.len();
        }
        self.saved_revision = self.revision;
        Ok(())
    }

    /// Applies ops atomically as one history entry. On error nothing changes.
    ///
    /// The author matters for captions: edits by a person (`"user"`, see [`crate::ops::is_human_author`])
    /// mark the captions they change as edited, and `generate_captions`
    /// keeps those (and does not recreate captions a person deleted) unless it is told `replace: true`.
    pub fn apply(&mut self, author: &str, label: &str, ops: Vec<Op>) -> Result<Receipt, EditError> {
        let (work, outcomes) = self.run(&ops, author)?;
        let label = if label.trim().is_empty() { default_label(&outcomes) } else { label.to_string() };
        Ok(self.commit(work, author, label, ops, outcomes))
    }

    /// Replaces the whole project as one undoable revision (autosave recovery, pasting a whole
    /// project). Recorded in the history as an edit with no ops; the receipt's outcome lists every id
    /// created/changed/removed. Refused only for a project of a newer format.
    pub fn replace_project(&mut self, author: &str, label: &str, project: Project) -> Result<Receipt, EditError> {
        if project.format > FORMAT_VERSION {
            return Err(EditError::new(format!(
                "the project uses format {}, but this AIFrames reads format {FORMAT_VERSION}; update AIFrames.",
                project.format
            )));
        }
        let mut work = project;
        self.normalize_media_paths(&mut work);
        let mut outcome = crate::ops::diff_projects(&self.project, &work);
        outcome.summary = format!("Replaced the whole project ({} clip(s) on {} track(s))", work.clips.len(), work.tracks.len());
        let label = if label.trim().is_empty() { "Replace project".to_string() } else { label.to_string() };
        Ok(self.commit(work, author, label, Vec::new(), vec![outcome]))
    }

    /// Follows a project file that another program changed on disk: `project` (what the file now holds)
    /// replaces the document as one undoable revision, the entries the other program appended to the
    /// history file become part of [`Document::history`], this document's unsaved entries are numbered
    /// after the highest revision in the file, and the document then counts as saved (it matches the
    /// file). The next save appends after the other program's entries. See also
    /// [`Document::reload_from_disk`].
    pub fn reload(&mut self, author: &str, label: &str, project: Project) -> Result<Receipt, EditError> {
        if project.format > FORMAT_VERSION {
            return Err(EditError::new(format!(
                "the project uses format {}, but this AIFrames reads format {FORMAT_VERSION}; update AIFrames.",
                project.format
            )));
        }
        if let Some(path) = self.path.clone() {
            self.sync_history(&history_path(&path));
        }
        let label = if label.trim().is_empty() { "Reload from disk" } else { label };
        let r = self.replace_project(author, label, project)?;
        self.mark_saved();
        Ok(r)
    }

    /// [`Document::reload`] with the project read from this document's file.
    pub fn reload_from_disk(&mut self, author: &str, label: &str) -> Result<Receipt, DocError> {
        let Some(path) = self.path.clone() else {
            return Err(DocError { kind: DocErrorKind::NoPath, path: None, message: "the project has no file to reload from".into() });
        };
        let stamp = FileStamp::of(&path);
        let text = std::fs::read_to_string(&path).map_err(|e| DocError::io(&path, "cannot read project", e))?;
        let project = parse_project(&text).map_err(|mut e| {
            e.path = Some(path.clone());
            e.message = format!("{}: {}", path.display(), e.message);
            e
        })?;
        let r = self.reload(author, label, project).map_err(|e| DocError { kind: DocErrorKind::Format, path: Some(path.clone()), message: e.to_string() })?;
        self.stamp = stamp;
        Ok(r)
    }

    /// Marks the document as matching its file (not dirty) and takes the file's current stamp.
    pub fn mark_saved(&mut self) {
        self.saved_revision = self.revision;
        self.stamp = self.path.as_deref().and_then(FileStamp::of);
    }

    /// The project file's stamp when this document last opened, saved or reloaded it.
    pub fn file_stamp(&self) -> Option<FileStamp> {
        self.stamp
    }

    /// True when the project file on disk is not the one this document last opened, saved or
    /// reloaded (another program wrote, replaced or deleted it). False for a document without a file.
    pub fn disk_changed(&self) -> bool {
        match &self.path {
            Some(p) => FileStamp::of(p) != self.stamp,
            None => false,
        }
    }

    /// Merges the history file into `history`: entries another program appended (not among the ones
    /// this document wrote or read) are inserted after the persisted ones, and this document's unsaved
    /// entries (with their undo/redo targets and undo steps) are renumbered after the highest revision
    /// in the file when they would repeat one. Returns the number of entries taken in.
    fn sync_history(&mut self, hp: &Path) -> usize {
        let Ok(text) = std::fs::read_to_string(hp) else { return 0 };
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        // Unreadable lines count as revisions too (as in `open`).
        let mut file_max = lines.len() as u64;
        let mut ours: HashMap<String, usize> = HashMap::new();
        for e in &self.history[..self.persisted.min(self.history.len())] {
            if let Ok(s) = serde_json::to_string(e) {
                *ours.entry(s).or_default() += 1;
            }
        }
        let mut foreign = Vec::new();
        for l in &lines {
            let Ok(e) = serde_json::from_str::<HistoryEntry>(l) else { continue };
            file_max = file_max.max(e.revision);
            let key = serde_json::to_string(&e).unwrap_or_default();
            match ours.get_mut(&key) {
                Some(n) if *n > 0 => *n -= 1,
                _ => foreign.push(e),
            }
        }
        let was_clean = !self.dirty();
        let n = foreign.len();
        let persisted = self.persisted.min(self.history.len());
        if n > 0 {
            let tail = self.history.split_off(persisted);
            self.history.extend(foreign);
            self.persisted = self.history.len();
            self.history.extend(tail);
        }
        let persisted = self.persisted.min(self.history.len());
        if self.history[persisted..].first().is_some_and(|e| e.revision <= file_max) {
            let mut map: HashMap<u64, u64> = HashMap::new();
            let mut next = file_max;
            for e in &mut self.history[persisted..] {
                next += 1;
                map.insert(e.revision, next);
                e.revision = next;
            }
            for e in &mut self.history[persisted..] {
                if let Some(t) = e.target.as_mut() {
                    if let Some(&m) = map.get(t) {
                        *t = m;
                    }
                }
            }
            for s in self.undo.iter_mut().chain(self.redo.iter_mut()) {
                if let Some(&m) = map.get(&s.revision) {
                    s.revision = m;
                }
            }
            if let Some(&m) = map.get(&self.saved_revision) {
                self.saved_revision = m;
            }
            self.revision = map.get(&self.revision).copied().unwrap_or(next).max(next);
        } else {
            self.revision = self.revision.max(file_max);
        }
        if was_clean {
            self.saved_revision = self.revision;
        }
        n
    }

    fn commit(&mut self, work: Project, author: &str, label: String, ops: Vec<Op>, outcomes: Vec<OpOutcome>) -> Receipt {
        let after = resolve(&work);
        self.revision += 1;
        let receipt = make_receipt(&self.resolved, &after, self.revision, author, &label, outcomes);
        let old = std::mem::replace(&mut self.project, work);
        self.push_undo(Step { project: old, revision: self.revision, label: label.clone() });
        self.redo.clear();
        self.resolved = after;
        self.history.push(HistoryEntry {
            revision: self.revision,
            author: author.to_string(),
            label,
            time: now(),
            kind: HistoryKind::Edit,
            target: None,
            ops,
            summary: receipt.outcomes.iter().map(|o| o.summary.as_str()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("; "),
        });
        receipt
    }

    /// Dry run: the receipt `apply` would return, without changing anything (planned as an AI edit;
    /// see [`Document::plan_as`]).
    pub fn plan(&self, ops: &[Op]) -> Result<Receipt, EditError> {
        self.plan_project(ops).map(|(r, _)| r)
    }

    /// Dry run that also returns the project as it would be after the ops (e.g. to validate it).
    pub fn plan_project(&self, ops: &[Op]) -> Result<(Receipt, Project), EditError> {
        self.plan_project_as("plan", ops)
    }

    /// [`Document::plan`] for a given author (what `apply(author, …)` would do).
    pub fn plan_as(&self, author: &str, ops: &[Op]) -> Result<Receipt, EditError> {
        self.plan_project_as(author, ops).map(|(r, _)| r)
    }

    /// [`Document::plan_project`] for a given author.
    pub fn plan_project_as(&self, author: &str, ops: &[Op]) -> Result<(Receipt, Project), EditError> {
        let (work, outcomes) = self.run(ops, author)?;
        let after = resolve(&work);
        let label = default_label(&outcomes);
        Ok((make_receipt(&self.resolved, &after, self.revision + 1, author, &label, outcomes), work))
    }

    fn run(&self, ops: &[Op], author: &str) -> Result<(Project, Vec<OpOutcome>), EditError> {
        if ops.is_empty() {
            return Err(EditError::new("no ops to apply; pass at least one op like {\"op\":\"move_clip\",\"clip\":\"c3\",\"start\":12.0}."));
        }
        let mut work = self.project.clone();
        let outcomes = apply_batch_as(&mut work, ops, author)?;
        self.normalize_media_paths(&mut work);
        Ok((work, outcomes))
    }

    /// Absolute media paths near the project file become relative (once the document has a path).
    fn normalize_media_paths(&self, work: &mut Project) {
        if let Some(dir) = self.path.as_ref().and_then(|p| p.parent()) {
            for m in &mut work.media {
                if Path::new(&m.path).is_absolute() {
                    m.path = portable_path(Path::new(&m.path), dir);
                }
            }
        }
    }

    fn push_undo(&mut self, step: Step) {
        self.undo.push_back(step);
        let per = estimate_bytes(&self.project).max(1);
        while self.undo.len() > self.undo_limit.max(1) || (self.undo.len() > 1 && self.undo.len() * per > self.undo_budget_bytes) {
            self.undo.pop_front();
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Label of the edit `undo` would revert.
    pub fn undo_label(&self) -> Option<&str> {
        self.undo.back().map(|s| s.label.as_str())
    }

    pub fn redo_label(&self) -> Option<&str> {
        self.redo.last().map(|s| s.label.as_str())
    }

    /// Undoes the last edit (author "user").
    pub fn undo(&mut self) -> Option<Receipt> {
        self.undo_by("user")
    }

    pub fn redo(&mut self) -> Option<Receipt> {
        self.redo_by("user")
    }

    pub fn undo_by(&mut self, author: &str) -> Option<Receipt> {
        let step = self.undo.pop_back()?;
        let current = std::mem::replace(&mut self.project, step.project);
        self.redo.push(Step { project: current, revision: step.revision, label: step.label.clone() });
        Some(self.after_history_move(author, HistoryKind::Undo, step.revision, &step.label))
    }

    pub fn redo_by(&mut self, author: &str) -> Option<Receipt> {
        let step = self.redo.pop()?;
        let current = std::mem::replace(&mut self.project, step.project);
        self.undo.push_back(Step { project: current, revision: step.revision, label: step.label.clone() });
        Some(self.after_history_move(author, HistoryKind::Redo, step.revision, &step.label))
    }

    fn after_history_move(&mut self, author: &str, kind: HistoryKind, target: u64, label: &str) -> Receipt {
        let after = resolve(&self.project);
        self.revision += 1;
        let label = format!("{} {label}", if kind == HistoryKind::Undo { "Undo" } else { "Redo" });
        let receipt = make_receipt(&self.resolved, &after, self.revision, author, &label, Vec::new());
        self.resolved = after;
        self.history.push(HistoryEntry {
            revision: self.revision,
            author: author.to_string(),
            label,
            time: now(),
            kind,
            target: Some(target),
            ops: Vec::new(),
            summary: String::new(),
        });
        receipt
    }

    /// Every history entry, oldest first (including entries loaded from the history file).
    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    /// Entries after `revision` ("what changed since my last turn").
    pub fn changes_since(&self, revision: u64) -> Vec<&HistoryEntry> {
        self.history.iter().filter(|e| e.revision > revision).collect()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Changed since the last save/open.
    pub fn dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    /// The cached resolution of `project` (call [`Document::refresh`] after mutating `project` directly).
    pub fn resolved(&self) -> &Resolved {
        &self.resolved
    }

    /// Recomputes the cached resolution after direct changes to `project`.
    pub fn refresh(&mut self) {
        self.resolved = resolve(&self.project);
    }

    /// The folder relative media paths are resolved against (the project file's folder, else the
    /// current directory).
    pub fn project_dir(&self) -> PathBuf {
        match self.path.as_ref().and_then(|p| p.parent()) {
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    /// Absolute path of a media asset's file.
    pub fn media_path(&self, asset: &MediaAsset) -> PathBuf {
        resolve_media(&self.project_dir(), &asset.path)
    }

    /// `validate` plus a check that every media file exists on disk (`media_offline`).
    pub fn validate(&self) -> Vec<Finding> {
        let mut out = validate(&self.project, &self.resolved);
        for m in &self.project.media {
            let p = self.media_path(m);
            if !p.exists() {
                let users: Vec<Id> = self
                    .project
                    .clips
                    .iter()
                    .filter(|c| matches!(&c.source, Source::Media { asset, .. } if *asset == m.id))
                    .map(|c| c.id.clone())
                    .collect();
                out.push(Finding {
                    severity: if users.is_empty() { Severity::Warning } else { Severity::Error },
                    code: "media_offline",
                    message: format!("media {} file not found: {} (used by {} clip(s)).", m.id, p.display(), users.len()),
                    at: None,
                    ids: std::iter::once(m.id.clone()).chain(users).collect(),
                    fix: Some(format!("relink it: {{\"op\":\"relink_media\",\"asset\":\"{}\",\"path\":\"<new path>\"}}", m.id)),
                    fix_ops: Vec::new(),
                });
            }
        }
        out
    }

    /// The text description of the edit (see [`crate::describe::describe`]).
    pub fn describe(&self) -> String {
        crate::describe::describe(&self.project, &self.resolved)
    }
}

fn default_label(outcomes: &[OpOutcome]) -> String {
    match outcomes {
        [] => "Edit".into(),
        [o] => o.summary.clone(),
        [o, rest @ ..] => format!("{} (+{} more)", o.summary, rest.len()),
    }
}

fn make_receipt(before: &Resolved, after: &Resolved, revision: u64, author: &str, label: &str, outcomes: Vec<OpOutcome>) -> Receipt {
    let mut ids: HashSet<&Id> = before.clips.keys().collect();
    ids.extend(after.clips.keys());
    let close = |a: &Span, b: &Span| (a.start - b.start).abs() < 1e-9 && (a.end - b.end).abs() < 1e-9;
    let mut moved: Vec<Moved> = ids
        .into_iter()
        .filter_map(|id| {
            let (b, a) = (before.span(id), after.span(id));
            let same = match (&b, &a) {
                (Some(x), Some(y)) => close(x, y),
                (None, None) => true,
                _ => false,
            };
            (!same).then(|| Moved { id: id.clone(), before: b, after: a })
        })
        .collect();
    let key = |m: &Moved| m.after.or(m.before).map(|s| s.start).unwrap_or(f64::MAX);
    moved.sort_by(|x, y| key(x).total_cmp(&key(y)).then(x.id.cmp(&y.id)));
    let old: HashMap<(&str, crate::resolve::ResolveErrorKind), ()> = before.errors.iter().map(|e| ((e.id.as_str(), e.kind), ())).collect();
    let new_errors = after.errors.iter().filter(|e| !old.contains_key(&(e.id.as_str(), e.kind))).map(|e| e.message.clone()).collect();
    let warnings = outcomes.iter().flat_map(|o| o.warnings.iter().cloned()).collect();
    Receipt { revision, author: author.to_string(), label: label.to_string(), outcomes, moved, warnings, new_errors }
}

/// Parses project JSON with a readable error (line/column and the format version check).
pub fn parse_project(text: &str) -> Result<Project, DocError> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| DocError {
        kind: DocErrorKind::Parse,
        path: None,
        message: format!("not valid JSON (line {}, column {}): {e}", e.line(), e.column()),
    })?;
    if let Some(f) = v.get("format").and_then(|f| f.as_u64()) {
        if f > FORMAT_VERSION as u64 {
            return Err(DocError {
                kind: DocErrorKind::Format,
                path: None,
                message: format!("this project uses format {f}, but this AIFrames reads format {FORMAT_VERSION}; update AIFrames."),
            });
        }
    }
    serde_json::from_value::<Project>(v).map_err(|e| DocError { kind: DocErrorKind::Parse, path: None, message: format!("invalid project: {e}") })
}

fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf())
    }
}

/// Absolute path of a media path string relative to `dir`.
fn resolve_media(dir: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        normalize(&dir.join(p))
    }
}

/// Removes `.` and `..` components lexically.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn comp_eq(a: &Component, b: &Component) -> bool {
    if cfg!(windows) {
        a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// The path to store for a media file: relative to `dir` when it is inside it or at most two folders
/// up on the same drive, else absolute. Always forward slashes.
pub fn portable_path(abs: &Path, dir: &Path) -> String {
    let abs = normalize(abs);
    let dir = normalize(dir);
    let a: Vec<Component> = abs.components().collect();
    let d: Vec<Component> = dir.components().collect();
    let common = a.iter().zip(d.iter()).take_while(|(x, y)| comp_eq(x, y)).count();
    // Length of the root part (drive prefix and/or root directory); it must be shared.
    let root_len = a.iter().take_while(|c| matches!(c, Component::Prefix(_) | Component::RootDir)).count();
    let ups = d.len().saturating_sub(common);
    let s = if abs.is_absolute() && dir.is_absolute() && root_len > 0 && common >= root_len && ups <= 2 {
        let mut rel = PathBuf::new();
        for _ in 0..ups {
            rel.push("..");
        }
        for c in &a[common..] {
            rel.push(c.as_os_str());
        }
        rel.to_string_lossy().to_string()
    } else {
        abs.to_string_lossy().to_string()
    };
    s.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use serde_json::json;

    fn op(v: serde_json::Value) -> Op {
        Op::from_json(&v).unwrap()
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aiframes-core-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn doc() -> Document {
        let mut p = project();
        p.clips.push(media_clip("c1", "v1", "mv", Time::At(0.0), 4.0, 0.0));
        p.clips.push(media_clip("c2", "v1", "mv", after("c1", 0.0), 4.0, 10.0));
        p.clips.push(media_clip("c3", "v1", "mv", after("c2", 0.0), 4.0, 20.0));
        Document::new(p)
    }

    fn file_revisions(path: &Path) -> Vec<(u64, String)> {
        std::fs::read_to_string(history_path(path))
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<HistoryEntry>(l).unwrap())
            .map(|e| (e.revision, e.author))
            .collect()
    }

    fn strictly_increasing(v: &[(u64, String)]) -> bool {
        v.windows(2).all(|w| w[0].0 < w[1].0)
    }

    #[test]
    fn save_refuses_to_overwrite_changes_made_on_disk() {
        let dir = tmp_dir("disk-changed");
        let path = dir.join("p.aifr");
        let mut win = doc();
        win.save_as(&path).unwrap();
        win.apply("user", "a", vec![op(json!({"op":"set_clip","clip":"c1","patch":{"name":"mine"}}))]).unwrap();
        // Another program saves the file meanwhile.
        let mut cli = Document::open(&path).unwrap();
        cli.apply("cli", "x", vec![op(json!({"op":"set_clip","clip":"c2","patch":{"name":"theirs"}}))]).unwrap();
        cli.save().unwrap();
        let theirs = std::fs::read_to_string(&path).unwrap();
        for e in [win.save().unwrap_err(), win.save_as(&path).unwrap_err()] {
            assert_eq!(e.kind, DocErrorKind::DiskChanged);
            assert!(e.message.contains("changed on disk") && e.message.contains("Reload") && e.message.contains("force"), "{}", e.message);
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs, "nothing was overwritten");
        assert!(win.dirty());
        // An explicit overwrite works (the other program's history entries stay in the history file),
        // after which plain saves work again.
        win.save_as_force(&path).unwrap();
        assert!(!win.disk_changed() && !win.dirty());
        assert!(std::fs::read_to_string(&path).unwrap().contains("mine"));
        assert!(file_revisions(&path).iter().any(|r| r.1 == "cli"));
        win.apply("user", "b", vec![op(json!({"op":"set_clip","clip":"c3","patch":{"name":"more"}}))]).unwrap();
        win.save().unwrap();
        // save_force after another external change.
        let mut cli = Document::open(&path).unwrap();
        cli.apply("cli", "y", vec![op(json!({"op":"set_clip","clip":"c2","patch":{"name":"again"}}))]).unwrap();
        cli.save().unwrap();
        assert_eq!(win.save().unwrap_err().kind, DocErrorKind::DiskChanged);
        win.save_force().unwrap();
        let revs = file_revisions(&path);
        assert!(strictly_increasing(&revs), "{revs:?}");
        // A deleted file is simply written again.
        std::fs::remove_file(&path).unwrap();
        win.save().unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_after_an_external_edit_continues_the_history() {
        let dir = tmp_dir("reload");
        let path = dir.join("p.aifr");
        // The window: two edits, saved.
        let mut win = doc();
        win.save_as(&path).unwrap();
        assert!(win.file_stamp().is_some() && !win.disk_changed());
        win.apply("user", "a", vec![op(json!({"op":"set_clip","clip":"c1","patch":{"name":"one"}}))]).unwrap();
        win.apply("user", "b", vec![op(json!({"op":"set_clip","clip":"c2","patch":{"name":"two"}}))]).unwrap();
        win.save().unwrap();
        assert!(!win.disk_changed());
        // The CLI: opens the file, edits, saves (appends revision 3).
        let mut cli = Document::open(&path).unwrap();
        cli.apply("cli", "trim", vec![op(json!({"op":"trim_clip","clip":"c3","edge":"end","delta":-1.0}))]).unwrap();
        cli.save().unwrap();
        assert!(win.disk_changed(), "the window notices the file changed");
        // The window reloads: one undoable revision, saved, with the CLI's entry in its history.
        let r = win.reload_from_disk("user", "").unwrap();
        assert_eq!(r.revision, 4);
        assert_eq!(r.label, "Reload from disk");
        assert!(r.moved.iter().any(|m| m.id == "c3"));
        assert!(!win.dirty() && !win.disk_changed());
        assert_eq!(win.project, cli.project);
        assert!(win.history().iter().any(|e| e.author == "cli" && e.revision == 3));
        assert_eq!(win.changes_since(2).len(), 2, "the CLI's edit and the reload");
        // A window edit, saved: the file has strictly increasing revisions and both authors.
        win.apply("user", "c", vec![op(json!({"op":"set_clip","clip":"c1","patch":{"name":"uno"}}))]).unwrap();
        assert_eq!(win.revision(), 5);
        win.save().unwrap();
        let revs = file_revisions(&path);
        assert_eq!(revs.iter().map(|r| r.0).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5], "{revs:?}");
        assert!(revs.iter().any(|r| r.1 == "cli") && revs.iter().filter(|r| r.1 == "user").count() == 4);
        // The reload itself can be undone (back to the window's own state before it).
        win.undo().unwrap();
        win.undo().unwrap();
        assert_eq!(win.project.clips[2].duration, 4.0, "before the CLI's trim");
        // Reopening reads the same history.
        let again = Document::open(&path).unwrap();
        assert_eq!(again.history().len(), 5);
        assert_eq!(again.revision(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_never_repeats_a_revision_in_the_history_file() {
        let dir = tmp_dir("dup-rev");
        let path = dir.join("p.aifr");
        let mut win = doc();
        win.save_as(&path).unwrap();
        win.apply("user", "a", vec![op(json!({"op":"set_clip","clip":"c1","patch":{"name":"one"}}))]).unwrap();
        win.save().unwrap();
        // Unsaved window work (revisions 2 and 3, one of them an undo) while the CLI appends revision 2.
        win.apply("user", "b", vec![op(json!({"op":"set_clip","clip":"c2","patch":{"name":"two"}}))]).unwrap();
        win.undo().unwrap();
        let mut cli = Document::open(&path).unwrap();
        cli.apply("cli", "x", vec![op(json!({"op":"set_clip","clip":"c3","patch":{"name":"three"}}))]).unwrap();
        cli.save().unwrap();
        // Saving without reloading is refused; forcing it numbers the window's entries after the CLI's.
        assert_eq!(win.save().unwrap_err().kind, DocErrorKind::DiskChanged);
        win.save_force().unwrap();
        let revs = file_revisions(&path);
        assert!(strictly_increasing(&revs), "{revs:?}");
        assert_eq!(revs.iter().map(|r| r.0).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        assert_eq!(revs[1].1, "cli");
        assert_eq!(win.revision(), 4);
        assert!(!win.dirty());
        let undo = win.history().iter().find(|e| e.kind == HistoryKind::Undo).unwrap();
        assert_eq!((undo.revision, undo.target), (4, Some(3)), "the undo still names the window's edit");
        assert!(win.history().iter().any(|e| e.author == "cli"));
        // Redo then edit: numbering continues.
        win.redo().unwrap();
        assert_eq!(win.revision(), 5);
        // Reload with unsaved window edits: they are kept in the history, numbered after the file's.
        let mut cli = Document::open(&path).unwrap();
        cli.apply("cli", "y", vec![op(json!({"op":"set_clip","clip":"c3","patch":{"name":"drei"}}))]).unwrap();
        cli.save().unwrap();
        let r = win.reload_from_disk("user", "Reload").unwrap();
        assert_eq!(r.revision, 7, "the CLI's 5, the window's redo as 6, the reload 7");
        win.save().unwrap();
        let revs = file_revisions(&path);
        assert!(strictly_increasing(&revs), "{revs:?}");
        assert_eq!(revs.len(), 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_records_history_and_receipt() {
        let mut d = doc();
        assert!(!d.dirty());
        let r = d.apply("ai", "shorten intro", vec![op(json!({"op":"trim_clip","clip":"c1","edge":"end","delta":-1.0}))]).unwrap();
        assert_eq!(r.revision, 1);
        assert_eq!(r.author, "ai");
        let moved: Vec<&str> = r.moved.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(moved, vec!["c1", "c2", "c3"], "the chain follows the trim");
        assert_eq!(r.moved[1].before.unwrap().start, 4.0);
        assert_eq!(r.moved[1].after.unwrap().start, 3.0);
        assert!(d.dirty());
        assert_eq!(d.history().len(), 1);
        assert_eq!(d.history()[0].label, "shorten intro");
        assert!(d.history()[0].summary.contains("Trimmed end of c1"));
        assert_eq!(d.resolved().span("c3").unwrap().start, 7.0);
    }

    #[test]
    fn plan_is_a_dry_run_with_the_same_receipt() {
        let mut d = doc();
        let ops = vec![op(json!({"op":"delete_clip","clip":"c2","ripple":true}))];
        let planned = d.plan(&ops).unwrap();
        assert_eq!(d.project.clips.len(), 3, "plan changes nothing");
        assert_eq!(d.revision(), 0);
        let applied = d.apply("ai", "", ops).unwrap();
        assert_eq!(planned.moved, applied.moved);
        assert_eq!(planned.outcomes, applied.outcomes);
        assert_eq!(planned.revision, applied.revision);
        assert_eq!(applied.label, applied.outcomes[0].summary, "empty label = summary");
    }

    #[test]
    fn failed_apply_changes_nothing() {
        let mut d = doc();
        let before = d.project.clone();
        let err = d
            .apply("ai", "x", vec![op(json!({"op":"trim_clip","clip":"c1","edge":"end","delta":-1.0})), op(json!({"op":"delete_clip","clip":"c99"}))])
            .unwrap_err();
        assert_eq!(err.index, Some(1));
        assert!(err.message.contains("c99"));
        assert_eq!(d.project, before);
        assert_eq!(d.revision(), 0);
        assert!(d.history().is_empty());
        assert!(!d.can_undo());
    }

    #[test]
    fn undo_redo_restore_snapshots() {
        let mut d = doc();
        let original = d.project.clone();
        d.apply("ai", "a", vec![op(json!({"op":"delete_clip","clip":"c2","ripple":true}))]).unwrap();
        let after_a = d.project.clone();
        d.apply("user", "b", vec![op(json!({"op":"set_clip","clip":"c1","patch":{"name":"intro"}}))]).unwrap();
        assert_eq!(d.undo_label(), Some("b"));
        let u = d.undo_by("ai").unwrap();
        assert_eq!(d.project, after_a);
        assert_eq!(u.label, "Undo b");
        assert!(u.moved.is_empty(), "renaming moves nothing");
        let u = d.undo().unwrap();
        assert_eq!(d.project, original);
        assert!(u.moved.iter().any(|m| m.id == "c2" && m.before.is_none() && m.after.is_some()));
        assert!(d.undo().is_none());
        d.redo().unwrap();
        assert_eq!(d.project, after_a);
        assert_eq!(d.redo_label(), Some("b"));
        // A new edit clears redo.
        d.apply("user", "c", vec![op(json!({"op":"set_project","name":"x"}))]).unwrap();
        assert!(!d.can_redo());
        // Revisions keep counting; undo/redo are history entries.
        assert_eq!(d.revision(), 6);
        let kinds: Vec<HistoryKind> = d.history().iter().map(|e| e.kind).collect();
        assert_eq!(kinds, vec![HistoryKind::Edit, HistoryKind::Edit, HistoryKind::Undo, HistoryKind::Undo, HistoryKind::Redo, HistoryKind::Edit]);
        assert_eq!(d.history()[2].target, Some(2));
        let since: Vec<u64> = d.changes_since(4).iter().map(|e| e.revision).collect();
        assert_eq!(since, vec![5, 6]);
    }

    #[test]
    fn replace_project_is_one_undoable_revision() {
        let mut d = doc();
        d.apply("ai", "first", vec![op(json!({"op":"set_project","name":"A"}))]).unwrap();
        let original = d.project.clone();
        let mut q = original.clone();
        q.name = "Recovered".into();
        q.clips.retain(|c| c.id != "c3");
        q.clips.push(media_clip("c9", "v1", "mv", after("c2", 0.0), 2.0, 40.0));
        let r = d.replace_project("user", "Recover autosave", q.clone()).unwrap();
        assert_eq!(r.revision, 2);
        assert_eq!(d.revision(), 2);
        assert_eq!(d.project, q);
        assert!(d.dirty());
        assert_eq!(r.outcomes.len(), 1);
        assert_eq!(r.outcomes[0].created, vec!["c9"]);
        assert_eq!(r.outcomes[0].removed, vec!["c3"]);
        assert!(r.moved.iter().any(|m| m.id == "c3" && m.after.is_none()));
        assert!(r.moved.iter().any(|m| m.id == "c9" && m.before.is_none()));
        let h = d.history().last().unwrap();
        assert_eq!((h.label.as_str(), h.kind, h.ops.len()), ("Recover autosave", HistoryKind::Edit, 0));
        assert!(h.summary.starts_with("Replaced the whole project"));
        assert_eq!(d.resolved().span("c9").unwrap().start, 8.0, "the cache is refreshed");
        d.undo().unwrap();
        assert_eq!(d.project, original);
        d.redo().unwrap();
        assert_eq!(d.project, q);
        let mut z = q.clone();
        z.format = 99;
        assert!(d.replace_project("user", "", z).unwrap_err().message.contains("format 99"));
        assert_eq!(d.revision(), 4);
        let r = d.replace_project("user", " ", q.clone()).unwrap();
        assert_eq!(r.label, "Replace project");
        assert!(r.moved.is_empty());
    }

    #[test]
    fn plan_project_returns_the_planned_project() {
        let d = doc();
        let ops = vec![op(json!({"op":"trim_clip","clip":"c1","edge":"end","delta":-1.0}))];
        let (receipt, planned) = d.plan_project(&ops).unwrap();
        let mut expected = d.project.clone();
        crate::ops::apply_batch(&mut expected, &ops).unwrap();
        assert_eq!(planned, expected);
        assert_eq!(receipt, d.plan(&ops).unwrap());
        assert_eq!(d.revision(), 0);
        let f = crate::validate::validate(&planned, &resolve(&planned));
        assert!(f.iter().all(|f| f.severity == crate::validate::Severity::Info));
    }

    #[test]
    fn undo_is_capped() {
        let mut d = doc();
        d.undo_limit = 3;
        for i in 0..6 {
            d.apply("user", &format!("e{i}"), vec![op(json!({"op":"set_project","name": format!("n{i}")}))]).unwrap();
        }
        let mut n = 0;
        while d.undo().is_some() {
            n += 1;
        }
        assert_eq!(n, 3);
        assert_eq!(d.project.name, "n2");
    }

    #[test]
    fn save_open_roundtrip_with_history_and_relative_media() {
        let dir = tmp_dir("save");
        let media_dir = dir.join("assets");
        let mut d = Document::new(project());
        d.project.media.clear();
        d.project.clips.clear();
        let abs = media_dir.join("clip.mp4");
        let abs_s = abs.to_string_lossy().replace('\\', "/");
        let path = dir.join("edit").join("my.aifr");
        d.save_as(&path).unwrap();
        d.apply(
            "ai",
            "import",
            vec![op(json!({"op":"add_media","path": abs_s, "kind":"video","info":{"duration":30.0,"has_video":true,"has_audio":true}}))],
        )
        .unwrap();
        assert_eq!(d.project.media[0].path, "../assets/clip.mp4", "absolute paths near the project become relative");
        assert_eq!(normalize(&d.media_path(&d.project.media[0])), normalize(&abs));
        d.apply("ai", "clip", vec![op(json!({"op":"append_clip","track":"v1","clip":{"asset":"m1","duration":5}}))]).unwrap();
        assert!(d.dirty());
        d.save().unwrap();
        assert!(!d.dirty());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("}\n"));
        assert!(text.contains("\n  \"name\": \"Test\""), "2-space indent");
        assert!(!text.contains("\"speed\""), "defaults omitted");
        let hist = std::fs::read_to_string(history_path(&path)).unwrap();
        assert_eq!(hist.lines().count(), 2);
        assert!(path.with_file_name("my.aifr.history.jsonl").exists());

        let d2 = Document::open(&path).unwrap();
        assert_eq!(d2.project, d.project);
        assert_eq!(d2.revision(), 2);
        assert_eq!(d2.history().len(), 2);
        assert!(!d2.dirty());
        assert_eq!(d2.history()[1].ops, d.history()[1].ops);

        // Saving again appends only new entries.
        let mut d3 = Document::open(&path).unwrap();
        d3.apply("user", "rename", vec![op(json!({"op":"set_project","name":"Renamed"}))]).unwrap();
        d3.save().unwrap();
        assert_eq!(std::fs::read_to_string(history_path(&path)).unwrap().lines().count(), 3);
        assert_eq!(d3.changes_since(2).len(), 1);

        // save_as elsewhere rebases relative media paths.
        let other = dir.join("deeper").join("x").join("copy.aifr");
        d3.save_as(&other).unwrap();
        assert_eq!(d3.project.media[0].path, "../../assets/clip.mp4");
        assert_eq!(std::fs::read_to_string(history_path(&other)).unwrap().lines().count(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_reports_bad_files() {
        let dir = tmp_dir("bad");
        let p = dir.join("bad.aifr");
        std::fs::write(&p, "{\"format\":1,").unwrap();
        let e = Document::open(&p).err().unwrap();
        assert_eq!(e.kind, DocErrorKind::Parse);
        assert!(e.message.contains("line"));
        std::fs::write(&p, "{\"format\":99}").unwrap();
        assert_eq!(Document::open(&p).err().unwrap().kind, DocErrorKind::Format);
        assert_eq!(Document::open(dir.join("missing.aifr")).err().unwrap().kind, DocErrorKind::Io);
        let mut d = Document::new(project());
        assert_eq!(d.save().unwrap_err().kind, DocErrorKind::NoPath);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn portable_paths() {
        if cfg!(windows) {
            assert_eq!(portable_path(Path::new("C:/a/b/media/x.mp4"), Path::new("C:/a/b")), "media/x.mp4");
            assert_eq!(portable_path(Path::new("c:/A/b/media/x.mp4"), Path::new("C:/a/B")), "media/x.mp4");
            assert_eq!(portable_path(Path::new("D:/x.mp4"), Path::new("C:/a")), "D:/x.mp4");
            assert_eq!(portable_path(Path::new("C:/x/y/z/w.mp4"), Path::new("C:/a/b/c/d")), "C:/x/y/z/w.mp4");
        } else {
            assert_eq!(portable_path(Path::new("/a/b/media/x.mp4"), Path::new("/a/b")), "media/x.mp4");
            assert_eq!(portable_path(Path::new("/x/y.mp4"), Path::new("/a/b/c/d")), "/x/y.mp4");
        }
    }

    #[test]
    fn validate_reports_offline_media() {
        let dir = tmp_dir("offline");
        let mut d = doc();
        d.save_as(dir.join("p.aifr")).unwrap();
        let f = d.validate();
        assert!(f.iter().any(|f| f.code == "media_offline" && f.ids.contains(&"mv".to_string())));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
