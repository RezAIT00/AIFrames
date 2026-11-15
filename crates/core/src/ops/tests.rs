//! Tests for every op: happy paths, main errors, ripple through anchor chains, split correctness,
//! text-based editing and captions, atomicity and JSON.

use super::*;
use crate::eval::sample;
use crate::resolve::{resolve, Span};
use crate::testutil::*;
use serde_json::json;

fn word_t(clip: &str, index: usize, end: bool, offset: f64, text: &str) -> Time {
    Time::Anchored(Anchor { to: AnchorTarget::Word(WordRef { clip: clip.into(), index, end, text: Some(text.into()) }), offset })
}

fn op(v: Value) -> Op {
    Op::from_json(&v).unwrap_or_else(|e| panic!("{e}"))
}

fn run(p: &mut Project, v: Value) -> OpOutcome {
    apply(p, &op(v.clone())).unwrap_or_else(|e| panic!("{v} failed: {e}"))
}

/// Applies an op that must fail; checks the project is unchanged; returns the message.
fn fails(p: &mut Project, v: Value) -> String {
    let before = p.clone();
    let e = apply(p, &op(v.clone())).err().unwrap_or_else(|| panic!("{v} should fail"));
    assert_eq!(*p, before, "project changed by a failed op {v}");
    e.message
}

fn span(p: &Project, id: &str) -> Span {
    resolve(p).span(id).unwrap_or_else(|| panic!("{id} is unresolved: {:?}", resolve(p).errors))
}

fn s(p: &Project, id: &str) -> (f64, f64) {
    let sp = span(p, id);
    ((sp.start * 1e6).round() / 1e6, (sp.end * 1e6).round() / 1e6)
}

fn clip<'a>(p: &'a Project, id: &str) -> &'a Clip {
    p.clips.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("no clip {id}"))
}

fn inp(p: &Project, id: &str) -> f64 {
    match &clip(p, id).source {
        Source::Media { in_point, .. } => (*in_point * 1e6).round() / 1e6,
        _ => panic!("{id} is not media"),
    }
}

/// v1: c1 [0–4] abs (mv@0), c2 after c1 [4–8] (mv@10), c3 after c2 [8–12] (mv@20); v2: t1 with c2 [4–6].
fn chain() -> Project {
    let mut p = project_with_voice();
    p.clips.push(media_clip("c1", "v1", "mv", Time::At(0.0), 4.0, 0.0));
    p.clips.push(media_clip("c2", "v1", "mv", after("c1", 0.0), 4.0, 10.0));
    p.clips.push(media_clip("c3", "v1", "mv", after("c2", 0.0), 4.0, 20.0));
    p.clips.push(text_clip("t1", "v2", with("c2", 0.0), 2.0));
    p
}

// ------------------------------------------------------------------------------------------ JSON

#[test]
fn every_op_has_a_spec_and_a_parseable_example() {
    assert_eq!(OPS.len(), 58);
    let mut names = HashSet::new();
    for spec in OPS {
        assert!(names.insert(spec.name), "duplicate {}", spec.name);
        let v: Value = serde_json::from_str(spec.example).unwrap_or_else(|e| panic!("{}: {e}", spec.name));
        let o = Op::from_json(&v).unwrap_or_else(|e| panic!("{}: {e}", spec.name));
        assert_eq!(o.name(), spec.name);
        // Round trip.
        let back = serde_json::to_value(&o).unwrap();
        assert_eq!(Op::from_json(&back).unwrap(), o, "{}", spec.name);
        assert_eq!(back["op"], spec.name);
        assert!(op_spec(spec.name).is_some());
    }
}

#[test]
fn op_json_is_strict_and_helpful() {
    let e = Op::from_json(&json!({"op":"move_clip","clip":"c3","strat":12})).unwrap_err();
    assert!(e.message.contains("unknown field `strat`"), "{}", e.message);
    assert!(e.message.contains("Example"), "{}", e.message);
    let e = Op::from_json(&json!({"op":"move_clp","clip":"c3"})).unwrap_err();
    assert!(e.message.contains("Did you mean move_clip?"), "{}", e.message);
    let e = Op::from_json(&json!({"clip":"c3"})).unwrap_err();
    assert!(e.message.contains("\"op\" field"), "{}", e.message);
    // Optional/default fields are omitted when serialising.
    let o = op(json!({"op":"delete_clip","clip":"c1"}));
    assert_eq!(serde_json::to_value(&o).unwrap(), json!({"op":"delete_clip","clip":"c1"}));
    let o = op(json!({"op":"set_settings","duration":null}));
    assert_eq!(o, Op::SetSettings { width: None, height: None, fps: None, sample_rate: None, background: None, duration: Some(None), title_safe: None, action_safe: None });
    assert_eq!(serde_json::to_value(&o).unwrap(), json!({"op":"set_settings","duration":null}));
    let o = op(json!({"op":"set_settings"}));
    assert_eq!(serde_json::to_value(&o).unwrap(), json!({"op":"set_settings"}));
}

#[test]
fn model_json_is_compact() {
    let mut c = text_clip("c1", "v1", Time::At(1.5), 2.0);
    c.opacity = Param::Value(0.5);
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v["opacity"], json!(0.5), "constant params are bare numbers");
    assert_eq!(v["start"], json!(1.5));
    for k in ["speed", "volume", "transform", "effects", "enabled", "tags", "notes", "reverse", "muted", "blend", "fade_in"] {
        assert!(v.get(k).is_none(), "{k} should be omitted: {v}");
    }
    c.start = after("c0", 0.4);
    c.transform.scale = keys(&[(0.0, 1.0), (1.0, 2.0)]);
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v["start"], json!({"after":"c0","offset":0.4}));
    assert_eq!(v["transform"], json!({"scale":{"keys":[{"t":0.0,"v":1.0},{"t":1.0,"v":2.0}]}}));
    let back: Clip = serde_json::from_value(v).unwrap();
    assert_eq!(back, c);
    let w = word("c3", 17, false, -0.1);
    assert_eq!(serde_json::to_value(&w).unwrap(), json!({"word":{"clip":"c3","index":17},"offset":-0.1}));
    // A whole project round-trips and omits defaults.
    let p = chain();
    let text = crate::document::canonical_json(&p);
    let back: Project = serde_json::from_str(&text).unwrap();
    assert_eq!(back, p);
    let v: Value = serde_json::from_str(&text).unwrap();
    assert!(v.get("export").is_none() && v.get("markers").is_none() && v.get("notes").is_none());
    let empty = serde_json::to_value(Project::default()).unwrap();
    assert_eq!(empty.as_object().unwrap().keys().cloned().collect::<Vec<_>>(), vec!["format", "name", "settings"]);
}

// ------------------------------------------------------------------------------------------ ids

#[test]
fn ids_are_short_unique_and_never_reused() {
    let mut p = chain();
    assert_eq!(new_id(&p, "c"), "c4");
    assert_eq!(new_id(&p, "mk"), "mk1");
    let o = run(&mut p, json!({"op":"append_clip","track":"v1","clip":{"asset":"mv","duration":1}}));
    assert_eq!(o.created, vec!["c4"]);
    run(&mut p, json!({"op":"delete_clip","clip":"c4"}));
    let o = run(&mut p, json!({"op":"append_clip","track":"v1","clip":{"asset":"mv","duration":1}}));
    assert_eq!(o.created, vec!["c5"], "the id of a deleted clip is never handed out again");
    assert_eq!(p.id_counters.get("c"), Some(&5), "the high-water mark is stored in the project");
    let saved: Project = serde_json::from_str(&crate::document::canonical_json(&p)).unwrap();
    assert_eq!(new_id(&saved, "c"), "c6", "and survives save/open");
    run(&mut p, json!({"op":"add_marker","time":1.0,"id":"c10"}));
    let o = run(&mut p, json!({"op":"append_clip","track":"v1","clip":{"asset":"mv","duration":1}}));
    assert_eq!(o.created, vec!["c11"], "unique across id spaces (a marker named c10 exists)");
    let m = fails(&mut p, json!({"op":"add_track","kind":"video","id":"c1"}));
    assert!(m.contains("already used by a clip"), "{m}");
    let m = fails(&mut p, json!({"op":"add_marker","time":1.0,"id":"bad id"}));
    assert!(m.contains("invalid id"), "{m}");
    let o = run(&mut p, json!({"op":"add_track","kind":"audio"}));
    assert_eq!(o.created, vec!["a3"]);
    let o = run(&mut p, json!({"op":"add_track","kind":"video","id":"titles"}));
    assert_eq!(o.created, vec!["titles"]);
}

// ------------------------------------------------------------------------------------------ project, media, tracks

#[test]
fn project_settings_and_export() {
    let mut p = chain();
    run(&mut p, json!({"op":"set_settings","width":1080,"height":1920,"fps":60,"duration":30}));
    assert_eq!((p.settings.width, p.settings.height, p.settings.fps, p.settings.duration), (1080, 1920, 60.0, Some(30.0)));
    run(&mut p, json!({"op":"set_settings","duration":null}));
    assert_eq!(p.settings.duration, None);
    assert!(fails(&mut p, json!({"op":"set_settings","fps":0})).contains("fps"));
    assert!(fails(&mut p, json!({"op":"set_settings","title_safe":1.5})).contains("fraction"));
    let o = run(&mut p, json!({"op":"set_settings","width":1081}));
    assert!(o.warnings[0].contains("odd"));
    run(&mut p, json!({"op":"set_project","name":"New","notes":"brief"}));
    assert_eq!((p.name.as_str(), p.notes.as_str()), ("New", "brief"));
    run(&mut p, json!({"op":"set_export","codec":"hevc","loudness_lufs":null,"range":[1,5]}));
    assert_eq!((p.export.codec.as_str(), p.export.loudness_lufs, p.export.range), ("hevc", None, Some([1.0, 5.0])));
    assert!(fails(&mut p, json!({"op":"set_export","codec":"h265"})).contains("Did you mean"));
}

#[test]
fn media_ops() {
    let mut p = chain();
    let o = run(&mut p, json!({"op":"add_media","path":"C:\\clips\\b.mp4","kind":"video","info":{"duration":10.0,"has_video":true}}));
    assert_eq!(o.created, vec!["m2"]);
    assert_eq!(p.media.last().unwrap().path, "C:/clips/b.mp4", "forward slashes");
    let o = run(&mut p, json!({"op":"add_media","path":"C:/clips/b.mp4","kind":"video"}));
    assert!(o.warnings[0].contains("already uses this file"));
    let m = fails(&mut p, json!({"op":"remove_media","asset":"mv"}));
    assert!(m.contains("used by 3 clip(s): c1, c2, c3") && m.contains("force"), "{m}");
    let o = run(&mut p, json!({"op":"remove_media","asset":"mv","force":true}));
    assert_eq!(o.removed.iter().filter(|i| i.starts_with('c')).count(), 3);
    assert!(!p.transcripts.contains_key("mv"));
    assert!(span(&p, "t1").start == 4.0, "t1 keeps its time as an absolute time");
    run(&mut p, json!({"op":"relink_media","asset":"m1","path":"D:/new/voice.wav","info":{"duration":5.0,"has_audio":true}}));
    assert_eq!(p.media[0].path, "D:/new/voice.wav");
    run(&mut p, json!({"op":"set_media","asset":"m1","notes":"VO take 2","tags":["vo"]}));
    assert_eq!(p.media[0].tags, vec!["vo"]);
    let m = fails(&mut p, json!({"op":"set_media","asset":"m9"}));
    assert!(m.contains("media m9 not found") && m.contains("m1 (voice.wav)"), "{m}");
}

