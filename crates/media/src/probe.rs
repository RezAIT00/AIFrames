//! `probe`: what a media file is (video / audio / still image) and its properties, from ffprobe's
//! JSON (images are read with the `image` crate first, which is faster and handles EXIF orientation).

use crate::error::{MediaError, Result};
use crate::tools::{command, ff_path, run_capture, tools};
use crate::util::snap_fps;
use aiframes_core::model::{MediaInfo, MediaKind};
use serde_json::Value;
use std::path::Path;

/// Probes a media file. Width/height are the *display* size (rotation metadata and non-square
/// pixels applied); frames returned by `MediaPool` already have that size and orientation, so
/// `info.rotation` is informational only. Still images have `duration == 0`, `fps == 0`.
pub fn probe(path: &Path) -> Result<(MediaKind, MediaInfo)> {
    probe_full(path).map(|p| (p.kind, p.info))
}

/// Everything the decoders need beyond the public `MediaInfo`.
#[derive(Clone, Debug)]
pub(crate) struct Probed {
    pub kind: MediaKind,
    pub info: MediaInfo,
    /// Absolute stream indexes (for `-map 0:N`).
    pub video_index: Option<usize>,
    pub audio_index: Option<usize>,
    /// Source time where the video stream ends: its start offset in the container plus its own
    /// duration (may be earlier than `info.duration` when the audio runs longer).
    pub video_end: f64,
    /// Source time of the video stream's first frame (0 unless it starts after the container start).
    pub video_start: f64,
    /// `video_end` came from the stream itself (not the container duration as a stand-in).
    pub video_end_exact: bool,
    /// Variable frame rate (timestamps irregular).
    pub vfr: bool,
    /// The stream has no colour matrix tag (the decoder then assumes BT.709 for HD, BT.601 for SD).
    pub untagged_matrix: bool,
    /// VP8/VP9 with an alpha channel in WebM (needs the libvpx decoder to keep alpha).
    pub vpx_alpha: Option<&'static str>,
    /// Pixel format of the video stream.
    pub pix_fmt: String,
    /// ffprobe's container name ("mov,mp4,m4a,3gp,3g2,mj2", "matroska,webm", "wav", "mp3" ...).
    pub format_name: String,
}

const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "jfif", "webp", "bmp", "tif", "tiff", "gif", "tga", "ico", "avif", "heic", "heif", "jxl", "exr",
    "dpx", "ppm", "pgm", "pbm", "qoi", "psd",
];

pub(crate) fn is_image_ext(path: &Path) -> bool {
    path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).is_some_and(|e| IMAGE_EXTS.contains(&e.as_str()))
}

pub(crate) fn probe_full(path: &Path) -> Result<Probed> {
    if !path.exists() {
        return Err(MediaError::NotFound { path: path.to_path_buf() });
    }
    if path.is_dir() {
        return Err(MediaError::unsupported(path, "is a folder, not a media file"));
    }
    if is_image_ext(path) {
        if let Some(p) = probe_image_crate(path) {
            return Ok(p);
        }
    }
    let t = tools()?;
    let mut c = command(&t.ffprobe);
    c.args(["-v", "error", "-hide_banner", "-print_format", "json", "-show_format", "-show_streams", "-i"]).arg(ff_path(path));
    let (out, _) = run_capture(c, "ffprobe", Some(path), "read the media file")?;
    let json: Value = serde_json::from_slice(&out)
        .map_err(|e| MediaError::unsupported(path, format!("ffprobe returned unreadable output ({e})")))?;
    parse_ffprobe(path, &json)
}

