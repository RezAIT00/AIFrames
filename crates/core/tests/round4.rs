//! Round-4 regression tests, mostly ported from the dogfood run (dogfood/out/t3/c20–c25 and the
//! cold-open project): frame-accurate spans, word re-anchoring, `until` end anchors, spacing,
//! batch post-checks, caption ownership, NBSP-joined words and precise overlap messages.
#![allow(clippy::approx_constant)] // word times like 6.28 s are data, not τ
mod common;
use aiframes_core::*;
use common::*;
use serde_json::{json, Value};

const WORDS_34: &[(&str, f64, f64)] = &[
    ("I", 0.0, 0.16),
    ("gave", 0.16, 0.36),
    ("Claude", 0.36, 0.72),
    ("the", 0.72, 0.84),
    ("exact", 0.84, 1.14),
    ("same", 1.14, 1.44),
    ("prompt", 1.44, 1.74),
    ("three", 1.74, 1.96),
    ("times.", 1.96, 2.38),
    ("Make", 2.66, 2.88),
    ("a", 2.88, 2.98),
    ("3D", 2.98, 3.4),
    ("Bloons", 3.4, 3.88),
    ("tower", 3.88, 4.14),
    ("defense", 4.14, 4.44),
    ("game.", 4.44, 4.76),
    ("The", 4.9, 5.18),
    ("only", 5.18, 5.34),
    ("thing", 5.34, 5.5),
    ("that", 5.5, 5.64),
    ("changes", 5.64, 5.94),
    ("is", 5.94, 6.16),
    ("how", 6.16, 6.28),
    ("much", 6.28, 6.44),
    ("time", 6.44, 6.62),
    ("it", 6.62, 6.76),
    ("gets.", 6.76, 7.08),
    ("10", 7.34, 7.72),
    ("minutes,", 7.72, 8.06),
    ("1", 8.32, 8.58),
    ("hour,", 8.58, 8.86),
    ("and", 9.02, 9.22),
    ("4", 9.22, 9.42),
    ("hours.", 9.42, 9.76),
];

fn transcript(words: &[(&str, f64, f64)]) -> Value {
    json!({"language": "en", "words": words.iter().map(|(t, s, e)| json!({"text": t, "start": s, "end": e})).collect::<Vec<_>>()})
}

/// The transcript with "3D" re-tokenised as "three" "D" (35 words: every later index + 1).
fn words_35() -> Vec<(&'static str, f64, f64)> {
    let mut w = WORDS_34.to_vec();
    w.splice(11..12, [("three", 2.98, 3.19), ("D", 3.19, 3.4)]);
    w
}

/// dogfood/out/t3/anchors.aifr: m1 voice (10.18 s), m2/m3 silent footage; v1, v2, a1, a2.
fn anchors() -> Project {
    serde_json::from_value(json!({
        "format": 1, "name": "anchors",
        "settings": {"width": 1920, "height": 1080, "fps": 30.0},
        "media": [
            {"id": "m1", "path": "voice/01.wav", "kind": "audio", "info": {"duration": 10.18, "has_audio": true, "audio_channels": 1, "audio_rate": 24000}},
            {"id": "m2", "path": "footage/s240_strike.mp4", "kind": "video", "info": {"duration": 16.0, "width": 1920, "height": 1080, "fps": 30.0, "has_video": true}},
            {"id": "m3", "path": "footage/s60_blimp.mp4", "kind": "video", "info": {"duration": 36.0, "width": 1920, "height": 1080, "fps": 30.0, "has_video": true}}
        ],
        "tracks": [
            {"id": "v1", "name": "Main", "kind": "video", "role": "main"},
            {"id": "v2", "name": "Overlay", "kind": "video", "role": "overlay"},
            {"id": "a1", "name": "Voice", "kind": "audio", "role": "voice"},
            {"id": "a2", "name": "Music", "kind": "audio", "role": "music"}
        ],
        "id_counters": {"m": 3}
    }))
    .unwrap()
}

/// The c20 setup: voice, a shot and a label on word 27 ("10").
fn labelled() -> Project {
    let mut p = anchors();
    let ops: Vec<Op> = vec![
        op(json!({"op": "set_transcript", "asset": "m1", "transcript": transcript(WORDS_34)})),
        op(json!({"op": "append_clip", "track": "a1", "clip": {"id": "vo", "asset": "m1"}})),
        op(json!({"op": "add_clip", "clip": {"id": "shotA", "track": "v1", "asset": "m2", "start": 0, "duration": 7.26}})),
        op(json!({"op": "add_clip", "clip": {"id": "shot10", "track": "v1", "asset": "m3", "start": {"word": {"clip": "vo", "index": 27}, "offset": -0.08}, "duration": 2.9}})),
        op(json!({"op": "add_clip", "clip": {"id": "lbl", "track": "v2", "text": "10 minutes", "start": {"word": {"clip": "vo", "index": 27}}, "duration": 0.9}})),
    ];
    apply_batch(&mut p, &ops).unwrap();
    p
}

fn word_anchor(c: &Clip) -> &WordRef {
    match &c.start {
        Time::Anchored(Anchor { to: AnchorTarget::Word(w), .. }) => w,
        other => panic!("{} is not word-anchored: {other:?}", c.id),
    }
}

fn on_frame(t: f64, num: f64, den: f64) -> bool {
    let f = t * num / den;
    (f - f.round()).abs() < 1e-6
}

fn codes(p: &Project) -> Vec<String> {
    validate(p, &resolve(p)).into_iter().map(|f| f.code.to_string()).collect()
}

// ------------------------------------------------------------------------------- 1. frame grid

#[test]
fn sub_frame_gap_from_dogfood_is_gone() {
    // c22/c23: B placed 0.4 ms after A ended rendered a black frame at 2.0 s.
    let mut p = anchors();
    apply_batch(
        &mut p,
        &[
            op(json!({"op": "add_clip", "clip": {"id": "A", "track": "v1", "asset": "m2", "start": 0, "duration": 2.0}})),
            op(json!({"op": "add_clip", "clip": {"id": "B", "track": "v1", "asset": "m3", "start": 2.0004, "duration": 2.0}})),
        ],
    )
    .unwrap();
    assert!(close(span(&p, "A").unwrap(), (0.0, 2.0)));
    assert!(close(span(&p, "B").unwrap(), (2.0, 4.0)), "B shows from frame 60: {:?}", span(&p, "B"));
    let c = codes(&p);
    assert!(!c.iter().any(|c| c.contains("gap") || c.contains("overlap")), "{c:?}");
    // Trimming A to 2.0006 lands on the same frame: no overlap.
    let o = try_ap(&mut p, json!({"op": "trim_clip", "clip": "A", "edge": "end", "to": 2.0006})).unwrap();
    assert!(o.warnings.is_empty(), "{:?}", o.warnings);
    assert!(close(span(&p, "A").unwrap(), (0.0, 2.0)));
    // The describe text shows frame-accurate times.
    let d = describe(&p, &resolve(&p));
    assert!(d.contains("[2.00–4.00] B"), "{d}");
}

