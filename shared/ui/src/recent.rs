//! Recently used files, persisted in `%APPDATA%\<App>\recent.json`.

use std::path::{Path, PathBuf};

const MAX: usize = 20;

fn store_path(app: &str) -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join(app).join("recent.json"))
}

pub fn load(app: &str) -> Vec<PathBuf> {
    let Some(p) = store_path(app) else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(p) else { return Vec::new() };
    let list: Vec<String> = serde_json::from_str(&text).unwrap_or_default();
    list.into_iter().map(PathBuf::from).collect()
}

fn save(app: &str, list: &[PathBuf]) {
    let Some(p) = store_path(app) else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let v: Vec<String> = list.iter().map(|p| p.to_string_lossy().to_string()).collect();
    if let Ok(s) = serde_json::to_string_pretty(&v) {
        let _ = std::fs::write(p, s);
    }
}

pub fn push(app: &str, path: &Path) {
    let mut list = load(app);
    let key = path.to_string_lossy().to_lowercase();
    list.retain(|p| p.to_string_lossy().to_lowercase() != key);
    list.insert(0, path.to_path_buf());
    list.truncate(MAX);
    save(app, &list);
}

pub fn remove(app: &str, path: &Path) {
    let mut list = load(app);
    let key = path.to_string_lossy().to_lowercase();
    list.retain(|p| p.to_string_lossy().to_lowercase() != key);
    save(app, &list);
}
