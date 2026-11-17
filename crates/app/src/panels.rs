//! Menu bar, project tabs, status bar, toasts and the empty state.

use crate::dialogs::FORMATS;
use crate::playback::{Quality, Stats};
use crate::shortcuts::{self, Command};
use crate::theme;
use crate::viewer::Zoom;
use aiframes_core::TrackKind;
use aiframes_mcp::{JobInfo, JobStatus};
use aiframes_ui::timeline::Tool;
use eframe::egui::{self, vec2, Align2, Color32, FontId, RichText};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideTab {
    Inspector,
    Effects,
    Transcript,
    History,
    Review,
    Ai,
}

impl SideTab {
    pub const ALL: [SideTab; 6] = [SideTab::Inspector, SideTab::Effects, SideTab::Transcript, SideTab::History, SideTab::Review, SideTab::Ai];
    pub fn label(self) -> &'static str {
        match self {
            SideTab::Inspector => "Inspector",
            SideTab::Effects => "Effects",
            SideTab::Transcript => "Transcript",
            SideTab::History => "History",
            SideTab::Review => "Review",
            SideTab::Ai => "AI",
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            SideTab::Inspector => "inspector",
            SideTab::Effects => "effects",
            SideTab::Transcript => "transcript",
            SideTab::History => "history",
            SideTab::Review => "review",
            SideTab::Ai => "ai",
        }
    }
    pub fn from_id(s: &str) -> SideTab {
        SideTab::ALL.into_iter().find(|t| t.id() == s).unwrap_or(SideTab::Inspector)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelId {
    Bin,
    Side,
    Timeline,
    /// (added)
    Source,
}

/// Everything the app can be asked to do from menus, tabs and the status bar.
#[derive(Clone, Debug)]
pub enum Cmd {
    Key(Command),
    NewFormat(usize),
    OpenPath(PathBuf),
    ClearRecent,
    CloseDoc(u64),
    Activate(u64),
    Exit,
    Cut,
    Copy,
    Paste,
    Speed,
    Freeze,
    DetachAudio,
    ToggleEnable,
    AddTrack(TrackKind),
    ToggleSnapping,
    Tool(Tool),
    TogglePanel(PanelId),
    Side(SideTab),
    Zoom(Zoom),
    Quality(Quality),
    Connect,
    About,
    CopyEndpoint,
    ResetStats,
    ToggleScrubAudio,
    ToggleFollowAi,
    ToggleFollowAiPlayhead,
    MatchLength,
    FixedLength,
    SetSpacing,
    CloseGaps,
    /// Generate captions from the transcript; `true` = replace all (asks first).
    Captions(bool),
    CancelJob(u64),
    OpenFolder(PathBuf),
    Validate,
    /// Export the cut as an OpenTimelineIO file (for DaVinci Resolve and other editors).
    ExportOtio,
}

pub struct MenuInfo<'a> {
    pub has_doc: bool,
    pub undo: Option<String>,
    pub redo: Option<String>,
    pub recent: &'a [PathBuf],
    pub has_selection: bool,
    pub has_clipboard: bool,
    pub snapping: bool,
    pub tool: Tool,
    pub show_bin: bool,
    pub show_side: bool,
    pub show_timeline: bool,
    /// (added)
    pub show_source: bool,
    pub safe: bool,
    pub zoom: Zoom,
    pub quality: Quality,
    pub looping: bool,
    pub scrub_audio: bool,
    pub follow_ai: bool,
    pub follow_ai_playhead: bool,
    pub port: Option<u16>,
    /// "Match Length of <base>": the base clip's label when a selected overlay can follow it.
    pub match_length: Option<String>,
    /// A selected clip has an end anchor ("Fixed Length" drops it).
    pub fixed_length: bool,
    /// The track "Set Spacing…" / "Close Gaps" act on (label) and whether it has two clips or more.
    pub gap_track: Option<(String, bool)>,
    /// Captions can be made (transcribed clips exist): (captions now, edited by a person).
    pub captions: Option<(usize, usize)>,
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}

fn item(ui: &mut egui::Ui, out: &mut Vec<Cmd>, label: &str, sc: &str, enabled: bool, cmd: Cmd) {
    if ui.add_enabled(enabled, egui::Button::new(label).shortcut_text(sc)).clicked() {
        out.push(cmd);
        ui.close();
    }
}