#[test]
fn ntsc_rate_uses_the_exact_rational_grid() {
    assert_eq!(frame_rate(29.97), (30000, 1001));
    assert_eq!(frame_rate(23.976), (24000, 1001));
    assert_eq!(frame_rate(59.94), (60000, 1001));
    assert_eq!(frame_rate(25.0), (25, 1));
    let mut p = base();
    p.settings.fps = 29.97;
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "a", "track": "v1", "asset": "mv", "start": 0.0, "duration": 1.0}}));
    ap(&mut p, json!({"op": "append_clip", "track": "v1", "clip": {"id": "b", "asset": "mv", "duration": 1.0}}));
    ap(&mut p, json!({"op": "append_clip", "track": "v1", "clip": {"id": "c", "asset": "mv", "duration": 1.0}}));
    let r = resolve(&p);
    for id in ["a", "b", "c"] {
        let s = r.span(id).unwrap();
        assert!(on_frame(s.start, 30000.0, 1001.0) && on_frame(s.end, 30000.0, 1001.0), "{id}: {s:?}");
    }
    // 1 s at 29.97 is 30 frames = 1.001 s; the chain stays contiguous.
    assert!((r.span("a").unwrap().end - 1.001).abs() < 1e-9);
    assert_eq!(r.span("a").unwrap().end, r.span("b").unwrap().start);
    assert_eq!(r.span("b").unwrap().end, r.span("c").unwrap().start);
    // Raw (unsnapped) times are still available.
    assert!((resolve_raw(&p).span("b").unwrap().start - 1.0).abs() < 1e-12);
}

#[test]
fn video_edits_land_on_frames_audio_edits_do_not() {
    let mut p = base();
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "v", "track": "v1", "asset": "mv", "start": 0.0, "duration": 4.0}}));
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "a", "track": "a1", "asset": "m1", "start": 0.0, "duration": 4.0}}));
    ap(&mut p, json!({"op": "split_clip", "clip": "v", "at": 1.01, "new_id": "v_r"}));
    ap(&mut p, json!({"op": "split_clip", "clip": "a", "at": 1.01, "new_id": "a_r"}));
    assert!(close(span(&p, "v").unwrap(), (0.0, 1.0)), "video split snaps to frame 30: {:?}", span(&p, "v"));
    assert!(close(span(&p, "v_r").unwrap(), (1.0, 4.0)));
    assert!(close(span(&p, "a").unwrap(), (0.0, 1.01)), "audio split stays exact");
    // The right half's media continues exactly where the left half's ends.
    let Source::Media { in_point, .. } = &clip(&p, "v_r").source else { panic!() };
    assert!((in_point - 1.0).abs() < 1e-9, "{in_point}");
    // Trims by a sub-frame delta land on frames.
    ap(&mut p, json!({"op": "trim_clip", "clip": "v_r", "edge": "end", "delta": -0.51}));
    let s = span(&p, "v_r").unwrap();
    assert!(on_frame(s.1, 30.0, 1.0) && (s.1 - 3.5).abs() < 1e-9, "{s:?}");
    // An insert at a sub-frame time splits on a frame.
    ap(&mut p, json!({"op": "insert_clip", "track": "v1", "at": 0.52, "clip": {"id": "ins", "asset": "mv", "duration": 0.5}}));
    let r = resolve(&p);
    for c in p.clips.iter().filter(|c| c.track == "v1") {
        let s = r.span(&c.id).unwrap();
        assert!(on_frame(s.start, 30.0, 1.0) && on_frame(s.end, 30.0, 1.0), "{}: {s:?}", c.id);
    }
    assert!(!codes(&p).iter().any(|c| c.contains("overlap")));
}

// ------------------------------------------------------------------------------- 2. word re-anchoring

#[test]
fn retokenised_transcript_keeps_the_label_on_its_word() {
    // c20/c21: "3D" → "three" "D" shifted every later index; the label stayed on index 27 ("gets.").
    let mut p = labelled();
    assert_eq!(word_anchor(clip(&p, "lbl")).text.as_deref(), Some("10"), "ops record the anchored word");
    let before = (span(&p, "lbl").unwrap(), span(&p, "shot10").unwrap());
    let o = ap(&mut p, json!({"op": "set_transcript", "asset": "m1", "transcript": transcript(&words_35())}));
    for id in ["lbl", "shot10"] {
        let w = word_anchor(clip(&p, id));
        assert_eq!((w.index, w.text.as_deref()), (28, Some("10")), "{id}");
    }
    assert!(close(span(&p, "lbl").unwrap(), before.0) && close(span(&p, "shot10").unwrap(), before.1));
    assert!(o.warnings.iter().all(|w| !w.contains("lbl")), "same word, no warning: {:?}", o.warnings);
    assert!(!codes(&p).iter().any(|c| c == "word_anchor_text_mismatch"));
    // c21: after the shot is deleted the label still follows.
    ap(&mut p, json!({"op": "delete_clip", "clip": "shot10"}));
    assert_eq!(word_anchor(clip(&p, "lbl")).index, 28);
}

#[test]
fn changed_word_text_warns_and_missing_words_become_absolute() {
    let mut p = labelled();
    let t0 = span(&p, "lbl").unwrap();
    let mut w = WORDS_34.to_vec();
    w[27].0 = "ten";
    let o = ap(&mut p, json!({"op": "set_transcript", "asset": "m1", "transcript": transcript(&w)}));
    assert!(o.warnings.iter().any(|x| x.contains("lbl") && x.contains("\"10\"") && x.contains("\"ten\"")), "{:?}", o.warnings);
    assert!(close(span(&p, "lbl").unwrap(), t0));
    // Words that no longer exist at that time: the clip keeps its time as an absolute time.
    let mut w = WORDS_34.to_vec();
    w.truncate(20);
    let o = ap(&mut p, json!({"op": "set_transcript", "asset": "m1", "transcript": transcript(&w)}));
    assert!(o.warnings.iter().any(|x| x.contains("lbl") && x.contains("absolute")), "{:?}", o.warnings);
    assert_eq!(clip(&p, "lbl").start, Time::At(7.34));
    assert!(resolve(&p).errors.is_empty());
}

#[test]
fn hand_edited_transcript_mismatch_is_found_and_fixed() {
    let mut p = labelled();
    // Someone replaces the transcript outside the ops (e.g. a re-transcription pasted into the file).
    let words: Vec<Word> = serde_json::from_value(transcript(&words_35())["words"].clone()).unwrap();
    p.transcripts.get_mut("m1").unwrap().words = words;
    let r = resolve(&p);
    let f = validate(&p, &r);
    let m = f.iter().find(|f| f.code == "word_anchor_text_mismatch" && f.ids.contains(&"lbl".to_string())).expect("finding");
    assert!(m.message.contains("\"10\"") && m.message.contains("gets."), "{}", m.message);
    assert_eq!(m.fix_ops.len(), 1);
    apply_batch(&mut p, &m.fix_ops).unwrap();
    assert_eq!(word_anchor(clip(&p, "lbl")).index, 28);
}

// ------------------------------------------------------------------------------- 3. until

