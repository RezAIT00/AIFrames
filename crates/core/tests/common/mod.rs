#![allow(dead_code)]
use aiframes_core::*;
use serde_json::{json, Value};

pub fn op(v: Value) -> Op {
    Op::from_json(&v).unwrap_or_else(|e| panic!("bad op {v}: {e}"))
}

pub fn ap(p: &mut Project, v: Value) -> OpOutcome {
    let o = op(v.clone());
    apply(p, &o).unwrap_or_else(|e| panic!("op {v} failed: {e}"))
}

pub fn try_ap(p: &mut Project, v: Value) -> Result<OpOutcome, EditError> {
    apply(p, &op(v))
}

/// v1 (main), v2, a1 video/audio tracks; mv = 120 s video with audio, m1 = 60 s audio,
/// transcript for mv and m1: word i = "w{i}" at source i+1.0 .. i+1.5 (50 words).
pub fn base() -> Project {
    let words: Vec<Value> = (0..50).map(|i| json!({"text": format!("w{i}"), "start": i as f64 + 1.0, "end": i as f64 + 1.5})).collect();
    serde_json::from_value(json!({
        "format": 1,
        "name": "T",
        "settings": {"width": 1920, "height": 1080, "fps": 30.0},
        "media": [
            {"id":"mv","path":"media/g.mp4","kind":"video","info":{"duration":120.0,"width":1920,"height":1080,"fps":30.0,"has_video":true,"has_audio":true}},
            {"id":"m1","path":"media/v.wav","kind":"audio","info":{"duration":60.0,"has_audio":true}}
        ],
        "tracks": [
            {"id":"v1","kind":"video","role":"main"},
            {"id":"v2","kind":"video"},
            {"id":"a1","kind":"audio"}
        ],
        "transcripts": {"mv": {"words": words.clone()}, "m1": {"words": words}}
    }))
    .unwrap()
}

pub fn span(p: &Project, id: &str) -> Option<(f64, f64)> {
    resolve(p).span(id).map(|s| (s.start, s.end))
}

pub fn clip<'a>(p: &'a Project, id: &str) -> &'a Clip {
    p.clips.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("no clip {id}"))
}

pub fn close(a: (f64, f64), b: (f64, f64)) -> bool {
    (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6
}

/// The audio side of frame snapping (see `resolve`): every audio-track clip keeps its raw length,
/// clips that touch (raw) still touch, clips that do not overlap (raw) still do not, and an audio clip
/// anchored to another audio clip moves by the same shift. Returns the violations.
pub fn audio_snap_violations(p: &Project) -> Vec<String> {
    let (shown, raw) = (resolve(p), resolve_raw(p));
    let audio: Vec<&Clip> = p.clips.iter().filter(|c| p.tracks.iter().any(|t| t.id == c.track && t.kind == TrackKind::Audio)).collect();
    let shift = |id: &str| Some(shown.span(id)?.start - raw.span(id)?.start);
    let mut bad = Vec::new();
    for c in &audio {
        let (Some(s), Some(r)) = (shown.span(&c.id), raw.span(&c.id)) else { continue };
        if ((s.end - s.start) - (r.end - r.start)).abs() > 1e-9 {
            bad.push(format!("{}: length {} became {}", c.id, r.end - r.start, s.end - s.start));
        }
        if let Time::Anchored(a) = &c.start {
            let t = match &a.to {
                AnchorTarget::After(t) | AnchorTarget::With(t) | AnchorTarget::Before(t) => Some(t.as_str()),
                AnchorTarget::Word(w) => Some(w.clip.as_str()),
                AnchorTarget::Marker(_) => None,
            };
            if let Some(t) = t.filter(|t| audio.iter().any(|x| x.id == *t)) {
                if let (Some(a), Some(b)) = (shift(&c.id), shift(t)) {
                    if (a - b).abs() > 1e-9 {
                        bad.push(format!("{} moved {a} but its anchor {t} moved {b}", c.id));
                    }
                }
            }
        }
    }
    let mut tracks: Vec<&str> = audio.iter().map(|c| c.track.as_str()).collect();
    tracks.sort();
    tracks.dedup();
    for t in tracks {
        let mut on: Vec<(&str, Span, Span)> = audio.iter().filter(|c| c.track == t).filter_map(|c| Some((c.id.as_str(), raw.span(&c.id)?, shown.span(&c.id)?))).collect();
        on.sort_by(|a, b| a.1.start.total_cmp(&b.1.start));
        for i in 0..on.len() {
            for j in i + 1..on.len() {
                let (a, b) = (&on[i], &on[j]);
                let (rg, sg) = (b.1.start - a.1.end, b.2.start - a.2.end);
                if rg.abs() <= 1e-9 && sg.abs() > 1e-9 {
                    bad.push(format!("{} and {} touch but snapping opened {sg} s between them", a.0, b.0));
                }
                if rg >= -1e-9 && sg < -1e-9 {
                    bad.push(format!("{} and {} were apart ({rg} s) but snapping made them overlap by {} s", a.0, b.0, -sg));
                }
            }
        }
    }
    bad
}