fn key(ui: &mut egui::Ui, out: &mut Vec<Cmd>, label: &str, c: Command, enabled: bool) {
    item(ui, out, label, &shortcuts::label(c), enabled, Cmd::Key(c));
}

/// A menu label with a check gutter: every item of a menu that has check items gets the same
/// gutter (a transparent ✔ when unchecked), so all labels line up. `PLACEHOLDER` keeps the
/// normal/hovered/disabled text colours.
fn gutter(label: &str, on: bool) -> egui::text::LayoutJob {
    let font = egui::FontId::proportional(12.5);
    let mut job = egui::text::LayoutJob::default();
    let tick = if on { theme::palette().accent } else { Color32::TRANSPARENT };
    job.append("✔", 0.0, egui::TextFormat { font_id: font.clone(), color: tick, ..Default::default() });
    job.append(label, 6.0, egui::TextFormat { font_id: font, color: Color32::PLACEHOLDER, ..Default::default() });
    job
}

fn check(ui: &mut egui::Ui, out: &mut Vec<Cmd>, label: &str, on: bool, sc: &str, cmd: Cmd) {
    if ui.add(egui::Button::new(gutter(label, on)).shortcut_text(sc)).clicked() {
        out.push(cmd);
        ui.close();
    }
}

/// `item` for menus with check items (aligned with them).
fn item_g(ui: &mut egui::Ui, out: &mut Vec<Cmd>, label: &str, sc: &str, enabled: bool, cmd: Cmd) {
    if ui.add_enabled(enabled, egui::Button::new(gutter(label, false)).shortcut_text(sc)).clicked() {
        out.push(cmd);
        ui.close();
    }
}

fn key_g(ui: &mut egui::Ui, out: &mut Vec<Cmd>, label: &str, c: Command, enabled: bool) {
    item_g(ui, out, label, &shortcuts::label(c), enabled, Cmd::Key(c));
}

