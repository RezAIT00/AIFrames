//! Sampling of animated values. Every time here is **clip-local** seconds (0 = the clip's first frame).
//!
//! * [`ease`] maps 0..1 progress through an easing curve.
//! * [`sample`] evaluates a [`Param`] (constant before the first key and after the last).
//! * [`source_time`] / [`local_time_of_source`] map between clip-local time and media source time,
//!   integrating (possibly animated) speed, honouring `reverse`; [`source_span`] is the source range used.
//! * [`fade_gain`] is the clip's fade-in/out envelope.

use crate::model::*;

/// Grid points per key segment when searching the inverse of the speed integral.
const SCAN_STEPS: usize = 32;

/// Eases progress `x` (clamped to 0..1). The result may leave 0..1 for overshooting curves
/// (`back_out`, `elastic`, some béziers); it is 0 at 0 and 1 at 1.
pub fn ease(e: &Ease, x: f64) -> f64 {
    let x = if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) };
    match e {
        Ease::Linear => x,
        Ease::Hold => {
            if x >= 1.0 {
                1.0
            } else {
                0.0
            }
        }
        Ease::In => x * x * x,
        Ease::Out => {
            let u = 1.0 - x;
            1.0 - u * u * u
        }
        Ease::InOut => {
            if x < 0.5 {
                4.0 * x * x * x
            } else {
                let u = -2.0 * x + 2.0;
                1.0 - u * u * u / 2.0
            }
        }
        Ease::BackOut => {
            const C1: f64 = 1.70158;
            const C3: f64 = C1 + 1.0;
            let u = x - 1.0;
            1.0 + C3 * u * u * u + C1 * u * u
        }
        Ease::Elastic => {
            if x <= 0.0 {
                0.0
            } else if x >= 1.0 {
                1.0
            } else {
                let c4 = std::f64::consts::TAU / 3.0;
                2f64.powf(-10.0 * x) * ((x * 10.0 - 0.75) * c4).sin() + 1.0
            }
        }
        Ease::Bezier([x1, y1, x2, y2]) => cubic_bezier(*x1, *y1, *x2, *y2, x),
    }
}

/// CSS `cubic-bezier(x1, y1, x2, y2)` evaluated at progress `x`: solves `Bx(s) = x` for the curve
/// parameter `s` (Newton–Raphson with a bisection fallback) and returns `By(s)`.
/// `x1`/`x2` are clamped to 0..1 as in CSS so the curve is a function of x.
pub fn cubic_bezier(x1: f64, y1: f64, x2: f64, y2: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let x1 = if x1.is_finite() { x1.clamp(0.0, 1.0) } else { 0.0 };
    let x2 = if x2.is_finite() { x2.clamp(0.0, 1.0) } else { 1.0 };
    let y1 = if y1.is_finite() { y1 } else { 0.0 };
    let y2 = if y2.is_finite() { y2 } else { 1.0 };
    let s = bezier_param_for_x(x1, x2, x);
    bezier_coord(y1, y2, s)
}

/// One coordinate of a cubic bézier with end points 0 and 1 at parameter `s`.
fn bezier_coord(p1: f64, p2: f64, s: f64) -> f64 {
    let c = 3.0 * p1;
    let b = 3.0 * (p2 - p1) - c;
    let a = 1.0 - c - b;
    ((a * s + b) * s + c) * s
}

fn bezier_coord_deriv(p1: f64, p2: f64, s: f64) -> f64 {
    let c = 3.0 * p1;
    let b = 3.0 * (p2 - p1) - c;
    let a = 1.0 - c - b;
    (3.0 * a * s + 2.0 * b) * s + c
}

/// The curve parameter whose x coordinate is `x` (x control points within 0..1, so x(s) is monotonic).
pub(crate) fn bezier_param_for_x(x1: f64, x2: f64, x: f64) -> f64 {
    let mut s = x;
    for _ in 0..8 {
        let err = bezier_coord(x1, x2, s) - x;
        if err.abs() < 1e-12 {
            return s;
        }
        let d = bezier_coord_deriv(x1, x2, s);
        if d.abs() < 1e-9 {
            break;
        }
        s -= err / d;
        if !(0.0..=1.0).contains(&s) {
            break;
        }
    }
    // Bisection: robust for flat derivatives.
    let (mut lo, mut hi) = (0.0, 1.0);
    s = x;
    for _ in 0..64 {
        let v = bezier_coord(x1, x2, s);
        if (v - x).abs() < 1e-12 {
            break;
        }
        if v < x {
            lo = s;
        } else {
            hi = s;
        }
        s = 0.5 * (lo + hi);
    }
    s
}

