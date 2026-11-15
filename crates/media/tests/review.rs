//! Regressions for the independent review (round 3). Each test is a port of a confirmed repro.
mod common;

use aiframes_media::{probe, AudioData, EncodeOptions, Encoder, MediaPool, VideoCodec};
use common::*;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static SERIAL: Mutex<()> = Mutex::new(());
fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

fn fresh(name: &str) -> Arc<MediaPool> {
    MediaPool::with_cache_dir(256 << 20, scratch(&format!("review-{name}")))
}

fn read(a: &AudioData, start: i64, frames: usize) -> Vec<f32> {
    let mut v = vec![0f32; frames * 2];
    assert!(a.read_frames_wait(start, &mut v, Duration::from_secs(30)), "read at {start} timed out");
    v
}

/// Review item 3: far-ahead (region) reads must be the samples of the full decode in every
/// container; Matroska/WebM (1 ms timestamps) must not use region decoders at all.
#[test]
fn region_reads_match_full_decode_in_all_exact_seek_containers() {
    let _s = serial();
    let cases: [(&str, &[&str], bool); 7] = [
        ("long_opus.webm", &["-c:a", "libopus", "-b:a", "128k"], false),
        ("long_opus.ogg", &["-c:a", "libopus", "-b:a", "128k"], true),
        ("long_vorbis.ogg", &["-c:a", "libvorbis"], true),
        ("long.flac", &["-c:a", "flac"], true),
        ("long_mp3.mkv", &["-c:a", "libmp3lame"], false),
        ("long_aac.mkv", &["-ar", "44100", "-c:a", "aac"], false),
        ("long_alac.m4a", &["-ar", "44100", "-c:a", "alac"], true),
    ];
    let mut report = Vec::new();
    for (name, codec, regions_expected) in cases {
        let path = long_coded(name, codec);
        let reference = fresh(&format!("ref-{name}")).audio_complete(&path, 48_000).unwrap();
        let pool = fresh(&format!("prog-{name}"));
        let before = aiframes_media::audio_regions_started();
        let a = pool.audio(&path, 48_000).unwrap();
        let spots = [(100 * 48_000 + 17, 4096usize), (70 * 48_000, 48_000), (110 * 48_000 + 5, 999)];
        let got: Vec<Vec<f32>> = spots.iter().map(|&(s, n)| read(&a, s, n)).collect();
        let regions = aiframes_media::audio_regions_started() - before;
        assert_eq!(regions > 0, regions_expected, "{name}: region decoders started: {regions}");
        a.wait_complete().unwrap();
        for ((s, n), v) in spots.iter().zip(got) {
            let want = read(&reference, *s, *n);
            let maxdiff = v.iter().zip(&want).map(|(x, y)| (x - y).abs()).fold(0f32, f32::max);
            if maxdiff > 1e-4 {
                report.push(format!("{name} @ {s}: max diff {maxdiff:.5}"));
            }
        }
    }
    assert!(report.is_empty(), "{report:#?}");
}

/// Review item 6: a video stream starting 1 s into the container is not clamped 1 s early.
#[test]
fn late_starting_video_is_not_clamped_early() {
    let path = late_video();
    let pool = fresh("late");
    let (_, info) = pool.probe(&path).unwrap();
    assert!((info.duration - 4.0).abs() < 0.05, "{}", info.duration);
    for &t in &[1.5f64, 2.5, 2.95, 3.2, 3.5, 3.9] {
        let f = pool.video_frame(&path, t, None).unwrap();
        let want = ((t - 1.0) * 30.0 + 1e-6).floor() as u32;
        assert_eq!(code(&f), want, "t={t}");
    }
    // before the video starts: its first frame; after it ends: its last
    assert_eq!(code(&pool.video_frame(&path, 0.3, None).unwrap()), 0);
    assert_eq!(code(&pool.video_frame(&path, 9.0, None).unwrap()), 89);
    // thumbnails use the same end
    assert_eq!(code(&pool.thumbnail(&path, 3.5, 36).unwrap()), 75);
}