/// Fast path for stills: header-only read with the `image` crate (EXIF orientation applied).
fn probe_image_crate(path: &Path) -> Option<Probed> {
    use image::ImageDecoder;
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    if ext == "gif" && gif_is_animated(path) {
        return None; // animated GIFs are videos; ffprobe handles them
    }
    let reader = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    let format = reader.format()?;
    let mut decoder = reader.into_decoder().ok()?;
    let (mut w, mut h) = decoder.dimensions();
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    use image::metadata::Orientation::*;
    let rotation = match orientation {
        Rotate90 | Rotate90FlipH => 90,
        Rotate180 | FlipVertical => 180,
        Rotate270 | Rotate270FlipH => 270,
        _ => 0,
    };
    if matches!(orientation, Rotate90 | Rotate270 | Rotate90FlipH | Rotate270FlipH) {
        std::mem::swap(&mut w, &mut h);
    }
    let codec = format!("{format:?}").to_ascii_lowercase();
    let pix_fmt = format!("{:?}", decoder.color_type()).to_ascii_lowercase();
    Some(Probed {
        kind: MediaKind::Image,
        info: MediaInfo { duration: 0.0, width: w, height: h, codec, rotation, ..Default::default() },
        video_index: None,
        audio_index: None,
        video_end: 0.0,
        video_start: 0.0,
        video_end_exact: true,
        vfr: false,
        untagged_matrix: false,
        vpx_alpha: None,
        pix_fmt,
        format_name: format!("{format:?}").to_ascii_lowercase(),
    })
}

fn gif_is_animated(path: &Path) -> bool {
    use image::AnimationDecoder;
    let Ok(f) = std::fs::File::open(path) else { return false };
    let Ok(dec) = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(f)) else { return false };
    dec.into_frames().take(2).filter(|f| f.is_ok()).count() > 1
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|x| x.is_finite())
}

fn ratio(v: &Value) -> Option<f64> {
    let s = v.as_str()?;
    let (a, b) = s.split_once('/').or_else(|| s.split_once(':'))?;
    let (a, b): (f64, f64) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (a > 0.0 && b > 0.0).then(|| a / b).filter(|x| x.is_finite())
}

/// Chooses the frame rate and whether the stream is variable-rate.
pub(crate) fn pick_fps(r: Option<f64>, avg: Option<f64>) -> (f64, bool) {
    let sane = |v: Option<f64>| v.filter(|x| *x >= 0.5 && *x <= 1000.0);
    match (sane(r), sane(avg)) {
        (Some(r), Some(a)) if (r - a).abs() / r < 0.01 => (snap_fps(r), false),
        // Nominal rate with dropped/duplicated frames (typical screen recording): report the nominal rate.
        (Some(r), Some(a)) if r <= 240.0 && a < r && a > r * 0.5 => (snap_fps(r), true),
        (_, Some(a)) => (snap_fps(a), true),
        (Some(r), None) => (snap_fps(r), false),
        (None, None) => (30.0, false),
    }
}