/// Evaluates a parameter at clip-local time `t_local`. Keys must be sorted by time (ops keep them so;
/// `validate` reports `keys_unsorted`). An animated param with no keys samples as 0.
pub fn sample(p: &Param, t_local: f64) -> f64 {
    match p {
        Param::Value(v) => *v,
        Param::Animated { keys } => sample_keys(keys, t_local),
    }
}

/// [`sample`] over a key list.
pub fn sample_keys(keys: &[Key], t: f64) -> f64 {
    match keys.len() {
        0 => 0.0,
        1 => keys[0].v,
        n => {
            if t.is_nan() || t <= keys[0].t {
                return keys[0].v;
            }
            if t >= keys[n - 1].t {
                return keys[n - 1].v;
            }
            let i = keys.partition_point(|k| k.t <= t).clamp(1, n - 1);
            segment_value(&keys[i - 1], &keys[i], t)
        }
    }
}

fn segment_value(a: &Key, b: &Key, t: f64) -> f64 {
    let span = b.t - a.t;
    if span <= 0.0 {
        return b.v;
    }
    let x = (t - a.t) / span;
    a.v + (b.v - a.v) * ease(&a.ease, x)
}

/// ∫ p(τ) dτ from `t0` to `t1` (signed: negative when `t1 < t0`). Exact for constant, linear, hold, in,
/// out, in_out and back_out segments; high-order quadrature for elastic and bézier eases.
pub fn integrate(p: &Param, t0: f64, t1: f64) -> f64 {
    if !(t0.is_finite() && t1.is_finite()) {
        return 0.0;
    }
    if t1 < t0 {
        return -integrate(p, t1, t0);
    }
    match p {
        Param::Value(v) => v * (t1 - t0),
        Param::Animated { keys } => integrate_keys(keys, t0, t1),
    }
}

fn integrate_keys(keys: &[Key], t0: f64, t1: f64) -> f64 {
    match keys.len() {
        0 => 0.0,
        1 => keys[0].v * (t1 - t0),
        n => {
            let mut total = 0.0;
            // Before the first key: constant.
            let first = &keys[0];
            if t0 < first.t {
                total += first.v * (t1.min(first.t) - t0);
            }
            for w in keys.windows(2) {
                let (a, b) = (&w[0], &w[1]);
                let lo = t0.max(a.t);
                let hi = t1.min(b.t);
                if hi > lo {
                    total += integrate_segment(a, b, lo, hi);
                }
            }
            let last = &keys[n - 1];
            if t1 > last.t {
                total += last.v * (t1 - t0.max(last.t));
            }
            total
        }
    }
}

fn integrate_segment(a: &Key, b: &Key, lo: f64, hi: f64) -> f64 {
    let span = b.t - a.t;
    if span <= 0.0 {
        return 0.0;
    }
    let x0 = ((lo - a.t) / span).clamp(0.0, 1.0);
    let x1 = ((hi - a.t) / span).clamp(0.0, 1.0);
    span * (a.v * (x1 - x0) + (b.v - a.v) * (ease_integral(&a.ease, x1) - ease_integral(&a.ease, x0)))
}

/// ∫₀ˣ ease(u) du for x in 0..1: closed form for the polynomial eases, fixed-panel Gauss–Legendre for
/// elastic and bézier (a deterministic function of x, so integrals over adjacent intervals add up exactly
/// and `local_time_of_source` inverts `source_time` consistently).
pub fn ease_integral(e: &Ease, x: f64) -> f64 {
    let x = if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) };
    match e {
        Ease::Linear => x * x / 2.0,
        Ease::Hold => 0.0,
        Ease::In => x * x * x * x / 4.0,
        Ease::Out => {
            let u = 1.0 - x;
            x + (u * u * u * u - 1.0) / 4.0
        }
        Ease::InOut => {
            if x <= 0.5 {
                x * x * x * x
            } else {
                let w = 2.0 - 2.0 * x;
                x - 0.5 + w * w * w * w / 16.0
            }
        }
        Ease::BackOut => {
            const C1: f64 = 1.70158;
            const C3: f64 = C1 + 1.0;
            let u = x - 1.0;
            x + C3 * (u * u * u * u - 1.0) / 4.0 + C1 * (u * u * u + 1.0) / 3.0
        }
        Ease::Elastic | Ease::Bezier(_) => gauss_legendre(|u| ease(e, u), x),
    }
}

