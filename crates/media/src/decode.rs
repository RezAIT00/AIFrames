//! One ffmpeg decode session: a child process streaming raw RGBA frames of one video stream at
//! one output size, starting at a seek point. A reader thread keeps a few frames decoded ahead.
//!
//! Timing: every frame's presentation timestamp comes from a `showinfo` filter at the end of the
//! filter chain (exact integer pts + time base, parsed from stderr), so frame selection is exact
//! for constant and variable frame rate sources alike: the frame for time `t` is the last frame
//! with `pts <= t`.

use crate::error::{MediaError, Result};
use crate::tools::{ff_path, ffmpeg_at, lock, ChildGuard, Priority, StderrPump};
use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::process::{ChildStdout, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Hardware decoding choice for video sessions.
///
/// Measured on a high-end NVIDIA GPU + 24-core CPU (ffmpeg 9): CPU decoding was faster than NVDEC and
/// D3D11VA in every case tried — 1080p and 4K, H.264 and 10-bit HEVC, full size and 960x540
/// previews, sequential and seeking (GPU decode has to download each frame and pays ~100-300 ms of
/// device setup per seek). So `Auto` decodes on the CPU; the GPU modes are for machines with weak CPUs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum HwAccel {
    /// Always decode on the CPU.
    Off,
    /// Currently the CPU (see above); may pick a GPU path where one is measured to be faster.
    #[default]
    Auto,
    /// NVDEC via `-hwaccel cuda` (falls back to the CPU if unavailable).
    Cuda,
    /// D3D11VA (any Windows GPU; falls back to the CPU if unavailable).
    D3d11va,
}

impl HwAccel {
    pub fn parse(s: &str) -> Option<HwAccel> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "cpu" | "0" | "false" => Some(HwAccel::Off),
            "auto" | "" => Some(HwAccel::Auto),
            "cuda" | "nvdec" => Some(HwAccel::Cuda),
            "d3d11va" | "d3d11" => Some(HwAccel::D3d11va),
            _ => None,
        }
    }
    fn args(self) -> &'static [&'static str] {
        match self {
            HwAccel::Cuda => &["-hwaccel", "cuda"],
            HwAccel::D3d11va => &["-hwaccel", "d3d11va"],
            _ => &[],
        }
    }
}

/// What to decode and how.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SessionSpec {
    pub path: PathBuf,
    pub video_index: usize,
    pub out_w: u32,
    pub out_h: u32,
    pub fps: f64,
    /// Force the input YCbCr matrix (untagged sources).
    pub matrix: Option<&'static str>,
    /// Force a decoder (libvpx for WebM alpha).
    pub decoder: Option<&'static str>,
    /// Resolved hardware path (`Off`, `Cuda` or `D3d11va`; never `Auto`).
    pub hw: HwAccel,
    pub threads: Option<u32>,
    /// Frames to keep decoded ahead of the consumer to start with (the pool adjusts it later
    /// with `Session::set_readahead`).
    pub readahead: usize,
    /// Thumbnails decode in the background; playback/export sessions at normal priority.
    pub background: bool,
    /// Opened for a request not known to be sequential (snapshot, first frame of a playback): at
    /// most 8 decoder threads. Measured on a 1080p session whose consumer stalls: 174 MB instead of
    /// 214 MB (312 MB before the output queue limit), still ~600 fps if it turns into playback.
    pub lean: bool,
}

impl SessionSpec {
    pub fn frame_bytes(&self) -> usize {
        self.out_w as usize * self.out_h as usize * 4
    }

    /// Whether a session started with `self` produces the frames `other` asks for (same stream,
    /// size and conversion; thread counts and readahead do not matter).
    pub fn compatible(&self, other: &SessionSpec) -> bool {
        self.path == other.path
            && self.video_index == other.video_index
            && (self.out_w, self.out_h) == (other.out_w, other.out_h)
            && self.fps == other.fps
            && self.matrix == other.matrix
            && self.decoder == other.decoder
            && self.hw == other.hw
    }
}