/// Plausible item, confirmed: Matroska video streams carry no `duration`, so a 5 s video in a
/// 20 s file used to be treated as 20 s; requests past its end walked the whole preroll ladder.
#[test]
fn matroska_video_shorter_than_the_file_ends_where_it_really_ends() {
    let path = short_video_mkv();
    let pool = fresh("mkvend");
    let t = Instant::now();
    assert_eq!(code(&pool.video_frame(&path, 12.0, None).unwrap()), 149);
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    let s0 = pool.stats().seeks;
    assert_eq!(code(&pool.video_frame(&path, 17.5, None).unwrap()), 149);
    assert_eq!(pool.stats().seeks, s0, "the end is known now");
    assert_eq!(code(&pool.thumbnail(&path, 15.0, 36).unwrap()), 149);
}

/// Review item 16: a thumbnail past the last decodable frame is the last frame at or before t,
/// not the first frame of the file.
#[test]
fn thumbnail_past_decodable_end_is_the_last_frame() {
    let path = truncated_bars();
    let pool = fresh("thumbtrunc");
    let (_, info) = pool.probe(&path).unwrap();
    assert!(info.duration > 11.0, "probes as the full length: {}", info.duration);
    let last = pool.video_frame(&path, 10.0, None).unwrap();
    let n = code(&last);
    assert!(n > 60 && n < 359, "last decodable frame {n}");
    let th = pool.thumbnail(&path, 10.0, 36).unwrap();
    assert_eq!(code(&th), n);
    // and a second pool (no learned end) gets it right too
    assert_eq!(code(&fresh("thumbtrunc2").thumbnail(&path, 10.0, 36).unwrap()), n);
}

fn solid(w: u32, h: u32, n: u32) -> Vec<u8> {
    let mut v = vec![0u8; (w * h * 4) as usize];
    for p in v.as_chunks_mut::<4>().0 {
        p.copy_from_slice(&[(n * 7 % 256) as u8, 100, 50, 255]);
    }
    v
}

fn encode(o: &EncodeOptions, frames: u32) -> String {
    let mut e = Encoder::start(o).unwrap();
    for n in 0..frames {
        e.push(&solid(o.width, o.height, n)).unwrap();
    }
    let name = e.encoder_name().to_string();
    e.finish().unwrap();
    name
}

/// Review item 12: a GPU encoder that cannot open at the requested size (H.264 NVENC > 4096 px)
/// falls back to the software encoder, and reports it.
#[test]
fn wide_h264_with_hardware_falls_back_to_software() {
    let dir = scratch("review-wide");
    let out = dir.join("wide.mp4");
    let mut o = EncodeOptions::new(&out, 4160, 64, 30.0);
    o.hardware = true;
    let name = encode(&o, 6);
    assert_ne!(name, "h264_nvenc");
    let (_, info) = probe(&out).unwrap();
    assert_eq!((info.width, info.height, info.codec.as_str()), (4160, 64, "h264"));
    // a size NVENC handles still uses it when present
    let mut o = EncodeOptions::new(dir.join("normal.mp4"), 1280, 720, 30.0);
    o.hardware = true;
    let name = encode(&o, 3);
    if aiframes_media::tools().unwrap().hardware_encoder_works("h264_nvenc") {
        assert_eq!(name, "h264_nvenc");
    }
}

