//! Export presets and the export dialog.
//!
//! A preset sets the codec and the output frame size (`ExportSettings::{width, height, fit}`): the
//! renderer draws the frames natively at that size, so "YouTube 4K" from a 1080p project is real 4K
//! and "Shorts" from a landscape project is a centre-cropped 1080×1920 (`cover`).

use crate::theme;
use aiframes_core::{ExportSettings, Fit, Project, Settings};
use eframe::egui::{self, RichText};
use std::path::{Path, PathBuf};

/// The output size a preset asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Size {
    /// The project's frame size.
    Project,
    Exact(u32, u32),
    /// This width, the height from the project's aspect.
    Width(u32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub codec: &'static str,
    pub quality: &'static str,
    pub ext: &'static str,
    pub size: Size,
    /// How the project frame maps onto an output of another aspect ratio.
    pub fit: Fit,
    /// Loudness normalisation makes sense (not for GIF).
    pub audio: bool,
    pub video: bool,
    pub help: &'static str,
}

#[allow(clippy::too_many_arguments)]
const fn preset(id: &'static str, label: &'static str, codec: &'static str, quality: &'static str, ext: &'static str, size: Size, fit: Fit, help: &'static str) -> Preset {
    let audio = !matches!(codec.as_bytes(), b"gif");
    let video = !matches!(codec.as_bytes(), b"wav");
    Preset { id, label, codec, quality, ext, size, fit, audio, video, help }
}

pub static PRESETS: &[Preset] = &[
    preset("youtube_1080", "YouTube 1080p (H.264)", "h264", "high", "mp4", Size::Exact(1920, 1080), Fit::Contain, "1920×1080 H.264 MP4, high quality, -14 LUFS: the safe upload default."),
    preset("youtube_4k", "YouTube 4K (HEVC)", "hevc", "high", "mp4", Size::Exact(3840, 2160), Fit::Contain, "3840×2160 HEVC MP4. Rendered natively at 4K (text and shapes stay sharp; media is upscaled once)."),
    preset("shorts", "Shorts / Reels 1080×1920", "h264", "high", "mp4", Size::Exact(1080, 1920), Fit::Cover, "Vertical 9:16 for YouTube Shorts, Reels and TikTok. A landscape edit is centre-cropped (cover)."),
    preset("square", "Square 1080×1080", "h264", "high", "mp4", Size::Exact(1080, 1080), Fit::Cover, "1:1 for feeds. A landscape edit is centre-cropped (cover)."),
    preset("hd720", "HD 720p (H.264)", "h264", "standard", "mp4", Size::Exact(1280, 720), Fit::Contain, "1280×720 H.264: small files for reviews and drafts."),
    preset("prores", "ProRes master", "prores", "high", "mov", Size::Project, Fit::Contain, "ProRes 422 HQ MOV at the project size, for archiving or finishing elsewhere."),
    preset("gif", "Animated GIF", "gif", "standard", "gif", Size::Width(640), Fit::Contain, "640 px wide, palette-optimised GIF (no sound)."),
    preset("wav", "Audio only (WAV)", "wav", "high", "wav", Size::Project, Fit::Contain, "The final mix as a WAV at the project sample rate."),
];

pub fn find(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// Output frame size of a preset for a project.
pub fn preset_size(p: &Preset, s: &Settings) -> (u32, u32) {
    match p.size {
        Size::Project => (s.width, s.height),
        Size::Exact(w, h) => (w, h),
        Size::Width(w) => ExportSettings { width: Some(w), height: None, ..ExportSettings::default() }.output_size(s),
    }
}

/// True when `w×h` has another aspect ratio than the project (so the fit matters).
pub fn aspect_differs(w: u32, h: u32, s: &Settings) -> bool {
    let a = w as f64 * s.height.max(1) as f64;
    let b = h as f64 * s.width.max(1) as f64;
    (a - b).abs() > 0.01 * a.max(b)
}

pub fn fit_label(f: Fit) -> &'static str {
    match f {
        Fit::Contain => "contain",
        Fit::Cover => "cover",
        Fit::Stretch => "stretch",
        Fit::None => "none",
    }
}