#[test]
fn badge_with_until_follows_a_speed_change() {
    let mut p = base();
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "c1", "track": "v1", "asset": "mv", "start": 0.0, "duration": 8.0}}));
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "badge", "track": "v2", "text": "8x", "start": {"with": "c1", "offset": 0.5}, "duration": 7.5}}));
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "plain", "track": "v2", "text": "x", "start": {"with": "c1", "offset": 0.0}, "duration": 0.4}}));
    ap(&mut p, json!({"op": "set_until", "clip": "badge", "until": {"after": "c1"}}));
    { let d = describe(&p, &resolve(&p)); assert!(d.contains("until end of c1"), "{d}"); }
    ap(&mut p, json!({"op": "set_speed", "clip": "c1", "speed": 8.0}));
    let (b, c) = (span(&p, "badge").unwrap(), span(&p, "c1").unwrap());
    assert!(close(c, (0.0, 1.0)), "{c:?}");
    assert!(close(b, (0.5, 1.0)), "the badge ends with c1: {b:?}");
    assert!((clip(&p, "badge").duration - 0.5).abs() < 1e-9, "duration kept in sync");
    // Trimming the base moves the end too.
    ap(&mut p, json!({"op": "trim_clip", "clip": "c1", "edge": "end", "delta": 1.0}));
    assert!(close(span(&p, "badge").unwrap(), (0.5, 2.0)));
    // An until before the start is refused.
    let e = try_ap(&mut p, json!({"op": "set_until", "clip": "badge", "until": 0.2})).unwrap_err().to_string();
    assert!(e.contains("before") || e.contains("start"), "{e}");
    // Clearing keeps the current length.
    ap(&mut p, json!({"op": "set_until", "clip": "badge", "until": null}));
    assert!(clip(&p, "badge").until.is_none());
    assert!(close(span(&p, "badge").unwrap(), (0.5, 2.0)));
}

#[test]
fn overlay_outliving_its_base_is_reported_with_a_fix() {
    let mut p = base();
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "c1", "track": "v1", "asset": "mv", "start": 0.0, "duration": 8.0}}));
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "badge", "track": "v2", "text": "8x", "start": {"with": "c1"}, "duration": 8.0}}));
    ap(&mut p, json!({"op": "set_speed", "clip": "c1", "speed": 8.0}));
    let f = validate(&p, &resolve(&p));
    let o = f.iter().find(|f| f.code == "overlay_outlives_base").expect("finding");
    assert_eq!(o.ids, vec!["badge".to_string()]);
    apply_batch(&mut p, &o.fix_ops).unwrap();
    assert!(close(span(&p, "badge").unwrap(), span(&p, "c1").unwrap()));
    assert!(!codes(&p).iter().any(|c| c == "overlay_outlives_base"));
}

#[test]
fn split_gives_the_right_half_the_until() {
    let mut p = base();
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "c1", "track": "v1", "asset": "mv", "start": 0.0, "duration": 6.0}}));
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "t", "track": "v2", "text": "T", "start": {"with": "c1"}, "duration": 6.0, "until": {"after": "c1"}}}));
    ap(&mut p, json!({"op": "split_clip", "clip": "t", "at": 2.0, "new_id": "t2"}));
    assert!(clip(&p, "t").until.is_none());
    assert!(clip(&p, "t2").until.is_some());
    assert!(close(span(&p, "t").unwrap(), (0.0, 2.0)) && close(span(&p, "t2").unwrap(), (2.0, 6.0)));
    ap(&mut p, json!({"op": "trim_clip", "clip": "c1", "edge": "end", "delta": -1.0}));
    assert!(close(span(&p, "t2").unwrap(), (2.0, 5.0)));
    // A hand-written until at or before the start is a resolve error with its own code.
    let mut q = p.clone();
    q.clips.iter_mut().find(|c| c.id == "t2").unwrap().until = Some(Time::At(1.0));
    assert!(codes(&q).iter().any(|c| c == "until_before_start"), "{:?}", codes(&q));
}

// ------------------------------------------------------------------------------- 4. spacing

#[test]
fn set_spacing_and_close_gaps_report_the_gaps() {
    let mut p = base();
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "a", "track": "a1", "asset": "m1", "start": 0.0, "duration": 1.0}}));
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "b", "track": "a1", "asset": "m1", "start": 2.4, "duration": 1.0}}));
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "c", "track": "a1", "asset": "m1", "start": 4.0, "duration": 1.0}}));
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "lbl", "track": "v2", "text": "B", "start": {"with": "b"}, "duration": 0.5}}));
    let o = ap(&mut p, json!({"op": "set_spacing", "track": "a1", "gap": 0.5}));
    assert!(close(span(&p, "b").unwrap(), (1.5, 2.5)) && close(span(&p, "c").unwrap(), (3.0, 4.0)));
    assert!(close(span(&p, "lbl").unwrap(), (1.5, 2.0)), "anchored clips follow");
    let g = o.details["gaps"].as_array().unwrap();
    assert_eq!(g.len(), 2);
    assert_eq!((g[0]["from"].as_str(), g[0]["to"].as_str()), (Some("a"), Some("b")));
    assert!((g[0]["before"].as_f64().unwrap() - 1.4).abs() < 1e-9 && (g[0]["after"].as_f64().unwrap() - 0.5).abs() < 1e-9, "{g:?}");
    assert!(o.summary.contains("1.4→0.5") || o.summary.contains("1.4 → 0.5") || o.summary.contains("1.4→0.5 s"), "{}", o.summary);
    // scale + clamps
    ap(&mut p, json!({"op": "set_spacing", "clips": ["a", "b", "c"], "scale": 2.0, "max_gap": 0.8}));
    assert!(close(span(&p, "b").unwrap(), (1.8, 2.8)), "{:?}", span(&p, "b"));
    // close_gaps only closes gaps up to max_gap
    ap(&mut p, json!({"op": "set_spacing", "track": "a1", "gap": 0.3}));
    ap(&mut p, json!({"op": "move_clip", "clip": "c", "start": 5.0}));
    let o = ap(&mut p, json!({"op": "close_gaps", "track": "a1", "max_gap": 0.5}));
    assert!(close(span(&p, "b").unwrap(), (1.0, 2.0)), "{:?}", span(&p, "b"));
    // Closing ripples: c keeps its 2.7 s gap after b, so it moves up by the 0.3 s that closed.
    assert!(close(span(&p, "c").unwrap(), (4.7, 5.7)), "the 2.7 s gap stays: {:?}", span(&p, "c"));
    assert!(o.details["gaps"].is_array());
    ap(&mut p, json!({"op": "close_gaps", "track": "a1"}));
    assert!(close(span(&p, "c").unwrap(), (2.0, 3.0)));
    assert!(try_ap(&mut p, json!({"op": "set_spacing", "track": "a1"})).is_err(), "nothing to do is an error");
}

// ------------------------------------------------------------------------------- 5. batch post-check

#[test]
fn batch_checks_the_final_state_and_names_the_responsible_op() {
    let mut p = labelled();
    // An intermediate overlap (shotA longer than the gap to shot10) that a later op repairs is fine.
    let ops = vec![
        op(json!({"op": "set_transcript", "asset": "m1", "transcript": transcript(&words_35())})),
        op(json!({"op": "set_clip", "clip": "shotA", "patch": {"duration": 7.5}})),
        op(json!({"op": "set_clip", "clip": "shotA", "patch": {"duration": 7.0}})),
    ];
    let mut q = p.clone();
    apply_batch(&mut q, &ops).unwrap();
    assert!(close(span(&q, "shotA").unwrap(), (0.0, 7.0)));
    // A problem that remains names the op that introduced it.
    let ops = vec![
        op(json!({"op": "set_transcript", "asset": "m1", "transcript": transcript(&words_35())})),
        op(json!({"op": "move_clip", "clip": "shot10", "start": 5.0})),
        op(json!({"op": "set_clip", "clip": "lbl", "patch": {"name": "label"}})),
    ];
    let before = p.clone();
    let e = apply_batch(&mut p, &ops).unwrap_err();
    assert_eq!(e.index, Some(1), "{e}");
    assert!(e.to_string().contains("move_clip") && e.to_string().contains("overlap"), "{e}");
    assert_eq!(p, before, "atomic");
}

