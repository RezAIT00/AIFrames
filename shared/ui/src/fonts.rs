//! System font loading. Calibri + Segoe UI are loaded at startup; other fonts used by a
//! document (Arial, Times New Roman, Consolas, ...) are loaded lazily the first time they appear.

use egui::{FontData, FontDefinitions, FontFamily};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// (display name, [regular, bold, italic, bold-italic] file names in C:\Windows\Fonts)
const KNOWN: &[(&str, [&str; 4])] = &[
    ("Calibri", ["calibri.ttf", "calibrib.ttf", "calibrii.ttf", "calibriz.ttf"]),
    ("Calibri Light", ["calibril.ttf", "calibrib.ttf", "calibrili.ttf", "calibriz.ttf"]),
    ("Arial", ["arial.ttf", "arialbd.ttf", "ariali.ttf", "arialbi.ttf"]),
    ("Arial Narrow", ["ARIALN.TTF", "ARIALNB.TTF", "ARIALNI.TTF", "ARIALNBI.TTF"]),
    ("Times New Roman", ["times.ttf", "timesbd.ttf", "timesi.ttf", "timesbi.ttf"]),
    ("Consolas", ["consola.ttf", "consolab.ttf", "consolai.ttf", "consolaz.ttf"]),
    ("Courier New", ["cour.ttf", "courbd.ttf", "couri.ttf", "courbi.ttf"]),
    ("Cambria", ["cambria.ttc", "cambriab.ttf", "cambriai.ttf", "cambriaz.ttf"]),
    ("Verdana", ["verdana.ttf", "verdanab.ttf", "verdanai.ttf", "verdanaz.ttf"]),
    ("Tahoma", ["tahoma.ttf", "tahomabd.ttf", "tahoma.ttf", "tahomabd.ttf"]),
    ("Georgia", ["georgia.ttf", "georgiab.ttf", "georgiai.ttf", "georgiaz.ttf"]),
    ("Segoe UI", ["segoeui.ttf", "segoeuib.ttf", "segoeuii.ttf", "segoeuiz.ttf"]),
];

/// Font names offered in the font dropdown.
pub fn font_names() -> Vec<&'static str> {
    KNOWN.iter().map(|(n, _)| *n).collect()
}

fn fonts_dir() -> std::path::PathBuf {
    let windir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| "C:\\Windows".into());
    windir.join("Fonts")
}

fn variant_index(bold: bool, italic: bool) -> usize {
    match (bold, italic) {
        (false, false) => 0,
        (true, false) => 1,
        (false, true) => 2,
        (true, true) => 3,
    }
}

const SUFFIX: [&str; 4] = ["", " Bold", " Italic", " Bold Italic"];

pub struct FontManager {
    defs: FontDefinitions,
    /// Canonical font names that are loaded (all four variants attempted).
    loaded: HashSet<String>,
    /// Names requested this frame that are known but not loaded yet.
    wanted: HashSet<String>,
    /// Lower-case lookup of known names.
    known: HashMap<String, usize>,
    /// The base fallback family name (Calibri if present, else egui default).
    base: Option<String>,
    failed: HashSet<String>,
}

impl FontManager {
    pub fn new(ctx: &egui::Context) -> Self {
        let mut fm = FontManager {
            defs: FontDefinitions::default(),
            loaded: HashSet::new(),
            wanted: HashSet::new(),
            known: KNOWN.iter().enumerate().map(|(i, (n, _))| (n.to_lowercase(), i)).collect(),
            base: None,
            failed: HashSet::new(),
        };
        fm.load_font("Calibri");
        if fm.loaded.contains("Calibri") {
            fm.base = Some("Calibri".to_string());
        }
        fm.load_font("Segoe UI");
        if let Ok(bytes) = std::fs::read(fonts_dir().join("seguisym.ttf")) {
            fm.defs.font_data.insert("Segoe UI Symbol".into(), Arc::new(FontData::from_owned(bytes)));
            for (key, fam) in fm.defs.families.iter_mut() {
                if !fam.iter().any(|f| f == "Segoe UI Symbol") {
                    let pos = if *key == FontFamily::Monospace { 1.min(fam.len()) } else { fam.iter().position(|f| !f.starts_with("Calibri") && !f.starts_with("Segoe UI")).unwrap_or(fam.len()) };
                    fam.insert(pos, "Segoe UI Symbol".into());
                }
            }
        }
        // UI text uses Segoe UI when available.
        if fm.loaded.contains("Segoe UI") {
            let prop = fm.defs.families.entry(FontFamily::Proportional).or_default();
            prop.insert(0, "Segoe UI".to_string());
        }
        ctx.set_fonts(fm.defs.clone());
        fm
    }