pub fn menu_bar(ui: &mut egui::Ui, m: &MenuInfo) -> Vec<Cmd> {
    let mut out = Vec::new();
    let d = m.has_doc;
    egui::MenuBar::new().ui(ui, |ui| {
        ui.menu_button("File", |ui| {
            ui.set_min_width(230.0);
            ui.menu_button("New Project", |ui| {
                for (i, (label, ..)) in FORMATS.iter().enumerate() {
                    let sc = if i == 0 { shortcuts::label(Command::New) } else { String::new() };
                    item(ui, &mut out, label, &sc, true, Cmd::NewFormat(i));
                }
            });
            key(ui, &mut out, "Open…", Command::Open, true);
            ui.menu_button("Open Recent", |ui| {
                if m.recent.is_empty() {
                    ui.add_enabled(false, egui::Button::new("(none)"));
                }
                for p in m.recent.iter().take(15) {
                    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if ui.button(name).on_hover_text(p.display().to_string()).clicked() {
                        out.push(Cmd::OpenPath(p.clone()));
                        ui.close();
                    }
                }
                ui.separator();
                item(ui, &mut out, "Clear Recent", "", !m.recent.is_empty(), Cmd::ClearRecent);
            });
            ui.separator();
            key(ui, &mut out, "Save", Command::Save, d);
            key(ui, &mut out, "Save As…", Command::SaveAs, d);
            ui.separator();
            key(ui, &mut out, "Import Media…", Command::Import, d);
            key(ui, &mut out, "Export…", Command::Export, d);
            item(ui, &mut out, "Export Timeline for Resolve (.otio)…", "", d, Cmd::ExportOtio);
            ui.separator();
            key(ui, &mut out, "Project Settings…", Command::ProjectSettings, d);
            key(ui, &mut out, "Close Project", Command::Close, d);
            item(ui, &mut out, "Exit", "Alt+F4", true, Cmd::Exit);
        });
        ui.menu_button("Edit", |ui| {
            ui.set_min_width(230.0);
            let u = m.undo.as_deref().map(|l| format!("Undo {l}")).unwrap_or_else(|| "Undo".into());
            let r = m.redo.as_deref().map(|l| format!("Redo {l}")).unwrap_or_else(|| "Redo".into());
            key(ui, &mut out, &u, Command::Undo, m.undo.is_some());
            key(ui, &mut out, &r, Command::Redo, m.redo.is_some());
            ui.separator();
            item(ui, &mut out, "Cut", "Ctrl+X", m.has_selection, Cmd::Cut);
            item(ui, &mut out, "Copy", "Ctrl+C", m.has_selection, Cmd::Copy);
            item(ui, &mut out, "Paste at Playhead", "Ctrl+V", d && m.has_clipboard, Cmd::Paste);
            key(ui, &mut out, "Delete", Command::Delete, m.has_selection);
            key(ui, &mut out, "Ripple Delete", Command::RippleDelete, m.has_selection);
            ui.separator();
            key(ui, &mut out, "Split at Playhead", Command::Split, d);
            key(ui, &mut out, "Select All", Command::SelectAll, d);
            key(ui, &mut out, "Deselect", Command::Deselect, m.has_selection);
            ui.separator();
            key(ui, &mut out, "Preferences…", Command::Preferences, true);
        });
        ui.menu_button("View", |ui| {
            ui.set_min_width(230.0);
            check(ui, &mut out, "Media Bin", m.show_bin, "", Cmd::TogglePanel(PanelId::Bin));
            check(ui, &mut out, "Inspector Panels", m.show_side, "", Cmd::TogglePanel(PanelId::Side));
            check(ui, &mut out, "Timeline", m.show_timeline, "", Cmd::TogglePanel(PanelId::Timeline));
            check(ui, &mut out, "Source Monitor", m.show_source, "", Cmd::TogglePanel(PanelId::Source));
            ui.menu_button(gutter("Panel", false), |ui| {
                for t in SideTab::ALL {
                    item(ui, &mut out, t.label(), "", true, Cmd::Side(t));
                }
            });
            ui.separator();
            check(ui, &mut out, "Safe-Area Guides", m.safe, &shortcuts::label(Command::ToggleSafe), Cmd::Key(Command::ToggleSafe));
            ui.menu_button(gutter("Viewer Zoom", false), |ui| {
                for z in Zoom::ALL {
                    check(ui, &mut out, z.label(), m.zoom == z, "", Cmd::Zoom(z));
                }
            });
            ui.menu_button(gutter("Preview Quality", false), |ui| {
                for q in Quality::ALL {
                    check(ui, &mut out, q.label(), m.quality == q, "", Cmd::Quality(q));
                }
            });
            ui.separator();
            key_g(ui, &mut out, "Zoom Timeline In", Command::ZoomIn, d);
            key_g(ui, &mut out, "Zoom Timeline Out", Command::ZoomOut, d);
            key_g(ui, &mut out, "Fit Timeline", Command::ZoomFit, d);
            ui.separator();
            check(ui, &mut out, "Audio Scrubbing", m.scrub_audio, "", Cmd::ToggleScrubAudio);
            check(ui, &mut out, "Follow AI Edits", m.follow_ai, "", Cmd::ToggleFollowAi);
            check(ui, &mut out, "Follow AI Playhead", m.follow_ai_playhead, "", Cmd::ToggleFollowAiPlayhead);
            item_g(ui, &mut out, "Reset Playback Stats", "", true, Cmd::ResetStats);
        });
        ui.menu_button("Timeline", |ui| {
            ui.set_min_width(230.0);
            item_g(ui, &mut out, "Add Video Track", "", d, Cmd::AddTrack(TrackKind::Video));
            item_g(ui, &mut out, "Add Audio Track", "", d, Cmd::AddTrack(TrackKind::Audio));
            ui.separator();
            check(ui, &mut out, "Snapping", m.snapping, "", Cmd::ToggleSnapping);
            ui.menu_button(gutter("Tool", false), |ui| {
                for t in Tool::ALL {
                    let r = ui.add(egui::Button::new(gutter(t.label(), m.tool == t)).shortcut_text(shortcuts::label(shortcuts::tool_command(t))));
                    if r.on_hover_text(t.help()).clicked() {
                        out.push(Cmd::Tool(t));
                        ui.close();
                    }
                }
            });
            ui.separator();
            key_g(ui, &mut out, "Add Marker", Command::AddMarker, d);
            key_g(ui, &mut out, "Set In", Command::SetIn, d);
            key_g(ui, &mut out, "Set Out", Command::SetOut, d);
            key_g(ui, &mut out, "Clear In/Out", Command::ClearInOut, d);
            check(ui, &mut out, "Loop Playback", m.looping, &shortcuts::label(Command::ToggleLoop), Cmd::Key(Command::ToggleLoop));
            key_g(ui, &mut out, "Play In to Out", Command::PlayFromIn, d);
            ui.separator();
            let (gap_label, gaps_ok) = m.gap_track.clone().unwrap_or_default();
            let on = if gap_label.is_empty() { String::new() } else { format!(" on {gap_label}") };
            item_g(ui, &mut out, &format!("Set Spacing{on}…"), "", d && gaps_ok, Cmd::SetSpacing);
            item_g(ui, &mut out, &format!("Close Gaps{on}"), "", d && gaps_ok, Cmd::CloseGaps);
            ui.separator();
            let (caps, edited) = m.captions.unwrap_or((0, 0));
            let gen = if caps == 0 { "Generate Captions".to_string() } else if edited > 0 { format!("Regenerate Captions (keep {edited} edited)") } else { "Regenerate Captions".to_string() };
            item_g(ui, &mut out, &gen, "", d && m.captions.is_some(), Cmd::Captions(false));
            item_g(ui, &mut out, "Replace All Captions…", "", d && m.captions.is_some() && caps > 0, Cmd::Captions(true));
            ui.separator();
            item_g(ui, &mut out, "Validate Project", "", d, Cmd::Validate);
        });
        ui.menu_button("Clip", |ui| {
            ui.set_min_width(230.0);
            let s = m.has_selection;
            item(ui, &mut out, "Speed…", "", s, Cmd::Speed);
            item(ui, &mut out, "Freeze Frame at Playhead", "", s, Cmd::Freeze);
            item(ui, &mut out, "Detach Audio", "", s, Cmd::DetachAudio);
            item(ui, &mut out, "Enable / Disable", "", s, Cmd::ToggleEnable);
            ui.separator();
            let base = m.match_length.as_deref().map(|b| format!("Match Length of {}", ellipsize(b, 28))).unwrap_or_else(|| "Match Length of Base Clip".into());
            let r = ui.add_enabled(m.match_length.is_some(), egui::Button::new(&base));
            if r.clicked() {
                out.push(Cmd::MatchLength);
                ui.close();
            } else {
                r.on_hover_text("For an overlay anchored with a base clip: end when the base ends (follows its trims and speed changes).");
            }
            let r = ui.add_enabled(m.fixed_length, egui::Button::new("Fixed Length"));
            if r.clicked() {
                out.push(Cmd::FixedLength);
                ui.close();
            } else {
                r.on_hover_text("Drop the end anchor (until): the clip keeps its current length.");
            }
            ui.separator();
            key(ui, &mut out, "Split at Playhead", Command::Split, d);
            key(ui, &mut out, "Ripple Delete", Command::RippleDelete, s);
        });
        ui.menu_button("AI", |ui| {
            ui.set_min_width(250.0);
            key(ui, &mut out, "Ask Claude…", Command::AiPanel, d);
            item(ui, &mut out, "Connect Claude Code…", "", true, Cmd::Connect);
            let ep = m.port.map(|p| format!("Copy MCP Endpoint (:{p})")).unwrap_or_else(|| "MCP server not running".into());
            item(ui, &mut out, &ep, "", m.port.is_some(), Cmd::CopyEndpoint);
            item(ui, &mut out, "AI Activity", "", true, Cmd::Side(SideTab::Ai));
        });
        ui.menu_button("Help", |ui| {
            ui.set_min_width(220.0);
            key(ui, &mut out, "Keyboard Shortcuts", Command::Shortcuts, true);
            item(ui, &mut out, "About AIFrames", "", true, Cmd::About);
        });
    });
    out
}