#[derive(Clone)]
pub(crate) struct DFrame {
    /// Source seconds.
    pub pts: f64,
    pub rgba: Arc<Vec<u8>>,
}

enum Msg {
    Frame(DFrame),
    End,
}

/// Frames decoded ahead, bounded by a capacity that can change while the session runs. Every
/// queued frame's bytes are counted in `mem` (shared by all sessions of a pool).
struct FrameQueue {
    st: Mutex<QState>,
    cv: Condvar,
    cap: AtomicUsize,
    mem: Arc<AtomicUsize>,
    frame_bytes: usize,
}

#[derive(Default)]
struct QState {
    items: VecDeque<Msg>,
    /// The session was dropped: the reader stops.
    closed: bool,
}

impl FrameQueue {
    /// Blocks while full; false once the session is gone.
    fn push(&self, m: Msg) -> bool {
        let mut st = lock(&self.st);
        while !st.closed && matches!(m, Msg::Frame(_)) && st.items.len() >= self.cap.load(Ordering::Relaxed).max(1) {
            st = self.cv.wait_timeout(st, Duration::from_millis(100)).unwrap_or_else(|p| p.into_inner()).0;
        }
        if st.closed {
            return false;
        }
        if matches!(m, Msg::Frame(_)) {
            self.mem.fetch_add(self.frame_bytes, Ordering::Relaxed);
        }
        st.items.push_back(m);
        self.cv.notify_all();
        true
    }

    fn pop(&self) -> Msg {
        let mut st = lock(&self.st);
        loop {
            if let Some(m) = st.items.pop_front() {
                if matches!(m, Msg::Frame(_)) {
                    self.mem.fetch_sub(self.frame_bytes, Ordering::Relaxed);
                }
                self.cv.notify_all();
                return m;
            }
            st = self.cv.wait(st).unwrap_or_else(|p| p.into_inner());
        }
    }

    fn close(&self) {
        let mut st = lock(&self.st);
        st.closed = true;
        let queued = st.items.iter().filter(|m| matches!(m, Msg::Frame(_))).count();
        st.items.clear();
        self.mem.fetch_sub(queued * self.frame_bytes, Ordering::Relaxed);
        self.cv.notify_all();
    }
}

/// Presentation timestamps parsed from stderr, in output order.
#[derive(Default)]
struct PtsState {
    tb: f64,
    queue: VecDeque<Option<f64>>,
    closed: bool,
}

type PtsShared = Arc<(Mutex<PtsState>, Condvar)>;

pub(crate) enum Served {
    Frame {
        frame: DFrame,
        /// Start of the interval this frame is shown for (`-inf` for the first frame of the stream).
        start: f64,
        /// Next frame's pts (`inf` at the end of the stream).
        end: f64,
        /// Frames passed over on the way, with their own `[pts, end)` (for the cache).
        passed: Vec<(DFrame, f64)>,
    },
    /// The frame shown at `t` lies before the seek point (variable frame rate gap): seek earlier.
    TooEarly,
    /// `t` is before the session's current frame.
    Behind,
    /// No frames at all from this seek point (past the end of the stream).
    Empty,
}

/// Frames passed over during one advance that are offered to the cache (more when stepping backward).
pub(crate) const KEEP_PASSED: usize = 4;

pub(crate) struct Session {
    pub spec: Arc<SessionSpec>,
    pub seek: f64,
    child: ChildGuard,
    queue: Arc<FrameQueue>,
    pump: Option<StderrPump>,
    cur: Option<DFrame>,
    next: Option<DFrame>,
    eof: bool,
    /// `cur` is the first frame decoded from the start of the file (serves any earlier time).
    at_stream_start: bool,
    pub frames: u64,
    /// The owning pool's live-session counter (decremented on drop).
    live: Option<Arc<AtomicUsize>>,
    /// Bytes of `cur` + `next` currently counted in the pool's session memory.
    held: usize,
}

impl Session {
    pub fn start(spec: Arc<SessionSpec>, seek: f64) -> Result<Session> {
        Self::start_counted(spec, seek, Arc::new(AtomicUsize::new(0)))
    }