#[test]
fn track_ops() {
    let mut p = chain();
    run(&mut p, json!({"op":"add_track","kind":"video","role":"captions","index":2}));
    assert_eq!(p.tracks[2].id, "v3");
    run(&mut p, json!({"op":"move_track","track":"v3","index":0}));
    assert_eq!(p.tracks[0].id, "v3");
    assert!(fails(&mut p, json!({"op":"move_track","track":"v3","index":9})).contains("out of range"));
    let m = fails(&mut p, json!({"op":"remove_track","track":"v1"}));
    assert!(m.contains("3 clip(s): c1, c2, c3"), "{m}");
    run(&mut p, json!({"op":"set_track","track":"v2","volume":0.5,"muted":true,"name":"Titles"}));
    assert_eq!((p.tracks[2].volume, p.tracks[2].muted, p.tracks[2].name.as_str()), (0.5, true, "Titles"));
    assert!(fails(&mut p, json!({"op":"set_track","track":"v2","volume":-1})).contains("volume"));
    let m = fails(&mut p, json!({"op":"set_track","track":"v7"}));
    assert!(m.contains("track v7 not found") && m.contains("v1 (video, main)"), "{m}");
    run(&mut p, json!({"op":"add_ducking","target":"a2","trigger":"a1"}));
    assert!(fails(&mut p, json!({"op":"remove_track","track":"a2"})).contains("ducking d1"));
    run(&mut p, json!({"op":"remove_track","track":"a2","force":true}));
    assert!(p.ducking.is_empty());
    run(&mut p, json!({"op":"remove_track","track":"v1","force":true}));
    assert!(p.clips.iter().all(|c| c.track != "v1"));
}

#[test]
fn locked_tracks_refuse_direct_and_indirect_changes() {
    let mut p = chain();
    run(&mut p, json!({"op":"set_track","track":"v1","locked":true}));
    let m = fails(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"end","delta":-1}));
    assert!(m.contains("v1 is locked") && m.contains("\"locked\":false"), "{m}");
    run(&mut p, json!({"op":"set_track","track":"v1","locked":false}));
    run(&mut p, json!({"op":"set_track","track":"v2","locked":true}));
    // t1 (on locked v2) is anchored with c2, so trimming c1 would move it.
    let m = fails(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"end","delta":-1}));
    assert!(m.contains("would move t1") && m.contains("with c2"), "{m}");
    // Without magnetism reaching t1 it works.
    run(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"end","delta":-1,"ripple":false}));
    assert_eq!(s(&p, "t1"), (4.0, 6.0));
}

// ------------------------------------------------------------------------------------------ adding clips

#[test]
fn add_clip_defaults_shorthands_and_errors() {
    let mut p = chain();
    let o = run(&mut p, json!({"op":"add_clip","clip":{"track":"v1","asset":"mv","in_point":100}}));
    let id = &o.created[0];
    assert_eq!(s(&p, id), (12.0, 32.0), "starts at the end of the track, lasts the rest of the media");
    let o = run(&mut p, json!({"op":"add_clip","clip":{"track":"v2","text":"Hello","start":{"word":{"clip":"c2","index":12}}}}));
    let t = &o.created[0];
    assert_eq!(clip(&p, t).duration, 3.0, "text defaults to 3 s");
    assert_eq!(s(&p, t).0, 7.0, "word 12 (source 13.0) is 3 s into c2 (in 10) at 4.0");
    let m = fails(&mut p, json!({"op":"add_clip","clip":{"track":"v1","asset":"mv","durration":2}}));
    assert!(m.contains("unknown clip field `durration`") && m.contains("Did you mean duration?"), "{m}");
    let m = fails(&mut p, json!({"op":"add_clip","clip":{"track":"v1","duration":2}}));
    assert!(m.contains("needs a source"), "{m}");
    let m = fails(&mut p, json!({"op":"add_clip","clip":{"track":"v1","asset":"mv","start":2,"duration":1}}));
    assert!(m.contains("would overlap") && m.contains("on track v1") && m.contains("overwrite"), "{m}");
    let m = fails(&mut p, json!({"op":"add_clip","clip":{"track":"a1","text":"x","start":50}}));
    assert!(m.contains("cannot go on audio track a1") && m.contains("v1, v2"), "{m}");
    let m = fails(&mut p, json!({"op":"add_clip","clip":{"track":"v2","asset":"m1","start":50}}));
    assert!(m.contains("audio-only media"), "{m}");
    let m = fails(&mut p, json!({"op":"add_clip","clip":{"track":"v2","asset":"mv","in_point":110,"duration":20,"start":50}}));
    assert!(m.contains("only 120 s long") && m.contains("10 s past the end"), "{m}");
    let m = fails(&mut p, json!({"op":"add_clip","clip":{"track":"v2","text":"x","start":{"after":"c99"}}}));
    assert!(m.contains("unresolved anchors") && m.contains("c99"), "{m}");
    let m = fails(&mut p, json!({"op":"add_clip","clip":{"track":"v2","text":"x","effects":[{"kind":"blur","params":{"radius":999}}]}}));
    assert!(m.contains("blur.radius = 999 is outside its range 0–200 px"), "{m}");
    // Overwrite carves the space out.
    run(&mut p, json!({"op":"add_clip","clip":{"id":"ov","track":"v1","asset":"mv","start":2,"duration":4},"overwrite":true}));
    assert_eq!(s(&p, "c1"), (0.0, 2.0));
    assert_eq!(s(&p, "ov"), (2.0, 6.0));
    assert_eq!(s(&p, "c2"), (6.0, 8.0), "c2's head was overwritten; the rest keeps its time");
    assert_eq!(inp(&p, "c2"), 12.0);
    assert_eq!(s(&p, "c3"), (8.0, 12.0));
}

#[test]
fn append_clip_is_magnetic() {
    let mut p = chain();
    let o = run(&mut p, json!({"op":"append_clip","track":"v1","clip":{"asset":"mv","duration":2}}));
    let a = o.created[0].clone();
    assert_eq!(clip(&p, &a).start, after("c3", 0.0));
    assert_eq!(s(&p, &a), (12.0, 14.0));
    // Appending after c1 slots into the chain: c2 and everything after it follow.
    let o = run(&mut p, json!({"op":"append_clip","track":"v1","after":"c1","clip":{"asset":"mv","duration":1}}));
    let b = o.created[0].clone();
    assert_eq!(s(&p, &b), (4.0, 5.0));
    assert_eq!(clip(&p, "c2").start, after(&b, 0.0));
    assert_eq!(s(&p, "c3"), (9.0, 13.0));
    assert_eq!(s(&p, &a), (13.0, 15.0));
    assert_eq!(s(&p, "t1"), (5.0, 7.0), "t1 rides along with c2");
    assert!(fails(&mut p, json!({"op":"append_clip","track":"v1","clip":{"asset":"mv","start":3}})).contains("remove `start`"));
    let mut q = project();
    run(&mut q, json!({"op":"append_clip","track":"v1","clip":{"asset":"mv","duration":2}}));
    assert_eq!(q.clips[0].start, Time::At(0.0), "first clip of an empty track starts at 0");
}

#[test]
fn insert_clip_ripple_and_overwrite() {
    let mut p = chain();
    let o = run(&mut p, json!({"op":"insert_clip","track":"v1","at":6.0,"clip":{"asset":"mv","in_point":50,"duration":2}}));
    assert_eq!(o.created, vec!["c4", "c5"], "the inserted clip first, then the right half of the split");
    assert_eq!(s(&p, "c2"), (4.0, 6.0));
    assert_eq!(s(&p, "c4"), (6.0, 8.0));
    assert_eq!(s(&p, "c5"), (8.0, 10.0), "right half of c2 pushed later");
    assert_eq!(inp(&p, "c5"), 12.0);
    assert_eq!(s(&p, "c3"), (10.0, 14.0));
    assert_eq!(s(&p, "t1"), (4.0, 6.0));
    assert!(o.summary.contains("split c2"), "{}", o.summary);
    // At a cut, nothing is split; an absolute clip later on the track moves too.
    let mut p = chain();
    p.clips.push(media_clip("far", "v1", "mv", Time::At(30.0), 1.0, 0.0));
    run(&mut p, json!({"op":"insert_clip","track":"v1","at":8.0,"clip":{"asset":"mv","duration":3}}));
    assert_eq!(s(&p, "c3"), (11.0, 15.0));
    assert_eq!(s(&p, "far"), (33.0, 34.0));
    // Overwrite mode.
    let mut p = chain();
    run(&mut p, json!({"op":"insert_clip","track":"v1","at":6.0,"ripple":false,"clip":{"id":"x","asset":"mv","duration":2}}));
    assert_eq!(s(&p, "c2"), (4.0, 6.0));
    assert_eq!(s(&p, "x"), (6.0, 8.0));
    assert_eq!(s(&p, "c3"), (8.0, 12.0));
}

// ------------------------------------------------------------------------------------------ moving and trimming

#[test]
fn move_clip_validates_anchors_and_tracks() {
    let mut p = chain();
    run(&mut p, json!({"op":"move_clip","clip":"c3","start":20}));
    assert_eq!(s(&p, "c3"), (20.0, 24.0));
    run(&mut p, json!({"op":"move_clip","clip":"c3","start":{"after":"c2","offset":0.5},"track":"v2"}));
    assert_eq!(s(&p, "c3"), (8.5, 12.5));
    assert_eq!(clip(&p, "c3").track, "v2");
    let m = fails(&mut p, json!({"op":"move_clip","clip":"c1","start":{"after":"c2"}}));
    assert!(m.contains("anchor cycle") && m.contains("c1 → c2 → c1"), "{m}");
    let m = fails(&mut p, json!({"op":"move_clip","clip":"c1","start":{"with":"c22"}}));
    assert!(m.contains("no clip or marker has the id c22") && m.contains("Did you mean c2?"), "{m}");
    let m = fails(&mut p, json!({"op":"move_clip","clip":"c9","start":1}));
    assert!(m.contains("clip c9 not found") && m.contains("v1: c1, c2") && m.contains("v2: c3, t1"), "{m}");
    let m = fails(&mut p, json!({"op":"move_clip","clip":"t1","start":0,"track":"a1"}));
    assert!(m.contains("cannot go on audio track"), "{m}");
    let m = fails(&mut p, json!({"op":"move_clip","clip":"c3","start":{"with":"c1","offset":-1}}));
    assert!(m.contains("before the start of the timeline"), "{m}");
    // Overwrite.
    run(&mut p, json!({"op":"move_clip","clip":"c3","start":2,"track":"v1","overwrite":true}));
    assert_eq!(s(&p, "c1"), (0.0, 2.0));
    assert_eq!(s(&p, "c3"), (2.0, 6.0));
    assert_eq!(s(&p, "c2"), (6.0, 8.0));
}

