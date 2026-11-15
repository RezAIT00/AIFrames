//! `Encoder`: an ffmpeg child that reads raw RGBA frames on stdin (plus an optional WAV) and writes
//! MP4 (H.264 / HEVC / AV1, NVENC when asked and available), ProRes MOV, GIF or a PNG sequence.
//!
//! Colour: frames are sRGB RGBA; YUV outputs are converted with the BT.709 matrix to limited range
//! and tagged bt709/bt709/bt709/tv, H.264/HEVC/AV1 as 8-bit 4:2:0 for compatibility (lossless:
//! 4:4:4). Single-file outputs are written to a temporary sibling and renamed on success, and PNG
//! sequences to a temporary sibling folder swapped in on success (removing the previous export's
//! extra frames), so a failed or cancelled export never leaves partial output (or destroys a
//! previous one). A GPU encoder that cannot open at the requested size falls back to software.
//! GIFs are made in two passes (palette statistics + a lossless intermediate on disk, then
//! `paletteuse`), so memory stays bounded however long the clip.

use crate::error::{MediaError, Result};
use crate::tools::{ff_path, ffmpeg, spawn_error, tools, ChildGuard, StderrPump};
use crate::util::{fps_rational, temp_sibling};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Stdio};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
    ProRes,
    Gif,
    PngSequence,
}

impl VideoCodec {
    /// Parses the names used by `ExportSettings::codec` ("h264", "hevc"/"h265", "av1", "prores", "gif", "png_sequence").
    pub fn parse(s: &str) -> Option<VideoCodec> {
        Some(match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "h264" | "avc" | "x264" | "mp4" => VideoCodec::H264,
            "hevc" | "h265" | "x265" => VideoCodec::Hevc,
            "av1" => VideoCodec::Av1,
            "prores" | "mov" => VideoCodec::ProRes,
            "gif" => VideoCodec::Gif,
            "png_sequence" | "png" | "pngs" | "png_seq" => VideoCodec::PngSequence,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::Hevc => "hevc",
            VideoCodec::Av1 => "av1",
            VideoCodec::ProRes => "prores",
            VideoCodec::Gif => "gif",
            VideoCodec::PngSequence => "png_sequence",
        }
    }
    /// Default file extension (without the dot; "" for PNG sequences, which write a folder).
    pub fn extension(self) -> &'static str {
        match self {
            VideoCodec::H264 | VideoCodec::Hevc | VideoCodec::Av1 => "mp4",
            VideoCodec::ProRes => "mov",
            VideoCodec::Gif => "gif",
            VideoCodec::PngSequence => "",
        }
    }
    fn needs_even_size(self) -> bool {
        matches!(self, VideoCodec::H264 | VideoCodec::Hevc | VideoCodec::Av1 | VideoCodec::ProRes)
    }
    fn takes_audio(self) -> bool {
        matches!(self, VideoCodec::H264 | VideoCodec::Hevc | VideoCodec::Av1 | VideoCodec::ProRes)
    }
}

/// Encoder quality presets. H.264/HEVC/AV1 use constant quality with a peak cap from
/// [`bitrate_cap_mbps`] (high: about 2.5x YouTube's recommended upload bitrate):
///
/// | preset | NVENC (H.264 / HEVC / AV1 `-cq`) | libx264 | libx265 | SVT-AV1 |
/// |---|---|---|---|---|
/// | draft | p2, cq 26 / 28 / 34, no AQ (speed) | veryfast crf 23 | fast crf 27 | preset 10 crf 36 |
/// | standard | p5 hq, cq 22 / 24 / 30, spatial+temporal AQ, lookahead 20, 3 B-frames | medium crf 20 | medium crf 23 | preset 7 crf 30 |
/// | high | p6 hq, cq 18 / 20 / 26, spatial+temporal AQ, lookahead 32, 3 B-frames | slow crf 18 | slow crf 20 | preset 5 crf 26 |
/// | lossless | p7 lossless (4:4:4) | qp 0 (4:4:4) | lossless | lossless |
///
/// ProRes maps draft/standard/high/lossless to Proxy/422/HQ/4444; PNG to compression levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Draft,
    Standard,
    #[default]
    High,
    Lossless,
}