/// ∫₀ˣ f with 32 equal panels of 5-point Gauss–Legendre.
fn gauss_legendre(f: impl Fn(f64) -> f64, x: f64) -> f64 {
    const N: usize = 32;
    const XS: [f64; 5] = [0.0, -0.538_469_310_105_683_1, 0.538_469_310_105_683_1, -0.906_179_845_938_664, 0.906_179_845_938_664];
    const WS: [f64; 5] = [0.568_888_888_888_888_9, 0.478_628_670_499_366_5, 0.478_628_670_499_366_5, 0.236_926_885_056_189_1, 0.236_926_885_056_189_1];
    if x <= 0.0 {
        return 0.0;
    }
    let h = x / N as f64;
    let mut total = 0.0;
    for k in 0..N {
        let c = h * (k as f64 + 0.5);
        let mut s = 0.0;
        for i in 0..5 {
            s += WS[i] * f(c + XS[i] * h / 2.0);
        }
        total += s * h / 2.0;
    }
    total
}

/// Media source seconds consumed by the whole clip: ∫ speed over `0..duration`.
pub fn consumed(clip: &Clip) -> f64 {
    integrate(&clip.speed, 0.0, clip.duration)
}

/// Playback speed at clip-local time.
pub fn speed_at(clip: &Clip, t_local: f64) -> f64 {
    sample(&clip.speed, t_local)
}

/// The media in-point of a media clip.
pub fn in_point(clip: &Clip) -> Option<f64> {
    match &clip.source {
        Source::Media { in_point, .. } => Some(*in_point),
        _ => None,
    }
}

/// Source seconds shown at clip-local time `t_local` for media clips (`in_point + ∫speed`, or from the
/// far end downwards when `reverse`); `None` for non-media clips. Not clamped to the clip, so a
/// transition can ask for handle frames just outside it.
pub fn source_time(clip: &Clip, t_local: f64) -> Option<f64> {
    let ip = in_point(clip)?;
    let f = integrate(&clip.speed, 0.0, t_local);
    if clip.reverse {
        Some(ip + consumed(clip) - f)
    } else {
        Some(ip + f)
    }
}

/// The source range `[min, max]` (seconds) a media clip plays; `None` for non-media clips.
pub fn source_span(clip: &Clip) -> Option<(f64, f64)> {
    let ip = in_point(clip)?;
    let (fmin, fmax) = integral_range(clip);
    Some((ip + fmin, ip + fmax))
}

/// Min and max of F(t) = ∫₀ᵗ speed over t ∈ [0, duration] (equal to 0..F(dur) for non-negative speed).
fn integral_range(clip: &Clip) -> (f64, f64) {
    let dur = clip.duration.max(0.0);
    let total = integrate(&clip.speed, 0.0, dur);
    let monotone = match &clip.speed {
        Param::Value(_) => true,
        Param::Animated { keys } => keys.iter().all(|k| k.v >= 0.0) && keys.iter().all(|k| !overshoots(&k.ease)),
    };
    if monotone {
        return (total.min(0.0), total.max(0.0));
    }
    let grid = scan_grid(&clip.speed, dur);
    let mut lo = 0f64;
    let mut hi = 0f64;
    let mut acc = 0.0;
    for w in grid.windows(2) {
        acc += integrate(&clip.speed, w[0], w[1]);
        lo = lo.min(acc);
        hi = hi.max(acc);
    }
    (lo, hi)
}

fn overshoots(e: &Ease) -> bool {
    match e {
        Ease::BackOut | Ease::Elastic => true,
        Ease::Bezier([_, y1, _, y2]) => !(0.0..=1.0).contains(y1) || !(0.0..=1.0).contains(y2),
        _ => false,
    }
}

/// Sample points over [0, dur]: the ends, every key time inside, and `SCAN_STEPS` points per piece.
fn scan_grid(speed: &Param, dur: f64) -> Vec<f64> {
    let mut stops = vec![0.0];
    if let Param::Animated { keys } = speed {
        for k in keys {
            if k.t > 0.0 && k.t < dur {
                stops.push(k.t);
            }
        }
    }
    stops.push(dur);
    stops.sort_by(f64::total_cmp);
    stops.dedup();
    let mut grid = Vec::with_capacity(stops.len() * SCAN_STEPS);
    for w in stops.windows(2) {
        for k in 0..SCAN_STEPS {
            grid.push(w[0] + (w[1] - w[0]) * k as f64 / SCAN_STEPS as f64);
        }
    }
    grid.push(dur);
    grid
}

