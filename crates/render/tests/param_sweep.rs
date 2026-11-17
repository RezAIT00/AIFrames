//! Every parameter of every effect and transition visibly changes the picture across its slider
//! travel, and not only at the very end of it (the audit behind "they don't seem to be configurable").
//!
//! The always-run test checks this on a generated test card. The ignored `sweep_report` writes the
//! full audit for a real frame: a contact sheet per effect/transition (rows = parameters, columns =
//! slider positions) and `sweep.md`, a table of how far each slider position moves the picture:
//!
//! ```text
//! set AIFRAMES_SWEEP_IMAGE=out\footage\still_a.png
//! set AIFRAMES_SWEEP_IMAGE_B=out\footage\still_b.png
//! set AIFRAMES_SWEEP_OUT=out\sweep
//! cargo test -p aiframes-render --test param_sweep -- --ignored --nocapture sweep_report
//! ```

mod common;
use aiframes_core::effects::{ParamSpec, EFFECTS, TRANSITIONS};
use aiframes_core::{Effect, Param, Project, Transition};
use aiframes_render::{FrameOptions, Image, Renderer};
use common::*;
use serde_json::json;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Slider positions sampled per parameter (0 = left end, 1 = right end of the inspector slider).
const STEPS: usize = 13;

/// `img` full frame (or at 70 % over grey with `pip`, so a drop shadow has room), gliding right so
/// time effects show (`fast` for echo and stutter), with `effects`.
fn fx_scene(img: &Path, effects: Vec<Effect>, pip: bool, fast: bool) -> Project {
    let mut p = project(1920, 1080, 30.0);
    add_media(&mut p, "m1", img);
    let scale = if pip { 0.7 } else { 1.0 };
    let reach = if fast { 2400.0 } else { 600.0 };
    p.clips = vec![
        solid("bg", "v1", 0.0, 4.0, "#5A6066"),
        clip(json!({"id": "c1", "track": "v2", "start": 0, "duration": 4, "source": {"type": "media", "asset": "m1"},
            "transform": {"fit": "stretch", "scale": scale, "x": {"keys": [{"t": 0, "v": -reach}, {"t": 4, "v": reach}]}},
            "effects": effects})),
    ];
    p
}

/// Clip-local time of the effect sweeps: inside a stutter step, not on its boundary.
const FX_T: f64 = 2.07;

/// Two stills cut at 2 s with `tr` (1 s, centred on the cut).
fn tr_scene(a: &Path, b: &Path, tr: Option<Transition>) -> Project {
    let mut p = project(1920, 1080, 30.0);
    add_media(&mut p, "a", a);
    add_media(&mut p, "b", b);
    p.clips = vec![
        clip(json!({"id": "A", "track": "v1", "start": 0, "duration": 2, "source": {"type": "media", "asset": "a"}, "transform": {"fit": "stretch"}})),
        clip(json!({"id": "B", "track": "v1", "start": {"after": "A"}, "duration": 2, "source": {"type": "media", "asset": "b"}, "transform": {"fit": "stretch"}})),
    ];
    p.transitions = tr.into_iter().collect();
    p
}

fn frame(r: &Renderer, p: &Project, t: f64, scale: f32) -> Image {
    let res = resolved(p);
    r.frame(Path::new("."), p, &res, t, &FrameOptions { scale, guides: false, draft: false }).unwrap()
}

/// The value at slider position `f` (0..1), as the inspector maps it.
fn at(ps: &ParamSpec, f: f64) -> f64 {
    aiframes_core::effects::slider_value(ps, f)
}

/// An effect with every parameter at its default except `name = v`, plus what it needs to show at all.
fn effect_with(kind: &str, name: Option<(&str, f64)>, lut: &Path) -> Effect {
    let spec = aiframes_core::effect_spec(kind).unwrap();
    let mut e = spec.instantiate();
    if kind == "lut" {
        e.strings.insert("path".into(), lut.to_string_lossy().into_owned());
    }
    if kind == "chroma_key" {
        // The screen's colour, as a person picks it.
        e.colors.insert("key".into(), aiframes_core::Color::rgba8(0x00, 0xB1, 0x40, 255));
    }
    if let Some((n, v)) = name {
        e.params.insert(n.into(), Param::Value(v));
    }
    e
}