impl Quality {
    pub fn parse(s: &str) -> Option<Quality> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "draft" | "preview" | "low" => Quality::Draft,
            "standard" | "medium" | "normal" => Quality::Standard,
            "high" | "best" => Quality::High,
            "lossless" | "master" => Quality::Lossless,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Quality::Draft => "draft",
            Quality::Standard => "standard",
            Quality::High => "high",
            Quality::Lossless => "lossless",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EncodeOptions {
    /// Output file. For `PngSequence`: a pattern containing `%d`/`%06d`, a `.png` path (frames become
    /// `name_000000.png`, ...), or a folder (frames become `frame_000000.png`). Numbering starts at 0.
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub codec: VideoCodec,
    pub quality: Quality,
    /// Use the GPU encoder (NVENC) when the machine has one; otherwise the software encoder.
    pub hardware: bool,
    /// Audio to mux (WAV written with `write_wav`), ideally exactly as long as the video.
    pub audio: Option<PathBuf>,
    pub audio_bitrate_kbps: u32,
    /// Keep the alpha channel where the format supports it (ProRes 4444, PNG, GIF transparency).
    pub alpha: bool,
}

impl EncodeOptions {
    /// H.264, high quality, hardware when available, 256 kb/s AAC, no audio.
    pub fn new(path: impl Into<PathBuf>, width: u32, height: u32, fps: f64) -> EncodeOptions {
        EncodeOptions {
            path: path.into(),
            width,
            height,
            fps,
            codec: VideoCodec::H264,
            quality: Quality::High,
            hardware: true,
            audio: None,
            audio_bitrate_kbps: 256,
            alpha: false,
        }
    }
}

/// Streams frames into an ffmpeg encoder. Call `push` once per frame, then `finish`.
/// Dropping without `finish` aborts: the child is killed and the partial output deleted.
pub struct Encoder {
    opts: EncodeOptions,
    encoder_name: String,
    child: ChildGuard,
    tx: Option<SyncSender<Vec<u8>>>,
    writer: Option<JoinHandle<std::io::Result<()>>>,
    write_error: Arc<Mutex<Option<String>>>,
    pump: Option<StderrPump>,
    /// Where ffmpeg writes; renamed to `opts.path` on success (`None` for PNG sequences).
    temp: Option<PathBuf>,
    /// PNG sequences: the temporary folder and where its frames go on success.
    seq: Option<SeqOut>,
    /// GIF: the second pass to run in `finish`.
    gif: Option<GifPass>,
    /// Encoded frame size (the input size, padded to even for 4:2:0 / 4:2:2 codecs).
    out_size: (u32, u32),
    frames: u64,
    frame_bytes: usize,
    done: bool,
}

struct SeqOut {
    temp_dir: PathBuf,
    final_dir: PathBuf,
    /// File name pattern ("frame_%06d.png").
    pattern: String,
}

struct GifPass {
    _dir: crate::util::TempDir,
    frames: PathBuf,
    palette: PathBuf,
    filter: String,
}

struct Plan {
    encoder: String,
    args: Vec<String>,
    pix_fmt: &'static str,
    yuv: bool,
    hardware: bool,
}

/// Bitrate cap in Mbit/s used by the constant-quality presets (`None` for lossless, ProRes, GIF
/// and PNG). `high` allows about 2.5x YouTube's recommended upload bitrate for the size and frame
/// rate (1080p30 = 20, 1080p60 = 30, 2160p30 = 60 Mbit/s for H.264), `standard` 70 % and `draft`
/// 45 % of that; HEVC gets 0.7x and AV1 0.6x of the H.264 cap (more efficient codecs). The cap only
/// limits peaks: calm footage stays well below it at the preset's constant quality.
pub fn bitrate_cap_mbps(codec: VideoCodec, quality: Quality, width: u32, height: u32, fps: f64) -> Option<f64> {
    let codec_factor = match codec {
        VideoCodec::H264 => 1.0,
        VideoCodec::Hevc => 0.7,
        VideoCodec::Av1 => 0.6,
        _ => return None,
    };
    let quality_factor = match quality {
        Quality::Draft => 0.45,
        Quality::Standard => 0.7,
        Quality::High => 1.0,
        Quality::Lossless => return None,
    };
    let pixels = (width.max(1) as f64 * height.max(1) as f64) / (1920.0 * 1080.0);
    let rate = if fps.is_finite() && fps > 0.0 { fps.clamp(1.0, 240.0) } else { 30.0 } / 30.0;
    Some((20.0 * pixels.powf(0.79) * rate.powf(0.585) * codec_factor * quality_factor).max(1.0))
}

/// Encoders to try, best first: the GPU encoder with all its options, the GPU encoder with basic
/// options (older GPUs lack B-frame references / temporal AQ), then the software encoder. GPU
/// plans are only included when `use_hw` and the encoder exists; `start` test-opens each at the
/// real size and takes the first that works.
fn plans(o: &EncodeOptions, use_hw: bool) -> Result<Vec<Plan>> {
    let t = tools()?;
    let q = o.quality;
    let hw = |name: &str| use_hw && t.hardware_encoder_works(name);
    let soft = |name: &str| t.has_encoder(name);
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let gop = ((o.fps * 2.0).round() as u32).max(1).to_string();
    let lossless_420 = |p: &'static str| if q == Quality::Lossless { "yuv444p" } else { p };
    let (ow, oh) = if o.codec.needs_even_size() { (o.width.div_ceil(2) * 2, o.height.div_ceil(2) * 2) } else { (o.width, o.height) };
    // Peak cap (VBV) for the constant-quality modes.
    let cap = bitrate_cap_mbps(o.codec, q, ow, oh, o.fps);
    let vbv = |a: &mut Vec<String>| {
        if let Some(c) = cap {
            let kb = (c * 1000.0).round() as u64;
            a.extend(["-maxrate".to_string(), format!("{kb}k"), "-bufsize".to_string(), format!("{}k", kb * 2)]);
        }
    };
    // NVENC: constant quality (`-rc vbr -cq N -b:v 0`) under the cap; standard/high add spatial and
    // temporal AQ, lookahead and B-frames used as references.
    let nvenc = |name: &'static str, cq: [&str; 3], full: bool| -> Plan {
        let mut a = s(&["-c:v", name]);
        if q == Quality::Lossless {
            a.extend(s(&["-preset", "p7", "-tune", "lossless"]));
        } else {
            // Draft is for speed: fast preset, no AQ/lookahead (measured both faster and no worse).
            match q {
                Quality::Draft => a.extend(s(&["-preset", "p2", "-rc", "vbr", "-cq", cq[0], "-b:v", "0"])),
                Quality::Standard => a.extend(s(&["-preset", "p5", "-tune", "hq", "-rc", "vbr", "-cq", cq[1], "-b:v", "0"])),
                _ => a.extend(s(&["-preset", "p6", "-tune", "hq", "-rc", "vbr", "-cq", cq[2], "-b:v", "0"])),
            }
            vbv(&mut a);
            if q != Quality::Draft {
                a.extend(s(&["-spatial-aq", "1"]));
                if full {
                    let lookahead = if q == Quality::Standard { "20" } else { "32" };
                    a.extend(s(&["-temporal-aq", "1", "-rc-lookahead", lookahead, "-bf", "3", "-b_ref_mode", "middle"]));
                }
            }
            if name == "h264_nvenc" {
                a.extend(s(&["-profile:v", "high"]));
            }
        }
        if name == "hevc_nvenc" {
            a.extend(s(&["-tag:v", "hvc1"]));
        }
        a.extend(s(&["-g", &gop]));
        let pix = if name == "av1_nvenc" { "yuv420p" } else { lossless_420("yuv420p") };
        Plan { encoder: name.into(), args: a, pix_fmt: pix, yuv: true, hardware: true }
    };
    let mut out = Vec::new();
    match o.codec {
        VideoCodec::H264 => {
            if hw("h264_nvenc") {
                out.push(nvenc("h264_nvenc", ["26", "22", "18"], true));
                out.push(nvenc("h264_nvenc", ["26", "22", "18"], false));
            }
            if soft("libx264") {
                let mut a = s(&["-c:v", "libx264"]);
                match q {
                    Quality::Draft => a.extend(s(&["-preset", "veryfast", "-crf", "23"])),
                    Quality::Standard => a.extend(s(&["-preset", "medium", "-crf", "20"])),
                    Quality::High => a.extend(s(&["-preset", "slow", "-crf", "18"])),
                    Quality::Lossless => a.extend(s(&["-preset", "veryfast", "-qp", "0"])),
                }
                vbv(&mut a);
                if q != Quality::Lossless {
                    a.extend(s(&["-profile:v", "high"]));
                }
                a.extend(s(&["-g", &gop]));
                out.push(Plan { encoder: "libx264".into(), args: a, pix_fmt: lossless_420("yuv420p"), yuv: true, hardware: false });
            }
            if out.is_empty() {
                return Err(missing_encoder("H.264", "libx264 or h264_nvenc"));
            }
        }
        VideoCodec::Hevc => {
            if hw("hevc_nvenc") {
                out.push(nvenc("hevc_nvenc", ["28", "24", "20"], true));
                out.push(nvenc("hevc_nvenc", ["28", "24", "20"], false));
            }
            if soft("libx265") {
                let mut a = s(&["-c:v", "libx265"]);
                match q {
                    Quality::Draft => a.extend(s(&["-preset", "fast", "-crf", "27", "-x265-params", "log-level=error"])),
                    Quality::Standard => a.extend(s(&["-preset", "medium", "-crf", "23", "-x265-params", "log-level=error"])),
                    Quality::High => a.extend(s(&["-preset", "slow", "-crf", "20", "-x265-params", "log-level=error"])),
                    Quality::Lossless => a.extend(s(&["-preset", "medium", "-x265-params", "lossless=1:log-level=error"])),
                }
                vbv(&mut a);
                a.extend(s(&["-tag:v", "hvc1", "-g", &gop]));
                out.push(Plan { encoder: "libx265".into(), args: a, pix_fmt: lossless_420("yuv420p"), yuv: true, hardware: false });
            }
            if out.is_empty() {
                return Err(missing_encoder("HEVC", "libx265 or hevc_nvenc"));
            }
        }
        VideoCodec::Av1 => {
            if hw("av1_nvenc") {
                out.push(nvenc("av1_nvenc", ["34", "30", "26"], true));
                out.push(nvenc("av1_nvenc", ["34", "30", "26"], false));
            }
            if soft("libsvtav1") {
                let mut a = s(&["-c:v", "libsvtav1"]);
                match q {
                    Quality::Draft => a.extend(s(&["-preset", "10", "-crf", "36"])),
                    Quality::Standard => a.extend(s(&["-preset", "7", "-crf", "30"])),
                    Quality::High => a.extend(s(&["-preset", "5", "-crf", "26"])),
                    Quality::Lossless => a.extend(s(&["-preset", "8", "-svtav1-params", "lossless=1"])),
                }
                vbv(&mut a);
                a.extend(s(&["-g", &gop]));
                out.push(Plan { encoder: "libsvtav1".into(), args: a, pix_fmt: "yuv420p", yuv: true, hardware: false });
            } else if soft("libaom-av1") {
                let crf = match q {
                    Quality::Draft => "38",
                    Quality::Standard => "30",
                    Quality::High => "26",
                    Quality::Lossless => "0",
                };
                let mut a = s(&["-c:v", "libaom-av1", "-cpu-used", "6", "-row-mt", "1", "-crf", crf, "-b:v", "0"]);
                vbv(&mut a);
                a.extend(s(&["-g", &gop]));
                out.push(Plan { encoder: "libaom-av1".into(), args: a, pix_fmt: "yuv420p", yuv: true, hardware: false });
            }
            if out.is_empty() {
                return Err(missing_encoder("AV1", "av1_nvenc, libsvtav1 or libaom-av1"));
            }
        }
        VideoCodec::ProRes => {
            if !soft("prores_ks") {
                return Err(missing_encoder("ProRes", "prores_ks"));
            }
            let (profile, pix) = match (q, o.alpha) {
                (_, true) => ("4", "yuva444p10le"),
                (Quality::Draft, _) => ("0", "yuv422p10le"),
                (Quality::Standard, _) => ("2", "yuv422p10le"),
                (Quality::High, _) => ("3", "yuv422p10le"),
                (Quality::Lossless, _) => ("4", "yuv444p10le"),
            };
            let a = s(&["-c:v", "prores_ks", "-profile:v", profile, "-vendor", "apl0"]);
            out.push(Plan { encoder: "prores_ks".into(), args: a, pix_fmt: pix, yuv: true, hardware: false });
        }
        VideoCodec::Gif => out.push(Plan { encoder: "gif".into(), args: s(&["-c:v", "gif", "-loop", "0"]), pix_fmt: "pal8", yuv: false, hardware: false }),
        VideoCodec::PngSequence => {
            let level = match q {
                Quality::Draft => "1",
                Quality::Standard | Quality::High => "3",
                Quality::Lossless => "6",
            };
            let a = s(&["-c:v", "png", "-compression_level", level]);
            out.push(Plan { encoder: "png".into(), args: a, pix_fmt: if o.alpha { "rgba" } else { "rgb24" }, yuv: false, hardware: false });
        }
    }
    Ok(out)
}