    /// Starts a session whose readahead and held frames are counted in `mem`.
    pub fn start_counted(spec: Arc<SessionSpec>, seek: f64, mem: Arc<AtomicUsize>) -> Result<Session> {
        let seek = if seek.is_finite() { seek.max(0.0) } else { 0.0 };
        let mut c = ffmpeg_at("info", if spec.background { Priority::Background } else { Priority::Normal })?;
        c.args(spec.hw.args());
        match spec.threads {
            Some(n) => {
                c.args(["-threads", &n.to_string()]);
            }
            None if spec.lean => {
                c.args(["-threads", "8"]);
            }
            None => {}
        }
        if let Some(d) = spec.decoder {
            c.args(["-c:v", d]);
        }
        if seek > 0.0 {
            c.args(["-ss", &format!("{seek:.6}")]);
        }
        c.arg("-i").arg(ff_path(&spec.path));
        c.args(["-map", &format!("0:{}", spec.video_index), "-an", "-sn", "-dn"]);
        let matrix = spec.matrix.map(|m| format!(":in_color_matrix={m}")).unwrap_or_default();
        let vf = format!(
            "scale={}:{}:flags=bilinear{matrix},format=rgba,setsar=1,showinfo=checksum=0",
            spec.out_w, spec.out_h
        );
        // A one-packet output queue: frames waiting for us are already in our readahead queue, and
        // ffmpeg's default queue held several more 8 MB frames per stalled 1080p session.
        c.args(["-vf", &vf, "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "rgba", "-thread_queue_size", "1", "pipe:1"]);
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = c.spawn().map_err(|e| crate::tools::spawn_error("ffmpeg", e))?;
        let mut child = ChildGuard::new(child);
        let stdout = child.child().stdout.take().expect("stdout piped");
        let stderr = child.child().stderr.take().expect("stderr piped");

        let pts: PtsShared = Arc::new((Mutex::new(PtsState { tb: 0.0, ..Default::default() }), Condvar::new()));
        let p_line = pts.clone();
        let p_close = pts.clone();
        let pump = StderrPump::start_with_close(
            stderr,
            move |line| parse_showinfo(line, &p_line),
            move || {
                let (m, cv) = &*p_close;
                lock(m).closed = true;
                cv.notify_all();
            },
        );

        let frame_bytes = spec.frame_bytes();
        let queue = Arc::new(FrameQueue {
            st: Mutex::new(QState::default()),
            cv: Condvar::new(),
            cap: AtomicUsize::new(spec.readahead.max(1)),
            mem,
            frame_bytes,
        });
        let q2 = queue.clone();
        let fps = spec.fps.max(1.0);
        std::thread::Builder::new()
            .name("ffmpeg-frames".into())
            .spawn(move || reader(stdout, q2, pts, frame_bytes, seek, fps))
            .map_err(|e| MediaError::InvalidInput(format!("could not start a decoder thread: {e}")))?;

        Ok(Session {
            spec,
            seek,
            child,
            queue,
            pump: Some(pump),
            cur: None,
            next: None,
            eof: false,
            at_stream_start: false,
            frames: 0,
            live: None,
            held: 0,
        })
    }

    /// Changes how many frames are decoded ahead (takes effect immediately for growth; a smaller
    /// value lets the queue drain to it).
    pub fn set_readahead(&self, frames: usize) {
        self.queue.cap.store(frames.max(1), Ordering::Relaxed);
        self.queue.cv.notify_all();
    }

    /// Updates the pool's count of bytes held in `cur` / `next`.
    fn account(&mut self) {
        let now = (self.cur.is_some() as usize + self.next.is_some() as usize) * self.spec.frame_bytes();
        if now > self.held {
            self.queue.mem.fetch_add(now - self.held, Ordering::Relaxed);
        } else {
            self.queue.mem.fetch_sub(self.held - now, Ordering::Relaxed);
        }
        self.held = now;
    }

    /// Takes over a place already reserved in `live`, released when the session is dropped.
    pub fn adopt_count(&mut self, live: Arc<AtomicUsize>) {
        self.live = Some(live);
    }

    /// Whether the stream yields any frame from the seek point (decodes the first one, kept).
    pub fn has_frames(&mut self) -> Result<bool> {
        if self.cur.is_some() || self.next.is_some() {
            return Ok(true);
        }
        let r = match self.pull()? {
            Some(f) => {
                self.next = Some(f);
                Ok(true)
            }
            None => Ok(false),
        };
        self.account();
        r
    }

    /// Source position of the session (its current frame, or the seek point before the first frame).
    pub fn position(&self) -> f64 {
        self.cur.as_ref().map(|f| f.pts).unwrap_or(self.seek)
    }

    /// Whether the session can serve times before `position()` (it is at the first frame of the file).
    pub fn serves_earlier(&self) -> bool {
        self.at_stream_start || (self.cur.is_none() && self.seek <= 0.0)
    }

    fn pull(&mut self) -> Result<Option<DFrame>> {
        if let Some(f) = self.next.take() {
            return Ok(Some(f));
        }
        if self.eof {
            return Ok(None);
        }
        match self.queue.pop() {
            Msg::Frame(f) => {
                self.frames += 1;
                Ok(Some(f))
            }
            Msg::End => {
                self.eof = true;
                self.check_exit()?;
                Ok(None)
            }
        }
    }

    /// At the end of the stream: if ffmpeg failed before producing anything, report what it said.
    fn check_exit(&mut self) -> Result<()> {
        let status = self.child.wait();
        let tail = self.pump.take().map(|p| p.finish()).unwrap_or_default();
        if self.frames > 0 {
            return Ok(());
        }
        match status {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(MediaError::tool("ffmpeg", Some(&self.spec.path), "decode video frames", s.code(), tail.summary())),
            Err(e) => Err(MediaError::io(&self.spec.path, "wait for the ffmpeg decoder", &e)),
        }
    }

    /// The frame displayed at source time `t` (the last frame with `pts <= t + eps`). Up to
    /// `keep_passed` frames passed over on the way are returned for caching.
    pub fn frame_at(&mut self, t: f64, eps: f64, keep_passed: usize) -> Result<Served> {
        let r = self.frame_at_inner(t, eps, keep_passed);
        self.account();
        r
    }

    fn frame_at_inner(&mut self, t: f64, eps: f64, keep_passed: usize) -> Result<Served> {
        match &self.cur {
            None => match self.pull()? {
                None => return Ok(Served::Empty),
                Some(f) => {
                    if f.pts > t + eps && self.seek > 0.0 {
                        self.next = Some(f);
                        return Ok(Served::TooEarly);
                    }
                    self.at_stream_start = self.seek <= 0.0;
                    self.cur = Some(f);
                }
            },
            Some(c) => {
                if t + eps < c.pts && !self.at_stream_start {
                    return Ok(Served::Behind);
                }
            }
        }
        let mut passed: VecDeque<(DFrame, f64)> = VecDeque::new();
        loop {
            match self.pull()? {
                Some(n) if n.pts <= t + eps => {
                    let end = n.pts;
                    let old = self.cur.replace(n).expect("current frame");
                    if keep_passed > 0 {
                        if passed.len() == keep_passed {
                            passed.pop_front();
                        }
                        passed.push_back((old, end));
                    }
                    self.at_stream_start = false;
                }
                Some(n) => {
                    self.next = Some(n);
                    break;
                }
                None => break,
            }
        }
        let frame = self.cur.clone().expect("current frame");
        let end = self.next.as_ref().map(|n| n.pts).unwrap_or(f64::INFINITY);
        let start = if self.at_stream_start { f64::NEG_INFINITY } else { frame.pts };
        Ok(Served::Frame { frame, start, end, passed: passed.into() })
    }

    /// The first frame at or after the seek point (one-shot use: thumbnails, stills).
    pub fn first(&mut self) -> Result<Option<DFrame>> {
        let r = self.pull();
        self.account();
        r
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.queue.close();
        self.child.kill();
        self.queue.mem.fetch_sub(self.held, Ordering::Relaxed);
        self.held = 0;
        if let Some(l) = self.live.take() {
            l.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Parses `showinfo` lines; returns true when the line was one of them (kept out of the error tail).
fn parse_showinfo(line: &str, pts: &PtsShared) -> bool {
    if !line.contains("Parsed_showinfo") {
        return false;
    }
    let (m, cv) = &**pts;
    if let Some(i) = line.find("config in time_base:") {
        let rest = line[i + "config in time_base:".len()..].trim_start();
        let tok = rest.split([',', ' ']).next().unwrap_or("");
        if let Some((a, b)) = tok.split_once('/') {
            if let (Ok(a), Ok(b)) = (a.parse::<f64>(), b.parse::<f64>()) {
                if a > 0.0 && b > 0.0 {
                    lock(m).tb = a / b;
                }
            }
        }
        return true;
    }
    if line.contains(" n:") {
        if let Some(i) = line.find(" pts:") {
            let tok = line[i + 5..].split_whitespace().next().unwrap_or("");
            let mut st = lock(m);
            let v = tok.parse::<i64>().ok().filter(|_| st.tb > 0.0).map(|p| p as f64 * st.tb);
            st.queue.push_back(v);
            drop(st);
            cv.notify_all();
        }
    }
    true
}

/// Reads exactly `buf.len()` bytes; `Ok(false)` on a clean end of stream before any byte.
fn read_frame(r: &mut ChildStdout, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                return if filled == 0 { Ok(false) } else { Err(std::io::ErrorKind::UnexpectedEof.into()) };
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn reader(mut stdout: ChildStdout, queue: Arc<FrameQueue>, pts: PtsShared, frame_bytes: usize, seek: f64, fps: f64) {
    let mut last: Option<f64> = None;
    loop {
        let mut buf = vec![0u8; frame_bytes];
        match read_frame(&mut stdout, &mut buf) {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                let _ = queue.push(Msg::End);
                return;
            }
        }
        let rel = {
            let (m, cv) = &*pts;
            let mut st = lock(m);
            loop {
                if let Some(v) = st.queue.pop_front() {
                    break v;
                }
                if st.closed {
                    break None;
                }
                let (g, to) = cv.wait_timeout(st, Duration::from_secs(20)).unwrap_or_else(|p| p.into_inner());
                st = g;
                if to.timed_out() && st.queue.is_empty() {
                    break None;
                }
            }
        };
        let t = match rel {
            Some(r) => seek + r,
            None => last.map(|l| l + 1.0 / fps).unwrap_or(seek),
        };
        // Timestamps must not go backwards for the "last frame with pts <= t" rule.
        let t = match last {
            Some(l) if t < l => l,
            _ => t,
        };
        last = Some(t);
        if !queue.push(Msg::Frame(DFrame { pts: t, rgba: Arc::new(buf) })) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn showinfo_parsing() {
        let pts: PtsShared = Arc::new((Mutex::new(PtsState::default()), Condvar::new()));
        assert!(!parse_showinfo("Input #0, mov,mp4", &pts));
        assert!(parse_showinfo("[Parsed_showinfo_2 @ 000001] config in time_base: 1/15360, frame_rate: 30/1", &pts));
        assert!(parse_showinfo(
            "[Parsed_showinfo_2 @ 000001] n:   0 pts:    358 pts_time:0.0233073 duration:    512 duration_time:0.0333333 fmt:rgba",
            &pts
        ));
        assert!(parse_showinfo("[Parsed_showinfo_2 @ 000001] color_range:pc color_space:gbr", &pts));
        assert!(parse_showinfo("[Parsed_showinfo_2 @ 000001] n:   1 pts:  NOPTS pts_time:NOPTS", &pts));
        let st = lock(&pts.0);
        assert_eq!(st.queue.len(), 2);
        assert!((st.queue[0].unwrap() - 358.0 / 15360.0).abs() < 1e-12);
        assert!(st.queue[1].is_none());
    }
}
