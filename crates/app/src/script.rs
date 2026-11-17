//! Scripted input for self-tests (`AIFRAMES_SCRIPT=<file>`): synthetic pointer and key events are
//! injected into egui's raw input frame by frame, with screenshots along the way.
//!
//! One command per line (`#` comments), coordinates in points:
//! `wait N` · `move X Y` · `click X Y [mods]` · `dblclick X Y` · `drag X1 Y1 X2 Y2 [mods]` · `press X Y` /
//! `release X Y` (a drag held across other commands, e.g. a `shot` mid-drag) ·
//! `key NAME [mods]` (mods: ctrl, shift, alt joined by `+`) · `text STRING` · `shot PATH` ·
//! `drop X Y PATH` (a file dropped from Explorer at X,Y) · `hover X Y PATH` (dragged over, not
//! dropped; `hover off` ends it) · `copy` / `cut` / `paste [TEXT]` (the clipboard events egui
//! sends for Ctrl+C/X/V) · `exit`.

use eframe::egui::{self, pos2, Event, Key, Modifiers, PointerButton, Pos2};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A file "dropped from Explorer" by a script.
#[derive(Debug)]
struct ScriptFile(PathBuf);

impl egui::DroppedFile for ScriptFile {
    fn path(&self) -> &Path {
        &self.0
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}

#[derive(Default)]
pub struct Frame {
    pub events: Vec<Event>,
    pub modifiers: Modifiers,
    pub shot: Option<PathBuf>,
    pub exit: bool,
    pub dropped: Vec<PathBuf>,
    /// Starts (`Some(files)`) or ends (`Some([])`) dragging files over the window; it lasts across
    /// frames until ended, like a real drag.
    pub hover: Option<Vec<PathBuf>>,
}

/// Frames before the first scripted event (the window settles).
const WARMUP: u32 = 60;

pub struct Script {
    frames: VecDeque<Frame>,
    warmup: u32,
    hovering: Vec<PathBuf>,
    /// A requested screenshot has not arrived yet: the script waits so the image shows the state
    /// at its `shot` line, not a few frames later.
    pub waiting: Arc<AtomicBool>,
}

fn mods(s: Option<&str>) -> Modifiers {
    let mut m = Modifiers::NONE;
    for part in s.unwrap_or("").split('+') {
        match part {
            "ctrl" | "cmd" => {
                m.ctrl = true;
                m.command = true;
            }
            "shift" => m.shift = true,
            "alt" => m.alt = true,
            _ => {}
        }
    }
    m
}

fn button(pos: Pos2, pressed: bool, modifiers: Modifiers) -> Event {
    Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers }
}

fn frame(events: Vec<Event>, modifiers: Modifiers) -> Frame {
    Frame { events, modifiers, ..Default::default() }
}

impl Script {
    pub fn from_env() -> Option<Script> {
        let path = std::env::var_os("AIFRAMES_SCRIPT")?;
        let text = std::fs::read_to_string(path).ok()?;
        Some(Script::parse(&text))
    }