#[test]
fn trim_end_is_magnetic_ripples_or_pins() {
    // Magnetic default: the chain and attached clips follow.
    let mut p = chain();
    let o = run(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"end","delta":-1}));
    assert_eq!(o.changed, vec!["c1"], "only c1's data changed; the others moved through anchors");
    assert_eq!(s(&p, "c1"), (0.0, 3.0));
    assert_eq!(s(&p, "c2"), (3.0, 7.0));
    assert_eq!(s(&p, "c3"), (7.0, 11.0));
    assert_eq!(s(&p, "t1"), (3.0, 5.0));
    // ripple: false — nothing else moves.
    let mut p = chain();
    run(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"end","delta":-1,"ripple":false}));
    assert_eq!(s(&p, "c2"), (4.0, 8.0));
    assert_eq!(clip(&p, "c2").start, after("c1", 1.0), "anchor kept, offset compensates");
    assert_eq!(s(&p, "t1"), (4.0, 6.0));
    // ripple: true — absolute clips later on the track shift too.
    let mut p = chain();
    p.clips.push(media_clip("far", "v1", "mv", Time::At(20.0), 1.0, 0.0));
    run(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"end","to":2.5,"ripple":true}));
    assert_eq!(s(&p, "c3"), (6.5, 10.5));
    assert_eq!(s(&p, "far"), (18.5, 19.5));
    // Extending works and the chain follows; too short and past-media-end are refused.
    let mut p = chain();
    run(&mut p, json!({"op":"trim_clip","clip":"c2","edge":"end","delta":2}));
    assert_eq!(s(&p, "c3"), (10.0, 14.0));
    let m = fails(&mut p, json!({"op":"trim_clip","clip":"c2","edge":"end","delta":-7}));
    assert!(m.contains("would leave -1 s") && m.contains("6 s long"), "{m}");
    let m = fails(&mut p, json!({"op":"trim_clip","clip":"c3","edge":"end","delta":100}));
    assert!(m.contains("past the end"), "{m}");
    assert!(fails(&mut p, json!({"op":"trim_clip","clip":"c3","edge":"end"})).contains("`to`"));
    assert!(fails(&mut p, json!({"op":"trim_clip","clip":"c3","edge":"end","to":1,"delta":1})).contains("not both"));
}

#[test]
fn trim_start_moves_in_point_through_speed() {
    // Absolute clip: its end stays, in_point advances by delta × speed.
    let mut p = chain();
    p.clips[0].speed = Param::Value(2.0);
    run(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"start","delta":1}));
    assert_eq!(s(&p, "c1"), (1.0, 4.0));
    assert_eq!(inp(&p, "c1"), 2.0);
    assert_eq!(s(&p, "c2"), (4.0, 8.0));
    // A chained clip keeps its start (the chain closes up).
    let mut p = chain();
    run(&mut p, json!({"op":"trim_clip","clip":"c2","edge":"start","delta":1}));
    assert_eq!(s(&p, "c2"), (4.0, 7.0));
    assert_eq!(inp(&p, "c2"), 11.0);
    assert_eq!(s(&p, "c3"), (7.0, 11.0));
    // ...unless ripple: false, which keeps its end and pins attached clips.
    let mut p = chain();
    run(&mut p, json!({"op":"trim_clip","clip":"c2","edge":"start","delta":1,"ripple":false}));
    assert_eq!(s(&p, "c2"), (5.0, 8.0));
    assert_eq!(s(&p, "c3"), (8.0, 12.0));
    assert_eq!(s(&p, "t1"), (4.0, 6.0));
    // ripple: true on an absolute clip keeps its start and pulls everything after it.
    let mut p = chain();
    run(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"start","delta":1,"ripple":true}));
    assert_eq!(s(&p, "c1"), (0.0, 3.0));
    assert_eq!(inp(&p, "c1"), 1.0);
    assert_eq!(s(&p, "c3"), (7.0, 11.0));
    // Keys stay locked to the content.
    let mut p = chain();
    p.clips[0].opacity = keys(&[(0.0, 0.0), (2.0, 1.0)]);
    run(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"start","delta":1}));
    assert_eq!(clip(&p, "c1").opacity, keys(&[(0.0, 0.5), (1.0, 1.0)]));
    // Reaching before the media's start is refused.
    let m = fails(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"start","delta":-2}));
    assert!(m.contains("before the beginning of mv"), "{m}");
    // Reversed clips keep their in-point when the head is trimmed.
    let mut p = chain();
    p.clips[2].reverse = true;
    run(&mut p, json!({"op":"trim_clip","clip":"c3","edge":"start","delta":1}));
    assert_eq!(inp(&p, "c3"), 20.0);
    assert_eq!(crate::eval::source_span(clip(&p, "c3")), Some((20.0, 23.0)));
}

// ------------------------------------------------------------------------------------------ split

#[test]
fn split_divides_content_keys_fades_and_anchors() {
    let mut p = chain();
    {
        let c2 = p.clips.iter_mut().find(|c| c.id == "c2").unwrap();
        c2.opacity = keys(&[(0.0, 0.0), (4.0, 1.0)]);
        c2.fade_in = 1.0;
        c2.fade_out = 1.0;
        c2.effects.push(effect("blur", &[]));
        c2.effects[0].params.insert("radius".into(), keys(&[(1.0, 0.0), (3.0, 20.0)]));
    }
    p.transitions.push(Transition { id: "tr1".into(), from: "c2".into(), to: "c3".into(), kind: "crossfade".into(), duration: 0.5, params: Default::default(), colors: Default::default() });
    p.markers.push(marker("mk1", after("c2", 0.0)));
    p.clips.push(text_clip("before_c2", "v2", before("c2", 0.0), 1.0));
    let o = run(&mut p, json!({"op":"split_clip","clip":"c2","at":6.0}));
    assert_eq!(o.created, vec!["c4"]);
    assert_eq!(s(&p, "c2"), (4.0, 6.0));
    assert_eq!(s(&p, "c4"), (6.0, 8.0));
    assert_eq!(clip(&p, "c4").start, after("c2", 0.0));
    assert_eq!(inp(&p, "c4"), 12.0);
    assert_eq!(clip(&p, "c2").opacity, keys(&[(0.0, 0.0), (2.0, 0.5)]));
    assert_eq!(clip(&p, "c4").opacity, keys(&[(0.0, 0.5), (2.0, 1.0)]));
    assert_eq!((clip(&p, "c2").fade_in, clip(&p, "c2").fade_out), (1.0, 0.0));
    assert_eq!((clip(&p, "c4").fade_in, clip(&p, "c4").fade_out), (0.0, 1.0));
    let rl = &clip(&p, "c2").effects[0].params["radius"];
    let rr = &clip(&p, "c4").effects[0].params["radius"];
    assert!((sample(rl, 2.0) - 10.0).abs() < 1e-9 && (sample(rr, 0.0) - 10.0).abs() < 1e-9 && (sample(rr, 1.0) - 20.0).abs() < 1e-9);
    assert_eq!(clip(&p, "c3").start, after("c4", 0.0), "after-anchors move to the right half");
    assert_eq!(p.markers[0].time, after("c4", 0.0));
    assert_eq!(clip(&p, "t1").start, with("c2", 0.0), "with-anchors stay with the left half");
    assert_eq!(clip(&p, "before_c2").start, before("c2", 0.0));
    assert_eq!(p.transitions[0].from, "c4");
    assert_eq!(s(&p, "c3"), (8.0, 12.0));
    // Errors and explicit ids.
    let m = fails(&mut p, json!({"op":"split_clip","clip":"c2","at":4.0}));
    assert!(m.contains("strictly inside") && m.contains("4.00–6.00"), "{m}");
    run(&mut p, json!({"op":"split_clip","clip":"c3","at":10,"new_id":"c3b"}));
    assert_eq!(s(&p, "c3b"), (10.0, 12.0));
}

#[test]
fn split_moves_word_anchors_and_highlight_windows() {
    let mut p = project_with_voice();
    p.clips.push(media_clip("vo", "a1", "m1", Time::At(0.0), 20.0, 0.0));
    p.clips.push(text_clip("k1", "v2", word("vo", 2, false, 0.0), 1.0)); // at 3.0
    p.clips.push(text_clip("k2", "v2", word("vo", 8, true, 0.0), 1.0)); // word 8 ends at 9.5
    let mut h = text_clip("h1", "v1", Time::At(0.0), 4.0);
    if let Source::Text(t) = &mut h.source {
        t.text = "a b c".into();
        t.highlight = Some(WordHighlight { color: Color::WHITE, words: vec![[0.0, 1.0], [1.0, 2.0], [2.0, 3.0]], background: None });
        t.reveal = TextReveal { kind: RevealKind::Typewriter, duration: 2.0 };
    }
    p.clips.push(h);
    run(&mut p, json!({"op":"split_clip","clip":"vo","at":5.0,"new_id":"vo2"}));
    assert_eq!(clip(&p, "k1").start, word("vo", 2, false, 0.0));
    assert_eq!(clip(&p, "k2").start, word_t("vo2", 8, true, 0.0, "w8"));
    assert_eq!(s(&p, "k2").0, 9.5);
    let o = run(&mut p, json!({"op":"split_clip","clip":"h1","at":1.5,"new_id":"h2"}));
    assert!(o.warnings.iter().any(|w| w.contains("reveal")));
    let Source::Text(t) = &clip(&p, "h2").source else { panic!() };
    assert_eq!(t.highlight.as_ref().unwrap().words, vec![[-1.5, -0.5], [-0.5, 0.5], [0.5, 1.5]]);
    assert_eq!(t.reveal.kind, RevealKind::None);
    let Source::Text(t) = &clip(&p, "h1").source else { panic!() };
    assert_eq!(t.highlight.as_ref().unwrap().words.len(), 3);
}

// ------------------------------------------------------------------------------------------ delete

