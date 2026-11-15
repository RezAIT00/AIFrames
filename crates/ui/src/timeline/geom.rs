//! Pure timeline geometry: time ↔ x mapping, zoom, ruler ticks, track rows (video on top in
//! stacking order, audio below) and hit-testing of clips, transitions and markers.

use aiframes_core::{Id, Project, TrackKind};
use egui::{Pos2, Rect};
use std::collections::HashMap;

/// Most zoomed out (pixels per second).
pub const MIN_ZOOM: f64 = 0.02;

/// Most zoomed in: a frame is 80 px wide.
pub fn max_zoom(fps: f64) -> f64 {
    fps.max(1.0) * 80.0
}

/// Maps timeline seconds to screen x: `x = x0 + (t - scroll) * zoom`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimeMap {
    /// Screen x of the left edge of the track lanes.
    pub x0: f32,
    /// Timeline seconds at `x0`.
    pub scroll: f64,
    /// Pixels per second.
    pub zoom: f64,
}

impl TimeMap {
    pub fn x(&self, t: f64) -> f32 {
        self.x0 + ((t - self.scroll) * self.zoom) as f32
    }
    pub fn t(&self, x: f32) -> f64 {
        self.scroll + (x - self.x0) as f64 / self.zoom
    }
    /// Seconds covered by `px` pixels.
    pub fn secs(&self, px: f32) -> f64 {
        px as f64 / self.zoom
    }
}

/// `(scroll, zoom)` after zooming by `factor` around screen `x`: the time under `x` stays put.
pub fn zoom_at(map: TimeMap, x: f32, factor: f64, min: f64, max: f64) -> (f64, f64) {
    let t = map.t(x);
    let z = (map.zoom * factor).clamp(min, max);
    let scroll = t - (x - map.x0) as f64 / z;
    (scroll.max(0.0), z)
}

/// `(scroll, zoom)` that shows `[0, duration]` in `width` pixels with a small margin.
/// (round 4) The scroll/zoom that shows `[t0, t1]` with the least change from `(scroll, zoom)`: the
/// same view when it is visible, a scroll when it fits, else a zoom out to it (5 % margins).
pub fn reveal(scroll: f64, zoom: f64, width: f64, t0: f64, t1: f64, min: f64, max: f64) -> (f64, f64) {
    let (a, b) = (t0.min(t1).max(0.0), t0.max(t1).max(0.0));
    let width = width.max(1.0);
    let view = width / zoom.max(1e-9);
    if a >= scroll - 1e-9 && b <= scroll + view + 1e-9 {
        return (scroll, zoom);
    }
    let margin = view * 0.05;
    if b - a + 2.0 * margin <= view {
        let s = if a < scroll { a - margin } else { b + margin - view };
        return (s.max(0.0), zoom);
    }
    let z = (width / ((b - a).max(1e-3) * 1.1)).clamp(min, max);
    ((a - (b - a) * 0.05).max(0.0), z)
}

/// (round 4) The vertical scroll that shows rows `[top, bottom)` (content coordinates) with the least
/// change: unchanged when visible, else just enough; when they cannot fit, the top is shown.
pub fn reveal_rows(scroll_y: f32, view_h: f32, top: f32, bottom: f32) -> f32 {
    if top >= scroll_y && bottom <= scroll_y + view_h {
        scroll_y
    } else if bottom - top > view_h || top < scroll_y {
        top
    } else {
        bottom - view_h
    }
}

pub fn fit(duration: f64, width: f32, min: f64, max: f64) -> (f64, f64) {
    let d = duration.max(1.0);
    let z = (width.max(50.0) as f64 * 0.94 / d).clamp(min, max);
    (0.0, z)
}

/// Ruler tick spacing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ticks {
    /// Seconds between labelled ticks.
    pub major: f64,
    /// Seconds between small ticks.
    pub minor: f64,
    /// Labels need frames (major step below a second).
    pub frames: bool,
}

