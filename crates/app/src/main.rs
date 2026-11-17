#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! `aiframes` — the AIFrames video editor.
//!
//! * no arguments / project files: the GUI (single instance: files are forwarded to a running window)
//! * `aiframes mcp`: MCP stdio server (proxies to a running window, else headless)
//! * `aiframes new|import|describe|validate|apply|snapshot|contact-sheet|render|transcribe …`: headless CLI
//! * `aiframes --register` / `--unregister`: `.aifr` file association
//! * `aiframes --selftest [--keep]`: end-to-end smoke test (media → project → playback → ops → export)
//! * `aiframes --bench-playback <project.aifr> [options]`: measures viewer playback (see `bench.rs`)

mod ai;
mod app;
mod bench;
mod dialogs;
mod edits;
mod export;
mod external;
mod follow;
mod panels;
mod playback;
mod prefs;
mod previews;
mod register;
mod script;
mod selftest;
mod shortcuts;
mod sysmem;
mod theme;
mod timecode;
mod viewer;

use aiframes_core::Document;
use aiframes_mcp::{ProjectDoc, Services};
use std::path::PathBuf;

/// A release build has no console; CLI commands write to the console they were started from.
#[cfg(all(windows, not(debug_assertions)))]
fn attach_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(pid: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    // SAFETY: documented Win32 call without pointer arguments; failure (no parent console, or
    // output already redirected to a pipe/file) is harmless.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn attach_console() {}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("mcp") | Some("--mcp") => {
            aiframes_mcp::run_stdio();
            return;
        }
        Some("--register") => {
            attach_console();
            if let Err(e) = register::register() {
                eprintln!("register failed: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some("--unregister") => {
            attach_console();
            if let Err(e) = register::unregister() {
                eprintln!("unregister failed: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some("--version") | Some("-V") => {
            attach_console();
            println!("AIFrames {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--selftest") => {
            attach_console();
            std::process::exit(selftest::run(&args[1..]));
        }
        Some("--bench-playback") => {
            attach_console();
            std::process::exit(bench::run(&args[1..]));
        }
        Some(cmd) if aiframes_mcp::is_cli_command(cmd) => {
            attach_console();
            std::process::exit(aiframes_mcp::cli(&args));
        }
        _ => {}
    }

    let paths: Vec<PathBuf> = args.iter().filter(|a| !a.starts_with("--")).map(PathBuf::from).map(|p| std::fs::canonicalize(&p).map(|c| PathBuf::from(c.to_string_lossy().trim_start_matches(r"\\?\"))).unwrap_or(p)).collect();
    let (projects, media): (Vec<PathBuf>, Vec<PathBuf>) = paths.into_iter().partition(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(aiframes_core::PROJECT_EXT)));
    if !projects.is_empty() && media.is_empty() && suite_mcp::forward_open(&aiframes_mcp::SPEC, &projects) {
        return;
    }

    let services = Services::new();
    let mut errors = Vec::new();
    for f in &projects {
        match Document::open(f) {
            Ok(doc) => {
                suite_ui::recent::push(app::APP_NAME, f);
                services.hub.add(ProjectDoc::new(doc));
            }
            Err(e) => errors.push(format!("{}\n\n{e}", f.display())),
        }
    }
    if services.hub.list().is_empty() {
        services.hub.add(ProjectDoc::new(Document::new(dialogs::new_project(1920, 1080, 30.0))));
    }
    let port = match aiframes_mcp::start_http(services.clone()) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("aiframes: MCP server not started: {e}");
            None
        }
    };

    let prefs = prefs::Prefs::load();
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("AIFrames")
        .with_inner_size([1600.0, 960.0])
        .with_min_inner_size([900.0, 560.0])
        .with_drag_and_drop(true)
        .with_icon(theme::icon())
        .with_maximized(prefs.layout.maximized);
    if let Some([x, y, w, h]) = prefs.layout.window {
        if w >= 600.0 && h >= 400.0 {
            viewport = viewport.with_position([x, y]).with_inner_size([w, h]);
        }
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let s = services.clone();
    let result = eframe::run_native("AIFrames", options, Box::new(move |cc| Ok(Box::new(app::AiFramesApp::new(cc, s, port, errors, media)))));
    aiframes_mcp::shutdown();
    if let Err(e) = result {
        eprintln!("AIFrames failed to start: {e}");
        std::process::exit(1);
    }
}