fn missing_encoder(what: &str, names: &str) -> MediaError {
    MediaError::ToolMissing {
        tool: "ffmpeg",
        help: format!("this ffmpeg build has no {what} encoder ({names}); install a full build such as `winget install Gyan.FFmpeg`"),
    }
}

/// The ffmpeg output pattern for a PNG sequence.
fn png_pattern(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.contains('%') {
        return path.to_path_buf();
    }
    match path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()) {
        Some(e) if e == "png" => {
            let stem = path.file_stem().map(|x| x.to_string_lossy().into_owned()).unwrap_or_else(|| "frame".into());
            path.with_file_name(format!("{stem}_%06d.png"))
        }
        _ => path.join("frame_%06d.png"),
    }
}

/// Maximum frame size of a GPU encoder (NVENC: H.264 4096, HEVC/AV1 8192 on current GPUs).
fn hw_max_size(encoder: &str) -> u32 {
    match encoder {
        "h264_nvenc" => 4096,
        _ => 8192,
    }
}

/// Whether the planned GPU encoder actually opens at this size and pixel format (a 2-frame test
/// encode with the real arguments; cached).
fn hw_opens(p: &Plan, w: u32, h: u32, fps: (u32, u32)) -> bool {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    let max = hw_max_size(&p.encoder);
    if w > max || h > max {
        return false;
    }
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let key = format!("{}|{}|{w}x{h}|{:?}", p.encoder, p.pix_fmt, p.args);
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = crate::tools::lock(cache).get(&key) {
        return *v;
    }
    let ok = (|| {
        let mut c = ffmpeg("error").ok()?;
        c.args(["-f", "lavfi", "-i", &format!("color=c=black:s={w}x{h}:r={}/{}:d=1", fps.0, fps.1)]);
        c.args(["-frames:v", "2", "-vf", &format!("format={}", p.pix_fmt)]);
        c.args(&p.args);
        c.args(["-f", "null", "-"]).stdout(Stdio::null()).stderr(Stdio::null());
        c.status().ok().map(|st| st.success())
    })()
    .unwrap_or(false);
    crate::tools::lock(cache).insert(key, ok);
    ok
}