// ------------------------------------------------------------------------------- 6. captions

fn voiced() -> Project {
    let mut p = anchors();
    ap(&mut p, json!({"op": "set_transcript", "asset": "m1", "transcript": transcript(WORDS_34)}));
    ap(&mut p, json!({"op": "append_clip", "track": "a1", "clip": {"id": "vo", "asset": "m1"}}));
    p
}

fn captions(p: &Project) -> Vec<&Clip> {
    let r = resolve(p);
    let mut v: Vec<&Clip> = p.clips.iter().filter(|c| c.track == "v2").collect();
    v.sort_by(|a, b| r.span(&a.id).unwrap().start.total_cmp(&r.span(&b.id).unwrap().start));
    v
}

fn text_of(c: &Clip) -> &TextSource {
    match &c.source {
        Source::Text(t) => t,
        _ => panic!("{} is not text", c.id),
    }
}

#[test]
fn regenerating_captions_keeps_human_work() {
    // c25: a person deletes one caption and recolours another; regenerating must keep both decisions.
    let mut p = voiced();
    let gen = json!({"op": "generate_captions", "clips": ["vo"], "track": "v2"});
    ap(&mut p, gen.clone());
    let caps: Vec<(Id, String)> = captions(&p).iter().map(|c| (c.id.clone(), text_of(c).text.clone())).collect();
    assert!(caps.len() >= 4, "{caps:?}");
    let (gone, recoloured) = (caps[1].clone(), caps[2].clone());
    apply_as(&mut p, &op(json!({"op": "delete_clip", "clip": gone.0})), "user").unwrap();
    apply_as(&mut p, &op(json!({"op": "set_clip", "clip": recoloured.0, "patch": {"source": {"color": "#FF0000"}}})), "user").unwrap();
    assert!(clip(&p, &recoloured.0).tags.iter().any(|t| t == "edited"), "a person's edit is remembered");

    let o = apply_as(&mut p, &op(gen.clone()), "ai").unwrap();
    let now = captions(&p);
    assert!(!now.iter().any(|c| text_of(c).text == gone.1), "the deleted caption is not recreated");
    let kept = now.iter().find(|c| c.id == recoloured.0).expect("the edited caption is kept");
    assert_eq!(text_of(kept).color.to_hex(), "#FF0000");
    assert_eq!(now.len(), caps.len() - 1);
    assert_eq!(o.details["kept"], json!([recoloured.0]));
    assert!(!o.details["skipped"].as_array().unwrap().is_empty(), "{}", o.details);
    assert_eq!(o.details["replaced"].as_array().unwrap().len(), caps.len() - 2);
    assert!(o.summary.contains("kept 1"), "{}", o.summary);
    assert!(!codes(&p).iter().any(|c| c.contains("overlap") || c == "highlight_mismatch"), "{:?}", codes(&p));

    // replace: true overrides both.
    let o = apply_as(&mut p, &op(json!({"op": "generate_captions", "clips": ["vo"], "track": "v2", "replace": true})), "ai").unwrap();
    let now = captions(&p);
    assert_eq!(now.len(), caps.len());
    assert!(now.iter().any(|c| text_of(c).text == gone.1));
    assert!(now.iter().all(|c| text_of(c).color.to_hex() != "#FF0000"));
    assert!(o.details["kept"].as_array().unwrap().is_empty());
}

#[test]
fn regenerated_captions_inherit_the_tracks_look() {
    let mut p = voiced();
    let gen = json!({"op": "generate_captions", "clips": ["vo"], "track": "v2"});
    ap(&mut p, gen.clone());
    // An AI restyles every caption (not a human edit, so they are still regenerated).
    let ids: Vec<Id> = captions(&p).iter().map(|c| c.id.clone()).collect();
    for id in &ids {
        ap(&mut p, json!({"op": "set_clip", "clip": id, "patch": {"source": {"size": 50, "font": "Inter"}}}));
    }
    ap(&mut p, gen);
    let now = captions(&p);
    assert!(now.iter().all(|c| !ids.contains(&c.id) || c.tags.iter().any(|t| t == "edited")), "all regenerated");
    assert!(now.iter().all(|c| text_of(c).size == 50.0 && text_of(c).font == "Inter"), "the look is inherited");
}

#[test]
fn multi_part_words_join_with_a_no_break_space() {
    let mut p = voiced();
    ap(&mut p, json!({"op": "set_transcript", "asset": "m1", "transcript": transcript(&{
        let mut w = WORDS_34.to_vec();
        w[11].0 = "three D";
        w
    })}));
    ap(&mut p, json!({"op": "generate_captions", "clips": ["vo"], "track": "v2", "max_words": 4}));
    let c = captions(&p).into_iter().find(|c| text_of(c).text.contains("three\u{a0}D")).expect("NBSP-joined word");
    let t = text_of(c);
    let n = t.text.split([' ', '\t', '\n']).filter(|w| !w.is_empty()).count();
    assert_eq!(t.highlight.as_ref().unwrap().words.len(), n, "one window per caption word: {:?}", t.text);
    assert!(!codes(&p).iter().any(|c| c == "highlight_mismatch"));
}

#[test]
fn captions_on_frames_highlight_when_words_are_heard() {
    let mut p = voiced();
    ap(&mut p, json!({"op": "generate_captions", "clips": ["vo"], "track": "v2"}));
    let r = resolve(&p);
    let words = &p.transcripts["m1"].words;
    let vo = clip(&p, "vo");
    let vo_start = r.span("vo").unwrap().start;
    for c in captions(&p) {
        let s = r.span(&c.id).unwrap();
        assert!(on_frame(s.start, 30.0, 1.0) && on_frame(s.end, 30.0, 1.0), "{}: {s:?}", c.id);
        let w = word_anchor(c);
        let heard = vo_start + local_time_of_source(vo, words[w.index].start).unwrap();
        let win = text_of(c).highlight.as_ref().unwrap().words[0];
        assert!((s.start + win[0] - heard).abs() < 1e-6, "{}: highlighted at {}, heard at {heard}", c.id, s.start + win[0]);
    }
}

// ------------------------------------------------------------------------------- 9. messages

#[test]
fn overlap_messages_show_distinguishing_precision() {
    let mut p = base();
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "a", "track": "a1", "asset": "m1", "start": 0.0, "duration": 2.0}}));
    let e = try_ap(&mut p, json!({"op": "add_clip", "clip": {"id": "b", "track": "a1", "asset": "m1", "start": 1.9996, "duration": 1.0}})).unwrap_err().to_string();
    assert!(e.contains("1.9996"), "{e}");
    assert!(!e.contains("2.00–2.00") && !e.contains("2.00-2.00"), "{e}");
}