fn pngs(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

/// Review item 17: re-exporting a shorter PNG sequence leaves exactly its own frames; a cancelled
/// export leaves nothing; unrelated files in the folder are kept.
#[test]
fn png_sequence_reexport_replaces_the_previous_frames() {
    let dir = scratch("review-png");
    let out = dir.join("seq");
    let mut o = EncodeOptions::new(&out, 32, 18, 30.0);
    o.codec = VideoCodec::PngSequence;
    encode(&o, 30);
    assert_eq!(pngs(&out).len(), 30);
    std::fs::write(out.join("notes.txt"), b"keep me").unwrap();
    encode(&o, 15);
    let files = pngs(&out);
    assert_eq!(files.iter().filter(|f| f.ends_with(".png")).count(), 15, "{files:?}");
    assert!(files.contains(&"notes.txt".to_string()));
    assert_eq!(files[0], "frame_000000.png");
    // cancelled: nothing appears, nothing is left next to the folder
    let other = dir.join("cancelled");
    let mut o2 = o.clone();
    o2.path = other.clone();
    let mut e = Encoder::start(&o2).unwrap();
    for n in 0..10 {
        e.push(&solid(32, 18, n)).unwrap();
    }
    e.abort();
    assert!(!other.exists() || pngs(&other).is_empty());
    let siblings: Vec<String> = pngs(&dir);
    assert!(siblings.iter().all(|n| !n.contains(".part")), "{siblings:?}");
    // the previous export is intact if a re-export is cancelled
    let mut e = Encoder::start(&o).unwrap();
    e.push(&solid(32, 18, 1)).unwrap();
    drop(e);
    assert_eq!(pngs(&out).iter().filter(|f| f.ends_with(".png")).count(), 15);
}

/// Review item 10: odd sizes are padded for 4:2:0, and the encoder reports the real size.
#[test]
fn odd_sizes_report_the_padded_output_size() {
    let dir = scratch("review-odd");
    let mut o = EncodeOptions::new(dir.join("odd.mp4"), 321, 241, 30.0);
    o.hardware = false;
    let e = Encoder::start(&o).unwrap();
    assert_eq!(e.output_size(), (322, 242));
    e.abort();
    let mut o = EncodeOptions::new(dir.join("odd"), 321, 241, 30.0);
    o.codec = VideoCodec::PngSequence;
    let e = Encoder::start(&o).unwrap();
    assert_eq!(e.output_size(), (321, 241));
    e.abort();
}

/// Plausible item: a file replaced on disk within a second is not served from stale caches.
#[test]
fn replaced_file_is_seen_immediately() {
    let dir = scratch("review-replaced-file");
    let p = dir.join("img.png");
    let make = |c: &str| ffmpeg(&["-f", "lavfi", "-i", &format!("color=c={c}:s=16x16"), "-frames:v", "1", p.to_str().unwrap()]);
    make("red");
    let pool = fresh("replace");
    assert!(pool.image(&p, None).unwrap().pixel(4, 4).unwrap()[0] > 240);
    make("blue");
    let px = pool.image(&p, None).unwrap().pixel(4, 4).unwrap();
    assert!(px[2] > 200 && px[0] < 50, "stale picture after replacing the file: {px:?}");
}

/// Plausible item: a burst of distinct streams never runs more decode sessions than the cap.
#[test]
fn session_cap_holds_during_a_burst() {
    let pool = fresh("cap");
    let path = cfr();
    let mut peak = 0;
    for k in 0..24u32 {
        let max = Some((320 - k * 8, 180));
        pool.video_frame(&path, 1.0 + k as f64 * 0.3, max).unwrap();
        peak = peak.max(pool.stats().sessions);
    }
    assert!(peak <= 12, "peak sessions {peak}");
}

/// Plausible item: GIF export keeps memory bounded (two passes via a disk intermediate) and
/// still produces the right animation.
#[test]
fn long_gif_is_made_in_two_passes() {
    let dir = scratch("review-gif");
    let out = dir.join("long.gif");
    let mut o = EncodeOptions::new(&out, 320, 180, 15.0);
    o.codec = VideoCodec::Gif;
    let mut e = Encoder::start(&o).unwrap();
    for n in 0..450u32 {
        let mut f = solid(320, 180, n);
        // a moving white bar so frames differ
        let x = (n * 3 % 300) as usize;
        for y in 0..180usize {
            for dx in 0..20usize {
                let i = (y * 320 + x + dx) * 4;
                f[i..i + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        e.push_owned(f).unwrap();
    }
    e.finish().unwrap();
    let (_, info) = probe(&out).unwrap();
    assert_eq!(info.codec, "gif");
    assert!((info.duration - 30.0).abs() < 0.3, "{}", info.duration);
    let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(leftovers.len(), 1, "{leftovers:?}");
}
