//! Timecode formatting/parsing and frame arithmetic (pure).

/// Tolerance used when snapping times to frames (guards against 0.1 * 30 = 2.9999999).
const SNAP_EPS: f64 = 1e-6;

pub fn safe_fps(fps: f64) -> f64 {
    if fps.is_finite() && fps > 0.0 {
        fps
    } else {
        30.0
    }
}

/// Index of the frame displayed at timeline time `t` (frame `i` covers `[i/fps, (i+1)/fps)`).
pub fn frame_index(t: f64, fps: f64) -> i64 {
    let fps = safe_fps(fps);
    if !t.is_finite() {
        return 0;
    }
    (t * fps + SNAP_EPS).floor() as i64
}

/// Start time of frame `i`.
pub fn frame_time(i: i64, fps: f64) -> f64 {
    i as f64 / safe_fps(fps)
}

/// Rounds `t` down to the start of its frame.
pub fn snap(t: f64, fps: f64) -> f64 {
    frame_time(frame_index(t, fps), fps)
}

/// `HH:MM:SS:FF` (frames counted at the rounded-up nominal rate, e.g. 30 for 29.97).
pub fn timecode(t: f64, fps: f64) -> String {
    let fps = safe_fps(fps);
    let nominal = fps.ceil().max(1.0) as i64;
    let neg = t < 0.0;
    let f = frame_index(t.abs(), fps);
    let total_secs = (f as f64 / fps + SNAP_EPS).floor() as i64;
    let frames = f - (total_secs as f64 * fps).round() as i64;
    let frames = frames.clamp(0, nominal - 1);
    let (h, m, s) = (total_secs / 3600, (total_secs / 60) % 60, total_secs % 60);
    format!("{}{h:02}:{m:02}:{s:02}:{frames:02}", if neg { "-" } else { "" })
}

/// Compact duration: `4.2s`, `1:05.3`, `1:02:03`.
pub fn short_duration(t: f64) -> String {
    let t = if t.is_finite() { t.max(0.0) } else { 0.0 };
    if t < 60.0 {
        format!("{t:.1}s")
    } else if t < 3600.0 {
        let m = (t / 60.0).floor();
        format!("{}:{:04.1}", m as i64, t - m * 60.0)
    } else {
        let s = t.round() as i64;
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    }
}

/// Parses `HH:MM:SS:FF`, `H:MM:SS`, `MM:SS`, `SS.s`, `3s` or `12f` (frames) into seconds.
/// Four fields are always timecode with frames; two or three fields are minutes/hours.
pub fn parse_time(s: &str, fps: f64) -> Option<f64> {
    let fps = safe_fps(fps);
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(f) = s.strip_suffix('f') {
        return f.trim().parse::<f64>().ok().filter(|v| v.is_finite()).map(|v| v / fps);
    }
    let s = s.strip_suffix('s').unwrap_or(s);
    let parts: Vec<&str> = s.split([':', ';']).collect();
    let nums: Option<Vec<f64>> = parts.iter().map(|p| p.trim().parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)).collect();
    let nums = nums?;
    let v = match nums.as_slice() {
        [x] => *x,
        [m, s] => m * 60.0 + s,
        [h, m, s] => h * 3600.0 + m * 60.0 + s,
        [h, m, s, f] => h * 3600.0 + m * 60.0 + s + f / fps.ceil(),
        _ => return None,
    };
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames() {
        assert_eq!(frame_index(0.1, 30.0), 3);
        assert_eq!(frame_index(1.0 / 30.0 * 7.0, 30.0), 7);
        assert_eq!(frame_index(0.0333, 30.0), 0);
        assert!((frame_time(45, 30.0) - 1.5).abs() < 1e-12);
        assert!((snap(1.51, 30.0) - 1.5).abs() < 1e-9);
        assert_eq!(frame_index(f64::NAN, 30.0), 0);
        assert_eq!(frame_index(1.0, 0.0), 30, "bad fps falls back to 30");
    }

    #[test]
    fn timecodes() {
        assert_eq!(timecode(0.0, 30.0), "00:00:00:00");
        assert_eq!(timecode(1.5, 30.0), "00:00:01:15");
        assert_eq!(timecode(3661.0 + 29.0 / 30.0, 30.0), "01:01:01:29");
        assert_eq!(timecode(10.0, 29.97), "00:00:09:29");
        assert_eq!(timecode(-1.0, 25.0), "-00:00:01:00");
        assert_eq!(timecode(2.0, 60.0), "00:00:02:00");
    }

    #[test]
    fn durations() {
        assert_eq!(short_duration(4.24), "4.2s");
        assert_eq!(short_duration(65.3), "1:05.3");
        assert_eq!(short_duration(3723.0), "1:02:03");
    }

    #[test]
    fn parsing() {
        assert_eq!(parse_time("12.5", 30.0), Some(12.5));
        assert_eq!(parse_time("1:30", 30.0), Some(90.0));
        assert_eq!(parse_time("1:00:00", 30.0), Some(3600.0));
        assert_eq!(parse_time("00:00:01:15", 30.0), Some(1.5));
        assert_eq!(parse_time("45f", 30.0), Some(1.5));
        assert_eq!(parse_time("3s", 30.0), Some(3.0));
        assert_eq!(parse_time("abc", 30.0), None);
        assert_eq!(parse_time("", 30.0), None);
        assert_eq!(parse_time("-3", 30.0), None);
    }
}