#[test]
fn ripple_delete_reanchors_dependants_to_the_deleted_clips_start() {
    let mut p = chain();
    p.clips.push(media_clip("far", "v1", "mv", Time::At(20.0), 1.0, 0.0));
    p.transitions.push(Transition { id: "tr1".into(), from: "c1".into(), to: "c2".into(), kind: "crossfade".into(), duration: 0.5, params: Default::default(), colors: Default::default() });
    let o = run(&mut p, json!({"op":"delete_clip","clip":"c2","ripple":true}));
    assert!(o.removed.contains(&"c2".to_string()) && o.removed.contains(&"tr1".to_string()));
    assert!(o.warnings.iter().any(|w| w.contains("transition")));
    assert_eq!(clip(&p, "c3").start, after("c1", 0.0));
    assert_eq!(s(&p, "c3"), (4.0, 8.0));
    assert_eq!(clip(&p, "t1").start, after("c1", 0.0), "t1 was with c2: now where c2 started");
    assert_eq!(s(&p, "far"), (16.0, 17.0), "absolute later clips close the gap too");
    // The deleted clip's own anchor offset carries over.
    let mut p = chain();
    p.clips[1].start = after("c1", 0.5);
    p.clips[2].start = after("c2", 0.25);
    run(&mut p, json!({"op":"delete_clip","clip":"c2","ripple":true}));
    assert_eq!(clip(&p, "c3").start, after("c1", 0.75));
    // An absolute deleted clip turns its followers absolute.
    let mut p = chain();
    run(&mut p, json!({"op":"delete_clip","clip":"c1","ripple":true}));
    assert_eq!(clip(&p, "c2").start, Time::At(0.0));
    assert_eq!(s(&p, "c3"), (4.0, 8.0));
}

#[test]
fn plain_delete_keeps_dependants_in_place() {
    let mut p = chain();
    run(&mut p, json!({"op":"delete_clip","clip":"c2"}));
    assert_eq!(clip(&p, "c3").start, Time::At(8.0));
    assert_eq!(clip(&p, "t1").start, Time::At(4.0));
    assert_eq!(s(&p, "c3"), (8.0, 12.0));
    assert!(fails(&mut p, json!({"op":"delete_clip","clip":"c2"})).contains("clip c2 not found"));
}

#[test]
fn delete_with_before_and_word_dependants() {
    let mut p = chain();
    p.clips.push(text_clip("pre", "v2", before("c3", -0.5), 1.0)); // ends 0.5 s before c3 starts: [6.5–7.5]
    assert_eq!(s(&p, "pre"), (6.5, 7.5));
    run(&mut p, json!({"op":"delete_clip","clip":"c3","ripple":true}));
    assert_eq!(s(&p, "pre"), (6.5, 7.5), "still ends 0.5 s before where c3 started");
    let mut p = chain();
    p.clips[0].start = before("c9x", 0.0);
    p.clips.push(text_clip("c9x", "v2", Time::At(10.0), 1.0));
    // c1 = before c9x → [6–10]; c2 after c1 [10–14]. Delete c1 with ripple: c2 takes c1's anchor (before c9x) adjusted for its own length.
    run(&mut p, json!({"op":"delete_clip","clip":"c1","ripple":true}));
    assert_eq!(s(&p, "c2"), (6.0, 10.0));
}

// ------------------------------------------------------------------------------------------ other clip ops

#[test]
fn duplicate_and_set_clip() {
    let mut p = chain();
    let o = run(&mut p, json!({"op":"duplicate_clip","clip":"c1"}));
    assert_eq!(o.created, vec!["c4"]);
    assert_eq!(s(&p, "c4"), (12.0, 16.0));
    run(&mut p, json!({"op":"duplicate_clip","clip":"t1","start":20,"track":"v2","new_id":"t2"}));
    assert_eq!(s(&p, "t2"), (20.0, 22.0));
    assert!(fails(&mut p, json!({"op":"duplicate_clip","clip":"c1","start":1})).contains("would overlap"));
    run(&mut p, json!({"op":"set_clip","clip":"t1","patch":{"name":"title","opacity":0.8,"start":{"with":"c1","offset":1}}}));
    assert_eq!(clip(&p, "t1").name, "title");
    assert_eq!(clip(&p, "t1").opacity, Param::Value(0.8));
    assert_eq!(clip(&p, "t1").start, with("c1", 1.0), "anchors are replaced, not merged");
    run(&mut p, json!({"op":"set_clip","clip":"t1","patch":{"name":null,"opacity":{"keys":[{"t":0,"v":0},{"t":1,"v":1}]}}}));
    assert_eq!(clip(&p, "t1").name, "");
    assert!(clip(&p, "t1").opacity.is_animated());
    run(&mut p, json!({"op":"set_clip","clip":"t1","patch":{"source":{"text":"Changed"}}}));
    let Source::Text(t) = &clip(&p, "t1").source else { panic!() };
    assert_eq!((t.text.as_str(), t.size), ("Changed", 72.0), "merge keeps the rest of the source");
    run(&mut p, json!({"op":"set_clip","clip":"t1","patch":{"source":{"type":"solid","color":"red"}}}));
    assert!(matches!(clip(&p, "t1").source, Source::Solid { .. }));
    assert!(fails(&mut p, json!({"op":"set_clip","clip":"t1","patch":{"id":"x"}})).contains("stable"));
    assert!(fails(&mut p, json!({"op":"set_clip","clip":"t1","patch":{"opactiy":1}})).contains("Did you mean opacity?"));
    assert!(fails(&mut p, json!({"op":"set_clip","clip":"t1","patch":{"duration":-1}})).contains("at least"));
    assert!(fails(&mut p, json!({"op":"set_clip","clip":"t1","patch":{"blend":"glow"}})).contains("invalid"));
}

#[test]
fn params_and_keyframes() {
    let mut p = chain();
    run(&mut p, json!({"op":"set_param","clip":"t1","path":"transform.scale","keys":[{"t":2,"v":1.2,"ease":"out"},{"t":0,"v":1}]}));
    let Param::Animated { keys: ks } = &clip(&p, "t1").transform.scale else { panic!() };
    assert_eq!(ks.iter().map(|k| k.t).collect::<Vec<_>>(), vec![0.0, 2.0], "sorted");
    run(&mut p, json!({"op":"set_param","clip":"t1","path":"opacity","keys":[{"t":0,"v":1},{"t":1,"v":1}]}));
    assert_eq!(clip(&p, "t1").opacity, Param::Value(1.0), "flat animation collapses to a constant");
    run(&mut p, json!({"op":"set_param","clip":"t1","path":"fade_in","value":0.5}));
    assert_eq!(clip(&p, "t1").fade_in, 0.5);
    assert!(fails(&mut p, json!({"op":"set_param","clip":"t1","path":"fade_out","value":1.8})).contains("exceed"));
    assert!(fails(&mut p, json!({"op":"set_param","clip":"t1","path":"fade_in","keys":[{"t":0,"v":1}]})).contains("cannot be animated"));
    assert!(fails(&mut p, json!({"op":"set_param","clip":"t1","path":"opacity","value":1,"keys":[{"t":0,"v":1}]})).contains("not both"));
    assert!(fails(&mut p, json!({"op":"set_param","clip":"t1","path":"transform.size","value":1})).contains("Paths:"));
    run(&mut p, json!({"op":"add_effect","clip":"c1","effect":"blur"}));
    run(&mut p, json!({"op":"set_param","clip":"c1","path":"effects.0.radius","value":25}));
    assert_eq!(clip(&p, "c1").effects[0].params["radius"], Param::Value(25.0));
    let o = run(&mut p, json!({"op":"set_param","clip":"t1","path":"transform.x","keys":[{"t":4,"v":0},{"t":6,"v":100}]}));
    assert!(o.warnings[0].contains("clip-local") && o.warnings[0].contains("subtract the clip start (4 s)"), "{:?}", o.warnings);

    // Keyframes.
    run(&mut p, json!({"op":"add_keyframe","clip":"c2","path":"volume","t":1,"v":0.2}));
    assert_eq!(clip(&p, "c2").volume, Param::Animated { keys: vec![Key { t: 1.0, v: 0.2, ease: Ease::Linear }] });
    run(&mut p, json!({"op":"add_keyframe","clip":"c2","path":"volume","t":0,"v":1,"ease":"in_out"}));
    run(&mut p, json!({"op":"add_keyframe","clip":"c2","path":"volume","t":1,"v":0.3}));
    let Param::Animated { keys: ks } = &clip(&p, "c2").volume else { panic!() };
    assert_eq!(ks.len(), 2);
    assert_eq!((ks[0].t, ks[0].ease.clone(), ks[1].v), (0.0, Ease::InOut, 0.3));
    let m = fails(&mut p, json!({"op":"add_keyframe","clip":"c2","path":"volume","t":6,"v":1}));
    assert!(m.contains("clip-local") && m.contains("t = 6 − 4 = 2"), "{m}");
    run(&mut p, json!({"op":"remove_keyframe","clip":"c2","path":"volume","t":0.01}));
    assert_eq!(clip(&p, "c2").volume, Param::Value(0.3), "one key left = constant");
    let m = fails(&mut p, json!({"op":"remove_keyframe","clip":"c2","path":"volume","t":0}));
    assert!(m.contains("not animated"), "{m}");
    run(&mut p, json!({"op":"add_keyframe","clip":"c2","path":"opacity","t":0,"v":0}));
    run(&mut p, json!({"op":"add_keyframe","clip":"c2","path":"opacity","t":1,"v":1}));
    let m = fails(&mut p, json!({"op":"remove_keyframe","clip":"c2","path":"opacity","t":0.5}));
    assert!(m.contains("keys at 0, 1 s"), "{m}");
}

#[test]
fn slip_roll_slide() {
    let mut p = chain();
    run(&mut p, json!({"op":"slip_clip","clip":"c2","delta":5}));
    assert_eq!(inp(&p, "c2"), 15.0);
    assert_eq!(s(&p, "c2"), (4.0, 8.0));
    assert!(fails(&mut p, json!({"op":"slip_clip","clip":"c3","delta":100})).contains("past the end"));
    assert!(fails(&mut p, json!({"op":"slip_clip","clip":"t1","delta":1})).contains("text clip"));
    // Words follow the slip; a slip that cuts an anchored word out is refused.
    let mut q = project_with_voice();
    q.clips.push(media_clip("vo", "a1", "m1", Time::At(0.0), 10.0, 0.0));
    q.clips.push(text_clip("k", "v2", word("vo", 3, false, 0.0), 1.0));
    run(&mut q, json!({"op":"slip_clip","clip":"vo","delta":1}));
    assert_eq!(s(&q, "k").0, 3.0);
    let m = fails(&mut q, json!({"op":"slip_clip","clip":"vo","delta":4}));
    assert!(m.contains("cut out of clip vo"), "{m}");

    let mut p = chain();
    run(&mut p, json!({"op":"roll_edit","left":"c1","right":"c2","delta":1}));
    assert_eq!(s(&p, "c1"), (0.0, 5.0));
    assert_eq!(s(&p, "c2"), (5.0, 8.0));
    assert_eq!(inp(&p, "c2"), 11.0);
    assert_eq!(s(&p, "c3"), (8.0, 12.0));
    assert_eq!(s(&p, "t1"), (4.0, 6.0), "content-synced items stay");
    assert!(fails(&mut p, json!({"op":"roll_edit","left":"c1","right":"c3","delta":1})).contains("touching"));
    assert!(fails(&mut p, json!({"op":"roll_edit","left":"c1","right":"c2","delta":5})).contains("cut can move"));

    let mut p = chain();
    run(&mut p, json!({"op":"slide_clip","clip":"c2","delta":1}));
    assert_eq!(s(&p, "c1"), (0.0, 5.0));
    assert_eq!(s(&p, "c2"), (5.0, 9.0));
    assert_eq!(inp(&p, "c2"), 10.0);
    assert_eq!(s(&p, "c3"), (9.0, 12.0));
    assert_eq!(inp(&p, "c3"), 21.0);
    assert_eq!(s(&p, "t1"), (5.0, 7.0), "attached title moves with the slid clip");
    assert!(fails(&mut p, json!({"op":"slide_clip","clip":"c2","delta":-6})).contains("shrink c1"));
}

