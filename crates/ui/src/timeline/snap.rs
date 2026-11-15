//! Snapping: the playhead, clip edges, markers, in/out and 0 attract dragged times within a few
//! pixels (so it works the same at every zoom).

use aiframes_core::{Id, Project, Resolved};
use std::collections::HashSet;

/// Snap distance in pixels.
pub const SNAP_PX: f64 = 8.0;

/// Times that attract: 0, the playhead, in/out, markers and the edges of every clip not in `exclude`.
pub fn targets(p: &Project, r: &Resolved, playhead: f64, in_out: [Option<f64>; 2], exclude: &HashSet<Id>) -> Vec<f64> {
    let mut v = vec![0.0, playhead];
    v.extend(in_out.iter().flatten().copied());
    v.extend(r.markers.values().copied());
    for c in &p.clips {
        if exclude.contains(&c.id) {
            continue;
        }
        if let Some(s) = r.span(&c.id) {
            v.push(s.start);
            v.push(s.end);
        }
    }
    v.retain(|t| t.is_finite());
    v.sort_by(f64::total_cmp);
    v.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    v
}

/// The target nearest to `t` within `px` pixels at `zoom` px/s.
pub fn snap(t: f64, targets: &[f64], zoom: f64, px: f64) -> Option<f64> {
    let tol = px / zoom.max(1e-9);
    targets.iter().copied().filter(|x| (x - t).abs() <= tol).min_by(|a, b| (a - t).abs().total_cmp(&(b - t).abs()))
}

/// For a group whose edges are at `edges` (already shifted by the drag): the extra shift that puts
/// the nearest edge on a target, and that target.
pub fn snap_edges(edges: &[f64], targets: &[f64], zoom: f64, px: f64) -> Option<(f64, f64)> {
    let mut best: Option<(f64, f64)> = None;
    for &e in edges {
        if let Some(t) = snap(e, targets, zoom, px) {
            let d = t - e;
            if best.is_none_or(|(bd, _)| d.abs() < bd.abs()) {
                best = Some((d, t));
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::tests::demo;
    use aiframes_core::resolve;

    #[test]
    fn snaps_within_pixels_at_any_zoom() {
        let t = vec![0.0, 5.0, 10.0];
        assert_eq!(snap(5.1, &t, 100.0, 8.0), None, "0.1 s is 10 px at 100 px/s: too far");
        assert_eq!(snap(5.05, &t, 100.0, 8.0), Some(5.0));
        assert_eq!(snap(5.2, &t, 100.0, 8.0), None);
        assert_eq!(snap(5.2, &t, 10.0, 8.0), Some(5.0), "zoomed out, 0.2 s is 2 px");
        assert_eq!(snap_edges(&[4.97, 9.0], &t, 100.0, 8.0), Some((5.0 - 4.97, 5.0)));
        assert_eq!(snap_edges(&[3.0, 9.98], &t, 100.0, 8.0).map(|x| x.1), Some(10.0));
        assert_eq!(snap_edges(&[3.0], &t, 100.0, 8.0), None);
    }

    #[test]
    fn targets_exclude_dragged_clips() {
        let p = demo();
        let r = resolve(&p);
        let ex: HashSet<Id> = ["c2".to_string()].into_iter().collect();
        let t = targets(&p, &r, 7.0, [Some(2.0), None], &ex);
        assert_eq!(t, vec![0.0, 2.0, 7.0, 10.0, 20.0]);
    }
}
