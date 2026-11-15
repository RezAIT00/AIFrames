//! 2-D affine and projective transforms, 3-D card geometry and integer rectangles.

/// `x' = a·x + c·y + e`, `y' = b·x + d·y + f` (the SVG/canvas matrix layout).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Affine {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Affine {
    pub const IDENTITY: Affine = Affine { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    pub fn translate(x: f64, y: f64) -> Affine {
        Affine { e: x, f: y, ..Affine::IDENTITY }
    }
    pub fn scale(sx: f64, sy: f64) -> Affine {
        Affine { a: sx, d: sy, ..Affine::IDENTITY }
    }
    /// Clockwise rotation on screen (+y down) by `deg` degrees.
    pub fn rotate_deg(deg: f64) -> Affine {
        let (s, c) = deg.to_radians().sin_cos();
        Affine { a: c, b: s, c: -s, d: c, e: 0.0, f: 0.0 }
    }
    /// Scale by `s` about point `(cx, cy)`.
    pub fn scale_about(s: f64, cx: f64, cy: f64) -> Affine {
        Affine::translate(-cx, -cy).then(&Affine::scale(s, s)).then(&Affine::translate(cx, cy))
    }
    /// `self` first, then `o`.
    pub fn then(&self, o: &Affine) -> Affine {
        Affine {
            a: o.a * self.a + o.c * self.b,
            b: o.b * self.a + o.d * self.b,
            c: o.a * self.c + o.c * self.d,
            d: o.b * self.c + o.d * self.d,
            e: o.a * self.e + o.c * self.f + o.e,
            f: o.b * self.e + o.d * self.f + o.f,
        }
    }
    #[inline]
    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (self.a * x + self.c * y + self.e, self.b * x + self.d * y + self.f)
    }
    pub fn det(&self) -> f64 {
        self.a * self.d - self.b * self.c
    }
    pub fn invert(&self) -> Option<Affine> {
        let det = self.det();
        if !det.is_finite() || det.abs() < 1e-12 {
            return None;
        }
        let id = 1.0 / det;
        let a = self.d * id;
        let b = -self.b * id;
        let c = -self.c * id;
        let d = self.a * id;
        let e = -(a * self.e + c * self.f);
        let f = -(b * self.e + d * self.f);
        Some(Affine { a, b, c, d, e, f })
    }
    /// Pure translation by whole pixels: `Some((dx, dy))`.
    pub fn integer_translation(&self) -> Option<(i64, i64)> {
        const E: f64 = 1e-7;
        if (self.a - 1.0).abs() > E || self.b.abs() > E || self.c.abs() > E || (self.d - 1.0).abs() > E {
            return None;
        }
        let (rx, ry) = (self.e.round(), self.f.round());
        if (self.e - rx).abs() > 1e-4 || (self.f - ry).abs() > 1e-4 {
            return None;
        }
        Some((rx as i64, ry as i64))
    }
    /// Mean linear scale factor (sqrt of |det|).
    pub fn mean_scale(&self) -> f64 {
        self.det().abs().sqrt()
    }
    pub fn to_skia(self) -> tiny_skia::Transform {
        tiny_skia::Transform::from_row(self.a as f32, self.b as f32, self.c as f32, self.d as f32, self.e as f32, self.f as f32)
    }
    /// Axis-aligned bounds `[x0, y0, x1, y1]` of a transformed rectangle.
    pub fn bounds_of(&self, x0: f64, y0: f64, x1: f64, y1: f64) -> [f64; 4] {
        let pts = [self.apply(x0, y0), self.apply(x1, y0), self.apply(x1, y1), self.apply(x0, y1)];
        let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for (x, y) in pts {
            b[0] = b[0].min(x);
            b[1] = b[1].min(y);
            b[2] = b[2].max(x);
            b[3] = b[3].max(y);
        }
        b
    }
}

/// A projective transform (homography): `[x', y', w']ᵀ = m · [x, y, 1]ᵀ`, row-major. Affine
/// transforms are the special case with bottom row `[0, 0, 1]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Proj {
    pub m: [f64; 9],
}

/// Points this close to (or behind) the camera plane are not drawn.
pub(crate) const W_EPS: f64 = 1e-6;