/// Splits "frame_%06d.png" into ("frame_", ".png"); `None` for other patterns.
fn pattern_parts(pattern: &str) -> Option<(String, String)> {
    let i = pattern.find('%')?;
    let rest = &pattern[i + 1..];
    let d = rest.find('d')?;
    if !rest[..d].chars().all(|c| c.is_ascii_digit()) || rest[d + 1..].contains('%') {
        return None;
    }
    Some((pattern[..i].to_string(), rest[d + 1..].to_string()))
}

fn matches_pattern(name: &str, parts: &(String, String)) -> bool {
    name.len() > parts.0.len() + parts.1.len()
        && name.starts_with(&parts.0)
        && name.ends_with(&parts.1)
        && name[parts.0.len()..name.len() - parts.1.len()].chars().all(|c| c.is_ascii_digit())
}

/// A temporary folder next to `dir` (same volume, so its files can be renamed into `dir`).
fn temp_folder_for(dir: &Path) -> PathBuf {
    let base = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let name = base.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "frames".into());
    let sibling = base.with_file_name(format!(".{name}"));
    let t = temp_sibling(&sibling);
    if base.parent().is_some_and(|p| !p.as_os_str().is_empty()) {
        t
    } else {
        base.join(t.file_name().unwrap_or_default())
    }
}