    fn load_font(&mut self, name: &str) {
        let Some(&idx) = self.known.get(&name.to_lowercase()) else { return };
        let (canon, files) = KNOWN[idx];
        if self.loaded.contains(canon) || self.failed.contains(canon) {
            return;
        }
        let dir = fonts_dir();
        let mut any = false;
        for (v, file) in files.iter().enumerate() {
            let key = format!("{canon}{}", SUFFIX[v]);
            if self.defs.font_data.contains_key(&key) {
                any = true;
                continue;
            }
            if let Ok(bytes) = std::fs::read(dir.join(file)) {
                self.defs.font_data.insert(key.clone(), Arc::new(FontData::from_owned(bytes)));
                any = true;
            }
        }
        if !any {
            self.failed.insert(canon.to_string());
            return;
        }
        let default_prop = FontDefinitions::default().families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
        for v in 0..4 {
            let key = format!("{canon}{}", SUFFIX[v]);
            let mut chain = Vec::new();
            if self.defs.font_data.contains_key(&key) {
                chain.push(key.clone());
            } else if self.defs.font_data.contains_key(canon) {
                chain.push(canon.to_string());
            }
            // Fallbacks: Calibri of the same variant, then egui defaults (symbols, emoji).
            let cal = format!("Calibri{}", SUFFIX[v]);
            if cal != key && self.defs.font_data.contains_key(&cal) {
                chain.push(cal);
            }
            if self.defs.font_data.contains_key("Segoe UI Symbol") {
                chain.push("Segoe UI Symbol".into());
            }
            chain.extend(default_prop.iter().cloned());
            self.defs.families.insert(FontFamily::Name(key.clone().into()), chain);
        }
        self.loaded.insert(canon.to_string());
    }

    /// Font family to use for a cell font. Unknown fonts map to Calibri.
    pub fn family(&mut self, name: &str, bold: bool, italic: bool) -> FontFamily {
        let v = variant_index(bold, italic);
        let canon = match self.known.get(&name.to_lowercase()) {
            Some(&i) => KNOWN[i].0,
            None => "Calibri",
        };
        if self.loaded.contains(canon) {
            return FontFamily::Name(format!("{canon}{}", SUFFIX[v]).into());
        }
        if !self.failed.contains(canon) {
            self.wanted.insert(canon.to_string());
        }
        match &self.base {
            Some(b) => FontFamily::Name(format!("{b}{}", SUFFIX[v]).into()),
            None => FontFamily::Proportional,
        }
    }

    /// Bold UI font family.
    #[allow(dead_code)]
    pub fn ui_bold(&mut self) -> FontFamily {
        if self.loaded.contains("Segoe UI") {
            FontFamily::Name("Segoe UI Bold".into())
        } else {
            FontFamily::Proportional
        }
    }

    /// Load fonts requested during the frame. Call once per frame.
    pub fn end_frame(&mut self, ctx: &egui::Context) {
        if self.wanted.is_empty() {
            return;
        }
        let wanted: Vec<String> = self.wanted.drain().collect();
        for w in wanted {
            self.load_font(&w);
        }
        ctx.set_fonts(self.defs.clone());
        ctx.request_repaint();
    }
}
