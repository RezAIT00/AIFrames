mod common;

use aiframes_core::model::MediaKind;
use aiframes_media::{probe, write_wav, EncodeOptions, Encoder, MediaError, Quality, VideoCodec};
use common::*;
use std::path::{Path, PathBuf};

const W: u32 = 320;
const H: u32 = 240;
const FPS: f64 = 30.0;
const FRAMES: u32 = 60;
/// Background colour of the synthetic frames (sRGB).
const BG: [u8; 3] = [200, 60, 40];

/// Frame `n`: frame-number bars in the top half, a flat BG colour in the bottom half.
fn synth(n: u32, w: u32, h: u32) -> Vec<u8> {
    let mut v = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let px = if y < h / 2 {
                let bit = x * BITS / w;
                if n & (1 << bit) != 0 { [255, 255, 255] } else { [0, 0, 0] }
            } else {
                BG
            };
            v[i..i + 3].copy_from_slice(&px);
            v[i + 3] = 255;
        }
    }
    v
}

fn bars_code(f: &aiframes_media::Frame) -> u32 {
    let y = f.height / 4;
    (0..BITS).fold(0, |n, k| {
        let x = ((k as f64 + 0.5) * f.width as f64 / BITS as f64) as u32;
        if f.pixel(x, y).unwrap()[0] > 128 { n | (1 << k) } else { n }
    })
}

fn wav(dir: &Path) -> PathBuf {
    let p = dir.join("audio.wav");
    let pcm: Vec<f32> = (0..(FRAMES as f64 / FPS * 48_000.0) as usize)
        .flat_map(|i| {
            let v = 0.2 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin();
            [v, v]
        })
        .collect();
    write_wav(&p, &pcm, 48_000).unwrap();
    p
}

fn encode(opts: &EncodeOptions) -> String {
    let mut e = Encoder::start(opts).unwrap();
    for n in 0..FRAMES {
        e.push(&synth(n, opts.width, opts.height)).unwrap();
    }
    assert_eq!(e.frames(), FRAMES as u64);
    let name = e.encoder_name().to_string();
    e.finish().unwrap_or_else(|err| panic!("{name}: {err}"));
    name
}

fn check_decoded(path: &Path, tolerance: i32) {
    let pool = common::pool(&format!("enc-{}", path.file_name().unwrap().to_string_lossy()), 32 << 20);
    for n in [0u32, 1, 2, 17, 30, 59] {
        let f = pool.video_frame(path, n as f64 / FPS, None).unwrap();
        assert_eq!(bars_code(&f), n, "{} frame {n}", path.display());
        let p = f.pixel(f.width / 2, f.height * 3 / 4).unwrap();
        for c in 0..3 {
            assert!((p[c] as i32 - BG[c] as i32).abs() <= tolerance, "{}: colour {p:?} vs {BG:?}", path.display());
        }
    }
}

#[test]
fn h264_round_trip_with_audio_hardware_and_software() {
    let dir = scratch("enc-h264");
    let audio = wav(&dir);
    for hardware in [true, false] {
        let out = dir.join(format!("out-{hardware}.mp4"));
        let mut o = EncodeOptions::new(&out, W, H, FPS);
        o.hardware = hardware;
        o.audio = Some(audio.clone());
        let name = encode(&o);
        if !hardware {
            assert_eq!(name, "libx264");
        }
        let (k, info) = probe(&out).unwrap();
        assert_eq!(k, MediaKind::Video);
        assert_eq!((info.width, info.height), (W, H));
        assert_eq!(info.fps, 30.0);
        assert_eq!(info.codec, "h264");
        assert!(info.has_audio, "{name}");
        assert!((info.duration - 2.0).abs() < 0.08, "{name}: {}", info.duration);
        // BT.709 tags + faststart
        let tags = probe_color(&out);
        assert!(tags.contains("bt709") && tags.contains("tv") && tags.contains("yuv420p"), "{name}: {tags}");
        check_decoded(&out, 6);
        // no temp files left behind
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains(".part")).collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}

