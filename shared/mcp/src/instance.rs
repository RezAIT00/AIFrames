//! The instance file advertises a running window's MCP port to other processes of the same app.

use crate::{client, AppSpec};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Instance {
    pub pid: u32,
    pub port: u16,
    pub exe: String,
    /// Unix timestamp (seconds).
    pub started: u64,
}

/// `%<APP>_INSTANCE_DIR%`, else `%LOCALAPPDATA%\<Name>`, else `$XDG_RUNTIME_DIR/<id>`, else temp.
pub fn instance_dir(spec: &AppSpec) -> PathBuf {
    if let Ok(d) = std::env::var(spec.env("INSTANCE_DIR")) {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Ok(d) = std::env::var("LOCALAPPDATA") {
        if !d.is_empty() {
            return PathBuf::from(d).join(spec.name);
        }
    }
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d).join(spec.id);
        }
    }
    std::env::temp_dir().join(spec.id)
}

pub fn instance_path(spec: &AppSpec) -> PathBuf {
    instance_dir(spec).join("instance.json")
}

pub fn write_instance(spec: &AppSpec, port: u16) -> std::io::Result<()> {
    let dir = instance_dir(spec);
    std::fs::create_dir_all(&dir)?;
    let inst = Instance {
        pid: std::process::id(),
        port,
        exe: std::env::current_exe().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
        started: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
    };
    let tmp = dir.join(format!("instance.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&inst).unwrap_or_default())?;
    std::fs::rename(&tmp, instance_path(spec))
}

pub fn read_instance(spec: &AppSpec) -> Option<Instance> {
    let bytes = std::fs::read(instance_path(spec)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Remove the instance file if it was written by this process.
pub fn remove_instance_if_ours(spec: &AppSpec) {
    if let Some(i) = read_instance(spec) {
        if i.pid == std::process::id() {
            let _ = std::fs::remove_file(instance_path(spec));
        }
    }
}

/// Port of a live window (instance file present and `/health` answers as this app).
pub fn live_instance_port(spec: &AppSpec) -> Option<u16> {
    let inst = read_instance(spec)?;
    if client::health(inst.port, spec.id) {
        Some(inst.port)
    } else {
        None
    }
}
