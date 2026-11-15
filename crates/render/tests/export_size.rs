//! Export output size: 4K from 1080p renders natively (crisp), cover centre-crops, contain bars are
//! the background colour, stretch distorts, none is 1:1, odd sizes follow `ExportSettings::output_size`.

mod common;
use aiframes_core::{ExportSettings, Fit, Project};
use aiframes_render::{export, FrameOptions, Image};
use common::*;
use serde_json::json;
use std::path::{Path, PathBuf};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

/// Exports the first frame as a PNG sequence (lossless) and reads it back.
fn export_frame(p: &Project, s: ExportSettings, name: &str) -> (Image, aiframes_render::ExportReport) {
    let dir = scratch(name);
    let s = ExportSettings { codec: "png_sequence".into(), range: Some([0.0, 1.0 / 30.0]), loudness_lufs: None, ..s };
    let rep = export(Path::new("."), p, pool(), &s, &dir, &mut |_| true).unwrap();
    (read_png(&dir.join("frame_000000.png")), rep)
}

fn read_png(path: &PathBuf) -> Image {
    let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
    let mut r = dec.read_info().unwrap();
    let mut buf = vec![0u8; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut buf).unwrap();
    let (w, h) = (info.width, info.height);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..(w * h * 4) as usize].to_vec(),
        png::ColorType::Rgb => buf[..(w * h * 3) as usize].as_chunks::<3>().0.iter().flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        other => panic!("unexpected PNG colour type {other:?}"),
    };
    Image { width: w, height: h, rgba }
}

/// Mean squared horizontal gradient over pixels where the text is (edge sharpness).
fn edge_energy(img: &Image) -> f64 {
    let (mut s, mut n) = (0.0, 0usize);
    for y in 0..img.height {
        for x in 1..img.width {
            let a = px(img, x - 1, y)[0] as f64;
            let b = px(img, x, y)[0] as f64;
            if a.max(b) > 20.0 {
                s += (a - b).powi(2);
                n += 1;
            }
        }
    }
    s / n.max(1) as f64
}

fn upscale2(img: &Image) -> Image {
    let (w, h) = (img.width * 2, img.height * 2);
    let mut out = Image::new(w, h);
    for y in 0..h {
        for x in 0..w {
            // Bilinear (pixel centres aligned).
            let fx = ((x as f64 + 0.5) / 2.0 - 0.5).max(0.0);
            let fy = ((y as f64 + 0.5) / 2.0 - 0.5).max(0.0);
            let (x0, y0) = (fx.floor() as u32, fy.floor() as u32);
            let (x1, y1) = ((x0 + 1).min(img.width - 1), (y0 + 1).min(img.height - 1));
            let (tx, ty) = (fx - x0 as f64, fy - y0 as f64);
            for k in 0..4 {
                let g = |xx, yy| px(img, xx, yy)[k] as f64;
                let a = g(x0, y0) + (g(x1, y0) - g(x0, y0)) * tx;
                let b = g(x0, y1) + (g(x1, y1) - g(x0, y1)) * tx;
                out.rgba[((y * w + x) * 4 + k as u32) as usize] = (a + (b - a) * ty).round() as u8;
            }
        }
    }
    out
}

#[test]
fn uhd_from_1080p_renders_natively() {
    let _g = serial();
    let r = renderer();
    let mut p = project(1920, 1080, 30.0);
    add_media(&mut p, "m1", &halves_png());
    p.clips = vec![
        clip(json!({"id": "img", "track": "v1", "start": 0, "duration": 2, "source": {"type": "media", "asset": "m1"}, "transform": {"scale": 0.3, "x": -600}})),
        clip(json!({"id": "t", "track": "v2", "start": 0, "duration": 2, "source": {"type": "text", "text": "Crisp edges", "size": 64}})),
    ];
    let (img, rep) = export_frame(&p, ExportSettings { width: Some(3840), ..ExportSettings::default() }, "uhd");
    assert_eq!((rep.width, rep.height), (3840, 2160), "height derived from the project's aspect");
    assert_eq!((img.width, img.height), (3840, 2160));
    // Identical to rendering the project at scale 2 (text rasterised at 4K, media upscaled once).
    let res = resolved(&p);
    let native = r.frame(Path::new("."), &p, &res, 0.0, &FrameOptions { scale: 2.0, ..FrameOptions::default() }).unwrap();
    assert!(diff(&img, &native) < 0.05, "export frame matches a native 4K render ({})", diff(&img, &native));
    // Crisper than a 1080p frame scaled up.
    let up = upscale2(&r.frame(Path::new("."), &p, &res, 0.0, &FrameOptions::default()).unwrap());
    let (crisp, soft) = (edge_energy(&img), edge_energy(&up));
    assert!(crisp > soft * 1.3, "text edges: native {crisp:.0} vs upscaled {soft:.0}");
}