impl Proj {
    pub const IDENTITY: Proj = Proj { m: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0] };

    pub fn from_affine(a: &Affine) -> Proj {
        Proj { m: [a.a, a.c, a.e, a.b, a.d, a.f, 0.0, 0.0, 1.0] }
    }
    /// The affine transform, when this is one (bottom row `[0, 0, 1]` up to scale).
    pub fn to_affine(self) -> Option<Affine> {
        let m = &self.m;
        let s = m[8];
        if s.abs() < 1e-12 || (m[6] / s).abs() > 1e-12 || (m[7] / s).abs() > 1e-12 {
            return None;
        }
        Some(Affine { a: m[0] / s, b: m[3] / s, c: m[1] / s, d: m[4] / s, e: m[2] / s, f: m[5] / s })
    }
    /// `self` first, then `o`.
    pub fn then(&self, o: &Proj) -> Proj {
        let (a, b) = (&o.m, &self.m);
        let mut r = [0f64; 9];
        for i in 0..3 {
            for j in 0..3 {
                r[i * 3 + j] = a[i * 3] * b[j] + a[i * 3 + 1] * b[3 + j] + a[i * 3 + 2] * b[6 + j];
            }
        }
        Proj { m: r }
    }
    #[inline]
    pub fn apply_h(&self, x: f64, y: f64) -> (f64, f64, f64) {
        let m = &self.m;
        (m[0] * x + m[1] * y + m[2], m[3] * x + m[4] * y + m[5], m[6] * x + m[7] * y + m[8])
    }
    /// The mapped point, or `None` behind the camera.
    #[inline]
    pub fn apply(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (a, b, w) = self.apply_h(x, y);
        (w > W_EPS).then(|| (a / w, b / w))
    }
    pub fn invert(&self) -> Option<Proj> {
        let m = &self.m;
        let c00 = m[4] * m[8] - m[5] * m[7];
        let c01 = m[5] * m[6] - m[3] * m[8];
        let c02 = m[3] * m[7] - m[4] * m[6];
        let det = m[0] * c00 + m[1] * c01 + m[2] * c02;
        if !det.is_finite() || det.abs() < 1e-18 {
            return None;
        }
        // The exact inverse (adjugate / det) maps a visible output point back to `p / w` with
        // `1 / w > 0`, so visibility tests on its w stay valid.
        let id = 1.0 / det;
        Some(Proj {
            m: [
                c00 * id,
                (m[2] * m[7] - m[1] * m[8]) * id,
                (m[1] * m[5] - m[2] * m[4]) * id,
                c01 * id,
                (m[0] * m[8] - m[2] * m[6]) * id,
                (m[2] * m[3] - m[0] * m[5]) * id,
                c02 * id,
                (m[1] * m[6] - m[0] * m[7]) * id,
                (m[0] * m[4] - m[1] * m[3]) * id,
            ],
        })
    }
    pub fn is_finite(&self) -> bool {
        self.m.iter().all(|v| v.is_finite())
    }
    /// Local linear scale (sqrt of |det J|) at a point.
    pub fn scale_at(&self, x: f64, y: f64) -> f64 {
        let m = &self.m;
        let (u, v, w) = self.apply_h(x, y);
        if w.abs() < 1e-12 {
            return 0.0;
        }
        let (px, py) = (u / w, v / w);
        let j00 = (m[0] - px * m[6]) / w;
        let j01 = (m[1] - px * m[7]) / w;
        let j10 = (m[3] - py * m[6]) / w;
        let j11 = (m[4] - py * m[7]) / w;
        (j00 * j11 - j01 * j10).abs().sqrt()
    }
    /// Largest local scale over a rectangle's corners and centre (how much detail is needed).
    pub fn max_scale(&self, x0: f64, y0: f64, x1: f64, y1: f64) -> f64 {
        [(x0, y0), (x1, y0), (x1, y1), (x0, y1), ((x0 + x1) / 2.0, (y0 + y1) / 2.0)]
            .iter()
            .filter(|&&(x, y)| self.apply_h(x, y).2 > W_EPS)
            .map(|&(x, y)| self.scale_at(x, y))
            .filter(|v| v.is_finite())
            .fold(0.0, f64::max)
    }
    /// Bounds `[x0, y0, x1, y1]` of a mapped rectangle; `None` when part of it is behind the camera
    /// (callers then use the whole canvas and test per pixel).
    pub fn bounds_of(&self, x0: f64, y0: f64, x1: f64, y1: f64) -> Option<[f64; 4]> {
        let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for (x, y) in [(x0, y0), (x1, y0), (x1, y1), (x0, y1)] {
            let (px, py) = self.apply(x, y)?;
            b[0] = b[0].min(px);
            b[1] = b[1].min(py);
            b[2] = b[2].max(px);
            b[3] = b[3].max(py);
        }
        Some(b)
    }
}

/// 3-vectors and rotations for 3-D cards (x right, y down, z away from the viewer).
pub(crate) type V3 = [f64; 3];
pub(crate) type M3 = [[f64; 3]; 3];