/// Picks "nice" tick steps so labels are at least `min_major_px` apart.
pub fn ticks(zoom: f64, fps: f64, min_major_px: f64) -> Ticks {
    let fps = crate::util::exact_fps(fps.max(1.0));
    let nominal = fps.round().max(1.0);
    let mut cands: Vec<(f64, bool)> = [1.0, 2.0, 5.0, 10.0, 15.0].iter().filter(|&&f| f < nominal).map(|&f| (f / fps, true)).collect();
    cands.extend([1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0, 14_400.0, 36_000.0].iter().map(|&s| (s, false)));
    let (major, frames) = cands.iter().copied().find(|(s, _)| s * zoom >= min_major_px).unwrap_or((36_000.0, false));
    let minor = [10.0, 5.0, 4.0, 2.0, 1.0]
        .iter()
        .map(|k| major / k)
        .find(|m| m * zoom >= 7.0 && (!frames || (m * fps - (m * fps).round()).abs() < 1e-6))
        .unwrap_or(major);
    Ticks { major, minor, frames }
}

/// The label of a major tick.
pub fn tick_label(t: f64, ticks: &Ticks, fps: f64) -> String {
    let n = crate::util::nominal_fps(crate::util::exact_fps(fps));
    let frames_total = crate::util::frame_index(t.max(0.0), fps).max(0) as u64;
    let secs = frames_total / n;
    let ff = frames_total % n;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if ticks.frames {
        if h > 0 {
            format!("{h}:{m:02}:{s:02}:{ff:02}")
        } else {
            format!("{m:02}:{s:02}:{ff:02}")
        }
    } else if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Every multiple of `step` in `[t0, t1]`.
pub fn tick_times(t0: f64, t1: f64, step: f64) -> Vec<f64> {
    if step.is_nan() || step <= 0.0 || t1 < t0 {
        return vec![];
    }
    let first = (t0 / step).floor() as i64;
    let last = (t1 / step).ceil() as i64;
    if last - first > 5000 {
        return vec![];
    }
    (first..=last).map(|i| i as f64 * step).filter(|t| *t >= 0.0).collect()
}

// ---------------------------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------------------------

pub const DEFAULT_VIDEO_H: f32 = 62.0;
pub const DEFAULT_AUDIO_H: f32 = 46.0;
pub const MIN_ROW_H: f32 = 22.0;
pub const MAX_ROW_H: f32 = 220.0;
/// Gap between the video and the audio tracks.
pub const DIVIDER: f32 = 5.0;

/// One displayed track row. `y` is relative to the top of the track area (before vertical scroll).
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub track: Id,
    /// Index in `project.tracks`.
    pub index: usize,
    pub kind: TrackKind,
    pub y: f32,
    pub h: f32,
}

impl Row {
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
}

/// Rows in display order: video tracks with the topmost layer (highest index) first, then a divider,
/// then audio tracks in project order.
pub fn layout_rows(p: &Project, heights: &HashMap<Id, f32>) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut y = 0.0;
    let video: Vec<usize> = (0..p.tracks.len()).rev().filter(|&i| p.tracks[i].kind == TrackKind::Video).collect();
    let audio: Vec<usize> = (0..p.tracks.len()).filter(|&i| p.tracks[i].kind == TrackKind::Audio).collect();
    for (k, list) in [video, audio].into_iter().enumerate() {
        if k == 1 && !rows.is_empty() && !list.is_empty() {
            y += DIVIDER;
        }
        for i in list {
            let t = &p.tracks[i];
            let def = if t.kind == TrackKind::Video { DEFAULT_VIDEO_H } else { DEFAULT_AUDIO_H };
            let h = heights.get(&t.id).copied().unwrap_or(def).clamp(MIN_ROW_H, MAX_ROW_H);
            rows.push(Row { track: t.id.clone(), index: i, kind: t.kind, y, h });
            y += h;
        }
    }
    rows
}

/// Bottom of the last row.
pub fn rows_height(rows: &[Row]) -> f32 {
    rows.last().map(|r| r.bottom()).unwrap_or(0.0)
}

/// The row at `y` (track-area coordinates).
pub fn row_at(rows: &[Row], y: f32) -> Option<usize> {
    rows.iter().position(|r| y >= r.y && y < r.bottom())
}

/// The row nearest to `y` of the given kind (for dragging clips over the other kind's tracks).
pub fn nearest_row_of_kind(rows: &[Row], y: f32, kind: TrackKind) -> Option<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| r.kind == kind)
        .min_by(|a, b| {
            let d = |r: &Row| if y < r.y { r.y - y } else if y > r.bottom() { y - r.bottom() } else { 0.0 };
            d(a.1).total_cmp(&d(b.1))
        })
        .map(|(i, _)| i)
}