fn probe_color(p: &Path) -> String {
    let out = std::process::Command::new(&aiframes_media::tools().unwrap().ffprobe)
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=pix_fmt,color_space,color_transfer,color_primaries,color_range", "-of", "csv=p=0"])
        .arg(p)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn other_codecs_round_trip() {
    let dir = scratch("enc-codecs");
    let cases = [
        (VideoCodec::Hevc, Quality::Standard, "mp4", "hevc", 8),
        (VideoCodec::Av1, Quality::Draft, "mp4", "av1", 10),
        (VideoCodec::ProRes, Quality::High, "mov", "prores", 4),
        (VideoCodec::H264, Quality::Lossless, "mp4", "h264", 2),
        (VideoCodec::H264, Quality::Draft, "mkv", "h264", 10),
    ];
    for (codec, q, ext, want, tol) in cases {
        let out = dir.join(format!("out-{}-{}.{ext}", codec.name(), q.name()));
        let mut o = EncodeOptions::new(&out, W, H, FPS);
        o.codec = codec;
        o.quality = q;
        let name = encode(&o);
        let (_, info) = probe(&out).unwrap();
        assert_eq!(info.codec, want, "{name}");
        assert_eq!((info.width, info.height), (W, H));
        check_decoded(&out, tol);
    }
}

#[test]
fn gif_and_png_sequence() {
    let dir = scratch("enc-gif-png");
    let gif = dir.join("anim.gif");
    let mut o = EncodeOptions::new(&gif, W, H, 15.0);
    o.codec = VideoCodec::Gif;
    encode(&o);
    let (k, info) = probe(&gif).unwrap();
    assert_eq!(k, MediaKind::Video);
    assert_eq!(info.codec, "gif");
    assert!((info.duration - 4.0).abs() < 0.15, "{}", info.duration);

    let seq = dir.join("frames");
    let mut o = EncodeOptions::new(&seq, W, H, FPS);
    o.codec = VideoCodec::PngSequence;
    encode(&o);
    let mut files: Vec<_> = std::fs::read_dir(&seq).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    files.sort();
    assert_eq!(files.len(), FRAMES as usize);
    assert_eq!(files[0], "frame_000000.png");
    let img = image::open(seq.join("frame_000017.png")).unwrap().into_rgba8();
    assert_eq!(img.as_raw(), &synth(17, W, H), "PNG frames are lossless");
}

#[test]
fn odd_sizes_are_padded_for_420() {
    let dir = scratch("enc-odd");
    let out = dir.join("odd.mp4");
    let mut o = EncodeOptions::new(&out, 321, 241, FPS);
    o.hardware = false;
    encode(&o);
    let (_, info) = probe(&out).unwrap();
    assert_eq!((info.width, info.height), (322, 242));
}

#[test]
fn encoder_errors_are_actionable() {
    let dir = scratch("enc-errors");
    let out = dir.join("x.mp4");
    let mut e = Encoder::start(&EncodeOptions::new(&out, W, H, FPS)).unwrap();
    let err = e.push(&[0u8; 10]).unwrap_err();
    assert!(matches!(err, MediaError::InvalidInput(_)), "{err}");
    let err = e.finish().unwrap_err();
    assert!(err.to_string().contains("no frames"), "{err}");
    assert!(!out.exists());

    // audio that is not audio is refused up front
    let mut o = EncodeOptions::new(&out, W, H, FPS);
    o.audio = Some(png());
    let err = Encoder::start(&o).err().expect("refused");
    assert!(err.to_string().contains("no audio stream"), "{err}");

    // ffmpeg itself fails (no muxer for this extension): its own message is surfaced.
    let bad = dir.join("x.notacontainer");
    let mut o = EncodeOptions::new(&bad, W, H, FPS);
    o.hardware = false;
    let mut e = Encoder::start(&o).unwrap();
    let mut failed = None;
    for n in 0..FRAMES {
        if let Err(err) = e.push(&synth(n, W, H)) {
            failed = Some(err);
            break;
        }
    }
    let err = match failed {
        Some(err) => err,
        None => e.finish().unwrap_err(),
    };
    let s = err.to_string();
    assert!(s.contains("ffmpeg") && s.contains("x.notacontainer") && s.to_lowercase().contains("format"), "{s}");
    assert!(!bad.exists());

    // dropping an encoder without finish removes its partial output
    let mut e = Encoder::start(&EncodeOptions::new(&out, W, H, FPS)).unwrap();
    for n in 0..10 {
        e.push(&synth(n, W, H)).unwrap();
    }
    drop(e);
    let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    assert!(Encoder::start(&EncodeOptions::new(&out, 0, 10, FPS)).is_err());
    assert!(Encoder::start(&EncodeOptions::new(&out, 10, 10, 0.0)).is_err());
}

#[test]
fn codec_and_quality_names() {
    for c in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1, VideoCodec::ProRes, VideoCodec::Gif, VideoCodec::PngSequence] {
        assert_eq!(VideoCodec::parse(c.name()), Some(c));
    }
    for q in [Quality::Draft, Quality::Standard, Quality::High, Quality::Lossless] {
        assert_eq!(Quality::parse(q.name()), Some(q));
    }
    assert_eq!(VideoCodec::parse("h265"), Some(VideoCodec::Hevc));
    assert_eq!(VideoCodec::parse("wav"), None);
}

