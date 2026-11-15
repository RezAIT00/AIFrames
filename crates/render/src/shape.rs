//! Vector shapes (rect with rounded corners, ellipse, line, arrow) with fill, stroke and the `draw`
//! 0..1 draw-on animation, rasterised with tiny-skia directly in canvas space.

use crate::canvas::{pixmap_layer, Layer};
use crate::geom::{Affine, IRect};
use crate::text::rounded_rect;
use aiframes_core::{Color, ShapeKind, ShapeSource};
use tiny_skia::{FillRule, LineCap, LineJoin, Mask, Paint, PathBuilder, PathSegment, Pixmap, Stroke, StrokeDash};

/// Default stroke width of lines and arrows that have none.
const DEFAULT_LINE_WIDTH: f64 = 4.0;

fn line_width(s: &ShapeSource) -> f64 {
    match &s.stroke {
        Some(st) => st.width.max(0.0),
        None if matches!(s.kind, ShapeKind::Line | ShapeKind::Arrow) => DEFAULT_LINE_WIDTH,
        None => 0.0,
    }
}

/// The content box of a shape (lines/arrows: at least as thick as their stroke).
pub(crate) fn content_size(s: &ShapeSource) -> (f64, f64) {
    let w = if s.width.is_finite() { s.width } else { 0.0 };
    let h = if s.height.is_finite() { s.height } else { 0.0 };
    match s.kind {
        ShapeKind::Rect | ShapeKind::Ellipse => (w.abs(), h.abs()),
        ShapeKind::Line | ShapeKind::Arrow => {
            let t = line_width(s).max(1.0);
            (w.abs().max(t), h.abs().max(t))
        }
    }
}

fn skia(c: Color, alpha: f32) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba(c.r.clamp(0.0, 1.0), c.g.clamp(0.0, 1.0), c.b.clamp(0.0, 1.0), (c.a * alpha).clamp(0.0, 1.0))
        .unwrap_or(tiny_skia::Color::TRANSPARENT)
}

/// An ellipse path starting at the top, clockwise.
fn ellipse_path(w: f32, h: f32) -> Option<tiny_skia::Path> {
    if !(w > 0.0 && h > 0.0) {
        return None;
    }
    let (rx, ry) = (w / 2.0, h / 2.0);
    let (cx, cy) = (rx, ry);
    let k = 0.552_284_8;
    let mut pb = PathBuilder::new();
    pb.move_to(cx, cy - ry);
    pb.cubic_to(cx + k * rx, cy - ry, cx + rx, cy - k * ry, cx + rx, cy);
    pb.cubic_to(cx + rx, cy + k * ry, cx + k * rx, cy + ry, cx, cy + ry);
    pb.cubic_to(cx - k * rx, cy + ry, cx - rx, cy + k * ry, cx - rx, cy);
    pb.cubic_to(cx - rx, cy - k * ry, cx - k * rx, cy - ry, cx, cy - ry);
    pb.close();
    pb.finish()
}

/// Approximate arc length of a path (curves flattened into 16 segments).
pub(crate) fn path_length(p: &tiny_skia::Path) -> f32 {
    let mut len = 0.0f32;
    let mut cur = tiny_skia::Point::zero();
    let mut start = cur;
    let dist = |a: tiny_skia::Point, b: tiny_skia::Point| ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
    for seg in p.segments() {
        match seg {
            PathSegment::MoveTo(pt) => {
                cur = pt;
                start = pt;
            }
            PathSegment::LineTo(pt) => {
                len += dist(cur, pt);
                cur = pt;
            }
            PathSegment::QuadTo(c, pt) => {
                let mut prev = cur;
                for i in 1..=16 {
                    let t = i as f32 / 16.0;
                    let u = 1.0 - t;
                    let q = tiny_skia::Point::from_xy(u * u * cur.x + 2.0 * u * t * c.x + t * t * pt.x, u * u * cur.y + 2.0 * u * t * c.y + t * t * pt.y);
                    len += dist(prev, q);
                    prev = q;
                }
                cur = pt;
            }
            PathSegment::CubicTo(c1, c2, pt) => {
                let mut prev = cur;
                for i in 1..=16 {
                    let t = i as f32 / 16.0;
                    let u = 1.0 - t;
                    let x = u * u * u * cur.x + 3.0 * u * u * t * c1.x + 3.0 * u * t * t * c2.x + t * t * t * pt.x;
                    let y = u * u * u * cur.y + 3.0 * u * u * t * c1.y + 3.0 * u * t * t * c2.y + t * t * t * pt.y;
                    let q = tiny_skia::Point::from_xy(x, y);
                    len += dist(prev, q);
                    prev = q;
                }
                cur = pt;
            }
            PathSegment::Close => {
                len += dist(cur, start);
                cur = start;
            }
        }
    }
    len
}