// ------------------------------------------------------------------------------- 7. schema docs

#[test]
fn schema_docs_cover_the_requested_types() {
    let d = schema_docs();
    for t in ["clip", "source", "text", "shape", "transform", "crop", "effect", "transition", "marker", "ducking", "word", "export", "settings", "track", "media", "transcript"] {
        assert!(d["types"][t].is_object(), "{t}");
    }
    for f in ["time", "until", "param", "ease", "color"] {
        assert!(d["forms"][f].is_object(), "{f}");
    }
    let u = &d["types"]["clip"]["fields"]["until"];
    assert_eq!(u["required"], json!(false));
    assert!(u["meaning"].as_str().unwrap().contains("after"));
    assert_eq!(d["types"]["text"]["fields"]["size"]["units"], json!("px"));
}

#[test]
fn document_passes_its_author_to_the_ops() {
    let mut p = voiced();
    ap(&mut p, json!({"op": "generate_captions", "clips": ["vo"], "track": "v2"}));
    let first = captions(&p)[0].id.clone();
    let mut d = Document::new(p);
    d.apply("user", "recolour", vec![op(json!({"op": "set_clip", "clip": first, "patch": {"source": {"color": "#00FF00"}}}))]).unwrap();
    assert!(clip(&d.project, &first).tags.iter().any(|t| t == "edited"));
    // An AI edit of another caption is not a human edit.
    let second = captions(&d.project)[1].id.clone();
    d.apply("ai", "recolour", vec![op(json!({"op": "set_clip", "clip": second, "patch": {"source": {"color": "#0000FF"}}}))]).unwrap();
    assert!(!clip(&d.project, &second).tags.iter().any(|t| t == "edited"));
    // The plan shows what the AI's regeneration would keep.
    let gen = vec![op(json!({"op": "generate_captions", "clips": ["vo"], "track": "v2"}))];
    let r = d.plan(&gen).unwrap();
    assert_eq!(r.outcomes[0].details["kept"], json!([first]));
    let r = d.plan_as("user", &gen).unwrap();
    assert_eq!(r.author, "user");
    assert!(is_human_author("user") && is_human_author("user:alice") && !is_human_author("ai") && !is_human_author("cli") && !is_human_author(""));
}

// ------------------------------------------------------------------------------- audio vs frame snapping

fn av_project(fps: f64) -> Project {
    let mut p: Project = serde_json::from_value(json!({
        "format": 1,
        "settings": {"width": 640, "height": 360, "fps": fps},
        "tracks": [{"id": "v1", "kind": "video", "role": "main"}, {"id": "a1", "kind": "audio", "role": "voice"}],
        "media": [{"id": "m1", "path": "cam.mp4", "kind": "video", "info": {"duration": 20.0, "has_video": true, "has_audio": true, "width": 640, "height": 360, "fps": 30}}]
    }))
    .unwrap();
    let words: Vec<Value> = (0..20).map(|i| json!({"text": format!("w{i}"), "start": i as f64 * 0.73 + 0.11, "end": i as f64 * 0.73 + 0.52})).collect();
    p.transcripts.insert("m1".into(), serde_json::from_value(json!({ "words": words })).unwrap());
    p
}

#[test]
fn l_cut_sound_is_cut_without_snapping_its_edges() {
    // the L-cut sound (longer than its picture, so not linked) and the
    // picture are cut separately; the sound's left piece must not be rounded up into its right piece.
    let mut p = av_project(30.0);
    p.clips = serde_json::from_value(json!([
        {"id": "c1", "track": "v1", "start": 0.0, "duration": 10.0, "source": {"type": "media", "asset": "m1", "in_point": 0.0}},
        {"id": "c2", "track": "a1", "start": {"with": "c1"}, "duration": 11.0, "source": {"type": "media", "asset": "m1", "in_point": 0.0}}
    ]))
    .unwrap();
    apply_batch(&mut p, &[op(json!({"op": "cut_source", "clip": "c1", "from": 3.12, "to": 4.88})), op(json!({"op": "cut_source", "clip": "c2", "from": 3.12, "to": 4.88}))]).unwrap();
    assert!(audio_snap_violations(&p).is_empty(), "{:?}", audio_snap_violations(&p));
    let r = resolve(&p);
    let mut a: Vec<Span> = p.clips.iter().filter(|c| c.track == "a1").map(|c| r.span(&c.id).unwrap()).collect();
    a.sort_by(|x, y| x.start.total_cmp(&y.start));
    assert_eq!(a.len(), 2);
    assert!(close((a[0].start, a[0].end), (0.0, 3.12)) && (a[1].start - 3.12).abs() < 1e-9, "the sound is cut exactly where asked: {a:?}");
    assert!((a[1].end - a[1].start - (11.0 - 4.88)).abs() < 1e-9);
    // The picture is cut on frames (3.1333 and 4.8667), so its pieces start and end on frames.
    let raw = resolve_raw(&p);
    for c in p.clips.iter().filter(|c| c.track == "v1") {
        let (s, w) = (r.span(&c.id).unwrap(), raw.span(&c.id).unwrap());
        assert!(on_frame(w.start, 30.0, 1.0) && on_frame(w.end, 30.0, 1.0), "{}: {w:?}", c.id);
        assert!(close((s.start, s.end), (w.start, w.end)));
    }
    assert!(!codes(&p).iter().any(|c| c.contains("overlap")));
}

#[test]
fn linked_sound_keeps_its_length_and_stays_on_its_picture() {
    for fps in [30.0, 29.97, 25.0, 23.976] {
        // A picture placed off the frame grid, its detached (linked) sound, and cuts at sub-frame times.
        let mut p = av_project(fps);
        ap(&mut p, json!({"op": "add_clip", "clip": {"id": "v", "track": "v1", "asset": "m1", "start": 0.013, "duration": 12.0}}));
        ap(&mut p, json!({"op": "detach_audio", "clip": "v", "track": "a1", "new_id": "a"}));
        ap(&mut p, json!({"op": "cut_source", "clip": "a", "from": 2.01, "to": 2.77}));
        ap(&mut p, json!({"op": "delete_words", "clip": "a", "from": 7, "to": 8}));
        ap(&mut p, json!({"op": "split_clip", "clip": "a", "at": 1.234}));
        let bad = audio_snap_violations(&p);
        assert!(bad.is_empty(), "{fps}: {bad:?}");
        // Every sound piece moved by the picture's start rounding, so it plays in sync with its picture.
        let (shown, raw) = (resolve(&p), resolve_raw(&p));
        let d = shown.span("v").unwrap().start - raw.span("v").unwrap().start;
        for c in p.clips.iter().filter(|c| c.track == "a1") {
            assert!((shown.span(&c.id).unwrap().start - raw.span(&c.id).unwrap().start - d).abs() < 1e-9, "{fps}: {}", c.id);
        }
        // Picture and sound are cut together and end together (within the picture's rounding).
        let end = |t: &str| p.clips.iter().filter(|c| c.track == t).map(|c| shown.span(&c.id).unwrap().end).fold(0.0, f64::max);
        let (num, den) = frame_rate(fps);
        assert!((end("v1") - end("a1")).abs() <= 0.5 * den as f64 / num as f64 + 1e-9, "{fps}: {} vs {}", end("v1"), end("a1"));
        assert!(!codes(&p).iter().any(|c| c.contains("overlap")), "{fps}: {:?}", codes(&p));
    }
}

