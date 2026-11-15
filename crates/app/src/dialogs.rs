//! Windows and prompts: project settings, preferences, keyboard shortcuts, about, connect Claude,
//! save-changes and autosave-recovery prompts, clip speed.

use crate::prefs::Prefs;
use crate::shortcuts::{self, SHORTCUTS};
use crate::theme;
use aiframes_core::{Color, Op, Project, Settings, Track, TrackKind};
use eframe::egui::{self, Color32, RichText};
use serde_json::json;
use std::path::PathBuf;

/// New-project formats (File > New).
pub static FORMATS: &[(&str, u32, u32, f64)] = &[
    ("HD 1080p 30", 1920, 1080, 30.0),
    ("HD 1080p 60", 1920, 1080, 60.0),
    ("HD 1080p 24", 1920, 1080, 24.0),
    ("4K UHD 30", 3840, 2160, 30.0),
    ("Vertical 1080×1920 30 (Shorts)", 1080, 1920, 30.0),
    ("Square 1080×1080 30", 1080, 1080, 30.0),
    ("HD 720p 30", 1280, 720, 30.0),
];

/// A fresh project: one main video track and one audio track.
pub fn new_project(w: u32, h: u32, fps: f64) -> Project {
    let mut p = Project { settings: Settings { width: w, height: h, fps, ..Settings::default() }, ..Default::default() };
    let track = |id: &str, name: &str, kind: TrackKind, role: &str| Track {
        id: id.into(),
        name: name.into(),
        kind,
        role: role.into(),
        muted: false,
        hidden: false,
        locked: false,
        solo: false,
        volume: 1.0,
    };
    p.tracks = vec![track("v1", "V1", TrackKind::Video, "main"), track("v2", "V2", TrackKind::Video, "overlay"), track("a1", "A1", TrackKind::Audio, "voice"), track("a2", "A2", TrackKind::Audio, "music")];
    p
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectForm {
    pub name: String,
    pub notes: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub sample_rate: u32,
    pub background: Color32,
    pub fixed: bool,
    pub duration: f64,
    pub title_safe: f64,
    pub action_safe: f64,
}

impl ProjectForm {
    pub fn from(p: &Project) -> Self {
        let s = &p.settings;
        let [r, g, b, a] = s.background.to_rgba8();
        ProjectForm {
            name: p.name.clone(),
            notes: p.notes.clone(),
            width: s.width,
            height: s.height,
            fps: s.fps,
            sample_rate: s.sample_rate,
            background: Color32::from_rgba_unmultiplied(r, g, b, a),
            fixed: s.duration.is_some(),
            // Not fixed: start from the length the edit has now (a round number when empty).
            duration: s.duration.unwrap_or_else(|| { let d = aiframes_core::resolve(p).duration; if d > 0.0 { (d * 10.0).round() / 10.0 } else { 60.0 } }),
            title_safe: s.title_safe,
            action_safe: s.action_safe,
        }
    }

    /// Ops for what changed.
    pub fn ops(&self, p: &Project) -> Vec<Op> {
        let mut ops = Vec::new();
        let mut proj = serde_json::Map::new();
        if self.name != p.name {
            proj.insert("name".into(), json!(self.name));
        }
        if self.notes != p.notes {
            proj.insert("notes".into(), json!(self.notes));
        }
        if !proj.is_empty() {
            proj.insert("op".into(), json!("set_project"));
            ops.extend(Op::from_json(&serde_json::Value::Object(proj)).ok());
        }
        let s = &p.settings;
        let mut set = serde_json::Map::new();
        if self.width != s.width {
            set.insert("width".into(), json!(self.width));
        }
        if self.height != s.height {
            set.insert("height".into(), json!(self.height));
        }
        if (self.fps - s.fps).abs() > 1e-9 {
            set.insert("fps".into(), json!(self.fps));
        }
        if self.sample_rate != s.sample_rate {
            set.insert("sample_rate".into(), json!(self.sample_rate));
        }
        let bg = Color::rgba8(self.background.r(), self.background.g(), self.background.b(), self.background.a());
        if bg.to_rgba8() != s.background.to_rgba8() {
            set.insert("background".into(), json!(bg.to_hex()));
        }
        let dur = self.fixed.then_some(self.duration.max(0.1));
        if dur != s.duration {
            set.insert("duration".into(), json!(dur));
        }
        if (self.title_safe - s.title_safe).abs() > 1e-9 {
            set.insert("title_safe".into(), json!(self.title_safe));
        }
        if (self.action_safe - s.action_safe).abs() > 1e-9 {
            set.insert("action_safe".into(), json!(self.action_safe));
        }
        if !set.is_empty() {
            set.insert("op".into(), json!("set_settings"));
            ops.extend(Op::from_json(&serde_json::Value::Object(set)).ok());
        }
        ops
    }
}

#[derive(Clone, Debug)]
pub enum AfterSave {
    Close,
    Exit,
}

#[derive(Clone, Debug)]
pub enum Modal {
    SaveChanges { doc: u64, title: String, then: AfterSave },
    Recover { doc: u64, autosave: PathBuf, title: String },
    Speed { speed: f64, keep_duration: bool },
    /// Asks before applying `ops` (e.g. replacing every caption).
    Confirm { title: String, text: String, ok: String, label: String, ops: Vec<Op> },
    Alert { title: String, text: String },
}

#[derive(Default)]
pub struct Dialogs {
    pub project: Option<ProjectForm>,
    pub prefs: Option<Prefs>,
    pub shortcuts: bool,
    pub about: bool,
    pub connect: bool,
    pub modal: Option<Modal>,
}

pub enum DialogOut {
    Apply { label: String, ops: Vec<Op> },
    Prefs(Prefs),
    /// Save-changes answer: Some(true) = save, Some(false) = discard, None = cancel.
    SaveChanges { doc: u64, then: AfterSave, save: Option<bool> },
    Recover { doc: u64, autosave: PathBuf, yes: bool },
    Speed { speed: f64, keep_duration: bool },
}

pub struct DialogCx<'a> {
    pub project: Option<&'a Project>,
    /// "Memory now: …" for the Preferences window.
    pub memory: String,
    /// The automatic frame-cache size for this machine (MB).
    pub auto_cache_mb: u32,
    pub port: Option<u16>,
    pub exe: &'a str,
}