    pub fn parse(text: &str) -> Script {
        let mut frames = VecDeque::new();
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
            let args: Vec<&str> = rest.split_whitespace().collect();
            let num = |i: usize| args.get(i).and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.0);
            match cmd {
                "wait" => {
                    for _ in 0..(num(0) as usize).max(1) {
                        frames.push_back(Frame::default());
                    }
                }
                "move" => frames.push_back(frame(vec![Event::PointerMoved(pos2(num(0), num(1)))], Modifiers::NONE)),
                "click" | "dblclick" => {
                    let p = pos2(num(0), num(1));
                    let m = mods(args.get(2).copied());
                    frames.push_back(frame(vec![Event::PointerMoved(p)], m));
                    let n = if cmd == "dblclick" { 2 } else { 1 };
                    for _ in 0..n {
                        frames.push_back(frame(vec![button(p, true, m)], m));
                        frames.push_back(frame(vec![button(p, false, m)], m));
                    }
                    frames.push_back(Frame::default());
                }
                "drag" => {
                    let (a, b) = (pos2(num(0), num(1)), pos2(num(2), num(3)));
                    let m = mods(args.get(4).copied());
                    frames.push_back(frame(vec![Event::PointerMoved(a)], m));
                    frames.push_back(frame(vec![button(a, true, m)], m));
                    for i in 1..=12 {
                        let p = a + (b - a) * (i as f32 / 12.0);
                        frames.push_back(frame(vec![Event::PointerMoved(p)], m));
                    }
                    frames.push_back(frame(vec![button(b, false, m)], m));
                    frames.push_back(Frame::default());
                }
                "press" | "release" => {
                    let p = pos2(num(0), num(1));
                    let m = mods(args.get(2).copied());
                    frames.push_back(frame(vec![Event::PointerMoved(p), button(p, cmd == "press", m)], m));
                }
                "key" => {
                    let m = mods(args.get(1).copied());
                    if let Some(key) = args.first().and_then(|k| Key::from_name(k)) {
                        let ev = |pressed| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: m };
                        frames.push_back(frame(vec![ev(true)], m));
                        frames.push_back(frame(vec![ev(false)], m));
                    }
                }
                "drop" => {
                    let p = pos2(num(0), num(1));
                    let file = args.get(2..).map(|a| a.join(" ")).unwrap_or_default();
                    frames.push_back(frame(vec![Event::PointerMoved(p)], Modifiers::NONE));
                    frames.push_back(Frame { events: vec![Event::PointerMoved(p)], dropped: vec![PathBuf::from(file)], ..Default::default() });
                }
                "hover" => {
                    if args.first() == Some(&"off") {
                        frames.push_back(Frame { hover: Some(Vec::new()), ..Default::default() });
                    } else {
                        let p = pos2(num(0), num(1));
                        let file = PathBuf::from(args.get(2..).map(|a| a.join(" ")).unwrap_or_default());
                        frames.push_back(Frame { events: vec![Event::PointerMoved(p)], hover: Some(vec![file]), ..Default::default() });
                        for _ in 0..5 {
                            frames.push_back(Frame::default());
                        }
                    }
                }
                "copy" => frames.push_back(frame(vec![Event::Copy], Modifiers::NONE)),
                "cut" => frames.push_back(frame(vec![Event::Cut], Modifiers::NONE)),
                "paste" => frames.push_back(frame(vec![Event::Paste(rest.to_string())], Modifiers::NONE)),
                "text" => frames.push_back(frame(vec![Event::Text(rest.to_string())], Modifiers::NONE)),
                "shot" => frames.push_back(Frame { shot: Some(PathBuf::from(rest.trim())), ..Default::default() }),
                "exit" => frames.push_back(Frame { exit: true, ..Default::default() }),
                _ => {}
            }
        }
        Script { frames, warmup: WARMUP, hovering: Vec::new(), waiting: Arc::new(AtomicBool::new(false)) }
    }

    /// Adds this frame's synthetic input. Returns the frame when it has a screenshot or exit request.
    pub fn inject(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) -> Option<Frame> {
        ctx.request_repaint();
        if self.warmup == WARMUP {
            // A covered window runs no ui passes (eframe only ticks its logic), which would swallow
            // scripted clicks: keep it on top while the script runs.
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
        }
        let v = raw.viewport();
        if v.occluded == Some(true) || v.minimized == Some(true) {
            return None;
        }
        // The script owns the pointer: a real mouse over the window would otherwise move it between a
        // scripted press and release (no click) or hover other widgets.
        raw.events.retain(|e| !matches!(e, Event::PointerMoved(_) | Event::PointerButton { .. } | Event::PointerGone | Event::MouseMoved(_) | Event::MouseWheel { .. }));
        if self.warmup > 0 {
            self.warmup -= 1;
            return None;
        }
        let hovering = |raw: &mut egui::RawInput, files: &[PathBuf]| {
            for p in files {
                raw.hovered_files.push(egui::HoveredFile { path: Some(p.clone()), mime: String::new() });
            }
        };
        if self.waiting.load(Ordering::SeqCst) {
            hovering(raw, &self.hovering);
            return None;
        }
        let f = self.frames.pop_front()?;
        raw.events.push(Event::ModifiersChanged(f.modifiers));
        raw.events.extend(f.events.iter().cloned());
        for p in &f.dropped {
            raw.dropped_files.push(Arc::new(ScriptFile(p.clone())));
        }
        if let Some(h) = &f.hover {
            self.hovering = h.clone();
        }
        hovering(raw, &self.hovering);
        if f.shot.is_some() {
            self.waiting.store(true, Ordering::SeqCst);
        }
        (f.shot.is_some() || f.exit).then_some(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        let s = Script::parse("# c\nwait 3\nclick 10 20 ctrl\ndrag 0 0 100 0 shift\nkey Space\nkey Z ctrl\ntext hi\nshot a.png\nexit\n");
        // 3 waits + click 4 + drag 16 + keys 4 + text + shot + exit
        assert_eq!(s.frames.len(), 3 + 4 + 16 + 4 + 1 + 1 + 1);
        let click = &s.frames[4];
        assert!(matches!(click.events[0], Event::PointerButton { pressed: true, .. }));
        assert!(click.modifiers.ctrl);
        assert!(s.frames.iter().any(|f| f.shot.is_some()));
        assert!(s.frames.back().unwrap().exit);
    }
}