/// Inverse of [`source_time`]: the earliest clip-local time (within `0..=duration`) at which the clip
/// shows source second `source_t`; `None` if the clip never shows it (cut out) or is not a media clip.
pub fn local_time_of_source(clip: &Clip, source_t: f64) -> Option<f64> {
    const TOL: f64 = 1e-6;
    let ip = in_point(clip)?;
    if !source_t.is_finite() || !clip.duration.is_finite() || clip.duration < 0.0 {
        return None;
    }
    let dur = clip.duration;
    // Target value of F(t) = ∫₀ᵗ speed.
    let x = if clip.reverse { ip + consumed(clip) - source_t } else { source_t - ip };
    let fit = |t: f64| -> Option<f64> {
        if t >= -TOL && t <= dur + TOL {
            Some(t.clamp(0.0, dur))
        } else {
            None
        }
    };
    match &clip.speed {
        Param::Value(v) => {
            if *v == 0.0 {
                return if x.abs() <= TOL { Some(0.0) } else { None };
            }
            fit(x / v)
        }
        Param::Animated { .. } => {
            // Rounding noise of the summed integral must not hide an exact hit (frozen stretches).
            let tol = 1e-12 * x.abs().max(1.0);
            let grid = scan_grid(&clip.speed, dur);
            let mut f_prev = 0.0;
            if (f_prev - x).abs() <= tol {
                return Some(0.0);
            }
            let mut near: Option<f64> = if (f_prev - x).abs() <= TOL { Some(0.0) } else { None };
            for w in grid.windows(2) {
                if (f_prev - x).abs() <= tol {
                    return Some(w[0]);
                }
                let f_next = f_prev + integrate(&clip.speed, w[0], w[1]);
                let below = f_prev < x;
                let crosses = if below { f_next >= x - tol } else { f_next <= x + tol };
                if crosses {
                    // Earliest t in [w0, w1] where F reaches x.
                    let (mut lo, mut hi) = (w[0], w[1]);
                    for _ in 0..100 {
                        let mid = 0.5 * (lo + hi);
                        let f_mid = f_prev + integrate(&clip.speed, w[0], mid);
                        let reached = if below { f_mid >= x - tol } else { f_mid <= x + tol };
                        if reached {
                            hi = mid;
                        } else {
                            lo = mid;
                        }
                        if hi - lo <= 1e-13 * dur.max(1.0) {
                            break;
                        }
                    }
                    return Some(hi);
                }
                if near.is_none() && (f_next - x).abs() <= TOL {
                    near = Some(w[1]);
                }
                f_prev = f_next;
            }
            near
        }
    }
}

/// The clip's fade envelope at clip-local time: rises linearly over `fade_in`, falls over `fade_out`
/// (0..1; 1 when there are no fades).
pub fn fade_gain(clip: &Clip, t_local: f64) -> f64 {
    let mut g: f64 = 1.0;
    if clip.fade_in > 0.0 {
        g = g.min((t_local / clip.fade_in).clamp(0.0, 1.0));
    }
    if clip.fade_out > 0.0 {
        g = g.min(((clip.duration - t_local) / clip.fade_out).clamp(0.0, 1.0));
    }
    if g.is_nan() {
        0.0
    } else {
        g
    }
}

/// Picture opacity at clip-local time: `opacity` × fade envelope (clamped 0..1).
pub fn visual_opacity(clip: &Clip, t_local: f64) -> f64 {
    (sample(&clip.opacity, t_local) * fade_gain(clip, t_local)).clamp(0.0, 1.0)
}

/// The clip's own sound gain at clip-local time: `volume` × fade envelope, 0 when muted
/// (track volume, mute and ducking are applied by the mixer).
pub fn audio_gain(clip: &Clip, t_local: f64) -> f64 {
    if clip.muted {
        return 0.0;
    }
    (sample(&clip.volume, t_local) * fade_gain(clip, t_local)).max(0.0)
}

// ---------------------------------------------------------------------------------------------
// Splitting animated parameters (used by split/trim ops)
// ---------------------------------------------------------------------------------------------