pub(crate) fn m3_mul(a: &M3, b: &M3) -> M3 {
    let mut r = [[0f64; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            r[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
        }
    }
    r
}
pub(crate) fn m3_apply(a: &M3, v: V3) -> V3 {
    [a[0][0] * v[0] + a[0][1] * v[1] + a[0][2] * v[2], a[1][0] * v[0] + a[1][1] * v[1] + a[1][2] * v[2], a[2][0] * v[0] + a[2][1] * v[1] + a[2][2] * v[2]]
}
/// Tilt: + = the top edge (y < 0) moves away from the viewer.
pub(crate) fn rot_x(deg: f64) -> M3 {
    let (s, c) = deg.to_radians().sin_cos();
    [[1.0, 0.0, 0.0], [0.0, c, s], [0.0, -s, c]]
}
/// Turn: + = the right edge (x > 0) moves away from the viewer.
pub(crate) fn rot_y(deg: f64) -> M3 {
    let (s, c) = deg.to_radians().sin_cos();
    [[c, 0.0, -s], [0.0, 1.0, 0.0], [s, 0.0, c]]
}
/// Screen rotation, clockwise (y down).
pub(crate) fn rot_z(deg: f64) -> M3 {
    let (s, c) = deg.to_radians().sin_cos();
    [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]]
}
pub(crate) fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

/// The plane `P(X, Y) = origin + X·eu + Y·ev` (3-D, relative to the camera axis point `(cx, cy, 0)`)
/// seen by a camera at distance `f` in front of the screen plane `z = 0`, as a homography from
/// plane coordinates to screen pixels. `f <= 0` = orthographic.
pub(crate) fn plane_homography(origin: V3, eu: V3, ev: V3, f: f64, cx: f64, cy: f64) -> Proj {
    if !(f > 0.0) {
        return Proj { m: [eu[0], ev[0], origin[0] + cx, eu[1], ev[1], origin[1] + cy, 0.0, 0.0, 1.0] };
    }
    let inv_f = 1.0 / f;
    Proj {
        m: [
            eu[0] + cx * eu[2] * inv_f,
            ev[0] + cx * ev[2] * inv_f,
            origin[0] + cx * (1.0 + origin[2] * inv_f),
            eu[1] + cy * eu[2] * inv_f,
            ev[1] + cy * ev[2] * inv_f,
            origin[1] + cy * (1.0 + origin[2] * inv_f),
            eu[2] * inv_f,
            ev[2] * inv_f,
            1.0 + origin[2] * inv_f,
        ],
    }
}

/// Whether the front of a plane (the side facing the viewer when `eu × ev` points away, i.e. an
/// unrotated card) faces a camera at distance `f` (orthographic for `f <= 0`).
pub(crate) fn front_facing(origin: V3, eu: V3, ev: V3, f: f64) -> bool {
    let n = cross(eu, ev); // points away from the viewer for the front side
    let to_cam = if f > 0.0 { [-origin[0], -origin[1], -f - origin[2]] } else { [0.0, 0.0, -1.0] };
    -(n[0] * to_cam[0] + n[1] * to_cam[1] + n[2] * to_cam[2]) > 0.0
}

/// How squarely a plane faces the camera axis (|cos| of the angle between its normal and the view
/// direction, 0..1), for shading.
pub(crate) fn facing(eu: V3, ev: V3) -> f64 {
    let n = cross(eu, ev);
    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if len <= 0.0 {
        0.0
    } else {
        (n[2] / len).abs()
    }
}