impl Dialogs {
    pub fn alert(&mut self, title: &str, text: &str) {
        self.modal = Some(Modal::Alert { title: title.into(), text: text.into() });
    }

    pub fn show(&mut self, ctx: &egui::Context, cx: &DialogCx) -> Vec<DialogOut> {
        let mut out = Vec::new();
        let pal = theme::palette();
        let center = ctx.content_rect().center();
        // Project settings.
        if let (Some(form), Some(p)) = (&mut self.project, cx.project) {
            let mut open = true;
            let mut close = false;
            egui::Window::new("Project Settings").open(&mut open).collapsible(false).resizable(false).default_width(420.0).pivot(egui::Align2::CENTER_CENTER).default_pos(center).show(ctx, |ui| {
                egui::Grid::new("proj-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut form.name).desired_width(260.0));
                    ui.end_row();
                    ui.label("Format");
                    egui::ComboBox::from_id_salt("proj-format").width(260.0).selected_text(format!("{}×{} @ {}", form.width, form.height, form.fps)).show_ui(ui, |ui| {
                        for (label, w, h, fps) in FORMATS {
                            if ui.selectable_label(form.width == *w && form.height == *h && (form.fps - fps).abs() < 1e-9, *label).clicked() {
                                form.width = *w;
                                form.height = *h;
                                form.fps = *fps;
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("Frame size");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut form.width).range(16..=8192).suffix(" px"));
                        ui.label("×");
                        ui.add(egui::DragValue::new(&mut form.height).range(16..=8192).suffix(" px"));
                        if ui.small_button("⇄").on_hover_text("Swap (landscape / portrait)").clicked() {
                            std::mem::swap(&mut form.width, &mut form.height);
                        }
                    });
                    ui.end_row();
                    ui.label("Frame rate");
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("proj-fps").width(80.0).selected_text(format!("{}", form.fps)).show_ui(ui, |ui| {
                            for f in [23.976, 24.0, 25.0, 29.97, 30.0, 50.0, 59.94, 60.0] {
                                ui.selectable_value(&mut form.fps, f, format!("{f}"));
                            }
                        });
                        ui.label("fps");
                    });
                    ui.end_row();
                    ui.label("Sample rate");
                    egui::ComboBox::from_id_salt("proj-rate").width(80.0).selected_text(format!("{} Hz", form.sample_rate)).show_ui(ui, |ui| {
                        for r in [44_100u32, 48_000, 96_000] {
                            ui.selectable_value(&mut form.sample_rate, r, format!("{r} Hz"));
                        }
                    });
                    ui.end_row();
                    ui.label("Background");
                    ui.color_edit_button_srgba(&mut form.background);
                    ui.end_row();
                    ui.label("Length");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut form.fixed, "Fixed");
                        ui.add_enabled(form.fixed, egui::DragValue::new(&mut form.duration).range(0.1..=86_400.0).speed(0.5).suffix(" s"));
                        if !form.fixed {
                            ui.label(RichText::new("ends with the last clip").small().color(pal.text_dim));
                        }
                    });
                    ui.end_row();
                    ui.label("Safe areas");
                    ui.horizontal(|ui| {
                        ui.label("title");
                        ui.add(egui::DragValue::new(&mut form.title_safe).range(0.5..=1.0).speed(0.01).max_decimals(2));
                        ui.label("action");
                        ui.add(egui::DragValue::new(&mut form.action_safe).range(0.5..=1.0).speed(0.01).max_decimals(2));
                    });
                    ui.end_row();
                    ui.label("Notes / brief");
                    ui.add(egui::TextEdit::multiline(&mut form.notes).desired_rows(4).desired_width(260.0).hint_text("Goal, audience, style rules: read by collaborators and the AI"));
                    ui.end_row();
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("OK").strong()).clicked() {
                        let ops = form.ops(p);
                        if !ops.is_empty() {
                            out.push(DialogOut::Apply { label: "Project settings".into(), ops });
                        }
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
            if !open || close {
                self.project = None;
            }
        } else if self.project.is_some() && cx.project.is_none() {
            self.project = None;
        }

        // Preferences.
        if let Some(pr) = &mut self.prefs {
            let mut open = true;
            let mut close = false;
            egui::Window::new("Preferences").open(&mut open).collapsible(false).resizable(false).default_width(420.0).pivot(egui::Align2::CENTER_CENTER).default_pos(center).show(ctx, |ui| {
                egui::Grid::new("prefs-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Frame cache");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut pr.cache_auto, format!("Automatic ({} MB)", cx.auto_cache_mb)).on_hover_text("1/64 of the installed memory, between 256 and 512 MB");
                        ui.add_enabled(!pr.cache_auto, egui::DragValue::new(&mut pr.cache_mb).range(128..=65_536).speed(16).suffix(" MB"));
                    });
                    ui.end_row();
                    ui.label("Decoder readahead");
                    ui.add(egui::DragValue::new(&mut pr.readahead_mb).range(32..=4096).speed(8).suffix(" MB"))
                        .on_hover_text("Frames decoded ahead of playback, shared by all open decoders (default 128 MB). More can smooth 4K playback; less saves memory.");
                    ui.end_row();
                    ui.label("");
                    ui.label(RichText::new(&cx.memory).small().color(pal.text_dim));
                    ui.end_row();
                    ui.label("Hardware decoding");
                    egui::ComboBox::from_id_salt("prefs-hw").selected_text(pr.hw_decode.clone()).show_ui(ui, |ui| {
                        for (v, l) in [("auto", "auto"), ("off", "off (CPU)"), ("cuda", "NVDEC (CUDA)"), ("d3d11va", "D3D11VA")] {
                            ui.selectable_value(&mut pr.hw_decode, v.to_string(), l);
                        }
                    });
                    ui.end_row();
                    ui.label("Preview quality");
                    egui::ComboBox::from_id_salt("prefs-q").selected_text(pr.quality.clone()).show_ui(ui, |ui| {
                        for q in ["auto", "full", "half", "quarter"] {
                            ui.selectable_value(&mut pr.quality, q.to_string(), q);
                        }
                    });
                    ui.end_row();
                    ui.label("Render threads");
                    ui.add(egui::DragValue::new(&mut pr.render_threads).range(1..=8)).on_hover_text("Background threads rendering preview frames (applies after restart)");
                    ui.end_row();
                    ui.label("Autosave every");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut pr.autosave_secs).range(0..=3600).speed(5).suffix(" s"));
                        ui.label(RichText::new("0 = off; writes <project>.aifr.autosave").small().color(pal.text_dim));
                    });
                    ui.end_row();
                    ui.label("Import folder");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut pr.import_dir).desired_width(220.0).hint_text("last used"));
                        if ui.small_button("…").clicked() {
                            if let Some(d) = rfd::FileDialog::new().set_title("Default import folder").pick_folder() {
                                pr.import_dir = d.to_string_lossy().to_string();
                            }
                        }
                    });
                    ui.end_row();
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("OK").strong()).clicked() {
                        out.push(DialogOut::Prefs(pr.clone().sanitized()));
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    if ui.button("Defaults").clicked() {
                        let layout = pr.layout.clone();
                        *pr = Prefs { layout, ..Prefs::default() };
                    }
                });
            });
            if !open || close {
                self.prefs = None;
            }
        }

        // Keyboard shortcuts.
        if self.shortcuts {
            let mut open = true;
            egui::Window::new("Keyboard Shortcuts").open(&mut open).default_width(420.0).default_height(520.0).pivot(egui::Align2::CENTER_CENTER).default_pos(center).show(ctx, |ui| {
                ui.label(RichText::new("The timeline handles its own keys (trim, ripple, nudge…) while it has focus; arrows step frames when the viewer has focus.").small().color(pal.text_dim));
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    egui::Grid::new("sc-grid").striped(true).num_columns(2).spacing([16.0, 3.0]).show(ui, |ui| {
                        for s in SHORTCUTS {
                            ui.label(RichText::new(shortcuts::text(s)).monospace().color(pal.accent));
                            let scope = if s.scope == shortcuts::Scope::Viewer { "  (viewer)" } else { "" };
                            ui.label(format!("{}{scope}", s.help));
                            ui.end_row();
                        }
                    });
                    ui.add_space(8.0);
                    ui.label(RichText::new("In the timeline (after clicking it)").strong());
                    egui::Grid::new("sc-grid-tl").striped(true).num_columns(2).spacing([16.0, 3.0]).show(ui, |ui| {
                        for (k, help) in shortcuts::TIMELINE_KEYS {
                            ui.label(RichText::new(*k).monospace().color(pal.accent));
                            ui.label(*help);
                            ui.end_row();
                        }
                    });
                });
            });
            self.shortcuts = open;
        }

        // About.
        if self.about {
            let mut open = true;
            egui::Window::new("About AIFrames").open(&mut open).collapsible(false).resizable(false).pivot(egui::Align2::CENTER_CENTER).default_pos(center).show(ctx, |ui| {
                ui.heading(RichText::new("AIFrames").color(pal.accent));
                ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                ui.label("An AI-first native video editor: edit like Premiere or Resolve while Claude edits the same project live through MCP.");
                ui.add_space(4.0);
                ui.label(RichText::new("AI-first video editor.").small().color(pal.text_dim));
            });
            self.about = open;
        }

        // Connect Claude.
        if self.connect {
            let mut open = true;
            egui::Window::new("Connect Claude").open(&mut open).collapsible(false).default_width(600.0).pivot(egui::Align2::CENTER_CENTER).default_pos(center).show(ctx, |ui| {
                ui.label("AIFrames has a built-in MCP server. Claude edits what is open in this window while you watch.");
                ui.add_space(4.0);
                ui.label(RichText::new("1. In this window").strong());
                ui.label("Use the AI tab (right panel): type what you want and press Run. It starts Claude Code headless against this window.");
                ui.add_space(4.0);
                ui.label(RichText::new("2. From your own Claude Code session").strong());
                let line = crate::ai::connect_command(cx.exe);
                copy_line(ui, &line);
                ui.label(RichText::new("`aiframes mcp` talks to this window when it is open and works headlessly otherwise.").small().color(pal.text_dim));
                if let Some(port) = cx.port {
                    ui.add_space(4.0);
                    ui.label(RichText::new("3. Or connect to the running window over HTTP").strong());
                    copy_line(ui, &format!("claude mcp add --transport http aiframes {}", crate::ai::mcp_url(port)));
                    ui.label(RichText::new(format!("Endpoint: {} (this window only; the port can change between runs)", crate::ai::mcp_url(port))).small().color(pal.text_dim));
                } else {
                    ui.label(RichText::new("The MCP server of this window is not running.").color(pal.error));
                }
            });
            self.connect = open;
        }

        // Modal prompts.
        if let Some(m) = self.modal.clone() {
            let mut done = false;
            let title = match &m {
                Modal::SaveChanges { .. } => "Save changes?",
                Modal::Recover { .. } => "Recover autosave?",
                Modal::Speed { .. } => "Clip Speed",
                Modal::Confirm { title, .. } => title.as_str(),
                Modal::Alert { title, .. } => title.as_str(),
            }
            .to_string();
            egui::Modal::new(egui::Id::unique("aiframes-modal"))
                .show(ctx, |ui| {
                    ui.set_min_width(360.0);
                    ui.set_max_width(if matches!(m, Modal::Speed { .. }) { 420.0 } else { 520.0 });
                    ui.label(RichText::new(&title).strong().size(14.0));
                    ui.separator();
                    match &m {
                    Modal::SaveChanges { doc, title, then } => {
                        ui.label(format!("Do you want to save the changes to {title}?"));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            let mut ans = None;
                            if ui.button(RichText::new("Save").strong()).clicked() {
                                ans = Some(Some(true));
                            }
                            if ui.button("Don't Save").clicked() {
                                ans = Some(Some(false));
                            }
                            if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                ans = Some(None);
                            }
                            if let Some(save) = ans {
                                out.push(DialogOut::SaveChanges { doc: *doc, then: then.clone(), save });
                                done = true;
                            }
                        });
                    }
                    Modal::Recover { doc, autosave, title } => {
                        ui.label(format!("{title} has autosaved changes that are newer than the saved file (AIFrames may have closed unexpectedly)."));
                        ui.label(RichText::new(autosave.display().to_string()).small().monospace().color(pal.text_dim));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button(RichText::new("Recover").strong()).clicked() {
                                out.push(DialogOut::Recover { doc: *doc, autosave: autosave.clone(), yes: true });
                                done = true;
                            }
                            if ui.button("Open the saved file").clicked() {
                                out.push(DialogOut::Recover { doc: *doc, autosave: autosave.clone(), yes: false });
                                done = true;
                            }
                        });
                    }
                    Modal::Speed { speed, keep_duration } => {
                        let (mut s, mut k) = (*speed, *keep_duration);
                        ui.horizontal(|ui| {
                            ui.label("Speed");
                            ui.add(egui::DragValue::new(&mut s).range(0.05..=100.0).speed(0.05).suffix("x"));
                            for v in [0.25, 0.5, 1.0, 2.0, 4.0] {
                                if ui.small_button(format!("{v}x")).clicked() {
                                    s = v;
                                }
                            }
                        });
                        ui.checkbox(&mut k, "Keep the clip's length (use more or less of the source)");
                        ui.horizontal(|ui| {
                            if ui.button(RichText::new("OK").strong()).clicked() {
                                out.push(DialogOut::Speed { speed: s, keep_duration: k });
                                done = true;
                            }
                            if ui.button("Cancel").clicked() {
                                done = true;
                            }
                        });
                        if !done {
                            self.modal = Some(Modal::Speed { speed: s, keep_duration: k });
                        }
                    }
                    Modal::Confirm { text, ok, label, ops, .. } => {
                        ui.label(text.as_str());
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button(RichText::new(ok.as_str()).strong()).clicked() {
                                out.push(DialogOut::Apply { label: label.clone(), ops: ops.clone() });
                                done = true;
                            }
                            if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                done = true;
                            }
                        });
                    }
                    Modal::Alert { text, .. } => {
                        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                            ui.label(text);
                        });
                        ui.add_space(6.0);
                        if ui.button("OK").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Escape)) {
                            done = true;
                        }
                    }
                    }
                });
            if done {
                self.modal = None;
            }
        }
        out
    }
}