/// Project tabs (hub documents). `docs` = (id, title, dirty).
pub fn tab_strip(ui: &mut egui::Ui, docs: &[(u64, String, bool)], active: Option<u64>) -> Vec<Cmd> {
    let mut out = Vec::new();
    let pal = theme::palette();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for (id, title, dirty) in docs {
            let on = Some(*id) == active;
            let text = format!("{title}{}", if *dirty { " •" } else { "" });
            let fill = if on { pal.panel } else { pal.panel_dark };
            egui::Frame::NONE.fill(fill).corner_radius(egui::CornerRadius { nw: 4, ne: 4, sw: 0, se: 0 }).inner_margin(egui::Margin::symmetric(8, 3)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    let r = ui.add(egui::Label::new(RichText::new(text).color(if on { pal.text } else { pal.text_dim })).sense(egui::Sense::click()));
                    if r.clicked() {
                        out.push(Cmd::Activate(*id));
                    }
                    if r.middle_clicked() {
                        out.push(Cmd::CloseDoc(*id));
                    }
                    if ui.add(egui::Button::new(RichText::new("×").color(pal.text_dim)).frame(false)).on_hover_text("Close").clicked() {
                        out.push(Cmd::CloseDoc(*id));
                    }
                });
            });
        }
        if ui.add(egui::Button::new(RichText::new("+").strong()).frame(false)).on_hover_text("New project").clicked() {
            out.push(Cmd::NewFormat(0));
        }
    });
    out
}