/// Control points `(x1, y1, x2, y2)` of an ease that is exactly a single cubic bézier.
fn as_bezier(e: &Ease) -> Option<[f64; 4]> {
    const T: f64 = 1.0 / 3.0;
    const T2: f64 = 2.0 / 3.0;
    match e {
        Ease::Linear => Some([T, T, T2, T2]),
        Ease::In => Some([T, 0.0, T2, 0.0]),
        Ease::Out => Some([T, 1.0, T2, 1.0]),
        // y = 1 + c3 (x-1)^3 + c1 (x-1)^2 with x(s) = s  ⇒  P1y = (c1 + 3)/3, P2y = 1.
        Ease::BackOut => Some([T, (1.70158 + 3.0) / 3.0, T2, 1.0]),
        Ease::Bezier([x1, y1, x2, y2]) => Some([x1.clamp(0.0, 1.0), *y1, x2.clamp(0.0, 1.0), *y2]),
        _ => None,
    }
}

/// Eases for the two halves of a key segment cut at progress `x` (0 < x < 1), so that the halves
/// reproduce the original curve. Exact for linear, hold, in, out, back-out and bézier eases; `in_out`
/// and `elastic` keep their ease on both halves (an approximation).
pub fn split_ease(e: &Ease, x: f64) -> (Ease, Ease) {
    match e {
        Ease::Linear => (Ease::Linear, Ease::Linear),
        Ease::Hold => (Ease::Hold, Ease::Hold),
        Ease::InOut | Ease::Elastic => (e.clone(), e.clone()),
        _ => {
            let Some([x1, y1, x2, y2]) = as_bezier(e) else { return (e.clone(), e.clone()) };
            let s = bezier_param_for_x(x1, x2, x);
            let lerp = |a: (f64, f64), b: (f64, f64)| (a.0 + (b.0 - a.0) * s, a.1 + (b.1 - a.1) * s);
            let p0 = (0.0, 0.0);
            let p1 = (x1, y1);
            let p2 = (x2, y2);
            let p3 = (1.0, 1.0);
            let a = lerp(p0, p1);
            let b = lerp(p1, p2);
            let c = lerp(p2, p3);
            let d = lerp(a, b);
            let f = lerp(b, c);
            let m = lerp(d, f); // the split point (x*, y*)
            let left = if m.0.abs() < 1e-12 || m.1.abs() < 1e-12 {
                Ease::Linear
            } else {
                Ease::Bezier([a.0 / m.0, a.1 / m.1, d.0 / m.0, d.1 / m.1])
            };
            let rx = 1.0 - m.0;
            let ry = 1.0 - m.1;
            let right = if rx.abs() < 1e-12 || ry.abs() < 1e-12 {
                Ease::Linear
            } else {
                Ease::Bezier([(f.0 - m.0) / rx, (f.1 - m.1) / ry, (c.0 - m.0) / rx, (c.1 - m.1) / ry])
            };
            (left, right)
        }
    }
}

/// Splits a parameter at clip-local time `a` into the part before (times unchanged) and the part
/// after (times shifted so that `a` becomes 0). Boundary keys are inserted so each half keeps its
/// exact values; constant results collapse to a bare value.
pub fn split_param(p: &Param, a: f64) -> (Param, Param) {
    const EPS: f64 = 1e-9;
    let keys = match p {
        Param::Value(_) => return (p.clone(), p.clone()),
        Param::Animated { keys } if keys.is_empty() => return (p.clone(), p.clone()),
        Param::Animated { keys } => keys,
    };
    let refined = refine_for_split(keys, a);
    let keys = &refined;
    let v_at = sample_keys(keys, a);
    // Index of the segment containing `a` (key i-1 .. key i), if strictly inside one.
    let seg = keys.windows(2).position(|w| w[0].t < a - EPS && w[1].t > a + EPS);
    let (ease_l, ease_r) = match seg {
        Some(i) => {
            let (k0, k1) = (&keys[i], &keys[i + 1]);
            split_ease(&k0.ease, (a - k0.t) / (k1.t - k0.t))
        }
        None => (Ease::Linear, Ease::Linear),
    };
    let mut left: Vec<Key> = Vec::new();
    let mut right: Vec<Key> = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        if k.t < a - EPS {
            let mut k2 = k.clone();
            if seg == Some(i) {
                k2.ease = ease_l.clone();
            }
            left.push(k2);
        } else if k.t > a + EPS {
            right.push(Key { t: k.t - a, v: k.v, ease: k.ease.clone() });
        } else {
            // A key exactly at the split point belongs to both halves.
            left.push(Key { t: a, v: k.v, ease: k.ease.clone() });
            right.push(Key { t: 0.0, v: k.v, ease: k.ease.clone() });
        }
    }
    let has_before = keys.iter().any(|k| k.t < a - EPS);
    let has_after = keys.iter().any(|k| k.t > a + EPS);
    if has_after && !left.iter().any(|k| (k.t - a).abs() <= EPS) {
        left.push(Key { t: a, v: v_at, ease: Ease::Linear });
    }
    if has_before && !right.iter().any(|k| k.t.abs() <= EPS) {
        right.insert(0, Key { t: 0.0, v: v_at, ease: ease_r });
    }
    (normalize_keys(left), normalize_keys(right))
}