/// A command shown whole (wrapped, selectable) with a Copy button.
fn copy_line(ui: &mut egui::Ui, line: &str) {
    egui::Frame::NONE.fill(theme::palette().field).corner_radius(3).inner_margin(egui::Margin::symmetric(8, 6)).show(ui, |ui| {
        ui.horizontal_top(|ui| {
            let w = ui.available_width() - 56.0;
            ui.allocate_ui(egui::vec2(w.max(120.0), 0.0), |ui| {
                ui.add(egui::Label::new(RichText::new(line).monospace()).wrap().selectable(true));
            });
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(line.to_string());
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_projects_have_tracks() {
        let p = new_project(1080, 1920, 30.0);
        assert_eq!(p.settings.width, 1080);
        assert_eq!(p.tracks.iter().filter(|t| t.kind == TrackKind::Video).count(), 2);
        assert!(aiframes_core::resolve(&p).errors.is_empty());
        let json = aiframes_core::document::canonical_json(&p);
        assert!(aiframes_core::document::parse_project(&json).is_ok());
    }

    #[test]
    fn settings_form_ops() {
        let p = new_project(1920, 1080, 30.0);
        let mut f = ProjectForm::from(&p);
        assert!(f.ops(&p).is_empty(), "unchanged form = no ops");
        f.name = "Recap".into();
        f.width = 1080;
        f.height = 1920;
        f.fixed = true;
        f.duration = 30.0;
        let ops = f.ops(&p);
        assert_eq!(ops.len(), 2);
        let mut doc = aiframes_core::Document::new(p);
        doc.apply("user", "settings", ops).unwrap();
        assert_eq!(doc.project.name, "Recap");
        assert_eq!((doc.project.settings.width, doc.project.settings.height), (1080, 1920));
        assert_eq!(doc.project.settings.duration, Some(30.0));
        let mut f = ProjectForm::from(&doc.project);
        f.fixed = false;
        let ops = f.ops(&doc.project);
        doc.apply("user", "auto length", ops).unwrap();
        assert_eq!(doc.project.settings.duration, None);
    }
}