#[test]
fn cover_centre_crops_and_contain_letterboxes() {
    let _g = serial();
    let mut p = project(1920, 1080, 30.0);
    p.settings.background = aiframes_core::Color::rgba8(0x20, 0x30, 0x40, 255);
    // Thirds: red | green | blue, and a white square in the middle.
    let third = |id: &str, color: &str, x: f64| {
        let mut c = solid(id, "v1", 0.0, 2.0, color);
        c.transform.crop.right = (2.0 / 3.0).into();
        c.transform.x = x.into();
        c
    };
    let mut dot = solid("dot", "v2", 0.0, 2.0, "#FFFFFF");
    dot.transform.scale = 0.05.into();
    p.tracks.push(track(json!({"id": "v4", "kind": "video"})));
    let mut g = third("g", "#00FF00", 0.0);
    g.track = "v2".into();
    dot.track = "v3".into();
    p.clips = vec![third("r", "#FF0000", -640.0), g, third("b", "#0000FF", 640.0), dot];
    let vertical = ExportSettings { width: Some(1080), height: Some(1920), fit: Fit::Cover, ..ExportSettings::default() };
    let (img, rep) = export_frame(&p, vertical, "cover");
    assert_eq!((rep.width, rep.height), (1080, 1920));
    assert_eq!((img.width, img.height), (1080, 1920));
    // The visible window is project x ≈ 656..1264: green with the white dot centred.
    assert!(near(px(&img, 540, 960), [255, 255, 255, 255], 2));
    for (x, y) in [(5, 960), (1074, 960), (540, 5), (540, 1914), (5, 5)] {
        assert!(near(px(&img, x, y), [0, 255, 0, 255], 2), "({x},{y}) = {:?}", px(&img, x, y));
    }
    // Contain: the frame is 1080x608 in the middle; bars are exactly the background.
    let contain = ExportSettings { width: Some(1080), height: Some(1920), fit: Fit::Contain, ..ExportSettings::default() };
    let (img, _) = export_frame(&p, contain, "contain");
    let bg = [0x20, 0x30, 0x40, 255];
    for (x, y) in [(540, 0), (540, 655), (0, 300), (1079, 1919), (540, 1264), (540, 1500)] {
        assert_eq!(px(&img, x, y), bg, "bar at ({x},{y})");
    }
    assert_eq!(px(&img, 100, 960), [255, 0, 0, 255]);
    assert_eq!(px(&img, 1000, 960), [0, 0, 255, 255]);
    assert_eq!(px(&img, 540, 656), [0, 255, 0, 255]);
    assert_eq!(px(&img, 540, 1263), [0, 255, 0, 255]);
    // Content moved off the project frame does not leak into the bars.
    let mut q = p.clone();
    q.clips[3].transform.y = (-600.0).into(); // dot above the frame
    let (img, _) = export_frame(&q, ExportSettings { width: Some(1080), height: Some(1920), ..ExportSettings::default() }, "contain-leak");
    assert!(img.rgba.as_chunks::<4>().0.iter().all(|q| *q != [255, 255, 255, 255]), "off-frame content stays hidden");
}

#[test]
fn stretch_none_and_odd_sizes() {
    let _g = serial();
    let mut p = project(1920, 1080, 30.0);
    let mut left = solid("l", "v1", 0.0, 2.0, "#FF0000");
    left.transform.crop.right = 0.5.into();
    left.transform.x = (-480.0).into();
    let mut right = solid("r", "v2", 0.0, 2.0, "#0000FF");
    right.transform.crop.left = 0.5.into();
    right.transform.x = 480.0.into();
    p.clips = vec![left, right];
    let (img, rep) = export_frame(&p, ExportSettings { width: Some(1000), height: Some(1000), fit: Fit::Stretch, ..ExportSettings::default() }, "stretch");
    assert_eq!((rep.width, rep.height), (1000, 1000));
    assert_eq!(px(&img, 2, 2), [255, 0, 0, 255]);
    assert_eq!(px(&img, 499, 998), [255, 0, 0, 255]);
    assert_eq!(px(&img, 500, 1), [0, 0, 255, 255]);
    // None: 1:1 pixels centred — a 960x540 output shows project x 480..1440.
    let (img, _) = export_frame(&p, ExportSettings { width: Some(960), height: Some(540), fit: Fit::None, ..ExportSettings::default() }, "none");
    assert_eq!(px(&img, 479, 270), [255, 0, 0, 255]);
    assert_eq!(px(&img, 480, 270), [0, 0, 255, 255]);
    // One side given: the other follows the project aspect, rounded to even (core's rule).
    let s = ExportSettings { width: Some(1001), ..ExportSettings::default() };
    assert_eq!(s.output_size(&p.settings), (1001, 564));
    let (img, rep) = export_frame(&p, s, "odd");
    assert_eq!((rep.width, rep.height), (1001, 564));
    assert_eq!((img.width, img.height), (1001, 564));
    assert_eq!(px(&img, 10, 282), [255, 0, 0, 255]);
    assert_eq!(px(&img, 990, 282), [0, 0, 255, 255]);
}