pub(crate) fn parse_ffprobe(path: &Path, json: &Value) -> Result<Probed> {
    let empty = Vec::new();
    let streams = json.get("streams").and_then(Value::as_array).unwrap_or(&empty);
    let format = json.get("format").cloned().unwrap_or(Value::Null);
    let format_name = format.get("format_name").and_then(Value::as_str).unwrap_or("").to_string();
    let format_duration = format.get("duration").and_then(num).unwrap_or(0.0).max(0.0);
    let format_start = format.get("start_time").and_then(num).unwrap_or(0.0);

    let is_attached_pic = |s: &Value| s.pointer("/disposition/attached_pic").and_then(Value::as_i64) == Some(1);
    let kind_of = |s: &Value| s.get("codec_type").and_then(Value::as_str).unwrap_or("").to_string();
    let video = streams.iter().find(|s| kind_of(s) == "video" && !is_attached_pic(s));
    let audio = streams.iter().find(|s| kind_of(s) == "audio");
    let index = |s: &Value| s.get("index").and_then(Value::as_u64).map(|i| i as usize);

    if video.is_none() && audio.is_none() {
        return Err(MediaError::unsupported(
            path,
            if streams.iter().any(is_attached_pic) {
                "contains only a cover picture, no audio or video".to_string()
            } else {
                format!("has no audio or video streams (container: {})", if format_name.is_empty() { "unknown" } else { &format_name })
            },
        ));
    }

    let mut info = MediaInfo::default();
    let mut p = Probed {
        kind: MediaKind::Video,
        info: MediaInfo::default(),
        video_index: None,
        audio_index: audio.and_then(index),
        video_end: 0.0,
        video_start: 0.0,
        video_end_exact: false,
        vfr: false,
        untagged_matrix: false,
        vpx_alpha: None,
        pix_fmt: String::new(),
        format_name: format_name.clone(),
    };

    if let Some(a) = audio {
        info.has_audio = true;
        info.audio_channels = a.get("channels").and_then(Value::as_u64).unwrap_or(0) as u32;
        info.audio_rate = a.get("sample_rate").and_then(num).unwrap_or(0.0) as u32;
    }

    if let Some(v) = video {
        let codec = v.get("codec_name").and_then(Value::as_str).unwrap_or("").to_string();
        let nb_frames = v.get("nb_frames").and_then(num).map(|x| x as u64);
        let image_container = format_name.ends_with("_pipe") || format_name == "image2" || format_name == "gif" && nb_frames.unwrap_or(1) <= 1;
        let still = image_container && !info.has_audio && nb_frames.unwrap_or(1) <= 1;

        let mut w = v.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
        let mut h = v.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
        if let Some(sar) = v.get("sample_aspect_ratio").and_then(ratio) {
            if (sar - 1.0).abs() > 0.01 && w > 0 {
                w = (((w as f64 * sar) / 2.0).round() * 2.0).max(2.0) as u32;
            }
        }
        let rotation = rotation_of(v);
        if rotation == 90 || rotation == 270 {
            std::mem::swap(&mut w, &mut h);
        }
        info.width = w;
        info.height = h;
        info.rotation = rotation;
        info.codec = codec.clone();
        p.pix_fmt = v.get("pix_fmt").and_then(Value::as_str).unwrap_or("").to_string();
        let cs = v.get("color_space").and_then(Value::as_str).unwrap_or("unknown");
        p.untagged_matrix = cs == "unknown" || cs.is_empty() || cs == "unspecified";
        // Matroska tags keep the writer's case: libvpx/ffmpeg write ALPHA_MODE, some tools alpha_mode.
        let alpha_tag = v
            .get("tags")
            .and_then(Value::as_object)
            .and_then(|t| t.iter().find(|(k, _)| k.eq_ignore_ascii_case("alpha_mode")))
            .and_then(|(_, val)| val.as_str())
            .map(|s| s.trim() == "1")
            .unwrap_or(false);
        p.vpx_alpha = match (codec.as_str(), alpha_tag) {
            ("vp9", true) => Some("libvpx-vp9"),
            ("vp8", true) => Some("libvpx"),
            _ => None,
        };

        if still {
            p.kind = MediaKind::Image;
            info.duration = 0.0;
            info.has_video = false;
        } else {
            p.kind = MediaKind::Video;
            p.video_index = index(v);
            info.has_video = true;
            let (fps, vfr) = pick_fps(v.get("r_frame_rate").and_then(ratio), v.get("avg_frame_rate").and_then(ratio));
            // The stream's own length: `duration`, else Matroska's DURATION tag, else frames / fps.
            let own = v
                .get("duration")
                .and_then(num)
                .or_else(|| tag_duration(v))
                .or_else(|| nb_frames.filter(|n| *n > 1).map(|n| n as f64 / fps))
                .filter(|d| *d > 0.0);
            let start = v.get("start_time").and_then(num).map(|s| (s - format_start).max(0.0)).unwrap_or(0.0);
            info.fps = fps;
            p.vfr = vfr;
            p.video_start = start;
            match own {
                Some(d) => {
                    p.video_end = start + d;
                    p.video_end_exact = true;
                }
                None => {
                    p.video_end = format_duration; // unknown: up to the end of the container
                    p.video_end_exact = false;
                }
            }
            info.duration = if format_duration > 0.0 { format_duration } else { p.video_end };
        }
    } else {
        p.kind = MediaKind::Audio;
        let a = audio.expect("audio stream");
        info.codec = a.get("codec_name").and_then(Value::as_str).unwrap_or("").to_string();
        let adur = a.get("duration").and_then(num).unwrap_or(0.0);
        info.duration = if format_duration > 0.0 { format_duration } else { adur };
    }

    p.info = info;
    Ok(p)
}