// ---------------------------------------------------------------------------------------------
// Hit testing
// ---------------------------------------------------------------------------------------------

/// Which part of a clip the pointer is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    Body,
    Start,
    End,
    FadeIn,
    FadeOut,
}

/// A clip as laid out this frame.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipBox {
    pub id: Id,
    pub row: usize,
    pub rect: Rect,
    /// Screen x of the end of the fade-in / start of the fade-out.
    pub fade_in_x: f32,
    pub fade_out_x: f32,
    /// False for clips drawn at a fallback position because their anchor is broken.
    pub resolved: bool,
    pub locked: bool,
}

/// Size of the square fade handles at the top corners.
pub const FADE_HANDLE: f32 = 9.0;

/// The clip and zone under `pos`. Fade handles first, then edges (within `edge_px`, on the side of
/// the pointer when two clips touch), then bodies.
pub fn hit_clip(boxes: &[ClipBox], pos: Pos2, edge_px: f32) -> Option<(usize, Zone)> {
    for (i, b) in boxes.iter().enumerate() {
        if b.rect.width() < 28.0 || pos.y < b.rect.top() || pos.y > b.rect.top() + FADE_HANDLE + 1.0 {
            continue;
        }
        let fi = b.fade_in_x.max(b.rect.left() + FADE_HANDLE * 0.5);
        let fo = b.fade_out_x.min(b.rect.right() - FADE_HANDLE * 0.5);
        if (pos.x - fi).abs() <= FADE_HANDLE * 0.5 + 1.0 {
            return Some((i, Zone::FadeIn));
        }
        if (pos.x - fo).abs() <= FADE_HANDLE * 0.5 + 1.0 {
            return Some((i, Zone::FadeOut));
        }
    }
    let inside_y = |b: &ClipBox| pos.y >= b.rect.top() && pos.y <= b.rect.bottom();
    if let Some((i, b)) = boxes.iter().enumerate().find(|(_, b)| inside_y(b) && pos.x >= b.rect.left() && pos.x <= b.rect.right()) {
        let e = edge_px.min(b.rect.width() / 3.0);
        let zone = if pos.x - b.rect.left() <= e {
            Zone::Start
        } else if b.rect.right() - pos.x <= e {
            Zone::End
        } else {
            Zone::Body
        };
        return Some((i, zone));
    }
    // Just outside an edge (thin clips, gaps).
    for (i, b) in boxes.iter().enumerate() {
        if !inside_y(b) {
            continue;
        }
        if pos.x < b.rect.left() && b.rect.left() - pos.x <= edge_px * 0.6 {
            return Some((i, Zone::Start));
        }
        if pos.x > b.rect.right() && pos.x - b.rect.right() <= edge_px * 0.6 {
            return Some((i, Zone::End));
        }
    }
    None
}