pub struct StatusInfo<'a> {
    pub revision: u64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration: f64,
    pub playhead: f64,
    pub stats: &'a Stats,
    pub playing: bool,
    pub jobs: &'a [JobInfo],
    pub port: Option<u16>,
    pub notice: Option<&'a str>,
    pub status: &'a str,
    pub last_export: Option<&'a PathBuf>,
    pub ai_running: bool,
}

pub fn status_bar(ui: &mut egui::Ui, s: &StatusInfo) -> Vec<Cmd> {
    let mut out = Vec::new();
    let pal = theme::palette();
    let dim = |t: String| RichText::new(t).small().color(pal.text_dim);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        ui.label(dim(format!("rev {}", s.revision)));
        ui.label(dim(format!("{}×{} · {} fps", s.width, s.height, s.fps)));
        ui.label(dim(format!("dur {}", crate::timecode::timecode(s.duration, s.fps))));
        ui.label(RichText::new(crate::timecode::timecode(s.playhead, s.fps)).small().monospace().color(pal.text));
        let st = s.stats;
        let drop_col = if st.dropped > 0 { pal.warn } else { pal.text_dim };
        ui.label(RichText::new(format!("dropped {}", st.dropped)).small().color(drop_col)).on_hover_text("Frames that should have been shown during playback but were not (click View > Reset Playback Stats to reset)");
        ui.label(dim(format!("render {:.1} ms @ {:.0}%", st.render_ms, st.scale * 100.0))).on_hover_text("Average preview render time per frame and the preview resolution");
        if s.playing {
            ui.label(dim(format!("{:.0} fps", st.fps)));
        }
        if !st.audio {
            ui.label(RichText::new("no audio device").small().color(pal.warn));
        } else if st.audio_resynced {
            ui.label(dim("audio synced to video".into())).on_hover_text("The sound was late (decoding a file's audio for the first time), so the picture runs on its own clock and the sound follows it");
        } else if st.underruns > 0 {
            ui.label(RichText::new(format!("audio underruns {}", st.underruns)).small().color(pal.warn));
        }
        if let Some(e) = &st.error {
            ui.label(RichText::new(format!("render: {}", truncate(e, 60))).small().color(pal.error)).on_hover_text(e.as_str());
        }
        for j in s.jobs.iter().filter(|j| j.status == JobStatus::Running) {
            ui.separator();
            ui.label(RichText::new(&j.label).small().color(pal.text));
            ui.add(egui::ProgressBar::new(j.progress).desired_width(90.0).desired_height(10.0)).on_hover_text(if j.message.is_empty() { j.kind.clone() } else { j.message.clone() });
            if ui.add(egui::Button::new(RichText::new("✕").small()).frame(false)).on_hover_text("Cancel").clicked() {
                out.push(Cmd::CancelJob(j.id));
            }
        }
        if let Some(p) = s.last_export {
            if ui.add(egui::Button::new(RichText::new("📂 Show export").small()).frame(false)).on_hover_text(p.display().to_string()).clicked() {
                out.push(Cmd::OpenFolder(p.clone()));
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            match s.port {
                Some(p) => {
                    let r = ui.add(egui::Label::new(RichText::new(format!("MCP :{p}")).small().color(if s.ai_running { pal.ai } else { pal.ok })).sense(egui::Sense::click()));
                    if r.on_hover_text(format!("{} (click to copy)", crate::ai::mcp_url(p))).clicked() {
                        out.push(Cmd::CopyEndpoint);
                    }
                }
                None => {
                    ui.label(RichText::new("MCP off").small().color(pal.error));
                }
            }
            if let Some(n) = s.notice {
                ui.label(RichText::new(truncate(n, 80)).small().color(pal.ai));
            } else if !s.status.is_empty() {
                ui.label(dim(truncate(s.status, 80)));
            }
        });
    });
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Ai,
    Error,
}

pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub at: f64,
}

pub const TOAST_SECS: f64 = 4.5;