#[test]
fn sound_pieces_on_differently_rounded_pictures_stay_adjacent() {
    // Two absolutely placed pictures whose starts round differently, each with linked sound exactly
    // adjacent on a1: one group, one shift, no gap opened between the sound pieces.
    let mut p = av_project(30.0);
    p.clips = serde_json::from_value(json!([
        {"id": "p1", "track": "v1", "start": 0.0, "duration": 3.12, "source": {"type": "media", "asset": "m1", "in_point": 0.0}},
        {"id": "p2", "track": "v1", "start": 3.12, "duration": 4.0, "source": {"type": "media", "asset": "m1", "in_point": 5.0}},
        {"id": "s1", "track": "a1", "start": {"with": "p1"}, "duration": 3.12, "source": {"type": "media", "asset": "m1", "in_point": 0.0}},
        {"id": "s2", "track": "a1", "start": {"with": "p2"}, "duration": 4.0, "source": {"type": "media", "asset": "m1", "in_point": 5.0}},
        {"id": "s3", "track": "a1", "start": {"after": "s2", "offset": 0.004}, "duration": 1.0, "source": {"type": "media", "asset": "m1", "in_point": 12.0}}
    ]))
    .unwrap();
    assert!(audio_snap_violations(&p).is_empty(), "{:?}", audio_snap_violations(&p));
    let r = resolve(&p);
    assert_eq!(r.span("s1").unwrap().end, r.span("s2").unwrap().start);
    assert!((r.span("s3").unwrap().start - r.span("s2").unwrap().end - 0.004).abs() < 1e-9, "a sub-frame gap is kept exactly");
    assert!(!codes(&p).iter().any(|c| c.contains("overlap")), "{:?}", codes(&p));
}

// ------------------------------------------------------------------------------- dogfood round 2

/// dogfood2/t3/roll.json: L ends `until` word 27 (−0.05), R starts at the same word and ends with vo.
fn roll_project() -> Project {
    let mut p = voiced();
    apply_batch(
        &mut p,
        &[
            op(json!({"op": "add_clip", "clip": {"id": "L", "track": "v1", "asset": "m2", "start": 0, "until": {"word": {"clip": "vo", "index": 27}, "offset": -0.05}}})),
            op(json!({"op": "add_clip", "clip": {"id": "R", "track": "v1", "asset": "m3", "start": {"word": {"clip": "vo", "index": 27}, "offset": -0.05}, "until": {"after": "vo"}}})),
        ],
    )
    .unwrap();
    assert!(close(span(&p, "L").unwrap(), (0.0, 7.3)) && close(span(&p, "R").unwrap(), (7.3, 10.166667)), "{:?} {:?}", span(&p, "L"), span(&p, "R"));
    p
}

fn gaps_or_overlaps(p: &Project) -> Vec<String> {
    validate(p, &resolve(p)).into_iter().filter(|f| f.code.contains("gap") || f.code.contains("overlap")).map(|f| format!("{}: {}", f.code, f.message)).collect()
}

#[test]
fn roll_and_slide_move_until_ends_with_the_cut() {
    let p = roll_project();
    // Roll: the cut moves, L's end anchor follows it, no gap opens.
    let mut q = p.clone();
    let o = ap(&mut q, json!({"op": "roll_edit", "left": "L", "right": "R", "delta": 0.5}));
    assert!(close(span(&q, "L").unwrap(), (0.0, 7.8)) && close(span(&q, "R").unwrap(), (7.8, 10.166667)), "{:?} {:?}", span(&q, "L"), span(&q, "R"));
    assert!(gaps_or_overlaps(&q).is_empty(), "{:?}", gaps_or_overlaps(&q));
    assert_eq!(o.summary, "Rolled the cut L|R by +0.500 s (15 frames) to 7.80 s");
    // Whole frames: the raw cut 7.29 (shown 7.30) moves by exactly 15 frames (shown 7.80).
    assert!(matches!(&clip(&q, "L").until, Some(Time::Anchored(Anchor { to: AnchorTarget::Word(_), offset })) if (offset - 0.45).abs() < 1e-9), "{:?}", clip(&q, "L").until);
    // Slide: R keeps its length and L absorbs the move.
    let mut q = p.clone();
    let o = ap(&mut q, json!({"op": "slide_clip", "clip": "R", "delta": -0.3}));
    assert!(close(span(&q, "L").unwrap(), (0.0, 7.0)) && close(span(&q, "R").unwrap(), (7.0, 9.866667)), "{:?} {:?}", span(&q, "L"), span(&q, "R"));
    assert!(gaps_or_overlaps(&q).is_empty());
    assert!(o.summary.starts_with("Slid R by -0.300 s (-9 frames); L absorbs it"), "{}", o.summary);
    // A ripple trim keeps R's length too (its end anchor follows).
    let mut q = p.clone();
    let o = ap(&mut q, json!({"op": "trim_clip", "clip": "L", "edge": "end", "delta": 1.0, "ripple": true}));
    assert!(close(span(&q, "L").unwrap(), (0.0, 8.3)) && close(span(&q, "R").unwrap(), (8.3, 11.166667)), "{:?} {:?}", span(&q, "L"), span(&q, "R"));
    assert!(o.summary.contains("by +1.000 s (30 frames)"), "{}", o.summary);
    // A magnetic trim of L's end into R is an overlap, as before.
    let mut q = p.clone();
    assert!(try_ap(&mut q, json!({"op": "trim_clip", "clip": "L", "edge": "end", "delta": 0.5})).unwrap_err().to_string().contains("overlap"));
}

#[test]
fn freeze_and_spacing_keep_lengths_of_clips_with_until() {
    let mut p = base();
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "a", "track": "v1", "asset": "mv", "start": 0.0, "duration": 4.0}}));
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "b", "track": "v1", "asset": "mv", "start": 5.0, "duration": 3.0, "until": 8.0}}));
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "c", "track": "v1", "asset": "mv", "start": 9.0, "duration": 2.0}}));
    // Freezing a frame of a pushes b later as a whole (its absolute end anchor follows).
    let mut q = p.clone();
    ap(&mut q, json!({"op": "freeze_frame", "clip": "a", "at": 2.0, "duration": 1.0}));
    assert!(close(span(&q, "b").unwrap(), (6.0, 9.0)), "{:?}", span(&q, "b"));
    assert_eq!(clip(&q, "b").until, Some(Time::At(9.0)));
    // set_spacing moves b as a whole too.
    let mut q = p.clone();
    ap(&mut q, json!({"op": "set_spacing", "track": "v1", "gap": 0.0}));
    assert!(close(span(&q, "b").unwrap(), (4.0, 7.0)) && close(span(&q, "c").unwrap(), (7.0, 9.0)), "{:?} {:?}", span(&q, "b"), span(&q, "c"));
    assert!(gaps_or_overlaps(&q).is_empty());
}