/// Makes the segment that contains `a` splittable exactly: an `in_out` segment becomes `in` + a
/// midpoint key + `out` (exact); an `elastic` segment becomes 64 cubic Hermite (bézier) pieces (a close
/// approximation). Other eases split exactly as they are.
fn refine_for_split(keys: &[Key], a: f64) -> Vec<Key> {
    const EPS: f64 = 1e-9;
    let Some(i) = keys.windows(2).position(|w| w[0].t < a - EPS && w[1].t > a + EPS) else { return keys.to_vec() };
    let (k0, k1) = (&keys[i], &keys[i + 1]);
    let (t0, t1, v0, dv) = (k0.t, k1.t, k0.v, k1.v - k0.v);
    let mut mid: Vec<Key> = Vec::new();
    match k0.ease {
        Ease::InOut => {
            mid.push(Key { t: t0, v: v0, ease: Ease::In });
            mid.push(Key { t: 0.5 * (t0 + t1), v: v0 + 0.5 * dv, ease: Ease::Out });
        }
        Ease::Elastic => {
            const N: usize = 64;
            let f = |x: f64| ease(&Ease::Elastic, x);
            let df = |x: f64| {
                let h = 1e-6;
                (f((x + h).min(1.0)) - f((x - h).max(0.0))) / ((x + h).min(1.0) - (x - h).max(0.0))
            };
            for j in 0..N {
                let (xa, xb) = (j as f64 / N as f64, (j + 1) as f64 / N as f64);
                let (ya, yb) = (f(xa), f(xb));
                let dy = yb - ya;
                let h = xb - xa;
                let e = if dy.abs() < 1e-12 {
                    Ease::Linear
                } else {
                    Ease::Bezier([1.0 / 3.0, df(xa) * h / (3.0 * dy), 2.0 / 3.0, 1.0 - df(xb) * h / (3.0 * dy)])
                };
                mid.push(Key { t: t0 + (t1 - t0) * xa, v: v0 + dv * ya, ease: e });
            }
        }
        _ => return keys.to_vec(),
    }
    let mut out: Vec<Key> = keys[..i].to_vec();
    out.extend(mid);
    out.extend(keys[i + 1..].iter().cloned());
    out
}

/// Shifts every key time by `dt`.
pub fn shift_param(p: &Param, dt: f64) -> Param {
    match p {
        Param::Value(_) => p.clone(),
        Param::Animated { keys } => {
            Param::Animated { keys: keys.iter().map(|k| Key { t: k.t + dt, v: k.v, ease: k.ease.clone() }).collect() }
        }
    }
}

/// Multiplies every key time by `factor` (speed changes that keep the source range).
pub fn scale_param_time(p: &Param, factor: f64) -> Param {
    match p {
        Param::Value(_) => p.clone(),
        Param::Animated { keys } => {
            Param::Animated { keys: keys.iter().map(|k| Key { t: k.t * factor, v: k.v, ease: k.ease.clone() }).collect() }
        }
    }
}

