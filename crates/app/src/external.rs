//! Changes made to an open project file by another program (the CLI, a text editor, git).
//!
//! The core document remembers the file's stamp (`Document::file_stamp`, `disk_changed`); the
//! window polls `disk_changed()` once a second. When the file changed and its content differs from
//! what the window has:
//! * the window has no unsaved changes → `Document::reload_from_disk` (one undoable revision by
//!   "external"; the other program's history entries are merged; the document counts as saved),
//!   with a toast;
//! * the window has unsaved changes → a banner offers Reload / Keep mine / Compare, and Save refuses
//!   to overwrite the newer file until the person chose.

use aiframes_core::document::{canonical_json, parse_project, FileStamp};
use aiframes_core::Project;

/// What to do about the file's current content.
#[derive(Debug)]
pub enum Decision {
    /// Same project as in the window (our own save, a touch): just take the new stamp.
    Same,
    /// The window is clean: reload.
    Reload,
    /// The window has unsaved changes: ask.
    Conflict(Box<Project>),
    /// Not readable as a project right now (being written?): look again later.
    Retry,
}

pub fn decide(current: &Project, dirty: bool, disk_text: &str) -> Decision {
    let Ok(p) = parse_project(disk_text) else { return Decision::Retry };
    if canonical_json(&p) == canonical_json(current) {
        Decision::Same
    } else if dirty {
        Decision::Conflict(Box::new(p))
    } else {
        Decision::Reload
    }
}

/// A newer version on disk the person has not decided about.
pub struct Conflict {
    pub stamp: Option<FileStamp>,
    pub project: Project,
}

/// Per-document watch state (the stamps themselves live in the core document).
#[derive(Default)]
pub struct Watch {
    /// A newer version the person chose to overwrite ("Keep mine").
    pub acknowledged: Option<FileStamp>,
    pub last_check: f64,
    pub conflict: Option<Conflict>,
    /// The Compare window is open.
    pub comparing: bool,
}

impl Watch {
    /// Whether the file version `now` (the document says the disk changed) still needs a look:
    /// not dismissed with "Keep mine" and not already the pending conflict.
    pub fn needs_look(&self, now: Option<FileStamp>) -> bool {
        now.is_some() && self.acknowledged != now && self.conflict.as_ref().is_none_or(|c| c.stamp != now)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use aiframes_core::Document;

    fn project(name: &str) -> Project {
        Project { name: name.into(), ..Project::default() }
    }

    #[test]
    fn decisions() {
        let cur = project("a");
        let same = canonical_json(&cur);
        assert!(matches!(decide(&cur, false, &same), Decision::Same));
        let other = canonical_json(&project("b"));
        assert!(matches!(decide(&cur, false, &other), Decision::Reload));
        assert!(matches!(decide(&cur, true, &other), Decision::Conflict(p) if p.name == "b"));
        assert!(matches!(decide(&cur, false, "{ half a fi"), Decision::Retry));
    }

    /// The window's bookkeeping on top of the core's stamps: another program writes the file, the
    /// window (dirty) must not save over it until "Keep mine"; a reload makes the window clean.
    #[test]
    fn stamps_reload_and_clobbering() {
        let dir = std::env::temp_dir().join(format!("aiframes-ext-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("p.aifr");
        let mut doc = Document::new(project("mine"));
        doc.save_as(&f).unwrap();
        assert!(!doc.disk_changed());
        let mut w = Watch::default();

        // Another program writes a longer version.
        std::fs::write(&f, canonical_json(&project("theirs, a longer name"))).unwrap();
        let now = FileStamp::of(&f);
        assert!(doc.disk_changed());
        assert!(w.needs_look(now));
        let e = doc.save().expect_err("the core refuses to overwrite the other program's version");
        assert_eq!(e.kind, aiframes_core::document::DocErrorKind::DiskChanged);

        // Conflict pending: not looked at again for the same version.
        w.conflict = Some(Conflict { stamp: now, project: project("theirs, a longer name") });
        assert!(!w.needs_look(now));
        // "Keep mine": no banner; the window then saves with force.
        w.conflict = None;
        w.acknowledged = now;
        assert!(!w.needs_look(now));

        // Reload instead: the window matches the file again and is clean.
        let rev = doc.revision();
        doc.reload_from_disk("external", "Reload p.aifr (changed on disk)").unwrap();
        assert!(doc.revision() > rev, "the reload is one undoable revision");
        assert!(!doc.dirty() && !doc.disk_changed());
        assert_eq!(doc.project.name, "theirs, a longer name");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