/// A stream's `DURATION` tag ("HH:MM:SS.fraction", written by Matroska muxers) in seconds.
fn tag_duration(v: &Value) -> Option<f64> {
    let tags = v.get("tags")?.as_object()?;
    let s = tags.iter().find(|(k, _)| k.eq_ignore_ascii_case("duration") || k.to_ascii_uppercase().starts_with("DURATION-"))?.1.as_str()?;
    let mut parts = s.trim().split(':').rev();
    let secs: f64 = parts.next()?.parse().ok()?;
    let mins: f64 = parts.next().map(|m| m.parse().ok()).unwrap_or(Some(0.0))?;
    let hours: f64 = parts.next().map(|h| h.parse().ok()).unwrap_or(Some(0.0))?;
    Some(hours * 3600.0 + mins * 60.0 + secs).filter(|d| d.is_finite())
}

/// Clockwise display rotation in degrees (0/90/180/270) from the display matrix or legacy tag.
fn rotation_of(v: &Value) -> i32 {
    let from_matrix = v.get("side_data_list").and_then(Value::as_array).and_then(|list| {
        list.iter().find_map(|sd| sd.get("rotation").and_then(num).map(|r| -r)) // matrix rotation is counter-clockwise
    });
    let from_tag = v.pointer("/tags/rotate").and_then(num);
    let deg = from_matrix.or(from_tag).unwrap_or(0.0);
    let snapped = ((deg / 90.0).round() as i64 * 90).rem_euclid(360);
    snapped as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fps_choice() {
        assert_eq!(pick_fps(Some(30.0), Some(30.0)), (30.0, false));
        assert_eq!(pick_fps(Some(30000.0 / 1001.0), Some(29.97)), (29.97, false));
        // Screen recording with drops: nominal 60, average 58.7
        assert_eq!(pick_fps(Some(60.0), Some(58.7)), (60.0, true));
        // Timebase-like r_frame_rate
        assert_eq!(pick_fps(Some(1000.0), Some(59.9)), (59.94, true));
        assert_eq!(pick_fps(None, Some(25.0)), (25.0, true));
        assert_eq!(pick_fps(None, None), (30.0, false));
    }

    #[test]
    fn parse_rotated_phone_video() {
        let j = json!({
            "streams": [
                {"index": 0, "codec_type": "video", "codec_name": "hevc", "width": 1920, "height": 1080,
                 "r_frame_rate": "30/1", "avg_frame_rate": "30/1", "duration": "5.0", "pix_fmt": "yuv420p",
                 "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}]},
                {"index": 1, "codec_type": "audio", "codec_name": "aac", "channels": 2, "sample_rate": "48000", "duration": "5.0"}
            ],
            "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "5.02"}
        });
        let p = parse_ffprobe(Path::new("x.mp4"), &j).unwrap();
        assert_eq!(p.kind, MediaKind::Video);
        assert_eq!((p.info.width, p.info.height), (1080, 1920));
        assert_eq!(p.info.rotation, 90);
        assert!(p.info.has_audio && p.info.has_video);
        assert_eq!(p.info.audio_rate, 48000);
        assert_eq!(p.video_index, Some(0));
        assert_eq!(p.audio_index, Some(1));
        assert!(p.untagged_matrix);
    }

    #[test]
    fn parse_audio_with_cover_art() {
        let j = json!({
            "streams": [
                {"index": 0, "codec_type": "audio", "codec_name": "mp3", "channels": 2, "sample_rate": "44100"},
                {"index": 1, "codec_type": "video", "codec_name": "mjpeg", "width": 500, "height": 500, "disposition": {"attached_pic": 1}}
            ],
            "format": {"format_name": "mp3", "duration": "180.5"}
        });
        let p = parse_ffprobe(Path::new("x.mp3"), &j).unwrap();
        assert_eq!(p.kind, MediaKind::Audio);
        assert_eq!(p.info.duration, 180.5);
        assert!(!p.info.has_video);
        assert_eq!(p.info.codec, "mp3");
    }

    #[test]
    fn parse_anamorphic() {
        let j = json!({
            "streams": [{"index": 0, "codec_type": "video", "codec_name": "dvvideo", "width": 720, "height": 480,
                "sample_aspect_ratio": "32:27", "r_frame_rate": "30000/1001", "avg_frame_rate": "30000/1001", "color_space": "bt470bg"}],
            "format": {"format_name": "dv", "duration": "2.0"}
        });
        let p = parse_ffprobe(Path::new("x.dv"), &j).unwrap();
        assert_eq!((p.info.width, p.info.height), (854, 480));
        assert_eq!(p.info.fps, 29.97);
        assert!(!p.untagged_matrix);
    }

    #[test]
    fn late_video_and_matroska_durations() {
        let late = json!({
            "streams": [
                {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 320, "height": 180, "r_frame_rate": "30/1",
                 "avg_frame_rate": "30/1", "start_time": "1.000000", "duration": "3.000000"},
                {"index": 1, "codec_type": "audio", "codec_name": "aac", "channels": 2, "sample_rate": "48000", "start_time": "0.0", "duration": "4.0"}
            ],
            "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "start_time": "0.000000", "duration": "4.000000"}
        });
        let p = parse_ffprobe(Path::new("late.mp4"), &late).unwrap();
        assert_eq!((p.video_start, p.video_end, p.video_end_exact), (1.0, 4.0, true));
        let mkv = json!({
            "streams": [
                {"index": 0, "codec_type": "video", "codec_name": "h264", "width": 160, "height": 90, "r_frame_rate": "30/1",
                 "avg_frame_rate": "30/1", "start_time": "0.000000", "tags": {"DURATION": "00:00:05.000000000"}},
                {"index": 1, "codec_type": "audio", "codec_name": "aac", "channels": 1, "sample_rate": "44100", "tags": {"DURATION": "00:00:20.023000000"}}
            ],
            "format": {"format_name": "matroska,webm", "start_time": "0.000000", "duration": "20.023000"}
        });
        let p = parse_ffprobe(Path::new("x.mkv"), &mkv).unwrap();
        assert_eq!((p.video_end, p.video_end_exact), (5.0, true));
        assert!((p.info.duration - 20.023).abs() < 1e-9);
        let unknown = json!({
            "streams": [{"index": 0, "codec_type": "video", "codec_name": "h264", "width": 160, "height": 90, "r_frame_rate": "30/1", "avg_frame_rate": "30/1"}],
            "format": {"format_name": "mpegts", "duration": "9.5"}
        });
        let p = parse_ffprobe(Path::new("x.ts"), &unknown).unwrap();
        assert_eq!((p.video_end, p.video_end_exact), (9.5, false));
    }

    #[test]
    fn webm_alpha_tag_in_any_case() {
        for key in ["ALPHA_MODE", "alpha_mode", "Alpha_Mode"] {
            let j = json!({"format": {"format_name": "matroska,webm", "duration": "1.6"},
                "streams": [{"index": 0, "codec_type": "video", "codec_name": "vp9", "width": 1920, "height": 1080,
                             "r_frame_rate": "24000/1001", "avg_frame_rate": "24000/1001", "pix_fmt": "yuv420p",
                             "tags": {key: "1"}}]});
            let p = parse_ffprobe(Path::new("cut.webm"), &j).unwrap();
            assert_eq!(p.vpx_alpha, Some("libvpx-vp9"), "{key}");
        }
        let j = json!({"format": {"format_name": "matroska,webm", "duration": "1.6"},
            "streams": [{"index": 0, "codec_type": "video", "codec_name": "vp9", "width": 64, "height": 64,
                         "r_frame_rate": "30/1", "avg_frame_rate": "30/1", "tags": {"ALPHA_MODE": "0"}}]});
        assert_eq!(parse_ffprobe(Path::new("plain.webm"), &j).unwrap().vpx_alpha, None);
    }

    #[test]
    fn parse_no_streams() {
        let j = json!({"streams": [], "format": {"format_name": "srt"}});
        assert!(matches!(parse_ffprobe(Path::new("x.srt"), &j), Err(MediaError::Unsupported { .. })));
    }
}