#[test]
fn speed_and_freeze() {
    let mut p = chain();
    p.clips[1].opacity = keys(&[(0.0, 0.0), (4.0, 1.0)]);
    run(&mut p, json!({"op":"set_speed","clip":"c2","speed":2}));
    assert_eq!(s(&p, "c2"), (4.0, 6.0));
    assert_eq!(s(&p, "c3"), (6.0, 10.0), "the chain follows");
    assert_eq!(clip(&p, "c2").opacity, keys(&[(0.0, 0.0), (2.0, 1.0)]), "keys scale with the clip");
    run(&mut p, json!({"op":"set_speed","clip":"c2","speed":0.5,"keep":"duration"}));
    assert_eq!(s(&p, "c2"), (4.0, 6.0));
    assert_eq!(crate::eval::source_span(clip(&p, "c2")), Some((10.0, 11.0)));
    assert!(fails(&mut p, json!({"op":"set_speed","clip":"c2","speed":0})).contains("keep: \"duration\""));
    assert!(fails(&mut p, json!({"op":"set_speed","clip":"c2","speed":101})).contains("out of range"));
    assert!(fails(&mut p, json!({"op":"set_speed","clip":"t1","speed":2})).contains("media clips"));

    let mut p = chain();
    let o = run(&mut p, json!({"op":"freeze_frame","clip":"c2","at":6,"duration":1}));
    assert_eq!(o.created, vec!["c4", "c5"]);
    assert_eq!(s(&p, "c2"), (4.0, 6.0));
    assert_eq!(s(&p, "c4"), (6.0, 7.0));
    assert_eq!(clip(&p, "c4").speed, Param::Value(0.0));
    assert_eq!(inp(&p, "c4"), 12.0);
    assert!(clip(&p, "c4").muted);
    assert_eq!(s(&p, "c5"), (7.0, 9.0));
    assert_eq!(inp(&p, "c5"), 12.0);
    assert_eq!(s(&p, "c3"), (9.0, 13.0));
    // At the first frame the freeze takes the clip's place.
    let mut p = chain();
    run(&mut p, json!({"op":"freeze_frame","clip":"c1","at":0,"duration":2,"new_id":"fz"}));
    assert_eq!(s(&p, "fz"), (0.0, 2.0));
    assert_eq!(s(&p, "c1"), (2.0, 6.0));
    assert_eq!(s(&p, "c3"), (10.0, 14.0));
    assert!(fails(&mut p, json!({"op":"freeze_frame","clip":"t1","at":5,"duration":1})).contains("video clip"));
}

#[test]
fn detach_audio_links_and_follows_edits() {
    let mut p = chain();
    let o = run(&mut p, json!({"op":"detach_audio","clip":"c2","track":"a2"}));
    let a = o.created[0].clone();
    assert_eq!(clip(&p, &a).start, with("c2", 0.0));
    assert_eq!(clip(&p, &a).track, "a2");
    assert!(clip(&p, "c2").muted);
    assert!(!clip(&p, &a).muted);
    assert_eq!(s(&p, &a), (4.0, 8.0));
    assert!(fails(&mut p, json!({"op":"detach_audio","clip":"c2","track":"a2"})).contains("already detached"));
    assert!(fails(&mut p, json!({"op":"detach_audio","clip":"c3","track":"v2"})).contains("video track"));
    // Trims, slips and splits keep the linked audio in sync.
    run(&mut p, json!({"op":"trim_clip","clip":"c2","edge":"start","delta":1}));
    assert_eq!(s(&p, &a), (4.0, 7.0));
    assert_eq!(inp(&p, &a), 11.0);
    run(&mut p, json!({"op":"slip_clip","clip":"c2","delta":2}));
    assert_eq!(inp(&p, &a), 13.0);
    let o = run(&mut p, json!({"op":"split_clip","clip":"c2","at":5.5}));
    let (r, ar) = (o.created[0].clone(), o.created[1].clone());
    assert_eq!(clip(&p, &ar).start, with(&r, 0.0));
    assert_eq!(s(&p, &ar), (5.5, 7.0));
    assert_eq!(inp(&p, &ar), 14.5);
    let o = run(&mut p, json!({"op":"delete_clip","clip":&r,"ripple":true}));
    assert!(o.removed.contains(&ar), "linked audio is deleted with its video");
    assert_eq!(s(&p, "c3"), (5.5, 9.5));
}

#[test]
fn replace_media_lift_extract_nudge() {
    let mut p = chain();
    run(&mut p, json!({"op":"add_media","path":"b.mp4","kind":"video","info":{"duration":5,"has_video":true}}));
    run(&mut p, json!({"op":"replace_media","clip":"c1","asset":"m2","in_point":0.5}));
    assert_eq!(clip(&p, "c1").source, Source::Media { asset: "m2".into(), in_point: 0.5 });
    assert!(fails(&mut p, json!({"op":"replace_media","clip":"c2","asset":"m2"})).contains("past the end"));
    assert!(fails(&mut p, json!({"op":"replace_media","clip":"t1","asset":"m2"})).contains("not a media clip"));

    let mut p = chain();
    run(&mut p, json!({"op":"lift_range","from":2,"to":6,"tracks":["v1"]}));
    assert_eq!(s(&p, "c1"), (0.0, 2.0));
    assert_eq!(s(&p, "c2"), (6.0, 8.0));
    assert_eq!(inp(&p, "c2"), 12.0);
    assert_eq!(s(&p, "c3"), (8.0, 12.0));
    assert_eq!(s(&p, "t1"), (4.0, 6.0));

    let mut p = chain();
    run(&mut p, json!({"op":"extract_range","from":2,"to":6,"tracks":["v1"]}));
    assert_eq!(s(&p, "c1"), (0.0, 2.0));
    assert_eq!(s(&p, "c2"), (2.0, 4.0));
    assert_eq!(s(&p, "c3"), (4.0, 8.0));
    assert_eq!(s(&p, "t1"), (2.0, 4.0), "attached clips on other tracks follow their clip");

    let mut p = chain();
    p.markers.push(marker("mk1", Time::At(10.0)));
    p.markers.push(marker("mk2", Time::At(3.0)));
    let o = run(&mut p, json!({"op":"extract_range","from":3,"to":5}));
    assert!(o.removed.is_empty());
    assert_eq!(s(&p, "c1"), (0.0, 3.0));
    assert_eq!(s(&p, "c2"), (3.0, 6.0));
    assert_eq!(s(&p, "c3"), (6.0, 10.0));
    assert_eq!(s(&p, "t1"), (3.0, 4.0), "t1 [4–6] lost its first second");
    assert_eq!(p.markers[0].time, Time::At(8.0));
    assert_eq!(p.markers[1].time, Time::At(3.0));
    let m = fails(&mut p, json!({"op":"extract_range","from":5,"to":5}));
    assert!(m.contains("from < to"), "{m}");

    let mut p = chain();
    run(&mut p, json!({"op":"nudge","clips":["c3"],"delta":1}));
    assert_eq!(s(&p, "c3"), (9.0, 13.0));
    assert_eq!(clip(&p, "c3").start, after("c2", 1.0));
    run(&mut p, json!({"op":"nudge","clips":["c2","c3"],"delta":0.5}));
    assert_eq!(s(&p, "c2"), (4.5, 8.5));
    assert_eq!(s(&p, "c3"), (9.5, 13.5), "a nudged chain moves as one");
    assert!(fails(&mut p, json!({"op":"nudge","clips":["c1"],"delta":-1})).contains("before the start"));
}

#[test]
fn ranges_keep_detached_audio_linked() {
    let mut p = chain();
    let o = run(&mut p, json!({"op":"detach_audio","clip":"c2","track":"a2"}));
    let a = o.created[0].clone();
    // Spanning: c2 [4–8] and its audio are split around 5..6 together.
    run(&mut p, json!({"op":"extract_range","from":5,"to":6,"tracks":["v1","a2"]}));
    assert_eq!(s(&p, "c2"), (4.0, 5.0));
    assert_eq!(s(&p, &a), (4.0, 5.0));
    assert_eq!(clip(&p, &a).start, with("c2", 0.0));
    let right = p.clips.iter().find(|c| c.track == "v1" && c.start == after("c2", 0.0)).unwrap().id.clone();
    assert_eq!(s(&p, &right), (5.0, 7.0));
    let ar = p.clips.iter().find(|c| c.track == "a2" && c.id != a).unwrap();
    assert_eq!(ar.start, with(&right, 0.0), "the audio's right half stays linked to the video's");
    assert_eq!(s(&p, &ar.id), (5.0, 7.0));
    assert_eq!(s(&p, "c3"), (7.0, 11.0));
    // Head-trimmed: lift 6.5..7.5 on both tracks.
    let ar_id = ar.id.clone();
    run(&mut p, json!({"op":"lift_range","from":4.5,"to":5.5,"tracks":["v1","a2"]}));
    assert_eq!(s(&p, &right), (5.5, 7.0));
    assert_eq!(s(&p, &ar_id), (5.5, 7.0));
    assert_eq!(clip(&p, &ar_id).start, with(&right, 0.0));
    assert_eq!(s(&p, "c2"), (4.0, 4.5));
    assert_eq!(s(&p, &a), (4.0, 4.5));
}

#[test]
fn broken_projects_can_be_repaired() {
    let mut p = chain();
    p.clips[0].start = after("c3", 0.0); // c1 → c3 → c2 → c1: a cycle; t1 is blocked
    assert_eq!(resolve(&p).errors.len(), 4);
    // Deleting c2 changes how c3/t1 are broken (unknown target instead of cycle/blocked): allowed.
    run(&mut p, json!({"op":"delete_clip","clip":"c2"}));
    run(&mut p, json!({"op":"move_clip","clip":"c1","start":0}));
    run(&mut p, json!({"op":"move_clip","clip":"c3","start":{"after":"c1"}}));
    run(&mut p, json!({"op":"move_clip","clip":"t1","start":{"with":"c3"}}));
    assert!(resolve(&p).errors.is_empty());
    assert_eq!(s(&p, "t1"), (4.0, 6.0));
}