impl Encoder {
    pub fn start(opts: &EncodeOptions) -> Result<Encoder> {
        let o = opts.clone();
        if o.width == 0 || o.height == 0 || o.width > 16384 || o.height > 16384 {
            return Err(MediaError::InvalidInput(format!("frame size {}x{} is out of range (1..16384)", o.width, o.height)));
        }
        if !(o.fps.is_finite() && o.fps > 0.0 && o.fps <= 1000.0) {
            return Err(MediaError::InvalidInput(format!("frame rate {} is out of range (0..1000]", o.fps)));
        }
        if let Some(a) = o.audio.as_ref().filter(|_| o.codec.takes_audio()) {
            if !a.is_file() {
                return Err(MediaError::NotFound { path: a.clone() });
            }
            let (_, info) = crate::probe::probe(a)?;
            if !info.has_audio {
                return Err(MediaError::unsupported(a, "was given as the export's audio but has no audio stream"));
            }
        }
        let (num, den) = fps_rational(o.fps);
        let even = o.codec.needs_even_size();
        let out_size = if even { (o.width.div_ceil(2) * 2, o.height.div_ceil(2) * 2) } else { (o.width, o.height) };
        // First plan that opens: a GPU encoder may be unable to at this size (H.264 NVENC is limited
        // to 4096 px) or lack some options on older GPUs; the software encoder is the last resort.
        let candidates = plans(&o, o.hardware)?;
        let n = candidates.len();
        let p = candidates
            .into_iter()
            .enumerate()
            .find(|(i, p)| !p.hardware || *i + 1 == n || hw_opens(p, out_size.0, out_size.1, (num, den)))
            .map(|(_, p)| p)
            .ok_or_else(|| MediaError::InvalidInput("no usable encoder".into()))?;

        // Output location.
        let mut seq = None;
        let mut gif = None;
        let (target, temp) = if o.codec == VideoCodec::PngSequence {
            let pat = png_pattern(&o.path);
            let final_dir = pat.parent().map(Path::to_path_buf).unwrap_or_default();
            let pattern = pat.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "frame_%06d.png".into());
            let temp_dir = temp_folder_for(&final_dir);
            std::fs::create_dir_all(&temp_dir).map_err(|e| MediaError::io(&temp_dir, "create the output folder", &e))?;
            let target = temp_dir.join(&pattern);
            seq = Some(SeqOut { temp_dir, final_dir, pattern });
            (target, None)
        } else {
            if let Some(dir) = o.path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir).map_err(|e| MediaError::io(dir, "create the output folder", &e))?;
            }
            // Keep the extension last so ffmpeg picks the right muxer.
            let ext = o.path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| o.codec.extension().to_string());
            let tmp = temp_sibling(&o.path).with_extension(format!("part.{ext}"));
            if o.codec == VideoCodec::Gif {
                let dir = crate::util::TempDir::new("aiframes-gif")?;
                let dither = if o.quality == Quality::Draft { "bayer:bayer_scale=4" } else { "sierra2_4a" };
                let thr = if o.alpha { ":alpha_threshold=128" } else { "" };
                gif = Some(GifPass {
                    frames: dir.0.join("frames.mkv"),
                    palette: dir.0.join("palette.png"),
                    filter: format!("[0:v][1:v]paletteuse=dither={dither}{thr}[out]"),
                    _dir: dir,
                });
            }
            (tmp.clone(), Some(tmp))
        };

        let mut a: Vec<OsString> = Vec::new();
        let mut arg = |x: &str| a.push(OsString::from(x));
        for x in ["-y", "-f", "rawvideo", "-pix_fmt", "rgba", "-s", &format!("{}x{}", o.width, o.height)] {
            arg(x);
        }
        for x in ["-framerate", &format!("{num}/{den}"), "-i", "pipe:0"] {
            arg(x);
        }
        let with_audio = o.audio.is_some() && o.codec.takes_audio();
        if let Some(au) = o.audio.as_ref().filter(|_| with_audio) {
            a.push("-i".into());
            a.push(ff_path(au));
        }
        let mut arg = |x: &str| a.push(OsString::from(x));

        // Filters: even size for 4:2:0/4:2:2, colour conversion, palette for GIF.
        let pad = if o.codec.needs_even_size() && (o.width % 2 == 1 || o.height % 2 == 1) { "pad=ceil(iw/2)*2:ceil(ih/2)*2," } else { "" };
        match o.codec {
            VideoCodec::Gif => {
                // Pass 1: palette statistics (bounded memory) + the frames to a lossless file.
                let reserve = if o.alpha { ":reserve_transparent=1" } else { "" };
                let fc = format!("[0:v]split[a][b];[a]palettegen=max_colors=256:stats_mode=full{reserve}[p]");
                for x in ["-filter_complex", &fc, "-map", "[p]", "-update", "1", "-frames:v", "1"] {
                    arg(x);
                }
            }
            _ if p.yuv => {
                let vf = format!("{pad}scale=out_color_matrix=bt709:out_range=tv:flags=bicubic+accurate_rnd,format={}", p.pix_fmt);
                for x in ["-map", "0:v:0", "-vf", &vf] {
                    arg(x);
                }
                for x in ["-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709", "-color_range", "tv"] {
                    arg(x);
                }
            }
            _ => {
                for x in ["-map", "0:v:0", "-vf", &format!("format={}", p.pix_fmt)] {
                    arg(x);
                }
            }
        }
        if with_audio {
            for x in ["-map", "1:a:0"] {
                arg(x);
            }
        }
        if o.codec != VideoCodec::Gif {
            for x in &p.args {
                arg(x);
            }
        }
        if p.yuv {
            for x in ["-pix_fmt", p.pix_fmt] {
                arg(x);
            }
        }
        if with_audio {
            if o.codec == VideoCodec::ProRes {
                for x in ["-c:a", "pcm_s24le"] {
                    arg(x);
                }
            } else {
                for x in ["-c:a", "aac", "-b:a", &format!("{}k", o.audio_bitrate_kbps.clamp(32, 1024))] {
                    arg(x);
                }
            }
        }
        let ext = target.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if matches!(ext.as_str(), "mp4" | "mov" | "m4v") {
            for x in ["-movflags", "+faststart"] {
                arg(x);
            }
        }
        if o.codec == VideoCodec::PngSequence {
            for x in ["-start_number", "0", "-f", "image2"] {
                arg(x);
            }
        }
        match &gif {
            Some(g) => {
                a.push(ff_path(&g.palette));
                for x in ["-map", "[b]", "-c:v", "ffv1", "-level", "3", "-slices", "4", "-pix_fmt", "bgra"] {
                    a.push(x.into());
                }
                a.push(ff_path(&g.frames));
            }
            None => a.push(ff_path(&target)),
        }
        let mut c = ffmpeg("warning")?;
        c.args(&a);
        c.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());

        let child = c.spawn().map_err(|e| spawn_error("ffmpeg", e))?;
        let mut child = ChildGuard::new(child);
        let stdin: ChildStdin = child.child().stdin.take().expect("stdin piped");
        let pump = StderrPump::start(child.child().stderr.take().expect("stderr piped"), |_| false);
        let (tx, rx) = sync_channel::<Vec<u8>>(3);
        let write_error = Arc::new(Mutex::new(None));
        let we = write_error.clone();
        let writer = std::thread::Builder::new()
            .name("ffmpeg-encode-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                for frame in rx {
                    if let Err(e) = stdin.write_all(&frame) {
                        *crate::tools::lock(&we) = Some(e.to_string());
                        return Err(e);
                    }
                }
                stdin.flush()
            })
            .map_err(|e| MediaError::InvalidInput(format!("could not start the encoder thread: {e}")))?;
        Ok(Encoder {
            frame_bytes: o.width as usize * o.height as usize * 4,
            opts: o,
            encoder_name: p.encoder,
            child,
            tx: Some(tx),
            writer: Some(writer),
            write_error,
            pump: Some(pump),
            temp,
            seq,
            gif,
            out_size,
            frames: 0,
            done: false,
        })
    }

    /// The size actually encoded: the input size, padded with black to even dimensions for
    /// 4:2:0 / 4:2:2 codecs (H.264, HEVC, AV1, ProRes).
    pub fn output_size(&self) -> (u32, u32) {
        self.out_size
    }

    /// Which ffmpeg encoder is used (e.g. `h264_nvenc`, `libx264`).
    pub fn encoder_name(&self) -> &str {
        &self.encoder_name
    }

    /// Frames pushed so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Queues one straight-RGBA frame of exactly `width * height * 4` bytes.
    pub fn push(&mut self, rgba: &[u8]) -> Result<()> {
        self.push_owned(rgba.to_vec())
    }

    /// Like `push` without copying the frame.
    pub fn push_owned(&mut self, rgba: Vec<u8>) -> Result<()> {
        if rgba.len() != self.frame_bytes {
            return Err(MediaError::InvalidInput(format!(
                "frame has {} bytes but {}x{} RGBA needs {}",
                rgba.len(),
                self.opts.width,
                self.opts.height,
                self.frame_bytes
            )));
        }
        let sent = self.tx.as_ref().map(|tx| tx.send(rgba).is_ok()).unwrap_or(false);
        if !sent {
            return Err(self.failure("encode the video (ffmpeg stopped reading frames)"));
        }
        self.frames += 1;
        Ok(())
    }

    fn failure(&mut self, action: &str) -> MediaError {
        self.tx = None;
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
        let status = self.child.wait().ok();
        let tail = self.pump.take().map(|p| p.finish()).unwrap_or_default();
        let mut msg = tail.summary();
        if let Some(e) = crate::tools::lock(&self.write_error).take() {
            if msg.is_empty() {
                msg = e;
            }
        }
        MediaError::tool("ffmpeg", Some(&self.opts.path), action, status.and_then(|s| s.code()), msg)
    }

    /// Waits for ffmpeg to finish writing and moves the output into place.
    pub fn finish(mut self) -> Result<()> {
        if self.frames == 0 {
            return Err(MediaError::InvalidInput(format!("no frames were pushed for {}", self.opts.path.display())));
        }
        self.tx = None; // closes stdin once the writer drains the queue
        let write_ok = self.writer.take().map(|w| w.join().map(|r| r.is_ok()).unwrap_or(false)).unwrap_or(true);
        let status = self.child.wait().map_err(|e| MediaError::io(&self.opts.path, "wait for ffmpeg", &e))?;
        let tail = self.pump.take().map(|p| p.finish()).unwrap_or_default();
        if !status.success() || !write_ok {
            let mut msg = tail.summary();
            if let Some(e) = crate::tools::lock(&self.write_error).take() {
                if msg.is_empty() {
                    msg = e;
                }
            }
            return Err(MediaError::tool("ffmpeg", Some(&self.opts.path), "encode the video", status.code(), msg));
        }
        if let Some(g) = &self.gif {
            // Pass 2: map the frames onto the palette.
            let tmp = self.temp.clone().unwrap_or_default();
            let mut c = ffmpeg("error")?;
            c.args(["-y", "-i"]).arg(ff_path(&g.frames)).arg("-i").arg(ff_path(&g.palette));
            c.args(["-filter_complex", &g.filter, "-map", "[out]", "-c:v", "gif", "-loop", "0", "-f", "gif"]).arg(ff_path(&tmp));
            if let Err(e) = crate::tools::run_capture(c, "ffmpeg", Some(&self.opts.path), "write the GIF") {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
        if let Some(seq) = self.seq.take() {
            let r = Self::swap_in(&seq);
            let _ = std::fs::remove_dir_all(&seq.temp_dir);
            r?;
        }
        if let Some(tmp) = self.temp.take() {
            let ok = std::fs::metadata(&tmp).map(|m| m.len() > 0).unwrap_or(false);
            if !ok {
                let _ = std::fs::remove_file(&tmp);
                return Err(MediaError::tool("ffmpeg", Some(&self.opts.path), "encode the video", status.code(), "it wrote an empty file"));
            }
            if let Err(e) = std::fs::rename(&tmp, &self.opts.path) {
                let _ = std::fs::remove_file(&tmp);
                return Err(MediaError::io(&self.opts.path, "replace (is it open in another program?)", &e));
            }
        }
        self.done = true;
        Ok(())
    }

    /// Moves a finished PNG sequence into its folder, deleting the previous export's frames that
    /// match the same pattern (other files in the folder are left alone).
    fn swap_in(seq: &SeqOut) -> Result<()> {
        let dir = if seq.final_dir.as_os_str().is_empty() { PathBuf::from(".") } else { seq.final_dir.clone() };
        std::fs::create_dir_all(&dir).map_err(|e| MediaError::io(&dir, "create the output folder", &e))?;
        let new: Vec<std::ffi::OsString> = std::fs::read_dir(&seq.temp_dir)
            .map_err(|e| MediaError::io(&seq.temp_dir, "read the frames", &e))?
            .flatten()
            .map(|e| e.file_name())
            .collect();
        if let Some(parts) = pattern_parts(&seq.pattern) {
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let name = e.file_name();
                    if !new.contains(&name) && matches_pattern(&name.to_string_lossy(), &parts) {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
        for name in &new {
            let to = dir.join(name);
            std::fs::rename(seq.temp_dir.join(name), &to).map_err(|e| MediaError::io(&to, "replace (is it open in another program?)", &e))?;
        }
        Ok(())
    }

    /// Stops encoding and deletes the partial output.
    pub fn abort(mut self) {
        self.cleanup();
    }

    fn cleanup(&mut self) {
        self.tx = None;
        self.child.kill();
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
        if let Some(tmp) = self.temp.take() {
            let _ = std::fs::remove_file(tmp);
        }
        if let Some(seq) = self.seq.take() {
            let _ = std::fs::remove_dir_all(&seq.temp_dir);
        }
        self.gif = None; // its temporary folder is removed on drop
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.done {
            self.cleanup();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_pattern_matching() {
        let parts = pattern_parts("frame_%06d.png").unwrap();
        assert!(matches_pattern("frame_000012.png", &parts));
        assert!(!matches_pattern("frame_.png", &parts));
        assert!(!matches_pattern("frame_00a012.png", &parts));
        assert!(!matches_pattern("notes.txt", &parts));
        assert_eq!(pattern_parts("img%d.png"), Some(("img".into(), ".png".into())));
        assert_eq!(pattern_parts("a%s.png"), None);
        assert_eq!(hw_max_size("h264_nvenc"), 4096);
    }

    #[test]
    fn bitrate_caps() {
        let c = |codec, q, w, h, fps| bitrate_cap_mbps(codec, q, w, h, fps).unwrap();
        assert!((c(VideoCodec::H264, Quality::High, 1920, 1080, 30.0) - 20.0).abs() < 0.01);
        assert!((c(VideoCodec::H264, Quality::High, 1920, 1080, 60.0) - 30.0).abs() < 0.1);
        assert!((c(VideoCodec::H264, Quality::High, 3840, 2160, 30.0) - 59.8).abs() < 0.5);
        assert!((c(VideoCodec::H264, Quality::High, 1080, 1920, 30.0) - 20.0).abs() < 0.01, "Shorts");
        assert!((c(VideoCodec::H264, Quality::Standard, 1920, 1080, 30.0) - 14.0).abs() < 0.01);
        assert!((c(VideoCodec::H264, Quality::Draft, 1920, 1080, 30.0) - 9.0).abs() < 0.01);
        assert!((c(VideoCodec::Hevc, Quality::High, 1920, 1080, 30.0) - 14.0).abs() < 0.01);
        assert!(c(VideoCodec::H264, Quality::High, 64, 36, 30.0) >= 1.0);
        assert_eq!(bitrate_cap_mbps(VideoCodec::H264, Quality::Lossless, 1920, 1080, 30.0), None);
        assert_eq!(bitrate_cap_mbps(VideoCodec::ProRes, Quality::High, 1920, 1080, 30.0), None);
    }
}