fn fit_help(f: Fit) -> &'static str {
    match f {
        Fit::Contain => "The whole frame, with bars in the background colour",
        Fit::Cover => "Fill the output, cropping the overflow (centred)",
        Fit::Stretch => "Distort to fill the output",
        Fit::None => "1:1 pixels, centred",
    }
}

/// "1920×1080", or "1080×1920 (cover)" when the aspect differs from the project.
pub fn output_label(w: u32, h: u32, fit: Fit, s: &Settings) -> String {
    if aspect_differs(w, h, s) {
        format!("{w}×{h} ({})", fit_label(fit))
    } else {
        format!("{w}×{h}")
    }
}

/// Export settings for a preset with an output size and fit. The size is left unset when it is the
/// project's own, so the project file stays clean.
#[allow(clippy::too_many_arguments)]
pub fn settings_for(p: &Preset, range: Option<[f64; 2]>, loudness: Option<f64>, hardware: bool, out: &Path, size: (u32, u32), fit: Fit, s: &Settings) -> ExportSettings {
    let native = size == (s.width, s.height) || !p.video;
    ExportSettings {
        path: Some(out.to_string_lossy().replace('\\', "/")),
        codec: p.codec.to_string(),
        quality: p.quality.to_string(),
        hardware: hardware && p.video && p.codec != "gif" && p.codec != "prores",
        loudness_lufs: if p.audio { loudness } else { None },
        range,
        width: (!native).then_some(size.0),
        height: (!native).then_some(size.1),
        fit: if native { Fit::Contain } else { fit },
        ..ExportSettings::default()
    }
}

/// `<project folder>/exports/<name>.<ext>` (or the fallback folder for unsaved projects).
pub fn default_output(project_path: Option<&Path>, name: &str, p: &Preset, fallback_dir: &Path) -> PathBuf {
    let dir = project_path.and_then(|f| f.parent()).map(|d| d.join("exports")).unwrap_or_else(|| fallback_dir.to_path_buf());
    let stem = if name.trim().is_empty() { "export".to_string() } else { sanitize(name) };
    let suffix = match p.id {
        "shorts" => "-shorts",
        "square" => "-square",
        "youtube_4k" => "-4k",
        "hd720" => "-720p",
        "prores" => "-master",
        _ => "",
    };
    dir.join(format!("{stem}{suffix}.{}", p.ext))
}

fn sanitize(name: &str) -> String {
    let stem = Path::new(name).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| name.to_string());
    stem.chars().map(|c| if r#"<>:"/\|?*"#.contains(c) || c.is_control() { '_' } else { c }).collect()
}

/// The preset that best matches a project's saved export defaults (codec + size).
pub fn preset_for_export(e: &ExportSettings, s: &Settings) -> &'static Preset {
    let size = e.output_size(s);
    PRESETS
        .iter()
        .find(|p| p.codec == e.codec && preset_size(p, s) == size)
        .or_else(|| PRESETS.iter().find(|p| p.codec == e.codec))
        .unwrap_or(&PRESETS[0])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RangeChoice {
    All,
    InOut,
}

pub struct ExportDialog {
    pub open: bool,
    pub preset: &'static str,
    pub range: RangeChoice,
    pub loudness_on: bool,
    pub lufs: f64,
    pub hardware: bool,
    pub out: String,
    /// The output was picked by hand (do not overwrite it when the preset changes).
    pub custom_out: bool,
    pub size: (u32, u32),
    pub fit: Fit,
}

impl Default for ExportDialog {
    fn default() -> Self {
        ExportDialog {
            open: false,
            preset: "youtube_1080",
            range: RangeChoice::All,
            loudness_on: true,
            lufs: -14.0,
            hardware: true,
            out: String::new(),
            custom_out: false,
            size: (1920, 1080),
            fit: Fit::Contain,
        }
    }
}

/// What the dialog asks the app to do.
pub enum ExportRequest {
    Start { settings: ExportSettings, out: PathBuf },
}

pub struct ExportCx<'a> {
    pub settings: &'a Settings,
    pub project_path: Option<&'a Path>,
    pub name: &'a str,
    pub in_out: Option<[f64; 2]>,
    pub duration: f64,
    pub fallback_dir: &'a Path,
}