pub fn toasts(ctx: &egui::Context, toasts: &mut Vec<Toast>, now: f64) {
    toasts.retain(|t| now - t.at < TOAST_SECS + if matches!(t.kind, ToastKind::Error | ToastKind::Ai) { 3.5 } else { 0.0 });
    if toasts.is_empty() {
        return;
    }
    let pal = theme::palette();
    egui::Area::new(egui::Id::unique("aiframes-toasts")).anchor(Align2::RIGHT_BOTTOM, vec2(-14.0, -34.0)).order(egui::Order::Foreground).interactable(false).show(ctx, |ui| {
        ui.set_max_width(420.0);
        for t in toasts.iter().rev().take(5).collect::<Vec<_>>().into_iter().rev() {
            let (bar, icon) = match t.kind {
                ToastKind::Info => (pal.accent, "•"),
                ToastKind::Ai => (pal.ai, "◆"),
                ToastKind::Error => (pal.error, "!"),
            };
            egui::Frame::NONE.fill(Color32::from_rgba_unmultiplied(0x2a, 0x2a, 0x30, 245)).stroke(egui::Stroke::new(1.0, bar)).corner_radius(5).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon).color(bar));
                    ui.add(egui::Label::new(RichText::new(&t.text).color(pal.text)).wrap());
                });
            });
            ui.add_space(4.0);
        }
    });
}

/// Shown when no project is open.
pub fn empty_state(ui: &mut egui::Ui, recent: &[PathBuf]) -> Vec<Cmd> {
    let mut out = Vec::new();
    let pal = theme::palette();
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.18);
        ui.label(RichText::new("AIFrames").size(30.0).strong().color(pal.accent));
        ui.label(RichText::new("Edit video with Claude, live.").color(pal.text_dim));
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            let w = 3.0 * 170.0;
            ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
            if ui.add(egui::Button::new("New 1080p project").min_size(vec2(160.0, 30.0))).clicked() {
                out.push(Cmd::NewFormat(0));
            }
            if ui.add(egui::Button::new("New vertical (Shorts)").min_size(vec2(160.0, 30.0))).clicked() {
                out.push(Cmd::NewFormat(4));
            }
            if ui.add(egui::Button::new("Open…").min_size(vec2(160.0, 30.0))).clicked() {
                out.push(Cmd::Key(Command::Open));
            }
        });
        if !recent.is_empty() {
            ui.add_space(18.0);
            ui.label(RichText::new("Recent").small().color(pal.text_dim));
            for p in recent.iter().take(8) {
                let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                if ui.link(name).on_hover_text(p.display().to_string()).clicked() {
                    out.push(Cmd::OpenPath(p.clone()));
                }
            }
        }
    });
    out
}

/// Overlay while files are dragged over the window from Explorer.
/// Overlay while files are dragged over the window from Explorer. `timeline` = the timeline's rect
/// when the pointer is over it (the files will also be placed there).
pub fn drop_overlay(ctx: &egui::Context, n: usize, timeline: Option<egui::Rect>) {
    let pal = theme::palette();
    let rect = ctx.content_rect();
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("aiframes-drop")));
    painter.rect_filled(rect, 0.0, Color32::from_black_alpha(150));
    let files = format!("{n} file{}", if n == 1 { "" } else { "s" });
    let them = if n == 1 { "it" } else { "them" };
    match timeline {
        Some(t) => {
            painter.rect_stroke(t.shrink(3.0), 4.0, egui::Stroke::new(2.5, pal.accent), egui::StrokeKind::Inside);
            painter.text(t.center_top() + vec2(0.0, 22.0), Align2::CENTER_TOP, format!("Drop {files} to import and place {them} here"), FontId::proportional(17.0), Color32::WHITE);
            painter.text(t.center_top() + vec2(0.0, 46.0), Align2::CENTER_TOP, "on the track under the pointer, at the time under the pointer", FontId::proportional(13.0), pal.text_dim);
        }
        None => {
            painter.rect_stroke(rect.shrink(6.0), 6.0, egui::Stroke::new(2.0, pal.accent), egui::StrokeKind::Inside);
            painter.text(rect.center() - vec2(0.0, 12.0), Align2::CENTER_CENTER, format!("Drop {files} to import into the media bin"), FontId::proportional(17.0), Color32::WHITE);
            painter.text(rect.center() + vec2(0.0, 14.0), Align2::CENTER_CENTER, "Projects (.aifr) open in a new tab  ·  drop on the timeline to also place them", FontId::proportional(13.0), pal.text_dim);
        }
    }
}