#[test]
fn improving_an_already_broken_clip_is_allowed() {
    let mut p = chain();
    p.clips[2].duration = 110.0; // c3 plays source 20–130 of a 120 s file
    run(&mut p, json!({"op":"trim_clip","clip":"c3","edge":"end","delta":-5}));
    assert_eq!(clip(&p, "c3").duration, 105.0);
    run(&mut p, json!({"op":"set_clip","clip":"c3","patch":{"name":"still too long"}}));
    // But a new kind of problem is still refused.
    assert!(fails(&mut p, json!({"op":"set_speed","clip":"c3","speed":200,"keep":"duration"})).contains("out of range"));
}

// ------------------------------------------------------------------------------------------ effects, transitions, markers, ducking

#[test]
fn effect_ops() {
    let mut p = chain();
    run(&mut p, json!({"op":"add_effect","clip":"c1","effect":"blur"}));
    run(&mut p, json!({"op":"add_effect","clip":"c1","effect":{"kind":"color","params":{"saturation":1.2}},"index":0}));
    assert_eq!(clip(&p, "c1").effects.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), vec!["color", "blur"]);
    let m = fails(&mut p, json!({"op":"add_effect","clip":"c1","effect":"blurr"}));
    assert!(m.contains("Did you mean blur?") && m.contains("chroma_key"), "{m}");
    let m = fails(&mut p, json!({"op":"add_effect","clip":"c1","effect":{"kind":"blur","params":{"radios":3}}}));
    assert!(m.contains("no parameter \"radios\"") && m.contains("Did you mean radius?"), "{m}");
    run(&mut p, json!({"op":"set_effect","clip":"c1","index":1,"params":{"radius":{"keys":[{"t":1,"v":0},{"t":0,"v":20}]}},"enabled":false}));
    let b = &clip(&p, "c1").effects[1];
    assert!(!b.enabled);
    assert_eq!(b.params["radius"], keys(&[(0.0, 20.0), (1.0, 0.0)]));
    run(&mut p, json!({"op":"set_effect","clip":"c1","index":1,"params":{"radius":null}}));
    assert!(clip(&p, "c1").effects[1].params.is_empty());
    run(&mut p, json!({"op":"add_effect","clip":"c1","effect":"vignette"}));
    run(&mut p, json!({"op":"set_effect","clip":"c1","index":2,"colors":{"color":"#102030"}}));
    assert!(fails(&mut p, json!({"op":"set_effect","clip":"c1","index":2,"colors":{"tint":"#102030"}})).contains("no colour"));
    assert!(fails(&mut p, json!({"op":"set_effect","clip":"c1","index":2,"strings":{"path":"x"}})).contains("no string parameter"));
    run(&mut p, json!({"op":"move_effect","clip":"c1","from":2,"to":0}));
    assert_eq!(clip(&p, "c1").effects[0].kind, "vignette");
    run(&mut p, json!({"op":"remove_effect","clip":"c1","index":0}));
    assert!(fails(&mut p, json!({"op":"remove_effect","clip":"c1","index":5})).contains("indices 0–1"));
}

#[test]
fn transition_ops() {
    let mut p = chain();
    let o = run(&mut p, json!({"op":"add_transition","from":"c1","to":"c2","kind":"crossfade"}));
    assert_eq!(o.created, vec!["tr1"]);
    assert_eq!(p.transitions[0].duration, 0.5, "registry default");
    assert!(fails(&mut p, json!({"op":"add_transition","from":"c1","to":"c2","kind":"crossfade"})).contains("already a transition tr1"));
    assert!(fails(&mut p, json!({"op":"add_transition","from":"c1","to":"c3","kind":"crossfade"})).contains("touching"));
    assert!(fails(&mut p, json!({"op":"add_transition","from":"c1","to":"t1","kind":"crossfade"})).contains("same track"));
    assert!(fails(&mut p, json!({"op":"add_transition","from":"c2","to":"c3","kind":"crosfade"})).contains("Did you mean crossfade?"));
    assert!(fails(&mut p, json!({"op":"add_transition","from":"c2","to":"c3","kind":"crossfade","duration":5})).contains("at most 4 s"));
    assert!(fails(&mut p, json!({"op":"add_transition","from":"c2","to":"c3","kind":"iris","params":{"size":1}})).contains("no parameter"));
    run(&mut p, json!({"op":"add_transition","from":"c2","to":"c3","kind":"iris","params":{"x":0.2},"id":"irisA"}));
    run(&mut p, json!({"op":"set_transition","transition":"irisA","kind":"whip","duration":0.3}));
    assert!(p.transitions[1].params.is_empty(), "iris params dropped for whip");
    run(&mut p, json!({"op":"set_transition","transition":"irisA","params":{"angle":180}}));
    assert_eq!(p.transitions[1].params["angle"], 180.0);
    assert!(fails(&mut p, json!({"op":"set_transition","transition":"irisA","params":{"angle":999}})).contains("outside"));
    run(&mut p, json!({"op":"remove_transition","transition":"tr1"}));
    assert!(fails(&mut p, json!({"op":"remove_transition","transition":"tr1"})).contains("Existing: irisA"));
    // Moving a clip off its track removes its transitions.
    let o = run(&mut p, json!({"op":"move_clip","clip":"c3","start":30,"track":"v2"}));
    assert!(o.removed.contains(&"irisA".to_string()));
}

#[test]
fn marker_ops() {
    let mut p = chain();
    run(&mut p, json!({"op":"add_marker","time":{"with":"c2","offset":3},"name":"beat","kind":"beat"}));
    assert_eq!(resolve(&p).marker("mk1"), Some(7.0));
    run(&mut p, json!({"op":"trim_clip","clip":"c1","edge":"end","delta":-1}));
    assert_eq!(resolve(&p).marker("mk1"), Some(6.0), "anchored markers follow");
    run(&mut p, json!({"op":"add_clip","clip":{"id":"k","track":"v2","text":"hit","start":{"marker":"mk1"},"duration":0.5}}));
    assert_eq!(s(&p, "k"), (6.0, 6.5));
    run(&mut p, json!({"op":"set_marker","marker":"mk1","time":9,"note":"moved","color":"red"}));
    assert_eq!(s(&p, "k").0, 9.0);
    run(&mut p, json!({"op":"set_marker","marker":"mk1","color":null}));
    assert_eq!(p.markers[0].color, None);
    let o = run(&mut p, json!({"op":"remove_marker","marker":"mk1"}));
    assert!(o.warnings[0].contains("k was anchored to the removed marker \"beat\""), "{:?}", o.warnings);
    assert!(!o.warnings[0].contains("mk1"), "removed ids are never named in warnings");
    assert_eq!(clip(&p, "k").start, Time::At(9.0));
    assert!(fails(&mut p, json!({"op":"add_marker","time":{"marker":"c1"}})).contains("is a clip"));
}

#[test]
fn ducking_ops() {
    let mut p = chain();
    let o = run(&mut p, json!({"op":"add_ducking","target":"a2","trigger":"a1","amount_db":-12}));
    assert_eq!(o.created, vec!["d1"]);
    assert_eq!((p.ducking[0].amount_db, p.ducking[0].threshold_db), (-12.0, -45.0));
    assert!(fails(&mut p, json!({"op":"add_ducking","target":"a2","trigger":"a2"})).contains("cannot duck itself"));
    assert!(fails(&mut p, json!({"op":"add_ducking","target":"a9","trigger":"a1"})).contains("track a9 not found"));
    assert!(fails(&mut p, json!({"op":"add_ducking","target":"a2","trigger":"a1","amount_db":6})).contains("-60 to 0"));
    run(&mut p, json!({"op":"set_ducking","ducking":"d1","amount_db":-18,"release":1}));
    assert_eq!((p.ducking[0].amount_db, p.ducking[0].release), (-18.0, 1.0));
    run(&mut p, json!({"op":"remove_ducking","ducking":"d1"}));
    assert!(p.ducking.is_empty());
}

// ------------------------------------------------------------------------------------------ transcripts

#[test]
fn set_transcript_validates_and_protects_anchors() {
    let mut p = project_with_voice();
    p.clips.push(media_clip("vo", "a1", "m1", Time::At(0.0), 20.0, 0.0));
    p.clips.push(text_clip("k", "v2", word("vo", 10, false, 0.0), 1.0));
    let m = fails(&mut p, json!({"op":"set_transcript","asset":"m1","transcript":{"words":[{"text":"b","start":2,"end":3},{"text":"a","start":1,"end":2}]}}));
    assert!(m.contains("time order"), "{m}");
    let m = fails(&mut p, json!({"op":"set_transcript","asset":"m1","transcript":{"words":[{"text":"a","start":1,"end":0.5}]}}));
    assert!(m.contains("invalid times"), "{m}");
    assert!(fails(&mut p, json!({"op":"set_transcript","asset":"m1"})).contains("required"));
    // Word 10 (11.0–11.5) is not in the new transcript: k keeps its time, with a warning.
    let o = run(&mut p, json!({"op":"set_transcript","asset":"m1","transcript":{"words":[{"text":"a","start":1,"end":2}]}}));
    assert!(o.changed.contains(&"m1".to_string()));
    assert!(o.warnings[0].contains("k was anchored to \"w10\" (word 10), which the new transcript no longer has"), "{:?}", o.warnings);
    assert!(resolve(&p).errors.is_empty(), "never leaves broken anchors");
    assert_eq!(clip(&p, "k").start, Time::At(11.0));
    run(&mut p, json!({"op":"delete_clip","clip":"k"}));
    run(&mut p, json!({"op":"set_transcript","asset":"m1","transcript":null}));
    assert!(!p.transcripts.contains_key("m1"));
}

#[test]
fn cut_source_removes_a_source_range() {
    let mut p = project_with_voice();
    p.clips.push(media_clip("vo", "a1", "m1", Time::At(0.0), 20.0, 0.0));
    p.clips.push(media_clip("next", "a1", "m1", after("vo", 0.0), 5.0, 30.0));
    run(&mut p, json!({"op":"cut_source","clip":"vo","from":5.0,"to":7.0}));
    assert_eq!(s(&p, "vo"), (0.0, 5.0));
    let right = p.clips.iter().find(|c| c.start == after("vo", 0.0)).unwrap().id.clone();
    assert_eq!(s(&p, &right), (5.0, 18.0));
    assert_eq!(inp(&p, &right), 7.0);
    assert_eq!(s(&p, "next"), (18.0, 23.0));
    let m = fails(&mut p, json!({"op":"cut_source","clip":"vo","from":40,"to":41}));
    assert!(m.contains("not in vo") && m.contains("0.00–5.00"), "{m}");
    // Cutting everything deletes the clip (ripple).
    run(&mut p, json!({"op":"cut_source","clip":"next","from":0,"to":100}));
    assert!(p.clips.iter().all(|c| c.id != "next"));
}