/// Busy frames (moving noise + gradient) so the bitrate is driven by the presets and caps.
fn busy(n: u32, w: u32, h: u32, noise: u8) -> Vec<u8> {
    let mut v = vec![0u8; (w * h * 4) as usize];
    let mut x: u32 = 0x9E37_79B9 ^ n.wrapping_mul(2_654_435_761);
    for y in 0..h {
        for xx in 0..w {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let i = ((y * w + xx) * 4) as usize;
            let g = ((xx + n * 4) % 256) as u8;
            let r = (x & 0xff) as u8 % noise.max(1);
            v[i..i + 4].copy_from_slice(&[g.wrapping_add(r), ((y + n) % 256) as u8, r, 255]);
        }
    }
    v
}

fn encode_busy(path: &Path, codec: VideoCodec, q: Quality, hardware: bool, noise: u8, frames: u32) -> (String, f64) {
    let (w, h) = (640u32, 360u32);
    let mut o = EncodeOptions::new(path, w, h, FPS);
    o.codec = codec;
    o.quality = q;
    o.hardware = hardware;
    let mut e = Encoder::start(&o).unwrap();
    let name = e.encoder_name().to_string();
    for n in 0..frames {
        e.push_owned(busy(n, w, h, noise)).unwrap();
    }
    e.finish().unwrap();
    let mbps = std::fs::metadata(path).unwrap().len() as f64 * 8.0 / (frames as f64 / FPS) / 1e6;
    (name, mbps)
}

#[test]
fn quality_presets_open_everywhere_are_ordered_and_capped() {
    let dir = scratch("enc-presets");
    for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
        for hardware in [true, false] {
            let mut rates = Vec::new();
            for q in [Quality::Draft, Quality::Standard, Quality::High] {
                let out = dir.join(format!("{}-{hardware}-{}.mp4", codec.name(), q.name()));
                // Pure noise for 6 s: every preset runs into its cap. The VBV buffer (2x the cap)
                // lets the average exceed it by at most buffer / duration.
                let (name, mbps) = encode_busy(&out, codec, q, hardware, 255, 180);
                let cap = aiframes_media::bitrate_cap_mbps(codec, q, 640, 360, FPS).unwrap();
                // SVT-AV1's capped CRF is approximate: on pure noise its fast presets land right at
                // the VBV bound (measured 1.40 against 1.40), so it gets a wider margin.
                let slack = if name.starts_with("libsvtav1") { 1.25 } else { 1.05 };
                let bound = cap * (1.0 + 2.0 / 6.0) * slack;
                assert!(mbps <= bound, "{name} {}: {mbps:.2} Mbit/s over the {cap:.2} cap (bound {bound:.2})", q.name());
                // moderate detail: constant quality decides, so better presets spend more bits
                let (_, m) = encode_busy(&dir.join(format!("m-{}-{hardware}-{}.mp4", codec.name(), q.name())), codec, q, hardware, 24, 60);
                rates.push((q.name(), name, m));
            }
            assert!(rates[0].2 < rates[1].2 && rates[1].2 < rates[2].2, "{} hw={hardware}: {rates:?}", codec.name());
        }
    }
}