#[test]
fn receipts_report_frame_accurate_changes() {
    let mut p = base();
    // Off the frame grid (start 0.013): the shown edge moves by exactly the asked whole frames.
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "a", "track": "v1", "asset": "mv", "start": 0.013, "duration": 4.01}}));
    let o = ap(&mut p, json!({"op": "trim_clip", "clip": "a", "edge": "end", "delta": 1.0}));
    assert_eq!(o.summary, "Trimmed end of a by +1.000 s (30 frames) → [0.00–5.03]", "{}", o.summary);
    let o = ap(&mut p, json!({"op": "trim_clip", "clip": "a", "edge": "end", "delta": -0.51}));
    assert!(o.summary.contains("by -0.500 s (-15 frames; asked -0.51 s)"), "{}", o.summary);
    // Audio keeps exact seconds.
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "s", "track": "a1", "asset": "m1", "start": 0.013, "duration": 4.0}}));
    let o = ap(&mut p, json!({"op": "trim_clip", "clip": "s", "edge": "end", "delta": 0.51}));
    assert!(o.summary.contains("by +0.51 s →"), "{}", o.summary);
}

/// The first caption followed by a pause of at least `pause` s before the next caption's first word.
fn caption_before_pause(p: &Project, pause: f64) -> (Id, Id, f64, f64) {
    let r = resolve(p);
    let caps = captions(p);
    let words = &p.transcripts["m1"].words;
    for k in 0..caps.len() - 1 {
        let (a, b) = (caps[k], caps[k + 1]);
        let n = text_of(a).highlight.as_ref().unwrap().words.len();
        let last = word_anchor(a).index + n - 1;
        let (end_a, start_b) = (words[last].end, words[word_anchor(b).index].start);
        if start_b - end_a >= pause {
            let _ = r;
            return (a.id.clone(), b.id.clone(), end_a, start_b);
        }
    }
    panic!("no pause");
}

#[test]
fn captions_edited_by_hand_follow_their_words_and_are_shortened_instead_of_refusing() {
    let mut p = voiced();
    ap(&mut p, json!({"op": "generate_captions", "clips": ["vo"], "track": "v2", "max_words": 4, "pause": 5.0}));
    let (a, b, end_a, start_b) = caption_before_pause(&p, 0.2);
    // A hand-written caption: text edited, no karaoke, and shown right up to the next caption.
    let r = resolve_raw(&p);
    let till_b = r.span(&b).unwrap().start - r.span(&a).unwrap().start;
    ap(&mut p, json!({"op": "set_clip", "clip": a, "patch": {"source": {"text": "Hand written!", "highlight": null}, "duration": till_b}}));
    assert!(clip(&p, &a).tags.iter().any(|t| t == "caption"));
    // Moving the voice by a non-whole number of frames: everything follows, nothing overlaps.
    let mut q = p.clone();
    let word_a = word_anchor(clip(&q, &a)).index;
    ap(&mut q, json!({"op": "move_clip", "clip": "vo", "start": 0.517}));
    assert!(gaps_or_overlaps(&q).iter().all(|f| !f.contains("overlap")), "{:?}", gaps_or_overlaps(&q));
    assert_eq!(word_anchor(clip(&q, &a)).index, word_a, "still on its word");
    assert_eq!(text_of(clip(&q, &a)).text, "Hand written!");
    // Cutting the pause before b pulls b earlier: a is shortened to end where b starts, and says so.
    let mut q = p.clone();
    let o = ap(&mut q, json!({"op": "cut_source", "clip": "vo", "from": end_a + 0.03, "to": start_b - 0.03}));
    assert!(o.summary.contains(&format!("shortened caption(s) {a} by")) && o.summary.contains(&format!("(it ran into {b})")), "{}", o.summary);
    let rq = resolve(&q);
    assert!((rq.span(&a).unwrap().end - rq.span(&b).unwrap().start).abs() < 1e-9, "a ends where b starts");
    assert_eq!(text_of(clip(&q, &a)).text, "Hand written!");
    assert!(gaps_or_overlaps(&q).iter().all(|f| !f.contains("overlap")));
}

#[test]
fn spacing_a_captioned_voice_with_hand_edited_captions_succeeds() {
    // dogfood2/t3/spacing3.json on run1_state.aifr, reduced: two voice clips, captions on the second,
    // some made plain by hand (one ends with the voice), then the pauses are scaled.
    let mut p = voiced();
    ap(&mut p, json!({"op": "move_clip", "clip": "vo", "start": 2.05}));
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "pre", "track": "a1", "asset": "m1", "in_point": 0.0, "start": 0.0, "duration": 1.0}}));
    ap(&mut p, json!({"op": "generate_captions", "clips": ["vo"], "track": "v2", "max_words": 4, "pause": 5.0}));
    let ids: Vec<Id> = captions(&p).iter().map(|c| c.id.clone()).collect();
    let (mid, last) = (ids[ids.len() / 2].clone(), ids[ids.len() - 1].clone());
    ap(&mut p, json!({"op": "set_clip", "clip": mid, "patch": {"source": {"text": "That's not 3D.", "highlight": null}}}));
    ap(&mut p, json!({"op": "set_clip", "clip": last, "patch": {"source": {"text": "It has to be properly 3D.", "highlight": null}}}));
    ap(&mut p, json!({"op": "set_until", "clip": last, "until": {"after": "vo"}}));
    let o = ap(&mut p, json!({"op": "set_spacing", "track": "a1", "scale": 0.7, "min_gap": 0.6}));
    assert!(o.summary.contains("pre→vo 1.05→0.735 s"), "{}", o.summary);
    assert!(gaps_or_overlaps(&p).iter().all(|f| !f.contains("overlap")), "{:?}", gaps_or_overlaps(&p));
    let r = resolve_raw(&p);
    assert!((r.span(&last).unwrap().end - r.span("vo").unwrap().end).abs() < 1e-9, "still ends with the voice");
    assert_eq!(text_of(clip(&p, &mid)).text, "That's not 3D.");
    assert!(text_of(clip(&p, &mid)).highlight.is_none());
}

// ------------------------------------------------------------------------------- showdown captions

/// Whisper-style tokens: numbers and hyphenated words split ("$2" ".11", "6" ",000", "four" "-hour").
const SPLIT_TOKENS: &[(&str, f64, f64)] = &[
    ("It", 0.10, 0.25),
    ("costs", 0.25, 0.60),
    ("$2", 0.60, 0.95),
    (".11", 0.95, 1.30),
    ("for", 1.30, 1.45),
    ("6", 1.45, 1.70),
    (",000", 1.70, 2.10),
    ("towers", 2.10, 2.60),
    ("in", 2.60, 2.70),
    ("a", 2.70, 2.78),
    ("four", 2.78, 3.00),
    ("-hour", 3.00, 3.40),
    ("run.", 3.40, 3.90),
];

fn split_token_project() -> Project {
    let mut p = base();
    ap(&mut p, json!({"op": "set_transcript", "asset": "m1", "transcript": transcript(SPLIT_TOKENS)}));
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "vo", "track": "a1", "asset": "m1", "start": 0.0, "duration": 4.0}}));
    p
}

fn texts_on(p: &Project, track: &str) -> Vec<String> {
    let r = resolve(p);
    let mut v: Vec<&Clip> = p.clips.iter().filter(|c| c.track == track).collect();
    v.sort_by(|a, b| r.span(&a.id).unwrap().start.total_cmp(&r.span(&b.id).unwrap().start));
    v.iter().map(|c| text_of(c).text.clone()).collect()
}