fn transition_with(kind: &str, name: Option<(&str, f64)>) -> Transition {
    let mut t = Transition { id: "tr1".into(), from: "A".into(), to: "B".into(), kind: kind.into(), duration: 1.0, params: Default::default(), colors: Default::default() };
    if let Some((n, v)) = name {
        t.params.insert(n.into(), v);
    }
    t
}

/// A warm grading LUT (17³) so every LUT intensity shows.
fn lut_file(dir: &Path) -> PathBuf {
    let n = 17;
    let mut cube = format!("LUT_3D_SIZE {n}\n");
    for b in 0..n {
        for g in 0..n {
            for r in 0..n {
                let (r, g, b) = (r as f64 / (n - 1) as f64, g as f64 / (n - 1) as f64, b as f64 / (n - 1) as f64);
                let l = 0.3 * r + 0.59 * g + 0.11 * b;
                writeln!(cube, "{:.5} {:.5} {:.5}", (l * 1.1 + 0.08).min(1.0), (l * 0.95 + 0.03).min(1.0), (l * 0.7).min(1.0)).unwrap();
            }
        }
    }
    let p = dir.join("warm.cube");
    std::fs::write(&p, cube).unwrap();
    p
}

/// `subject` in a disc on studio green (#00B140), with a 20 % green cast on the subject (spill).
fn green_screen(subject: &Path) -> PathBuf {
    let name = format!("green_{}.png", subject.file_stem().unwrap().to_string_lossy());
    let subject = subject.to_string_lossy().into_owned();
    fixture(&name, |out| {
        ffmpeg(&[
            "-f", "lavfi", "-i", "color=c=0x00B140:s=1920x1080", "-i", &subject, "-filter_complex",
            "[1]scale=900:900,format=rgba,geq=r='r(X,Y)*0.8':g='g(X,Y)*0.8+35':b='b(X,Y)*0.8+13':a='if(lt(hypot(X-450,Y-450),430),255,0)'[s];[0][s]overlay=510:90",
            "-frames:v", "1", out,
        ])
    })
}

/// How a parameter's slider moves the picture.
struct Sweep {
    what: String,
    spec: ParamSpec,
    /// Mean abs difference (0..255) from the left end, per slider position.
    from_min: Vec<f64>,
    /// Whether each step between neighbouring slider positions changes the picture visibly.
    steps: Vec<bool>,
    /// Whether any slider position looks different from the default.
    differs: bool,
    frames: Vec<Image>,
}

/// A visible change: at least 0.5 % of the pixels move by 3/255 or more in some channel, or a small
/// detail changes a lot (16 pixels by 24/255: rounded corners, a thin shadow). Renders are
/// deterministic, so nothing else moves pixels.
fn visible(a: &Image, b: &Image) -> bool {
    let n = (a.width * a.height) as usize;
    let d = |p: &[u8; 4], q: &[u8; 4]| (0..3).map(|k| (p[k] as i32 - q[k] as i32).abs()).max().unwrap_or(0);
    let pairs = || a.rgba.as_chunks::<4>().0.iter().zip(b.rgba.as_chunks::<4>().0);
    pairs().filter(|(p, q)| d(p, q) >= 3).count() * 200 >= n || pairs().filter(|(p, q)| d(p, q) >= 24).count() >= 16
}

impl Sweep {
    /// Share of the slider's travel over which the picture visibly changes nothing.
    fn dead(&self) -> f64 {
        self.steps.iter().filter(|v| !**v).count() as f64 / self.steps.len() as f64
    }
    /// Problems a person dragging the slider would notice.
    fn flags(&self) -> Vec<String> {
        if !self.differs && !self.steps.iter().any(|v| *v) {
            return vec!["NO VISIBLE EFFECT".to_string()];
        }
        // Seeds and toggles change a pattern or switch: any change counts. Named values (directions,
        // axes, modes) are drop-downs, not sliders.
        if self.spec.name == "seed" || self.spec.unit == "toggle" || self.spec.choices().is_some() {
            return Vec::new();
        }
        // A stutter's hold is the part of each step frozen, counted in whole frames: at 8 steps a
        // second and 30 fps a step is 3.75 frames, so the hold has four distinct results.
        if self.what == "stutter.hold" {
            return Vec::new();
        }
        let dead = self.dead();
        if dead > 0.34 {
            return vec![format!("{:.0} % OF THE SLIDER DOES NOTHING", dead * 100.0)];
        }
        Vec::new()
    }
}