/// A key list as a `Param`: no keys or all-equal values collapse to a bare value.
pub fn normalize_keys(keys: Vec<Key>) -> Param {
    match keys.first() {
        None => Param::Value(0.0),
        Some(k0) if keys.iter().all(|k| k.v == k0.v) => Param::Value(k0.v),
        Some(_) => Param::Animated { keys },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    fn media_clip(dur: f64, in_point: f64, speed: Param, reverse: bool) -> Clip {
        Clip {
            id: "c1".into(),
            track: "v1".into(),
            name: String::new(),
            start: Time::At(0.0),
            duration: dur,
            until: None,
            source: Source::Media { asset: "m1".into(), in_point },
            speed,
            reverse,
            transform: Transform::default(),
            opacity: Param::Value(1.0),
            volume: Param::Value(1.0),
            fade_in: 0.0,
            fade_out: 0.0,
            muted: false,
            blend: BlendMode::Normal,
            effects: vec![],
            enabled: true,
            tags: vec![],
            notes: String::new(),
            motion_blur: None,
            frame_blend: false,
            matte: None,
        }
    }

    fn keys(k: &[(f64, f64, Ease)]) -> Param {
        Param::Animated { keys: k.iter().map(|(t, v, e)| Key { t: *t, v: *v, ease: e.clone() }).collect() }
    }

    #[test]
    fn eases_hit_end_points() {
        let all = [
            Ease::Linear,
            Ease::In,
            Ease::Out,
            Ease::InOut,
            Ease::BackOut,
            Ease::Elastic,
            Ease::Bezier([0.25, 0.1, 0.25, 1.0]),
        ];
        for e in &all {
            assert!(close(ease(e, 0.0), 0.0), "{e:?}");
            assert!(close(ease(e, 1.0), 1.0), "{e:?}");
        }
        assert_eq!(ease(&Ease::Hold, 0.99), 0.0);
        assert_eq!(ease(&Ease::Hold, 1.0), 1.0);
        assert!(close(ease(&Ease::In, 0.5), 0.125));
        assert!(close(ease(&Ease::Out, 0.5), 0.875));
        assert!(close(ease(&Ease::InOut, 0.5), 0.5));
        assert!(ease(&Ease::BackOut, 0.7) > 1.0, "back_out overshoots");
        assert!(close(ease(&Ease::Linear, 2.0), 1.0), "clamped input");
    }

    #[test]
    fn bezier_matches_known_curves() {
        // cubic-bezier(1/3, 0, 2/3, 0) is exactly x^3; (0,0,1,1) is linear.
        for i in 0..=20 {
            let x = i as f64 / 20.0;
            assert!(close(cubic_bezier(1.0 / 3.0, 0.0, 2.0 / 3.0, 0.0, x), x * x * x));
            assert!(close(cubic_bezier(0.0, 0.0, 1.0, 1.0, x), x));
            // CSS "ease" is monotonic.
            if i > 0 {
                let prev = cubic_bezier(0.25, 0.1, 0.25, 1.0, (i - 1) as f64 / 20.0);
                assert!(cubic_bezier(0.25, 0.1, 0.25, 1.0, x) >= prev);
            }
        }
        // A known value of CSS ease-in-out (0.42,0,0.58,1) at 0.5 is 0.5 by symmetry.
        assert!(close(cubic_bezier(0.42, 0.0, 0.58, 1.0, 0.5), 0.5));
        // Flat derivative at the ends (x1 = 1) still solves.
        let v = cubic_bezier(1.0, 0.0, 1.0, 1.0, 0.9);
        assert!((0.0..=1.0).contains(&v));
    }

    #[test]
    fn sampling_clamps_and_interpolates() {
        let p = keys(&[(1.0, 0.0, Ease::Linear), (3.0, 10.0, Ease::Hold), (4.0, 20.0, Ease::Linear)]);
        assert_eq!(sample(&p, 0.0), 0.0);
        assert!(close(sample(&p, 2.0), 5.0));
        assert_eq!(sample(&p, 3.5), 10.0);
        assert_eq!(sample(&p, 4.0), 20.0);
        assert_eq!(sample(&p, 99.0), 20.0);
        assert_eq!(sample(&p, f64::NAN), 0.0);
        assert_eq!(sample(&Param::Value(3.0), 1.0), 3.0);
        assert_eq!(sample(&Param::Animated { keys: vec![] }, 1.0), 0.0);
    }

    #[test]
    fn integral_of_linear_ramp_and_curves() {
        // Speed ramps 1 → 3 over 2 s: area = 4; before/after are constant.
        let p = keys(&[(0.0, 1.0, Ease::Linear), (2.0, 3.0, Ease::Linear)]);
        assert!(close(integrate(&p, 0.0, 2.0), 4.0));
        assert!(close(integrate(&p, -1.0, 0.0), 1.0));
        assert!(close(integrate(&p, 2.0, 3.0), 3.0));
        assert!(close(integrate(&p, 2.0, 0.0), -4.0));
        // Ease-in from 0 to 1 over 1 s: ∫x^3 = 0.25.
        let q = keys(&[(0.0, 0.0, Ease::In), (1.0, 1.0, Ease::Linear)]);
        assert!(close(integrate(&q, 0.0, 1.0), 0.25));
        let h = keys(&[(0.0, 2.0, Ease::Hold), (1.0, 5.0, Ease::Linear)]);
        assert!(close(integrate(&h, 0.0, 1.0), 2.0));
    }

    #[test]
    fn source_time_constant_speed_and_reverse() {
        let c = media_clip(4.0, 10.0, Param::Value(2.0), false);
        assert_eq!(source_time(&c, 0.0), Some(10.0));
        assert_eq!(source_time(&c, 1.5), Some(13.0));
        assert_eq!(source_span(&c), Some((10.0, 18.0)));
        assert_eq!(local_time_of_source(&c, 13.0), Some(1.5));
        assert_eq!(local_time_of_source(&c, 18.0), Some(4.0));
        assert_eq!(local_time_of_source(&c, 18.5), None);
        assert_eq!(local_time_of_source(&c, 9.0), None);

        let r = media_clip(4.0, 10.0, Param::Value(1.0), true);
        assert_eq!(source_time(&r, 0.0), Some(14.0));
        assert_eq!(source_time(&r, 4.0), Some(10.0));
        assert_eq!(source_span(&r), Some((10.0, 14.0)));
        assert_eq!(local_time_of_source(&r, 13.0), Some(1.0));
    }

    #[test]
    fn freeze_and_ramp_inverse() {
        let f = media_clip(3.0, 5.0, Param::Value(0.0), false);
        assert_eq!(source_time(&f, 2.0), Some(5.0));
        assert_eq!(source_span(&f), Some((5.0, 5.0)));
        assert_eq!(local_time_of_source(&f, 5.0), Some(0.0));
        assert_eq!(local_time_of_source(&f, 5.1), None);

        // 1 s normal, then a 1 s freeze, then normal again.
        let sp = keys(&[(1.0, 1.0, Ease::Hold), (1.0001, 0.0, Ease::Hold), (2.0, 1.0, Ease::Linear)]);
        let c = media_clip(4.0, 0.0, sp, false);
        let s = source_time(&c, 3.0).unwrap();
        assert!((s - 2.0).abs() < 1e-3, "{s}");
        // The frozen source second (≈1.0) is first shown at local 1.0.
        let t = local_time_of_source(&c, 1.0).unwrap();
        assert!((t - 1.0).abs() < 1e-3, "{t}");
        let t = local_time_of_source(&c, 1.5).unwrap();
        assert!((t - 2.5).abs() < 1e-3, "{t}");

        // Ramp 1 → 3: source at 2 s is 4; inverse gives 2.
        let ramp = media_clip(2.0, 0.0, keys(&[(0.0, 1.0, Ease::Linear), (2.0, 3.0, Ease::Linear)]), false);
        assert!(close(source_time(&ramp, 2.0).unwrap(), 4.0));
        assert!((local_time_of_source(&ramp, 4.0).unwrap() - 2.0).abs() < 1e-6);
        let mid = source_time(&ramp, 1.3).unwrap();
        assert!((local_time_of_source(&ramp, mid).unwrap() - 1.3).abs() < 1e-6);
    }

    #[test]
    fn fades_and_gain() {
        let mut c = media_clip(4.0, 0.0, Param::Value(1.0), false);
        c.fade_in = 1.0;
        c.fade_out = 2.0;
        assert_eq!(fade_gain(&c, 0.0), 0.0);
        assert!(close(fade_gain(&c, 0.5), 0.5));
        assert_eq!(fade_gain(&c, 1.5), 1.0);
        assert!(close(fade_gain(&c, 3.0), 0.5));
        assert_eq!(fade_gain(&c, 4.0), 0.0);
        c.muted = true;
        assert_eq!(audio_gain(&c, 1.5), 0.0);
        assert_eq!(visual_opacity(&c, 1.5), 1.0);
    }

    #[test]
    fn split_param_is_exact() {
        for e in [Ease::Linear, Ease::In, Ease::Out, Ease::BackOut, Ease::Bezier([0.2, 0.7, 0.4, 1.0]), Ease::Hold] {
            let p = keys(&[(0.0, 0.0, e.clone()), (2.0, 10.0, Ease::Linear), (3.0, 4.0, Ease::Linear)]);
            let a = 0.7;
            let (l, r) = split_param(&p, a);
            for i in 0..=40 {
                let t = i as f64 * 3.0 / 40.0;
                let want = sample(&p, t);
                let got = if t <= a { sample(&l, t) } else { sample(&r, t - a) };
                assert!((want - got).abs() < 1e-6, "{e:?} t={t} want {want} got {got}");
            }
        }
        // Keys entirely on one side collapse.
        let p = keys(&[(5.0, 1.0, Ease::Linear), (6.0, 2.0, Ease::Linear)]);
        let (l, r) = split_param(&p, 1.0);
        assert_eq!(l, Param::Value(1.0));
        assert!(r.is_animated());
        assert_eq!(split_param(&Param::Value(3.0), 1.0), (Param::Value(3.0), Param::Value(3.0)));
    }
}