impl ExportDialog {
    /// Opens the dialog starting from the project's export defaults (the inspector's export
    /// presets write them with `set_export`/`set_export_size`).
    pub fn open_for(&mut self, p: &Project, has_range: bool) {
        let e = &p.export;
        let pr = preset_for_export(e, &p.settings);
        self.preset = pr.id;
        self.size = if e.width.is_some() || e.height.is_some() { e.output_size(&p.settings) } else { preset_size(pr, &p.settings) };
        self.fit = if e.width.is_some() || e.height.is_some() { e.fit } else { pr.fit };
        self.loudness_on = e.loudness_lufs.is_some();
        if let Some(l) = e.loudness_lufs {
            self.lufs = l;
        }
        self.hardware = e.hardware;
        self.range = if has_range { RangeChoice::InOut } else { RangeChoice::All };
        self.custom_out = false;
        self.out.clear();
        self.open = true;
    }

    pub fn show(&mut self, ctx: &egui::Context, cx: &ExportCx) -> Option<ExportRequest> {
        if !self.open {
            return None;
        }
        let pal = theme::palette();
        let mut req = None;
        let mut open = self.open;
        let p = find(self.preset).unwrap_or(&PRESETS[0]);
        if self.out.is_empty() || !self.custom_out {
            self.out = default_output(cx.project_path, cx.name, p, cx.fallback_dir).to_string_lossy().to_string();
        }
        egui::Window::new("Export").open(&mut open).collapsible(false).resizable(false).default_width(480.0).pivot(egui::Align2::CENTER_CENTER).default_pos(ctx.content_rect().center()).show(ctx, |ui| {
            egui::Grid::new("export-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Preset");
                let before = self.preset;
                egui::ComboBox::from_id_salt("export-preset").width(280.0).selected_text(p.label).show_ui(ui, |ui| {
                    for pr in PRESETS {
                        ui.selectable_value(&mut self.preset, pr.id, pr.label).on_hover_text(pr.help);
                    }
                });
                if before != self.preset {
                    if let Some(np) = find(self.preset) {
                        self.size = preset_size(np, cx.settings);
                        self.fit = np.fit;
                    }
                    if !self.custom_out {
                        self.out.clear();
                    }
                }
                ui.end_row();
                ui.label("");
                ui.add(egui::Label::new(RichText::new(p.help).small().color(pal.text_dim)).wrap());
                ui.end_row();
                if p.video {
                    ui.label("Output");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut self.size.0).range(16..=8192).speed(2).suffix(" px"));
                        ui.label("×");
                        ui.add(egui::DragValue::new(&mut self.size.1).range(16..=8192).speed(2).suffix(" px"));
                        let native = self.size == (cx.settings.width, cx.settings.height);
                        if ui.add_enabled(!native, egui::Button::new("Project size").small()).on_hover_text(format!("{}×{}", cx.settings.width, cx.settings.height)).clicked() {
                            self.size = (cx.settings.width, cx.settings.height);
                        }
                    });
                    ui.end_row();
                    if aspect_differs(self.size.0, self.size.1, cx.settings) {
                        ui.label("Fit");
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt("export-fit").width(90.0).selected_text(fit_label(self.fit)).show_ui(ui, |ui| {
                                for f in [Fit::Contain, Fit::Cover, Fit::Stretch, Fit::None] {
                                    ui.selectable_value(&mut self.fit, f, fit_label(f)).on_hover_text(fit_help(f));
                                }
                            });
                            ui.label(RichText::new(fit_help(self.fit)).small().color(pal.text_dim));
                        });
                        ui.end_row();
                    }
                    ui.label("");
                    let native = self.size == (cx.settings.width, cx.settings.height);
                    let text = if native {
                        format!("Output: {} (the project size)", output_label(self.size.0, self.size.1, self.fit, cx.settings))
                    } else {
                        format!("Output: {}  ·  project {}×{}", output_label(self.size.0, self.size.1, self.fit, cx.settings), cx.settings.width, cx.settings.height)
                    };
                    ui.label(RichText::new(text).small().color(pal.accent));
                    ui.end_row();
                }
                ui.label("Range");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.range, RangeChoice::All, format!("Whole timeline ({:.1}s)", cx.duration));
                    let label = match cx.in_out {
                        Some([a, b]) => format!("In to out ({:.1}s)", b - a),
                        None => "In to out (set I/O first)".to_string(),
                    };
                    ui.add_enabled_ui(cx.in_out.is_some(), |ui| ui.radio_value(&mut self.range, RangeChoice::InOut, label));
                });
                ui.end_row();
                ui.label("Loudness");
                ui.add_enabled_ui(p.audio, |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.loudness_on, "Normalize to");
                        ui.add_enabled(self.loudness_on, egui::DragValue::new(&mut self.lufs).range(-40.0..=-5.0).speed(0.5).suffix(" LUFS"));
                        if ui.small_button("-14").on_hover_text("YouTube / Spotify").clicked() {
                            self.lufs = -14.0;
                        }
                        if ui.small_button("-16").on_hover_text("Apple / podcasts").clicked() {
                            self.lufs = -16.0;
                        }
                    });
                });
                ui.end_row();
                if p.video {
                    ui.label("Encoder");
                    ui.add_enabled(p.codec != "gif" && p.codec != "prores", egui::Checkbox::new(&mut self.hardware, "Hardware encoding (NVENC) when available"));
                    ui.end_row();
                }
                ui.label("File");
                ui.horizontal(|ui| {
                    // The file name is editable here; the folder is shown below (paths are long).
                    let path = PathBuf::from(&self.out);
                    let mut name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if ui.add(egui::TextEdit::singleline(&mut name).desired_width(260.0)).changed() {
                        self.out = path.with_file_name(name.trim()).to_string_lossy().to_string();
                        self.custom_out = true;
                    }
                    if ui.button("…").on_hover_text("Choose the output file").clicked() {
                        let path = PathBuf::from(&self.out);
                        let mut dlg = rfd::FileDialog::new().set_title("Export to").add_filter(p.label, &[p.ext]);
                        if let Some(d) = path.parent().filter(|d| d.is_dir()) {
                            dlg = dlg.set_directory(d);
                        }
                        if let Some(n) = path.file_name() {
                            dlg = dlg.set_file_name(n.to_string_lossy());
                        }
                        if let Some(f) = dlg.save_file() {
                            self.out = f.to_string_lossy().to_string();
                            self.custom_out = true;
                        }
                    }
                });
                ui.end_row();
                ui.label("");
                let folder = PathBuf::from(&self.out).parent().map(|d| d.display().to_string()).unwrap_or_default();
                ui.label(RichText::new(format!("in {}", tail(&folder, 64))).small().color(pal.text_dim)).on_hover_text(folder);
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                let out = PathBuf::from(self.out.trim());
                let ok = !self.out.trim().is_empty() && cx.duration > 0.0;
                if ui.add_enabled(ok, egui::Button::new(RichText::new("Export").strong())).clicked() {
                    let range = match (&self.range, cx.in_out) {
                        (RangeChoice::InOut, Some(r)) => Some(r),
                        _ => None,
                    };
                    let out = with_ext(&out, p.ext);
                    let settings = settings_for(p, range, self.loudness_on.then_some(self.lufs), self.hardware, &out, self.size, self.fit, cx.settings);
                    req = Some(ExportRequest::Start { settings, out });
                }
                if ui.button("Cancel").clicked() {
                    self.open = false;
                }
                if cx.duration <= 0.0 {
                    ui.label(RichText::new("The timeline is empty.").small().color(pal.warn));
                }
            });
        });
        if !open {
            self.open = false;
        }
        if req.is_some() {
            self.open = false;
        }
        req
    }
}