#[test]
fn split_number_and_hyphen_tokens_read_as_one_caption_word() {
    for max_words in [2, 3, 5] {
        let mut p = split_token_project();
        ap(&mut p, json!({"op": "generate_captions", "clips": ["vo"], "track": "v2", "max_words": max_words, "pause": 5.0}));
        let all = texts_on(&p, "v2").join(" | ");
        for want in ["$2.11", "6,000", "four-hour"] {
            assert!(all.contains(want), "{max_words}: {all}");
        }
        for bad in ["$2 .11", "6 ,000", "four -hour", "| .11", "| ,000", "| -hour"] {
            assert!(!all.contains(bad), "{max_words}: {all}");
        }
        // One highlight window per caption word; "$2.11" is lit from "$2" to the end of ".11".
        let r = resolve(&p);
        for c in p.clips.iter().filter(|c| c.track == "v2") {
            let t = text_of(c);
            let n = t.text.split(' ').count();
            assert_eq!(t.highlight.as_ref().unwrap().words.len(), n, "{:?}", t.text);
            if let Some(k) = t.text.split(' ').position(|w| w == "$2.11") {
                let w = t.highlight.as_ref().unwrap().words[k];
                let s = r.span(&c.id).unwrap().start;
                assert!((s + w[0] - 0.60).abs() < 1e-6 && (s + w[1] - 1.30).abs() < 1e-6, "{w:?} from {s}");
            }
        }
        assert!(!codes(&p).iter().any(|c| c == "highlight_mismatch"), "{:?}", codes(&p));
    }
}

#[test]
fn split_tokens_stay_joined_when_captions_are_retimed() {
    let mut p = split_token_project();
    ap(&mut p, json!({"op": "generate_captions", "clips": ["vo"], "track": "v2", "max_words": 3, "pause": 5.0}));
    let before = texts_on(&p, "v2");
    // Move the voice by a non-whole number of frames, then cut a word: captions re-time from the words.
    ap(&mut p, json!({"op": "move_clip", "clip": "vo", "start": 0.517}));
    assert_eq!(texts_on(&p, "v2"), before);
    ap(&mut p, json!({"op": "delete_words", "clip": "vo", "from": 8, "to": 8, "pad": 0.0}));
    let after = texts_on(&p, "v2").join(" | ");
    assert!(after.contains("$2.11") && after.contains("6,000") && after.contains("four-hour") && !after.contains(" in "), "{after}");
    let r = resolve(&p);
    let words = &p.transcripts["m1"].words;
    // When a source time is heard (the voice is in pieces after the cut).
    let at = |s: f64| p.clips.iter().filter(|c| c.track == "a1").find_map(|c| local_time_of_source(c, s).filter(|l| *l < c.duration).map(|l| r.span(&c.id).unwrap().start + l)).unwrap();
    let (heard, heard_end) = (|i: usize| at(words[i].start), |i: usize| at(words[i].end - 1e-9) + 1e-9);
    for c in p.clips.iter().filter(|c| c.track == "v2") {
        let t = text_of(c);
        if let Some(k) = t.text.split(' ').position(|w| w == "four-hour") {
            let w = t.highlight.as_ref().unwrap().words[k];
            let s = r.span(&c.id).unwrap().start;
            assert!((s + w[0] - heard(10)).abs() < 1e-6 && (s + w[1] - heard_end(11)).abs() < 1e-6, "{w:?}");
        }
    }
    assert!(!codes(&p).iter().any(|c| c == "highlight_mismatch" || c.contains("overlap")), "{:?}", codes(&p));
    // Summaries read the same way.
    let o = ap(&mut p, json!({"op": "delete_words", "clip": "vo", "from": 2, "to": 3, "pad": 0.0}));
    assert!(o.summary.contains("\"$2.11\""), "{}", o.summary);
}

#[test]
fn captions_belong_to_their_voice_clip_not_to_its_media() {
    // One voice file played by two clips (the same "Victory." line in two chapters).
    let mut p = split_token_project();
    ap(&mut p, json!({"op": "add_clip", "clip": {"id": "vo2", "track": "a1", "asset": "m1", "start": 10.0, "duration": 4.0}}));
    let gen = |p: &mut Project, clip: &str| apply_as(p, &op(json!({"op": "generate_captions", "clips": [clip], "track": "v2", "max_words": 3, "pause": 5.0})), "ai").unwrap();
    let anchored = |p: &Project, clip: &str| -> Vec<Id> { p.clips.iter().filter(|c| c.track == "v2" && word_anchor(c).clip == clip).map(|c| c.id.clone()).collect() };
    gen(&mut p, "vo");
    let first = anchored(&p, "vo");
    assert!(!first.is_empty());
    // Captioning the second clip leaves the first clip's captions alone.
    let o = gen(&mut p, "vo2");
    assert_eq!(anchored(&p, "vo"), first, "vo's captions survive");
    assert!(o.details["replaced"].as_array().unwrap().is_empty(), "{}", o.details);
    let second = anchored(&p, "vo2");
    assert_eq!(second.len(), first.len());
    // Regenerating the second replaces only its own.
    let o = gen(&mut p, "vo2");
    assert_eq!(anchored(&p, "vo"), first);
    let replaced: Vec<Id> = serde_json::from_value(o.details["replaced"].clone()).unwrap();
    assert_eq!(replaced, second);
    // A person deletes one of vo's captions: vo2 still gets all of its captions, vo does not get it back.
    apply_as(&mut p, &op(json!({"op": "delete_clip", "clip": first[0]})), "user").unwrap();
    assert!(p.caption_exclusions.contains_key("vo") && !p.caption_exclusions.contains_key("m1"), "{:?}", p.caption_exclusions);
    let n2 = anchored(&p, "vo2").len();
    let o = gen(&mut p, "vo2");
    assert_eq!(anchored(&p, "vo2").len(), n2);
    assert!(o.details["skipped"].as_array().unwrap().is_empty(), "{}", o.details);
    let o = gen(&mut p, "vo");
    assert_eq!(anchored(&p, "vo").len(), first.len() - 1);
    assert!(!o.details["skipped"].as_array().unwrap().is_empty(), "{}", o.details);
    // Captioning both at once handles each clip's own captions.
    let o = apply_as(&mut p, &op(json!({"op": "generate_captions", "clips": ["vo", "vo2"], "track": "v2", "max_words": 3, "pause": 5.0, "replace": true})), "ai").unwrap();
    assert_eq!(anchored(&p, "vo").len(), first.len());
    assert_eq!(anchored(&p, "vo2").len(), first.len());
    assert!(!p.caption_exclusions.contains_key("vo"));
    assert!(o.summary.contains("replaced"), "{}", o.summary);
}

#[test]
fn media_keyed_exclusions_from_older_projects_still_apply() {
    let mut p = split_token_project();
    p.caption_exclusions.insert("m1".into(), vec![[0.6, 1.3]]);
    let o = apply_as(&mut p, &op(json!({"op": "generate_captions", "clips": ["vo"], "track": "v2", "max_words": 3, "pause": 5.0})), "ai").unwrap();
    assert!(!texts_on(&p, "v2").join(" ").contains("$2.11"));
    assert_eq!(o.details["skipped"], json!(["$2.11"]));
}