/// Integer pixel rectangle (`w`/`h` may be 0 = empty).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct IRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl IRect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> IRect {
        IRect { x, y, w: w.max(0), h: h.max(0) }
    }
    pub fn of_size(w: u32, h: u32) -> IRect {
        IRect::new(0, 0, w as i32, h as i32)
    }
    /// The pixels touched by the real-valued bounds `[x0, y0, x1, y1]`.
    pub fn from_bounds(b: [f64; 4]) -> IRect {
        if !b.iter().all(|v| v.is_finite()) || b[2] <= b[0] || b[3] <= b[1] {
            return IRect::default();
        }
        let clampi = |v: f64| v.clamp(-1.0e8, 1.0e8);
        let x0 = clampi(b[0].floor()) as i32;
        let y0 = clampi(b[1].floor()) as i32;
        let x1 = clampi(b[2].ceil()) as i32;
        let y1 = clampi(b[3].ceil()) as i32;
        IRect::new(x0, y0, x1 - x0, y1 - y0)
    }
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.w)
    }
    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.h)
    }
    pub fn area(&self) -> usize {
        if self.is_empty() {
            0
        } else {
            self.w as usize * self.h as usize
        }
    }
    pub fn intersect(&self, o: &IRect) -> IRect {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = self.right().min(o.right());
        let y1 = self.bottom().min(o.bottom());
        IRect::new(x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
    }
    pub fn union(&self, o: &IRect) -> IRect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = self.right().max(o.right());
        let y1 = self.bottom().max(o.bottom());
        IRect::new(x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
    }
    pub fn expand(&self, n: i32) -> IRect {
        if self.is_empty() {
            return *self;
        }
        let n2 = n.saturating_mul(2);
        IRect::new(self.x.saturating_sub(n), self.y.saturating_sub(n), self.w.saturating_add(n2), self.h.saturating_add(n2))
    }
    pub fn offset(&self, dx: i32, dy: i32) -> IRect {
        IRect::new(self.x.saturating_add(dx), self.y.saturating_add(dy), self.w, self.h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_and_invert() {
        let m = Affine::translate(-5.0, 2.0).then(&Affine::rotate_deg(30.0)).then(&Affine::scale(2.0, 3.0));
        let inv = m.invert().unwrap();
        let (x, y) = m.apply(7.0, -3.0);
        let (bx, by) = inv.apply(x, y);
        assert!((bx - 7.0).abs() < 1e-9 && (by + 3.0).abs() < 1e-9);
        // Clockwise on screen: +x rotates towards +y.
        let (x, y) = Affine::rotate_deg(90.0).apply(1.0, 0.0);
        assert!(x.abs() < 1e-12 && (y - 1.0).abs() < 1e-12);
        assert_eq!(Affine::translate(3.0, -2.0).integer_translation(), Some((3, -2)));
        assert_eq!(Affine::translate(3.5, -2.0).integer_translation(), None);
    }

    #[test]
    fn projective_basics() {
        let a = Affine::translate(3.0, 4.0).then(&Affine::rotate_deg(20.0)).then(&Affine::scale(2.0, 0.5));
        let p = Proj::from_affine(&a);
        let b = p.to_affine().unwrap();
        assert!((b.a - a.a).abs() < 1e-12 && (b.f - a.f).abs() < 1e-12);
        let q = Proj { m: [1.0, 0.2, 5.0, 0.1, 1.0, -3.0, 0.001, 0.002, 1.0] };
        let inv = q.invert().unwrap();
        let (x, y) = q.apply(10.0, 20.0).unwrap();
        let (bx, by) = inv.apply(x, y).unwrap();
        assert!((bx - 10.0).abs() < 1e-9 && (by - 20.0).abs() < 1e-9);
        assert!(q.to_affine().is_none());
        // An unrotated plane at depth 0 is the identity around the camera axis.
        let h = plane_homography([-50.0, -20.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], 1000.0, 50.0, 20.0);
        let (x, y) = h.apply(7.0, 9.0).unwrap();
        assert!((x - 7.0).abs() < 1e-9 && (y - 9.0).abs() < 1e-9);
        // Turned +60 degrees: the right edge recedes (shorter), the left edge comes closer (taller).
        let r = rot_y(60.0);
        let (eu, ev) = (m3_apply(&r, [1.0, 0.0, 0.0]), m3_apply(&r, [0.0, 1.0, 0.0]));
        let h = plane_homography(m3_apply(&r, [-100.0, -50.0, 0.0]), eu, ev, 500.0, 0.0, 0.0);
        let left = h.apply(0.0, 100.0).unwrap().1 - h.apply(0.0, 0.0).unwrap().1;
        let right = h.apply(200.0, 100.0).unwrap().1 - h.apply(200.0, 0.0).unwrap().1;
        assert!(right < 100.0 && left > 100.0, "{left} {right}");
        assert!(front_facing([0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], 500.0));
        let r = rot_y(150.0);
        assert!(!front_facing([0.0, 0.0, 0.0], m3_apply(&r, [1.0, 0.0, 0.0]), m3_apply(&r, [0.0, 1.0, 0.0]), 500.0));
        let _ = (rot_x(10.0), rot_z(10.0), m3_mul(&rot_x(1.0), &rot_y(1.0)));
    }

    #[test]
    fn rects() {
        let a = IRect::new(0, 0, 10, 10);
        let b = IRect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(&b), IRect::new(5, 5, 5, 5));
        assert_eq!(a.union(&b), IRect::new(0, 0, 15, 15));
        assert!(IRect::new(20, 20, 3, 3).intersect(&a).is_empty());
        assert_eq!(IRect::from_bounds([0.5, 0.5, 2.1, 3.0]), IRect::new(0, 0, 3, 3));
    }
}