#[test]
fn sized_video_export_probes_at_the_output_size() {
    let _g = serial();
    let mut p = project(640, 360, 30.0);
    add_media(&mut p, "v", &bars(30, 6));
    p.clips = vec![clip(json!({"id": "c1", "track": "v1", "start": 0, "duration": 1.0, "source": {"type": "media", "asset": "v"}}))];
    let dir = scratch("sized-mp4");
    let out = dir.join("vertical.mp4");
    let s = ExportSettings { width: Some(360), height: Some(640), fit: Fit::Cover, loudness_lufs: Some(-16.0), ..ExportSettings::default() };
    let rep = export(Path::new("."), &p, pool(), &s, &out, &mut |_| true).unwrap();
    assert_eq!((rep.width, rep.height, rep.frames), (360, 640, 30));
    let probe = ffprobe_json(&out);
    let v = probe["streams"].as_array().unwrap().iter().find(|s| s["codec_type"] == "video").unwrap().clone();
    assert_eq!((v["width"].as_u64(), v["height"].as_u64()), (Some(360), Some(640)));
    // Project-space rendering is unaffected by the export size.
    let r = renderer();
    let img = r.frame(Path::new("."), &p, &resolved(&p), 0.5, &FrameOptions::default()).unwrap();
    assert_eq!((img.width, img.height), (640, 360));
}

/// Per-second RMS (dBFS) of a 24-bit stereo WAV.
fn wav24_rms_per_second(path: &Path) -> Vec<f64> {
    let b = std::fs::read(path).unwrap();
    let data = b.windows(4).position(|w| w == b"data").unwrap() + 8;
    let samples: Vec<f64> = b[data..]
        .as_chunks::<3>()
        .0
        .iter()
        .map(|c| (i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8) as f64 / 8_388_607.0)
        .collect();
    samples.chunks(48_000 * 2).filter(|c| c.len() == 96_000).map(|c| 10.0 * (c.iter().map(|v| v * v).sum::<f64>() / c.len() as f64).log10()).collect()
}

#[test]
fn export_waits_for_progressively_decoded_audio() {
    let _g = serial();
    let long = sine_wav("sine1k_90s", 1000.0, 0.1, 90.0);
    let mut p = project(64, 36, 30.0);
    add_media(&mut p, "s", &long);
    p.clips = vec![clip(json!({"id": "a", "track": "a1", "start": 0, "duration": 90, "source": {"type": "media", "asset": "s"}}))];
    let res = resolved(&p);
    // Playback on a pool that has never seen the file: returns promptly with whatever is decoded.
    let pool = fresh_pool("progressive-mix");
    let r = aiframes_render::Renderer::new(pool.clone());
    let started = std::time::Instant::now();
    let mut block = vec![0f32; 1024 * 2];
    r.mix(Path::new("."), &p, &res, 85.0, &mut block).unwrap();
    assert!(started.elapsed().as_secs_f64() < 1.5, "playback mix blocked for {:?}", started.elapsed());
    // Export on another fresh pool: every second of the mix is the full sine (no undecoded gaps).
    let pool = fresh_pool("progressive-export");
    let dir = scratch("progressive");
    let wav = dir.join("mix.wav");
    let s = ExportSettings { codec: "wav".into(), loudness_lufs: None, ..ExportSettings::default() };
    let rep = export(Path::new("."), &p, pool, &s, &wav, &mut |_| true).unwrap();
    assert!((rep.duration - 90.0).abs() < 1e-6);
    let rms = wav24_rms_per_second(&wav);
    assert_eq!(rms.len(), 90);
    for (i, v) in rms.iter().enumerate() {
        assert!((v + 23.01).abs() < 0.05, "second {i}: {v:.2} dBFS (expected the -20 dBFS peak sine everywhere)");
    }
}