#[test]
fn delete_words_cuts_through_in_point_and_speed() {
    // Words: word i is source i+1 .. i+1.5. Clip plays source 2..22 at 2× from t=10.
    let mut p = project_with_voice();
    let mut vo = media_clip("vo", "a1", "m1", Time::At(10.0), 10.0, 2.0);
    vo.speed = Param::Value(2.0);
    p.clips.push(vo);
    p.clips.push(media_clip("tail", "a1", "m1", after("vo", 0.0), 2.0, 40.0));
    // Delete words 3–4 (source 4.0–5.5); neighbours: word 2 ends 3.5, word 5 starts 6.0.
    let o = run(&mut p, json!({"op":"delete_words","clip":"vo","from":3,"to":4}));
    assert!(o.summary.contains("w3 w4") && o.summary.contains("3.96–5.54"), "{}", o.summary);
    // Source cut 3.96..5.54 = 1.58 s of source = 0.79 s of timeline at 2×.
    assert_eq!(s(&p, "vo"), (10.0, 10.98));
    let right = p.clips.iter().find(|c| c.start == after("vo", 0.0)).unwrap().id.clone();
    assert_eq!(inp(&p, &right), 5.54);
    assert_eq!(s(&p, "tail").0, 10.0 + (20.0 - 1.58) / 2.0);
    // Neighbouring words survive whole.
    let r = resolve(&p);
    let vo = clip(&p, "vo");
    assert!(crate::eval::local_time_of_source(vo, 3.5).is_some(), "word 2's end is kept");
    let rc = clip(&p, &right);
    assert!(crate::eval::local_time_of_source(rc, 6.0).is_some(), "word 5's start is kept");
    assert!(r.errors.is_empty());
    let m = fails(&mut p, json!({"op":"delete_words","clip":"vo","from":5,"to":2}));
    assert!(m.contains("from must be ≤ to"), "{m}");
    let m = fails(&mut p, json!({"op":"delete_words","clip":"vo","from":45,"to":46}));
    assert!(m.contains("not in vo"), "{m}");
    // Words in another piece of the same recording on the track are found there.
    let o = run(&mut p, json!({"op":"delete_words","clip":"vo","from":40,"to":40}));
    assert!(o.changed.contains(&"tail".to_string()) || o.created.len() == 1, "{o:?}");
    let m = fails(&mut p, json!({"op":"delete_words","clip":"vo","from":0,"to":99}));
    assert!(m.contains("0–49"), "{m}");
}

fn captioned() -> Project {
    let mut p = project_with_voice();
    p.clips.push(media_clip("vo", "a1", "m1", Time::At(0.0), 30.0, 0.0));
    p.tracks.push(track("v3", TrackKind::Video, "captions"));
    run(&mut p, json!({"op":"generate_captions","clips":["vo"],"track":"v3","max_words":3,"pause":0.6}));
    p
}

fn caption_text(c: &Clip) -> (&str, Vec<[f64; 2]>) {
    let Source::Text(t) = &c.source else { panic!() };
    (t.text.as_str(), t.highlight.as_ref().unwrap().words.clone())
}

#[test]
fn generate_captions_groups_words_and_anchors_them() {
    let p = captioned();
    let caps: Vec<&Clip> = p.clips.iter().filter(|c| c.track == "v3").collect();
    // Groups: [0 1 2] [3 4 5] [6.] [7 8 9] … sentences end at 6, 13, 20, 27 (words 0–28 are in 0–30 s).
    let texts: Vec<&str> = caps.iter().map(|c| caption_text(c).0).collect();
    assert_eq!(&texts[..4], &["w0 w1 w2", "w3 w4 w5", "w6.", "w7 w8 w9"]);
    let c = caps[1];
    assert_eq!(c.start, word_t("vo", 3, false, 0.0, "w3"), "the anchor remembers its word");
    assert_eq!(s(&p, &c.id), (4.0, 7.0), "bridges the short pause to the next caption");
    assert_eq!(caption_text(c).1, vec![[0.0, 0.5], [1.0, 1.5], [2.0, 2.5]]);
    assert_eq!(c.tags, vec!["caption"]);
    let Source::Text(t) = &c.source else { panic!() };
    assert_eq!((t.size, t.weight, t.color), (65.0, 800, Color::WHITE));
    assert_eq!(t.background.as_ref().unwrap().color.to_hex(), "#000000B3");
    assert_eq!(t.highlight.as_ref().unwrap().color.to_hex(), "#FFD60A");
    assert_eq!(c.transform.y, Param::Value(358.0), "bottom of the title-safe area");
    assert_eq!(t.max_width, Some(1496.0));
    // No overlaps, all inside title safe, nothing unresolved.
    let r = resolve(&p);
    assert!(r.errors.is_empty());
    let f = crate::validate::validate(&p, &r);
    assert!(f.iter().all(|f| f.severity == crate::validate::Severity::Info), "{f:#?}");
    // Regenerating replaces.
    let mut q = p.clone();
    let n = caps.len();
    let o = run(&mut q, json!({"op":"generate_captions","clips":["vo"],"track":"v3","max_words":3,"pause":0.6}));
    assert_eq!(q.clips.iter().filter(|c| c.track == "v3").count(), n);
    assert!(o.summary.contains(&format!("replaced {n} earlier caption(s) nobody had edited")), "{}", o.summary);
    assert_eq!(o.details["replaced"].as_array().unwrap().len(), n);
    // Captions follow the voice when it moves.
    let mut q = p.clone();
    run(&mut q, json!({"op":"move_clip","clip":"vo","start":5}));
    assert_eq!(s(&q, &caps[1].id).0, 9.0);
    let mut q = p.clone();
    assert!(fails(&mut q, json!({"op":"generate_captions","clips":["vo"],"track":"a1"})).contains("audio track"));
    let mut q = p.clone();
    q.settings.width = 1080;
    q.settings.height = 1920;
    run(&mut q, json!({"op":"generate_captions","clips":["vo"],"track":"v3","pause":0.6,"style":{"uppercase":true,"highlight":"#00FF00","size":80}}));
    let c = q.clips.iter().find(|c| c.track == "v3").unwrap();
    let Source::Text(t) = &c.source else { panic!() };
    assert_eq!((t.text.as_str(), t.size), ("W0 W1 W2 W3 W4", 80.0), "5 words per line by default");
    assert!(t.text.chars().count() <= 20);
    // A spoken reveal is part of the look: set by the style and kept when the captions are regenerated.
    let mut q = p.clone();
    run(&mut q, json!({"op":"generate_captions","clips":["vo"],"track":"v3","replace":true,"style":{"reveal":"spoken"}}));
    run(&mut q, json!({"op":"generate_captions","clips":["vo"],"track":"v3","max_words":3}));
    for c in q.clips.iter().filter(|c| c.track == "v3") {
        let Source::Text(t) = &c.source else { panic!() };
        assert_eq!(t.reveal.kind, RevealKind::Spoken, "{}", c.id);
        assert!(!t.highlight.as_ref().unwrap().words.is_empty());
    }
}

#[test]
fn delete_words_retimes_and_removes_captions() {
    let mut p = captioned();
    let ids: Vec<Id> = p.clips.iter().filter(|c| c.track == "v3").map(|c| c.id.clone()).collect();
    let o = run(&mut p, json!({"op":"delete_words","clip":"vo","from":3,"to":4}));
    assert!(o.summary.contains(&format!("re-timed captions {}", ids[1])), "{}", o.summary);
    let c = clip(&p, &ids[1]);
    let (text, wins) = caption_text(c);
    assert_eq!(text, "w5");
    // Windows count from where the caption is shown (4.433333, the frame nearest 4.42), so the highlight
    // starts exactly when w5 is heard: 4.433333 − 0.013333 = 4.42.
    assert!((wins[0][0] - (4.42 - 133.0 / 30.0)).abs() < 1e-9 && (wins[0][1] - (4.92 - 133.0 / 30.0)).abs() < 1e-9, "{wins:?}");
    let Time::Anchored(Anchor { to: AnchorTarget::Word(w), .. }) = &c.start else { panic!() };
    assert_eq!(w.index, 5);
    // Word 5 (source 6.0) now plays at 3.96 + (6.0 − 5.54) = 4.42; captions sit on frames (1/30 s).
    assert_eq!(s(&p, &ids[1]), (4.433333, 5.433333));
    assert_eq!(s(&p, &ids[2]).0, 5.433333, "the next caption moved up with its word");
    // Deleting all words of a caption removes it.
    let o = run(&mut p, json!({"op":"delete_words","clip":"vo","from":6,"to":6}));
    assert!(o.removed.contains(&ids[2]), "{:?}", o);
    let r = resolve(&p);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let f = crate::validate::validate(&p, &r);
    assert!(!f.iter().any(|f| f.code == "overlap_same_track" || f.code == "highlight_mismatch"), "{f:#?}");
}

// ------------------------------------------------------------------------------------------ round 2

#[test]
fn export_size_fields_and_op() {
    let mut p = chain();
    // Defaults are omitted from JSON; the fields round-trip.
    let v = serde_json::to_value(ExportSettings::default()).unwrap();
    assert!(v.get("width").is_none() && v.get("height").is_none() && v.get("fit").is_none());
    assert_eq!(p.export.output_size(&p.settings), (1920, 1080));
    let o = run(&mut p, json!({"op":"set_export_size","width":3840,"height":2160}));
    assert_eq!((p.export.width, p.export.height, p.export.fit), (Some(3840), Some(2160), Fit::Contain));
    assert!(o.warnings.is_empty(), "same aspect: no bars");
    assert_eq!(o.summary, "Export frame: 3840×2160, fit contain (project 1920×1080)");
    let o = run(&mut p, json!({"op":"set_export_size","width":1080,"height":1920}));
    assert!(o.warnings[0].contains("letterbox (top/bottom)"), "{:?}", o.warnings);
    let o = run(&mut p, json!({"op":"set_export_size","fit":"cover"}));
    assert!(o.warnings.is_empty());
    let v = serde_json::to_value(&p.export).unwrap();
    assert_eq!((v["width"].clone(), v["height"].clone(), v["fit"].clone()), (json!(1080), json!(1920), json!("cover")));
    let back: ExportSettings = serde_json::from_value(v).unwrap();
    assert_eq!(back, p.export);
    // One side keeps the project's aspect (even sizes); null returns to the project size.
    run(&mut p, json!({"op":"set_export_size","width":null,"height":1440}));
    assert_eq!(p.export.output_size(&p.settings), (2560, 1440));
    run(&mut p, json!({"op":"set_export_size","width":1000,"height":null}));
    assert_eq!(p.export.output_size(&p.settings), (1000, 562));
    run(&mut p, json!({"op":"set_export_size","width":null}));
    assert_eq!(p.export.output_size(&p.settings), (1920, 1080));
    assert!(fails(&mut p, json!({"op":"set_export_size","width":1})).contains("2–16384"));
    let o = run(&mut p, json!({"op":"set_export_size","width":1081,"height":1080}));
    assert!(o.warnings[0].contains("odd"));
    // set_export points at the new op when given a size.
    let e = Op::from_json(&json!({"op":"set_export","width":3840})).unwrap_err();
    assert!(e.message.contains("set_export_size"), "{}", e.message);
    // Describe shows it.
    let d = crate::describe::describe(&p, &resolve(&p));
    assert!(d.contains("1081×1080 fit cover"), "{d}");
}

