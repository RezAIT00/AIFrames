//! Thread-safe registry of open documents, shared by an app's GUI thread and its MCP server.
//!
//! Generic over the document type so every app uses the same
//! plumbing: document list + active document, UI waker, activity notices for the AI log,
//! window-focus requests and screenshot requests answered by the GUI.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

/// What the hub needs to know about a document.
pub trait HubDoc: Send + 'static {
    /// File the document was loaded from / saved to.
    fn path(&self) -> Option<&Path>;
    /// Called when an unsaved document is registered so it can title itself "Book3", "Presentation3", ...
    fn set_untitled_index(&mut self, i: u32);
}

pub type DocRef<D> = Arc<Mutex<D>>;

pub struct DocEntry<D> {
    pub id: u64,
    pub doc: DocRef<D>,
}

/// Channel on which the GUI answers a screenshot request with PNG bytes.
pub type ScreenshotReply = std::sync::mpsc::Sender<Result<Vec<u8>, String>>;

pub struct HubState<D> {
    pub docs: Vec<DocEntry<D>>,
    pub active: Option<u64>,
    next_id: u64,
    next_untitled: u32,
    /// Messages for the UI (e.g. "AI: saved Book1.xlsx").
    pub notices: Vec<String>,
    /// Set when the window should come to the foreground (e.g. a file was opened from Explorer).
    pub focus_requested: bool,
    /// Pending requests for a PNG screenshot of the window (served by the GUI).
    pub screenshot_waiters: Vec<ScreenshotReply>,
}

impl<D> Default for HubState<D> {
    fn default() -> Self {
        HubState {
            docs: Vec::new(),
            active: None,
            next_id: 0,
            next_untitled: 0,
            notices: Vec::new(),
            focus_requested: false,
            screenshot_waiters: Vec::new(),
        }
    }
}

type Waker = Arc<dyn Fn() + Send + Sync>;

pub struct Hub<D> {
    state: Mutex<HubState<D>>,
    waker: Mutex<Option<Waker>>,
}

impl<D> Default for Hub<D> {
    fn default() -> Self {
        Hub { state: Mutex::new(HubState::default()), waker: Mutex::new(None) }
    }
}

/// Lock a mutex, recovering from poisoning (a panicked holder must not take the app down).
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl<D: HubDoc> Hub<D> {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn state(&self) -> MutexGuard<'_, HubState<D>> {
        lock(&self.state)
    }

    /// Register a document and make it active. Returns its id.
    pub fn add(&self, mut doc: D) -> u64 {
        let mut st = self.state();
        if doc.path().is_none() {
            st.next_untitled += 1;
            doc.set_untitled_index(st.next_untitled);
        }
        st.next_id += 1;
        let id = st.next_id;
        st.docs.push(DocEntry { id, doc: Arc::new(Mutex::new(doc)) });
        st.active = Some(id);
        drop(st);
        self.wake();
        id
    }

    pub fn get(&self, id: u64) -> Option<DocRef<D>> {
        self.state().docs.iter().find(|d| d.id == id).map(|d| d.doc.clone())
    }

    pub fn active(&self) -> Option<(u64, DocRef<D>)> {
        let st = self.state();
        let id = st.active.or_else(|| st.docs.first().map(|d| d.id))?;
        st.docs.iter().find(|d| d.id == id).map(|d| (d.id, d.doc.clone()))
    }

    pub fn set_active(&self, id: u64) -> bool {
        let mut st = self.state();
        if st.docs.iter().any(|d| d.id == id) {
            st.active = Some(id);
            drop(st);
            self.wake();
            true
        } else {
            false
        }
    }

    pub fn remove(&self, id: u64) -> bool {
        let mut st = self.state();
        let before = st.docs.len();
        st.docs.retain(|d| d.id != id);
        if st.active == Some(id) {
            st.active = st.docs.last().map(|d| d.id);
        }
        let removed = st.docs.len() != before;
        drop(st);
        self.wake();
        removed
    }

    pub fn list(&self) -> Vec<(u64, DocRef<D>)> {
        self.state().docs.iter().map(|d| (d.id, d.doc.clone())).collect()
    }

    /// Find an open document by file path (case-insensitive on Windows).
    pub fn find_path(&self, path: &Path) -> Option<u64> {
        let target = normalize(path);
        for (id, d) in self.list() {
            let doc = lock(&d);
            if let Some(p) = doc.path() {
                if normalize(p) == target {
                    return Some(id);
                }
            }
        }
        None
    }

    pub fn notify(&self, msg: impl Into<String>) {
        self.state().notices.push(msg.into());
        self.wake();
    }

    pub fn take_notices(&self) -> Vec<String> {
        std::mem::take(&mut self.state().notices)
    }

    /// Install a callback used to wake the UI after background changes.
    pub fn set_waker(&self, f: impl Fn() + Send + Sync + 'static) {
        *lock(&self.waker) = Some(Arc::new(f));
    }

    pub fn wake(&self) {
        let w = lock(&self.waker).clone();
        if let Some(w) = w {
            w();
        }
    }

    /// True when a UI is attached (it installed a waker).
    pub fn has_ui(&self) -> bool {
        lock(&self.waker).is_some()
    }

    /// Ask the UI to bring its window to the foreground.
    pub fn request_focus(&self) {
        self.state().focus_requested = true;
        self.wake();
    }

    pub fn take_focus_request(&self) -> bool {
        std::mem::take(&mut self.state().focus_requested)
    }

    /// Ask the UI for a PNG screenshot of the window. The answer arrives on the returned channel.
    pub fn request_screenshot(&self) -> std::sync::mpsc::Receiver<Result<Vec<u8>, String>> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.state().screenshot_waiters.push(tx);
        self.wake();
        rx
    }

    pub fn take_screenshot_waiters(&self) -> Vec<ScreenshotReply> {
        std::mem::take(&mut self.state().screenshot_waiters)
    }
}

fn normalize(p: &Path) -> String {
    let s = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().to_string();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Doc {
        path: Option<PathBuf>,
        n: u32,
    }
    impl HubDoc for Doc {
        fn path(&self) -> Option<&Path> {
            self.path.as_deref()
        }
        fn set_untitled_index(&mut self, i: u32) {
            self.n = i;
        }
    }

    #[test]
    fn basics() {
        let hub: Arc<Hub<Doc>> = Hub::new();
        let a = hub.add(Doc { path: None, n: 0 });
        let b = hub.add(Doc { path: None, n: 0 });
        assert_eq!(lock(&hub.get(b).unwrap()).n, 2);
        assert_eq!(hub.active().unwrap().0, b);
        assert!(hub.set_active(a));
        assert!(hub.remove(a));
        assert_eq!(hub.active().unwrap().0, b);
        hub.notify("x");
        assert_eq!(hub.take_notices(), vec!["x".to_string()]);
        assert!(!hub.has_ui());
    }
}
