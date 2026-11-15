//! Every transition: exact at its edges, different in the middle; media handles beyond the cut.

mod common;
use aiframes_core::effects::TRANSITIONS;
use aiframes_core::{Project, Transition};
use aiframes_render::Image;
use common::*;
use serde_json::json;

fn two_clips(kind: Option<&str>) -> Project {
    let mut p = project(64, 36, 30.0);
    add_media(&mut p, "red", &flat_video("red4", "red", 4));
    add_media(&mut p, "blue", &flat_video("blue4", "blue", 4));
    p.clips = vec![
        clip(json!({"id": "A", "track": "v1", "start": 0, "duration": 2, "source": {"type": "media", "asset": "red"}})),
        clip(json!({"id": "B", "track": "v1", "start": {"after": "A"}, "duration": 2, "source": {"type": "media", "asset": "blue", "in_point": 1.0}})),
    ];
    if let Some(k) = kind {
        p.transitions.push(Transition { id: "tr1".into(), from: "A".into(), to: "B".into(), kind: k.into(), duration: 1.0, params: Default::default(), colors: Default::default() });
    }
    p
}

#[test]
fn every_transition_is_exact_at_the_edges_and_differs_in_the_middle() {
    let r = renderer();
    let plain = two_clips(None);
    let a_only = render(&r, &plain, 1.5, 1.0);
    let b_only = render(&r, &plain, 2.5, 1.0);
    let ab = diff(&a_only, &b_only);
    assert!(ab > 50.0);
    for spec in TRANSITIONS {
        let p = two_clips(Some(spec.kind));
        let start = render(&r, &p, 1.5, 1.0);
        let mid = render(&r, &p, 2.0, 1.0);
        let last = render(&r, &p, 2.5 - 1.0 / 30.0, 1.0);
        let end = render(&r, &p, 2.5, 1.0);
        assert!(diff(&start, &a_only) < 0.5, "{}: window start is the outgoing clip ({})", spec.kind, diff(&start, &a_only));
        assert!(diff(&end, &b_only) < 0.5, "{}: window end is the incoming clip", spec.kind);
        assert!(diff(&last, &b_only) < ab * 0.25, "{}: last frame converges on B ({})", spec.kind, diff(&last, &b_only));
        if spec.kind != "luma_wipe" {
            // (A luma wipe of a flat-colour clip switches at once: tested with a gradient elsewhere.)
            assert!(diff(&mid, &a_only) > ab * 0.1 && diff(&mid, &b_only) > ab * 0.1, "{}: middle differs from both ends", spec.kind);
        }
        // Before the window: plain A.
        assert!(diff(&render(&r, &p, 1.0, 1.0), &render(&r, &plain, 1.0, 1.0)) < 0.5);
    }
}

/// Frame number from bars with an adaptive threshold (works on dimmed pictures).
fn dim_code(img: &Image) -> u32 {
    let y = img.height / 2;
    let vals: Vec<u8> = (0..9).map(|k| px(img, ((k as f64 + 0.5) * img.width as f64 / 9.0) as u32, y)[0]).collect();
    let (lo, hi) = (*vals.iter().min().unwrap() as u32, *vals.iter().max().unwrap() as u32);
    let thr = (lo + hi) / 2;
    vals.iter().enumerate().filter(|(_, v)| **v as u32 > thr).map(|(k, _)| 1 << k).sum()
}

#[test]
fn transitions_use_media_handles_or_hold_edge_frames() {
    let r = renderer();
    let bars = bars(30, 6);
    let mk = |a_in: f64, b_in: f64| {
        let mut p = project(320, 180, 30.0);
        add_media(&mut p, "m", &bars);
        p.clips = vec![
            clip(json!({"id": "A", "track": "v1", "start": 0, "duration": 2, "source": {"type": "media", "asset": "m", "in_point": a_in}})),
            clip(json!({"id": "B", "track": "v1", "start": 2, "duration": 2, "source": {"type": "solid", "color": "#000000"}})),
            clip(json!({"id": "C", "track": "v2", "start": 2, "duration": 2, "source": {"type": "media", "asset": "m", "in_point": b_in}})),
            clip(json!({"id": "D", "track": "v2", "start": 0, "duration": 2, "source": {"type": "solid", "color": "#000000"}})),
        ];
        p.transitions = vec![
            Transition { id: "t1".into(), from: "A".into(), to: "B".into(), kind: "crossfade".into(), duration: 1.0, params: Default::default(), colors: Default::default() },
            Transition { id: "t2".into(), from: "D".into(), to: "C".into(), kind: "crossfade".into(), duration: 1.0, params: Default::default(), colors: Default::default() },
        ];
        p
    };
    // Outgoing A (v1) past its end: track v2 hidden.
    let mut p = mk(0.0, 3.0);
    p.tracks[1].hidden = true;
    assert_eq!(dim_code(&render(&r, &p, 2.1, 1.0)), 63, "A plays its handle after the cut");
    let mut p = mk(4.0, 3.0);
    p.tracks[1].hidden = true;
    assert_eq!(dim_code(&render(&r, &p, 2.1, 1.0)), 179, "no handle: the last frame is held");
    // Incoming C (v2) before its start.
    let p = mk(0.0, 3.0);
    let mut p2 = p.clone();
    p2.tracks[0].hidden = true;
    assert_eq!(dim_code(&render(&r, &p2, 1.9, 1.0)), 87, "C plays its handle before the cut");
    let mut p3 = mk(0.0, 0.0);
    p3.tracks[0].hidden = true;
    assert_eq!(dim_code(&render(&r, &p3, 1.9, 1.0)), 0, "no handle before the in-point: the first frame is held");
}

#[test]
fn layers_at_follows_sliding_transitions() {
    let r = renderer();
    let p = two_clips(Some("push_left"));
    let res = resolved(&p);
    let boxes = r.layers_at(std::path::Path::new("."), &p, &res, 2.0);
    assert_eq!(boxes.len(), 2);
    // Halfway through a push the outgoing clip is half off to the left, the incoming half in.
    assert!((boxes[0].bounds[0] + 32.0).abs() < 0.5, "{:?}", boxes[0].bounds);
    assert!((boxes[1].bounds[0] - 32.0).abs() < 0.5, "{:?}", boxes[1].bounds);
    let img = render(&r, &p, 2.0, 1.0);
    assert!(px(&img, 10, 18)[0] > 200 && px(&img, 54, 18)[2] > 200);
}