fn sweep_param(what: &str, ps: &ParamSpec, steps: usize, render: &dyn Fn(Option<f64>) -> Image) -> Sweep {
    let base = render(None);
    let frames: Vec<Image> = (0..steps).map(|k| render(Some(at(ps, k as f64 / (steps - 1) as f64)))).collect();
    let from_min = frames.iter().map(|f| diff(f, &frames[0])).collect();
    let steps = frames.windows(2).map(|w| visible(&w[0], &w[1])).collect();
    let differs = frames.iter().any(|f| visible(f, &base));
    Sweep { what: what.into(), spec: *ps, from_min, steps, differs, frames }
}

/// Every effect parameter and every transition parameter, swept on `img` (and `img_b` for transitions).
fn sweep_all(r: &Renderer, img: &Path, img_b: &Path, key: &Path, scale: f32, steps: usize, only: Option<&str>) -> Vec<Sweep> {
    let dir = scratch("sweep");
    let lut = lut_file(&dir);
    let mut out = Vec::new();
    for spec in EFFECTS {
        if only.is_some_and(|o| o != spec.kind) {
            continue;
        }
        for ps in spec.params {
            let src = if spec.kind == "chroma_key" { key } else { img };
            // Time effects act over time: six consecutive frames side by side.
            let timed = matches!(spec.kind, "echo" | "stutter");
            let render = |v: Option<f64>| {
                let p = fx_scene(src, vec![effect_with(spec.kind, v.map(|v| (ps.name, v)), &lut)], spec.kind == "drop_shadow", timed);
                if timed {
                    strip(&(0..6).map(|k| frame(r, &p, 2.0 + k as f64 / 30.0, scale)).collect::<Vec<_>>())
                } else {
                    frame(r, &p, FX_T, scale)
                }
            };
            out.push(sweep_param(&format!("{}.{}", spec.kind, ps.name), ps, steps, &render));
        }
    }
    for spec in TRANSITIONS {
        if only.is_some_and(|o| o != spec.kind) {
            continue;
        }
        for ps in spec.params {
            // Three moments of the transition side by side, so parameters that shape its timing count.
            let render = |v: Option<f64>| {
                let p = tr_scene(img, img_b, Some(transition_with(spec.kind, v.map(|v| (ps.name, v)))));
                strip(&[frame(r, &p, 1.75, scale), frame(r, &p, 2.0, scale), frame(r, &p, 2.25, scale)])
            };
            out.push(sweep_param(&format!("transition {}.{}", spec.kind, ps.name), ps, steps, &render));
        }
    }
    out
}

/// Images side by side.
fn strip(imgs: &[Image]) -> Image {
    let w: u32 = imgs.iter().map(|i| i.width).sum();
    let h = imgs.iter().map(|i| i.height).max().unwrap_or(0);
    let mut out = Image::new(w, h);
    let mut x0 = 0;
    for i in imgs {
        for y in 0..i.height {
            let s = (y * i.width * 4) as usize;
            let d = ((y * w + x0) * 4) as usize;
            out.rgba[d..d + (i.width * 4) as usize].copy_from_slice(&i.rgba[s..s + (i.width * 4) as usize]);
        }
        x0 += i.width;
    }
    out
}

/// Rows of frames (one row per sweep), `cols` frames each.
fn sheet(rows: &[&Sweep], cols: &[usize], cell_w: u32) -> Image {
    let first = &rows[0].frames[0];
    let cell_h = (cell_w as f64 * first.height as f64 / first.width as f64).round() as u32;
    let (gap, label_h) = (4u32, 0u32);
    let w = cols.len() as u32 * (cell_w + gap) + gap;
    let h = rows.len() as u32 * (cell_h + gap + label_h) + gap;
    let mut out = Image::new(w, h);
    for p in out.rgba.chunks_mut(4) {
        p.copy_from_slice(&[24, 24, 28, 255]);
    }
    for (ri, s) in rows.iter().enumerate() {
        for (ci, k) in cols.iter().enumerate() {
            let f = &s.frames[*k];
            let (x0, y0) = (gap + ci as u32 * (cell_w + gap), gap + ri as u32 * (cell_h + gap + label_h));
            for y in 0..cell_h {
                for x in 0..cell_w {
                    let sx = (x as f64 * f.width as f64 / cell_w as f64) as u32;
                    let sy = (y as f64 * f.height as f64 / cell_h as f64) as u32;
                    let q = f.pixel(sx.min(f.width - 1), sy.min(f.height - 1)).unwrap();
                    let d = (((y0 + y) * w + x0 + x) * 4) as usize;
                    out.rgba[d..d + 4].copy_from_slice(&q);
                }
            }
        }
    }
    out
}