#[test]
fn move_clips_is_atomic() {
    let mut p = project();
    p.tracks.push(track("v3", TrackKind::Video, ""));
    p.clips.push(media_clip("a", "v1", "mv", Time::At(0.0), 2.0, 0.0));
    p.clips.push(media_clip("b", "v1", "mv", Time::At(2.0), 2.0, 10.0));
    p.clips.push(text_clip("t", "v2", with("a", 0.0), 1.0));
    // One move at a time cannot swap them...
    assert!(fails(&mut p, json!({"op":"move_clip","clip":"a","start":2})).contains("would overlap"));
    // ...one move_clips can.
    let o = run(&mut p, json!({"op":"move_clips","moves":[{"clip":"a","start":2},{"clip":"b","start":0}]}));
    assert_eq!(s(&p, "a"), (2.0, 4.0));
    assert_eq!(s(&p, "b"), (0.0, 2.0));
    assert_eq!(s(&p, "t"), (2.0, 3.0), "attached clips follow");
    assert!(o.summary.starts_with("Moved a [2.00–4.00], b [0.00–2.00]"), "{}", o.summary);
    // A group moves across tracks and keeps its transition.
    p.transitions.push(Transition { id: "tr1".into(), from: "b".into(), to: "a".into(), kind: "crossfade".into(), duration: 0.5, params: Default::default(), colors: Default::default() });
    run(&mut p, json!({"op":"move_clips","moves":[{"clip":"b","start":10,"track":"v3"},{"clip":"a","start":{"after":"b"},"track":"v3"}]}));
    assert_eq!(p.transitions.len(), 1);
    assert_eq!(s(&p, "a"), (12.0, 14.0));
    assert_eq!(clip(&p, "a").track, "v3");
    // Splitting the pair drops the transition (the warning names no removed id).
    let o = run(&mut p, json!({"op":"move_clips","moves":[{"clip":"a","start":20,"track":"v1"}]}));
    assert!(p.transitions.is_empty());
    assert!(o.warnings[0].contains("1 transition") && !o.warnings[0].contains("tr1"), "{:?}", o.warnings);
    // Errors.
    assert!(fails(&mut p, json!({"op":"move_clips","moves":[]})).contains("empty"));
    assert!(fails(&mut p, json!({"op":"move_clips","moves":[{"clip":"a","start":1},{"clip":"a","start":2}]})).contains("twice"));
    assert!(fails(&mut p, json!({"op":"move_clips","moves":[{"clip":"zz","start":1}]})).contains("clip zz not found"));
    assert!(fails(&mut p, json!({"op":"move_clips","moves":[{"clip":"b","start":20,"track":"v1"}]})).contains("would overlap"));
    let e = Op::from_json(&json!({"op":"move_clips","moves":[{"clip":"b","start":1,"tracks":"v1"}]})).unwrap_err();
    assert!(e.message.contains("unknown field `tracks`"), "{}", e.message);
    let e = Op::from_json(&json!({"op":"move_clip","moves":[{"clip":"a","start":1}]})).unwrap_err();
    assert!(e.message.contains("move_clips"), "{}", e.message);
    // Overwrite carves out what is under the moved clips.
    let mut q = project();
    q.clips.push(media_clip("x", "v1", "mv", Time::At(0.0), 4.0, 0.0));
    q.clips.push(media_clip("y", "v2", "mv", Time::At(0.0), 4.0, 0.0));
    run(&mut q, json!({"op":"move_clips","moves":[{"clip":"y","start":1,"track":"v1"}],"overwrite":true}));
    assert_eq!(s(&q, "x"), (0.0, 1.0));
    assert_eq!(s(&q, "y"), (1.0, 5.0));
}

#[test]
fn warnings_never_name_removed_ids() {
    let mut p = project_with_voice();
    p.clips.push(media_clip("vo", "a1", "m1", Time::At(0.0), 30.0, 0.0));
    p.clips.push(text_clip("broll", "v2", word("vo", 4, false, 0.0), 1.0)); // word 4 is deleted below
    let o = run(&mut p, json!({"op":"delete_words","clip":"vo","from":3,"to":4}));
    let w = o.warnings.iter().find(|w| w.starts_with("broll")).unwrap_or_else(|| panic!("{:?}", o.warnings));
    assert_eq!(w, "broll was anchored to a word that was cut; it now starts after vo (3.967 s).");
    let alive: HashSet<String> = all_ids(&p).map(|(i, _)| i.to_string()).collect();
    let mut texts = o.warnings.clone();
    texts.push(o.summary.clone());
    for t in &texts {
        for tok in t.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if tok.len() > 1 && tok.starts_with('c') && tok[1..].parse::<u32>().is_ok() {
                assert!(alive.contains(tok), "{t:?} names {tok}, which does not exist");
            }
        }
    }
    // Transitions removed with a clip are described, not named.
    let mut p = chain();
    p.transitions.push(Transition { id: "tr1".into(), from: "c1".into(), to: "c2".into(), kind: "crossfade".into(), duration: 0.5, params: Default::default(), colors: Default::default() });
    let o = run(&mut p, json!({"op":"delete_clip","clip":"c2","ripple":true}));
    assert_eq!(o.warnings, vec!["removed 1 transition(s) on the cuts of a deleted clip: crossfade at 4.00 s."]);
    // The safety net rewrites any id that is gone.
    let gone: HashMap<String, &'static str> = [("c5".to_string(), "clip")].into_iter().collect();
    assert_eq!(scrub_ids("c5 moved; see c5. c50 stays", &gone), "a removed clip moved; see a removed clip. c50 stays");
}

#[test]
fn public_helpers_match_the_ops() {
    let mut p = chain();
    let o = run(&mut p, json!({"op":"detach_audio","clip":"c2","track":"a2"}));
    let a = o.created[0].clone();
    assert_eq!(linked_partners(&p, "c2"), vec![a.clone()]);
    assert_eq!(linked_partners(&p, &a), vec!["c2".to_string()]);
    assert_eq!(linked_clips(&p, "c2"), vec![a.clone()]);
    assert!(linked_clips(&p, &a).is_empty());
    assert!(linked_partners(&p, "c3").is_empty());
    run(&mut p, json!({"op":"add_effect","clip":"t1","effect":"vignette"}));
    p.clips.push(base_clip(
        "sh",
        "v2",
        Time::At(20.0),
        2.0,
        Source::Shape(ShapeSource { kind: ShapeKind::Arrow, width: 100.0, height: 0.0, fill: None, stroke: None, radius: 0.0, draw: Param::Value(1.0) }),
    ));
    for id in ["t1", "sh", "c2"] {
        let c = clip(&p, id).clone();
        let paths = param_paths(&c);
        assert_eq!(paths.contains(&"source.draw".to_string()), id == "sh");
        for path in &paths {
            // Every listed path takes a keyframe (at its current value, so it stays in range).
            let v = match path.strip_prefix("effects.") {
                Some(rest) => {
                    let (i, n) = rest.split_once('.').unwrap();
                    crate::effects::effect_param(&c.effects[i.parse::<usize>().unwrap()], n, 0.0)
                }
                None => sample(clip_params(&c).into_iter().find(|(p, _)| p == path).unwrap().1, 0.0),
            };
            let mut q = p.clone();
            apply(&mut q, &op(json!({"op":"add_keyframe","clip":id,"path":path,"t":0.0,"v":v}))).unwrap_or_else(|e| panic!("{id}.{path}: {e}"));
            if !path.starts_with("effects.") {
                assert!(crate::effects::clip_param_spec(path).is_some(), "{path} has no CLIP_PARAMS entry");
            }
        }
        for path in SCALAR_PARAM_PATHS {
            let mut q = p.clone();
            apply(&mut q, &op(json!({"op":"set_param","clip":id,"path":path,"value":0.25}))).unwrap_or_else(|e| panic!("{id}.{path}: {e}"));
            assert!(crate::effects::clip_param_spec(path).is_some());
        }
    }
    let t1 = clip(&p, "t1");
    assert!(param_paths(t1).contains(&"effects.0.amount".to_string()), "registry params are listed even when unset");
    assert!(!clip_params(t1).iter().any(|(p, _)| p.starts_with("effects.")), "clip_params lists only set values");
}

// ------------------------------------------------------------------------------------------ atomicity and scale

#[test]
fn batches_are_atomic_and_errors_name_the_op() {
    let mut p = chain();
    let before = p.clone();
    let ops = vec![op(json!({"op":"trim_clip","clip":"c1","edge":"end","delta":-1})), op(json!({"op":"split_clip","clip":"c2","at":100}))];
    let e = apply_batch(&mut p, &ops).unwrap_err();
    assert_eq!(e.index, Some(1));
    assert_eq!(e.op.as_deref(), Some("split_clip"));
    assert!(e.to_string().starts_with("op #1 (split_clip): "));
    assert_eq!(p, before);
    let outs = apply_batch(&mut p, &ops[..1]).unwrap();
    assert_eq!(outs.len(), 1);
    // Later ops in a batch see earlier results.
    let outs = apply_batch(
        &mut p,
        &[
            op(json!({"op":"append_clip","track":"v2","clip":{"id":"later","text":"x","duration":1}})),
            op(json!({"op":"move_clip","clip":"later","start":{"after":"c3"}})),
        ],
    )
    .unwrap();
    assert_eq!(outs[0].created, vec!["later"]);
    assert_eq!(s(&p, "later").0, 11.0);
}

#[test]
fn ops_scale_to_10k_clips() {
    // On an audio track: sub-frame clips keep their exact (unsnapped) spans there.
    let mut p = project();
    p.clips.push(media_clip("c0", "a1", "m1", Time::At(0.0), 0.01, 0.0));
    for i in 1..10_000 {
        p.clips.push(media_clip(&format!("c{i}"), "a1", "m1", after(&format!("c{}", i - 1), 0.0), 0.01, 0.0));
    }
    let t = std::time::Instant::now();
    let o = run(&mut p, json!({"op":"trim_clip","clip":"c0","edge":"end","delta":0.01}));
    run(&mut p, json!({"op":"delete_clip","clip":"c5000","ripple":true}));
    run(&mut p, json!({"op":"split_clip","clip":"c2","at":0.035}));
    let el = t.elapsed();
    assert!(el.as_secs_f64() < 20.0, "three ops on 10k clips took {el:?}");
    assert_eq!(o.changed, vec!["c0"]);
    assert!((span(&p, "c9999").end - (100.0 + 0.01 - 0.01)).abs() < 1e-6);
}