/// A transition as laid out this frame.
#[derive(Clone, Debug, PartialEq)]
pub struct TransBox {
    pub id: Id,
    pub rect: Rect,
    pub cut_x: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransZone {
    Body,
    Left,
    Right,
}

pub fn hit_transition(boxes: &[TransBox], pos: Pos2, edge_px: f32) -> Option<(usize, TransZone)> {
    for (i, b) in boxes.iter().enumerate() {
        let r = b.rect.expand2(egui::vec2(edge_px * 0.5, 0.0));
        if !r.contains(pos) {
            continue;
        }
        let e = edge_px.min(b.rect.width() / 3.0).max(3.0);
        let z = if (pos.x - b.rect.left()).abs() <= e {
            TransZone::Left
        } else if (pos.x - b.rect.right()).abs() <= e {
            TransZone::Right
        } else {
            TransZone::Body
        };
        return Some((i, z));
    }
    None
}

/// The marker nearest to `x` within `tol` pixels.
pub fn hit_marker(xs: &[(Id, f32)], x: f32, tol: f32) -> Option<usize> {
    xs.iter().enumerate().filter(|(_, (_, mx))| (mx - x).abs() <= tol).min_by(|a, b| (a.1 .1 - x).abs().total_cmp(&(b.1 .1 - x).abs())).map(|(i, _)| i)
}

/// Ids of the clips whose boxes intersect `rect` (box selection).
pub fn clips_in_rect(boxes: &[ClipBox], rect: Rect) -> Vec<Id> {
    boxes.iter().filter(|b| b.rect.intersects(rect)).map(|b| b.id.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::tests::demo;
    use egui::{pos2, vec2};

    #[test]
    fn reveal_changes_the_view_as_little_as_possible() {
        // 1000 px at 100 px/s: 10 s visible from 20 s.
        let r = |a: f64, b: f64| reveal(20.0, 100.0, 1000.0, a, b, MIN_ZOOM, 2400.0);
        assert_eq!(r(22.0, 25.0), (20.0, 100.0), "visible: unchanged");
        let (s, z) = r(31.0, 33.0);
        assert_eq!(z, 100.0);
        assert!(s > 20.0 && 33.0 <= s + 10.0 && s <= 31.0, "scrolls right just enough: {s}");
        let (s, z) = r(5.0, 6.0);
        assert_eq!(z, 100.0);
        assert!(s <= 5.0 && 6.0 <= s + 10.0 && s >= 4.0, "scrolls left just enough: {s}");
        let (s, z) = r(0.0, 60.0);
        assert!(z < 100.0 && s <= 0.0 + 1e-9 && 60.0 <= s + 1000.0 / z, "zooms out to fit: {s} {z}");
        // Reversed and negative inputs are tolerated.
        let (s, _) = r(-3.0, 2.0);
        assert!(s.abs() < 1e-9);
        // Rows: 200 px of view from y 100.
        assert_eq!(reveal_rows(100.0, 200.0, 150.0, 250.0), 100.0, "visible");
        assert_eq!(reveal_rows(100.0, 200.0, 280.0, 360.0), 160.0, "down just enough");
        assert_eq!(reveal_rows(100.0, 200.0, 20.0, 80.0), 20.0, "up just enough");
        assert_eq!(reveal_rows(100.0, 200.0, 20.0, 600.0), 20.0, "too tall: the top");
    }

    #[test]
    fn time_mapping_and_zoom() {
        let m = TimeMap { x0: 100.0, scroll: 10.0, zoom: 50.0 };
        assert_eq!(m.x(10.0), 100.0);
        assert_eq!(m.x(12.0), 200.0);
        assert!((m.t(200.0) - 12.0).abs() < 1e-9);
        // Zooming keeps the time under the cursor.
        let (s, z) = zoom_at(m, 300.0, 2.0, MIN_ZOOM, 1000.0);
        let m2 = TimeMap { x0: 100.0, scroll: s, zoom: z };
        assert_eq!(z, 100.0);
        assert!((m2.t(300.0) - m.t(300.0)).abs() < 1e-9);
        // Clamped, and never scrolls before 0.
        let (s, z) = zoom_at(TimeMap { x0: 0.0, scroll: 0.0, zoom: 10.0 }, 0.0, 0.0001, MIN_ZOOM, 1000.0);
        assert_eq!(z, MIN_ZOOM);
        assert_eq!(s, 0.0);
        let (s, z) = fit(100.0, 1000.0, MIN_ZOOM, 1000.0);
        assert_eq!(s, 0.0);
        assert!((z - 9.4).abs() < 1e-9);
    }

    #[test]
    fn ticks_adapt_to_zoom() {
        let t = ticks(100.0, 30.0, 80.0);
        assert_eq!(t.major, 1.0);
        assert!(!t.frames);
        assert!((t.minor - 0.1).abs() < 1e-9 || (t.minor - 0.2).abs() < 1e-9 || (t.minor - 0.25).abs() < 1e-9 || (t.minor - 0.5).abs() < 1e-9);
        let t = ticks(2000.0, 30.0, 80.0);
        assert!(t.frames, "{t:?}");
        assert!((t.major - 2.0 / 30.0).abs() < 1e-9);
        assert!((t.minor * 30.0 - (t.minor * 30.0).round()).abs() < 1e-6, "minor ticks land on frames");
        let t = ticks(1.0, 30.0, 80.0);
        assert_eq!(t.major, 120.0);
        let t = ticks(0.02, 30.0, 80.0);
        assert_eq!(t.major, 7200.0);
        assert_eq!(tick_label(83.5, &Ticks { major: 1.0, minor: 0.5, frames: false }, 30.0), "01:23");
        assert_eq!(tick_label(83.5, &Ticks { major: 0.1, minor: 0.1, frames: true }, 30.0), "01:23:15");
        assert_eq!(tick_label(3723.0, &Ticks { major: 60.0, minor: 10.0, frames: false }, 30.0), "1:02:03");
        assert_eq!(tick_times(0.9, 3.1, 1.0), vec![0.0, 1.0, 2.0, 3.0, 4.0]);
        assert!(tick_times(0.0, 1e9, 0.001).is_empty(), "absurd ranges draw nothing");
    }

    #[test]
    fn rows_put_video_on_top_in_stack_order() {
        let p = demo();
        let mut h = HashMap::new();
        h.insert("a2".to_string(), 30.0);
        let rows = layout_rows(&p, &h);
        let ids: Vec<&str> = rows.iter().map(|r| r.track.as_str()).collect();
        assert_eq!(ids, vec!["v2", "v1", "a1", "a2"]);
        assert_eq!(rows[0].y, 0.0);
        assert_eq!(rows[1].y, DEFAULT_VIDEO_H);
        assert_eq!(rows[2].y, 2.0 * DEFAULT_VIDEO_H + DIVIDER);
        assert_eq!(rows[3].h, 30.0);
        assert_eq!(row_at(&rows, 1.0), Some(0));
        assert_eq!(row_at(&rows, 2.0 * DEFAULT_VIDEO_H + 1.0), None, "divider");
        assert_eq!(nearest_row_of_kind(&rows, 0.0, TrackKind::Audio), Some(2));
        assert_eq!(nearest_row_of_kind(&rows, 1000.0, TrackKind::Video), Some(1));
        assert!((rows_height(&rows) - (2.0 * DEFAULT_VIDEO_H + DIVIDER + DEFAULT_AUDIO_H + 30.0)).abs() < 1e-3);
    }

    fn cb(id: &str, x0: f32, x1: f32) -> ClipBox {
        ClipBox { id: id.into(), row: 0, rect: Rect::from_min_max(pos2(x0, 0.0), pos2(x1, 50.0)), fade_in_x: x0, fade_out_x: x1, resolved: true, locked: false }
    }

    #[test]
    fn hit_testing_prefers_handles_then_edges() {
        let boxes = vec![cb("a", 0.0, 100.0), cb("b", 100.0, 200.0), cb("thin", 300.0, 304.0)];
        assert_eq!(hit_clip(&boxes, pos2(50.0, 30.0), 6.0), Some((0, Zone::Body)));
        assert_eq!(hit_clip(&boxes, pos2(97.0, 30.0), 6.0), Some((0, Zone::End)));
        assert_eq!(hit_clip(&boxes, pos2(103.0, 30.0), 6.0), Some((1, Zone::Start)), "pointer side wins at a cut");
        assert_eq!(hit_clip(&boxes, pos2(5.0, 3.0), 6.0), Some((0, Zone::FadeIn)));
        assert_eq!(hit_clip(&boxes, pos2(195.0, 3.0), 6.0), Some((1, Zone::FadeOut)));
        assert_eq!(hit_clip(&boxes, pos2(306.0, 30.0), 6.0), Some((2, Zone::End)), "thin clips stay grabbable");
        assert_eq!(hit_clip(&boxes, pos2(250.0, 30.0), 6.0), None);
        assert_eq!(hit_clip(&boxes, pos2(50.0, 80.0), 6.0), None);
        let sel = clips_in_rect(&boxes, Rect::from_min_size(pos2(90.0, 10.0), vec2(20.0, 5.0)));
        assert_eq!(sel, vec!["a", "b"]);

        let tb = vec![TransBox { id: "tr1".into(), rect: Rect::from_min_max(pos2(80.0, 10.0), pos2(120.0, 40.0)), cut_x: 100.0 }];
        assert_eq!(hit_transition(&tb, pos2(100.0, 20.0), 6.0), Some((0, TransZone::Body)));
        assert_eq!(hit_transition(&tb, pos2(81.0, 20.0), 6.0), Some((0, TransZone::Left)));
        assert_eq!(hit_transition(&tb, pos2(122.0, 20.0), 6.0), Some((0, TransZone::Right)));
        assert_eq!(hit_transition(&tb, pos2(100.0, 45.0), 6.0), None);
        let ms = vec![("m1".to_string(), 10.0), ("m2".to_string(), 14.0)];
        assert_eq!(hit_marker(&ms, 13.0, 5.0), Some(1));
        assert_eq!(hit_marker(&ms, 30.0, 5.0), None);
    }
}