#[test]
fn every_parameter_moves_the_picture_across_its_slider() {
    let r = renderer();
    let card = fixture("sweep_card.png", |out| ffmpeg(&["-f", "lavfi", "-i", "testsrc2=s=1920x1080", "-frames:v", "1", out]));
    let card_b = fixture("sweep_card_b.png", |out| ffmpeg(&["-f", "lavfi", "-i", "testsrc2=s=1920x1080,hue=h=150,negate", "-frames:v", "1", out]));
    let key = green_screen(&card);
    let mut bad = Vec::new();
    for s in sweep_all(&r, &card, &card_b, &key, 0.25, 7, None) {
        let flags = s.flags();
        if !flags.is_empty() {
            bad.push(format!("{}: {} (from the left end: {:?})", s.what, flags.join(", "), s.from_min.iter().map(|v| (v * 10.0).round() / 10.0).collect::<Vec<_>>()));
        }
    }
    assert!(bad.is_empty(), "parameters that do not read as configurable:\n{}", bad.join("\n"));
}

/// "0–100, curve 2" / "log" / "linear" for the report.
fn slider_text(p: &ParamSpec) -> String {
    let [lo, hi] = p.slider_range();
    let curve = if p.curve == 0.0 { ", log".to_string() } else if p.curve != 1.0 { format!(", curve {}", p.curve) } else { String::new() };
    format!("{lo}–{hi}{curve}")
}

#[test]
#[ignore = "writes the audit for a real frame; see the module docs"]
fn sweep_report() {
    let r = renderer();
    let img = PathBuf::from(std::env::var("AIFRAMES_SWEEP_IMAGE").expect("AIFRAMES_SWEEP_IMAGE=<png>"));
    let img_b = std::env::var("AIFRAMES_SWEEP_IMAGE_B").map(PathBuf::from).unwrap_or_else(|_| img.clone());
    let out = PathBuf::from(std::env::var("AIFRAMES_SWEEP_OUT").unwrap_or_else(|_| "sweep".into()));
    let only = std::env::var("AIFRAMES_SWEEP_ONLY").ok();
    std::fs::create_dir_all(&out).unwrap();
    let key = green_screen(&img_b);
    let sweeps = sweep_all(&r, &img, &img_b, &key, 0.5, STEPS, only.as_deref());
    let mut md = String::from("| parameter | unit | range | slider | Δ at 0 · ¼ · ½ · ¾ · 1 of the slider (mean from the left end, 0–255) | travel that does nothing | flags |\n|---|---|---|---|---|---|---|\n");
    for s in &sweeps {
        let q = |x: f64| s.from_min[((STEPS - 1) as f64 * x).round() as usize];
        writeln!(
            md,
            "| {} | {} | {}–{} | {} | {:.1} · {:.1} · {:.1} · {:.1} · {:.1} | {:.0} % | {} |",
            s.what,
            s.spec.unit,
            s.spec.min,
            s.spec.max,
            slider_text(&s.spec),
            q(0.0),
            q(0.25),
            q(0.5),
            q(0.75),
            q(1.0),
            s.dead() * 100.0,
            s.flags().join(", ")
        )
        .unwrap();
    }
    std::fs::write(out.join("sweep.md"), &md).unwrap();
    println!("{md}");
    // One contact sheet per effect / transition: rows = parameters, columns = slider 0, ¼, ½, ¾, 1.
    let mut groups: Vec<(String, Vec<&Sweep>)> = Vec::new();
    for s in &sweeps {
        let key = s.what.rsplit_once('.').map(|(k, _)| k.to_string()).unwrap_or_default();
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(s),
            None => groups.push((key, vec![s])),
        }
    }
    let cols = [0, 3, 6, 9, 12];
    for (key, rows) in groups {
        let cell = if key.starts_with("transition") { 420 } else { 240 };
        let img = sheet(&rows, &cols, cell);
        std::fs::write(out.join(format!("{}.png", key.replace(' ', "_"))), aiframes_render::encode_png(&img)).unwrap();
    }
}