/// The end of a long path ("…\\app\\exports").
pub fn tail(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        s.to_string()
    } else {
        format!("…{}", s.chars().skip(count - n + 1).collect::<String>())
    }
}

/// Forces the preset's extension.
pub fn with_ext(p: &Path, ext: &str) -> PathBuf {
    match p.extension() {
        Some(e) if e.to_string_lossy().eq_ignore_ascii_case(ext) => p.to_path_buf(),
        _ => p.with_extension(ext),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hd() -> Settings {
        Settings::default()
    }

    #[test]
    fn presets_are_complete() {
        for id in ["youtube_1080", "youtube_4k", "shorts", "square", "hd720", "prores", "gif", "wav"] {
            assert!(find(id).is_some(), "{id}");
        }
        for p in PRESETS {
            assert!(["h264", "hevc", "av1", "prores", "gif", "png_sequence", "wav"].contains(&p.codec), "{} is not a model codec", p.codec);
        }
        assert!(!find("gif").unwrap().audio && find("gif").unwrap().video);
        assert!(!find("wav").unwrap().video);
    }

    #[test]
    fn sizes() {
        let s = hd();
        assert_eq!(preset_size(find("youtube_4k").unwrap(), &s), (3840, 2160));
        assert_eq!(preset_size(find("shorts").unwrap(), &s), (1080, 1920));
        assert_eq!(preset_size(find("prores").unwrap(), &s), (1920, 1080));
        assert_eq!(preset_size(find("gif").unwrap(), &s), (640, 360));
        assert!(!aspect_differs(3840, 2160, &s));
        assert!(aspect_differs(1080, 1920, &s));
        assert!(aspect_differs(1080, 1080, &s));
        assert_eq!(output_label(1080, 1920, Fit::Cover, &s), "1080×1920 (cover)");
        assert_eq!(output_label(3840, 2160, Fit::Contain, &s), "3840×2160");
    }

    #[test]
    fn settings() {
        let s = hd();
        let p = find("youtube_1080").unwrap();
        let e = settings_for(p, Some([1.0, 3.0]), Some(-14.0), true, Path::new("C:\\out\\a.mp4"), (1920, 1080), Fit::Contain, &s);
        assert_eq!((e.codec.as_str(), e.quality.as_str()), ("h264", "high"));
        assert!(e.hardware);
        assert_eq!(e.loudness_lufs, Some(-14.0));
        assert_eq!(e.range, Some([1.0, 3.0]));
        assert_eq!(e.path.as_deref(), Some("C:/out/a.mp4"));
        assert_eq!((e.width, e.height), (None, None), "the project size is left implicit");
        let k = settings_for(find("youtube_4k").unwrap(), None, None, true, Path::new("a.mp4"), (3840, 2160), Fit::Contain, &s);
        assert_eq!(k.output_size(&s), (3840, 2160));
        let sh = settings_for(find("shorts").unwrap(), None, None, true, Path::new("a.mp4"), (1080, 1920), Fit::Cover, &s);
        assert_eq!((sh.width, sh.height, sh.fit), (Some(1080), Some(1920), Fit::Cover));
        let gif = settings_for(find("gif").unwrap(), None, Some(-14.0), true, Path::new("a.gif"), (640, 360), Fit::Contain, &s);
        assert_eq!(gif.loudness_lufs, None, "no loudness for GIF");
        assert!(!gif.hardware);
        let wav = settings_for(find("wav").unwrap(), None, None, true, Path::new("a.wav"), (1920, 1080), Fit::Contain, &s);
        assert_eq!((wav.codec.as_str(), wav.width), ("wav", None));
        assert!(!wav.hardware);
    }

    #[test]
    fn defaults_from_the_project() {
        let mut p = Project::default();
        p.export.codec = "h264".into();
        p.export.width = Some(1080);
        p.export.height = Some(1920);
        p.export.fit = Fit::Cover;
        assert_eq!(preset_for_export(&p.export, &p.settings).id, "shorts");
        let mut d = ExportDialog::default();
        d.open_for(&p, false);
        assert!(d.open);
        assert_eq!((d.size, d.fit, d.preset), ((1080, 1920), Fit::Cover, "shorts"));
        let p2 = Project::default();
        d.open_for(&p2, true);
        assert_eq!((d.size, d.preset, d.range.clone()), ((1920, 1080), "youtube_1080", RangeChoice::InOut));
    }

    #[test]
    fn outputs() {
        let p = find("shorts").unwrap();
        let o = default_output(Some(Path::new("C:/v/My: edit.aifr")), "My: edit.aifr", p, Path::new("C:/tmp"));
        assert_eq!(o, PathBuf::from("C:/v/exports/My_ edit-shorts.mp4"));
        let o = default_output(None, "", find("wav").unwrap(), Path::new("C:/tmp"));
        assert_eq!(o, PathBuf::from("C:/tmp/export.wav"));
        assert_eq!(with_ext(Path::new("a.MP4"), "mp4"), PathBuf::from("a.MP4"));
        assert_eq!(with_ext(Path::new("a.mov"), "mp4"), PathBuf::from("a.mp4"));
        assert_eq!(tail("abcdef", 10), "abcdef");
        assert_eq!(tail("abcdefghij", 5), "…ghij");
    }
}
