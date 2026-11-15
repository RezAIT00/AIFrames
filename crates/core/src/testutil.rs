//! Builders shared by the unit tests.
#![allow(dead_code)]

use crate::model::*;
use std::collections::BTreeMap;

pub fn track(id: &str, kind: TrackKind, role: &str) -> Track {
    Track {
        id: id.into(),
        name: String::new(),
        kind,
        role: role.into(),
        muted: false,
        hidden: false,
        locked: false,
        solo: false,
        volume: 1.0,
    }
}

pub fn asset(id: &str, path: &str, kind: MediaKind, duration: f64, has_audio: bool) -> MediaAsset {
    MediaAsset {
        id: id.into(),
        name: String::new(),
        path: path.into(),
        kind,
        info: MediaInfo {
            duration,
            width: if kind == MediaKind::Audio { 0 } else { 1920 },
            height: if kind == MediaKind::Audio { 0 } else { 1080 },
            fps: if kind == MediaKind::Video { 60.0 } else { 0.0 },
            has_video: kind != MediaKind::Audio,
            has_audio,
            ..Default::default()
        },
        tags: vec![],
        notes: String::new(),
    }
}

/// Tracks v1 (video, main), v2 (video, overlay), a1 (audio, voice), a2 (audio, music);
/// media m1 = voice.wav (audio, 60 s), mv = gameplay.mp4 (video+audio, 120 s), mi = logo.png.
pub fn project() -> Project {
    Project {
        name: "Test".into(),
        tracks: vec![
            track("v1", TrackKind::Video, "main"),
            track("v2", TrackKind::Video, "overlay"),
            track("a1", TrackKind::Audio, "voice"),
            track("a2", TrackKind::Audio, "music"),
        ],
        media: vec![
            asset("m1", "media/voice.wav", MediaKind::Audio, 60.0, true),
            asset("mv", "media/gameplay.mp4", MediaKind::Video, 120.0, true),
            asset("mi", "media/logo.png", MediaKind::Image, 0.0, false),
        ],
        ..Default::default()
    }
}

/// [`project`] plus a transcript for m1 and mv: word i is `w{i}` from source i+1.0 to i+1.5
/// (every 7th word ends a sentence with "."), 50 words.
pub fn project_with_voice() -> Project {
    let mut p = project();
    let words: Vec<Word> = (0..50)
        .map(|i| Word {
            text: if i % 7 == 6 { format!(" w{i}.") } else { format!(" w{i}") },
            start: i as f64 + 1.0,
            end: i as f64 + 1.5,
            conf: 1.0,
        })
        .collect();
    p.transcripts.insert("m1".into(), Transcript { language: "en".into(), words: words.clone() });
    p.transcripts.insert("mv".into(), Transcript { language: "en".into(), words });
    p
}

pub fn base_clip(id: &str, track: &str, start: Time, duration: f64, source: Source) -> Clip {
    Clip {
        id: id.into(),
        track: track.into(),
        name: String::new(),
        start,
        duration,
        until: None,
        source,
        speed: Param::Value(1.0),
        reverse: false,
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

pub fn text_clip(id: &str, track: &str, start: Time, duration: f64) -> Clip {
    base_clip(id, track, start, duration, Source::Text(TextSource { text: format!("text {id}"), ..Default::default() }))
}

pub fn media_clip(id: &str, track: &str, asset: &str, start: Time, duration: f64, in_point: f64) -> Clip {
    base_clip(id, track, start, duration, Source::Media { asset: asset.into(), in_point })
}

pub fn after(id: &str, offset: f64) -> Time {
    Time::Anchored(Anchor { to: AnchorTarget::After(id.into()), offset })
}

pub fn with(id: &str, offset: f64) -> Time {
    Time::Anchored(Anchor { to: AnchorTarget::With(id.into()), offset })
}

pub fn before(id: &str, offset: f64) -> Time {
    Time::Anchored(Anchor { to: AnchorTarget::Before(id.into()), offset })
}

pub fn at_marker(id: &str, offset: f64) -> Time {
    Time::Anchored(Anchor { to: AnchorTarget::Marker(id.into()), offset })
}

pub fn word(clip: &str, index: usize, end: bool, offset: f64) -> Time {
    Time::Anchored(Anchor { to: AnchorTarget::Word(WordRef { clip: clip.into(), index, end, text: None }), offset })
}

pub fn marker(id: &str, time: Time) -> Marker {
    Marker { id: id.into(), time, name: String::new(), kind: MarkerKind::Marker, color: None, note: String::new() }
}

pub fn keys(k: &[(f64, f64)]) -> Param {
    Param::Animated { keys: k.iter().map(|(t, v)| Key { t: *t, v: *v, ease: Ease::Linear }).collect() }
}

pub fn effect(kind: &str, params: &[(&str, f64)]) -> Effect {
    Effect {
        kind: kind.into(),
        enabled: true,
        params: params.iter().map(|(n, v)| (n.to_string(), Param::Value(*v))).collect::<BTreeMap<_, _>>(),
        colors: Default::default(),
        strings: Default::default(),
    }
}

pub fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}