/// A pie wedge from 12 o'clock clockwise over `frac` of a turn, covering a `w`×`h` box.
fn sweep_path(w: f32, h: f32, frac: f32) -> Option<tiny_skia::Path> {
    let (cx, cy) = (w / 2.0, h / 2.0);
    let r = (w * w + h * h).sqrt();
    let steps = (64.0 * frac).ceil().max(2.0) as usize;
    let mut pb = PathBuilder::new();
    pb.move_to(cx, cy);
    for i in 0..=steps {
        let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * frac * i as f32 / steps as f32;
        pb.line_to(cx + r * a.cos(), cy + r * a.sin());
    }
    pb.close();
    pb.finish()
}

/// Renders the shape's content box through `to_out` (content coords → canvas px).
pub(crate) fn render(s: &ShapeSource, draw: f64, to_out: Affine, canvas: (u32, u32)) -> Layer {
    let (cw, ch) = canvas;
    let draw = if draw.is_finite() { draw.clamp(0.0, 1.0) as f32 } else { 1.0 };
    let (bw, bh) = content_size(s);
    let lw = line_width(s);
    let margin = lw * 2.0 + 4.0;
    let rect = IRect::from_bounds(to_out.bounds_of(-margin, -margin, bw + margin, bh + margin)).expand(1).intersect(&IRect::of_size(cw, ch));
    if rect.is_empty() || draw <= 0.0 && !matches!(s.kind, ShapeKind::Rect | ShapeKind::Ellipse) {
        return Layer::empty(cw, ch);
    }
    let Some(mut pm) = Pixmap::new(rect.w as u32, rect.h as u32) else { return Layer::empty(cw, ch) };
    let xf = to_out.then(&Affine::translate(-rect.x as f64, -rect.y as f64)).to_skia();
    let mut paint = Paint { anti_alias: true, ..Paint::default() };
    let (bw32, bh32) = (bw as f32, bh as f32);
    match s.kind {
        ShapeKind::Rect | ShapeKind::Ellipse => {
            let path = if s.kind == ShapeKind::Rect { rounded_rect(0.0, 0.0, bw32, bh32, s.radius as f32) } else { ellipse_path(bw32, bh32) };
            let Some(path) = path else { return Layer::empty(cw, ch) };
            let stroke = s.stroke.as_ref().filter(|st| st.width > 0.0 && st.color.a > 0.0);
            if let Some(fill) = s.fill.filter(|f| f.a > 0.0) {
                if stroke.is_some() || draw >= 1.0 {
                    // With an outline the fill fades in while the outline draws on.
                    let a = if draw >= 1.0 { 1.0 } else { draw };
                    if a > 0.0 {
                        paint.set_color(skia(fill, a));
                        pm.fill_path(&path, &paint, FillRule::Winding, xf, None);
                    }
                } else if draw > 0.0 {
                    // Fill only: a clockwise sweep from 12 o'clock reveals it.
                    if let (Some(mut mask), Some(sweep)) = (Mask::new(rect.w as u32, rect.h as u32), sweep_path(bw32, bh32, draw)) {
                        mask.fill_path(&sweep, FillRule::Winding, true, xf);
                        paint.set_color(skia(fill, 1.0));
                        pm.fill_path(&path, &paint, FillRule::Winding, xf, Some(&mask));
                    }
                }
            }
            if let Some(st) = stroke {
                if draw > 0.0 {
                    let mut stroke = Stroke { width: st.width as f32, line_join: LineJoin::Miter, ..Stroke::default() };
                    if draw < 1.0 {
                        let len = path_length(&path);
                        stroke.line_cap = LineCap::Butt;
                        stroke.dash = StrokeDash::new(vec![(len * draw).max(0.01), len + 1.0], 0.0);
                    }
                    paint.set_color(skia(st.color, 1.0));
                    pm.stroke_path(&path, &paint, &stroke, xf, None);
                }
            }
        }
        ShapeKind::Line | ShapeKind::Arrow => {
            let color = s.stroke.as_ref().map(|st| st.color).or(s.fill).unwrap_or(Color::WHITE);
            let (cx, cy) = (bw / 2.0, bh / 2.0);
            let (w, h) = (if s.width.is_finite() { s.width } else { 0.0 }, if s.height.is_finite() { s.height } else { 0.0 });
            let (x0, y0) = (cx - w / 2.0, cy - h / 2.0);
            let len = (w * w + h * h).sqrt();
            if len < 1e-6 {
                return Layer::empty(cw, ch);
            }
            let (ux, uy) = (w / len, h / len);
            let drawn = len * draw as f64;
            let (tx, ty) = (x0 + ux * drawn, y0 + uy * drawn);
            let arrow = s.kind == ShapeKind::Arrow;
            let head = if arrow { (lw * 3.5).max(12.0).min(len * 0.5) * (drawn / (lw * 3.5).max(12.0)).min(1.0) } else { 0.0 };
            let shaft = if arrow { (drawn - head * 0.6).max(0.0) } else { drawn };
            paint.set_color(skia(color, 1.0));
            if shaft > 0.01 {
                let mut pb = PathBuilder::new();
                pb.move_to(x0 as f32, y0 as f32);
                pb.line_to((x0 + ux * shaft) as f32, (y0 + uy * shaft) as f32);
                if let Some(p) = pb.finish() {
                    let stroke = Stroke { width: lw as f32, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Stroke::default() };
                    pm.stroke_path(&p, &paint, &stroke, xf, None);
                }
            }
            if arrow && head > 0.5 {
                let (nx, ny) = (-uy, ux);
                let bx = tx - ux * head;
                let by = ty - uy * head;
                let hw = head * 0.55;
                let mut pb = PathBuilder::new();
                pb.move_to(tx as f32, ty as f32);
                pb.line_to((bx + nx * hw) as f32, (by + ny * hw) as f32);
                pb.line_to((bx - nx * hw) as f32, (by - ny * hw) as f32);
                pb.close();
                if let Some(p) = pb.finish() {
                    pm.fill_path(&p, &paint, FillRule::Winding, xf, None);
                    let stroke = Stroke { width: (lw * 0.5) as f32, line_join: LineJoin::Round, ..Stroke::default() };
                    pm.stroke_path(&p, &paint, &stroke, xf, None);
                }
            }
        }
    }
    pixmap_layer(cw, ch, &pm, rect.x, rect.y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths() {
        let r = rounded_rect(0.0, 0.0, 100.0, 50.0, 0.0).unwrap();
        assert!((path_length(&r) - 300.0).abs() < 0.5);
        let e = ellipse_path(100.0, 100.0).unwrap();
        assert!((path_length(&e) - std::f32::consts::PI * 100.0).abs() < 1.0);
    }

    #[test]
    fn draw_on_partial_outline() {
        let s = ShapeSource {
            kind: ShapeKind::Rect,
            width: 100.0,
            height: 100.0,
            fill: None,
            stroke: Some(aiframes_core::Stroke { color: Color::WHITE, width: 6.0 }),
            radius: 0.0,
            draw: aiframes_core::Param::Value(1.0),
        };
        let full = render(&s, 1.0, Affine::translate(10.0, 10.0), (120, 120));
        let half = render(&s, 0.5, Affine::translate(10.0, 10.0), (120, 120));
        let ink = |l: &Layer| l.px.iter().map(|p| p[3]).sum::<f32>();
        let (a, b) = (ink(&full), ink(&half));
        assert!(b > a * 0.35 && b < a * 0.65, "{a} {b}");
        // Starts at the top middle going clockwise: the top-right corner is drawn, bottom-left not.
        assert!(half.get(108, 10)[3] > 0.5);
        assert!(half.get(10, 108)[3] < 0.1);
    }
}
