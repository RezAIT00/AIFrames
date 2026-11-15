//! OpenTimelineIO export (`.otio` JSON), to finish an edit in DaVinci Resolve, Premiere or anything else
//! that reads OTIO.
//!
//! What carries over: the cut — every media clip with its source range on its track, gaps, video
//! track stacking order, sound of video clips (as companion audio tracks, the way NLEs keep it),
//! constant speed changes (`LinearTimeWarp`), freeze frames, crossfade-style transitions, markers and
//! chapters. What does not: text, shapes, solids and adjustment clips (exported as named offline
//! placeholders so their place and length survive), effects, keyframes, transforms, volume, ducking.
//! Captions are left out of the timeline (dozens of offline clips are noise in an editor); hosts write
//! them next to the .otio as subtitles with [`captions_srt`], which every editor imports.
//! Each exported clip carries `metadata.aiframes.clip` (its id here).
//!
//! Schema versions are the long-stable ones OTIO readers accept (Clip.1, Track.1…; Marker.2, as OTIO
//! 0.15+ and DaVinci Resolve 18.5+ read). Checked against the reference library (opentimelineio 0.18).

use crate::eval::source_time;
use crate::model::*;
use crate::resolve::Resolved;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// What [`to_otio`] produced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OtioReport {
    pub tracks: usize,
    pub clips: usize,
    /// Text, shape, solid and adjustment clips exported as offline placeholders.
    pub placeholders: usize,
    pub transitions: usize,
    pub markers: usize,
    /// Caption clips left out of the timeline (see [`captions_srt`]).
    pub captions: usize,
    /// Things that did not carry over exactly, one line each.
    pub notes: Vec<String>,
}

fn rt(v: f64, rate: f64) -> Value {
    json!({"OTIO_SCHEMA": "RationalTime.1", "rate": rate, "value": (v * rate).round()})
}

fn range(start: f64, dur: f64, rate: f64) -> Value {
    json!({"OTIO_SCHEMA": "TimeRange.1", "start_time": rt(start, rate), "duration": rt(dur, rate)})
}

/// `file:///C:/media/a%20b.mp4` for a local path (OTIO wants URLs).
pub fn file_url(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !s.starts_with('/') {
        out.push('/');
    }
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b':' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn gap(dur: f64, rate: f64) -> Value {
    json!({"OTIO_SCHEMA": "Gap.1", "name": "", "source_range": range(0.0, dur, rate), "effects": [], "markers": [], "metadata": {}, "enabled": true})
}

fn marker_color(k: MarkerKind) -> &'static str {
    match k {
        MarkerKind::Marker => "BLUE",
        MarkerKind::Chapter => "GREEN",
        MarkerKind::Beat => "CYAN",
        MarkerKind::Todo => "RED",
    }
}

/// A caption: a text clip generated from a transcript (karaoke words or the `caption` tag), or any text
/// clip on a captions track.
pub fn is_caption_clip(p: &Project, c: &Clip) -> bool {
    let Source::Text(t) = &c.source else { return false };
    t.highlight.as_ref().is_some_and(|h| !h.words.is_empty())
        || c.tags.iter().any(|x| x == "caption")
        || p.tracks.iter().any(|tr| tr.id == c.track && (tr.role == "captions" || tr.name.eq_ignore_ascii_case("captions")))
}

fn srt_time(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// The project's captions as SubRip subtitles (timeline times), or `None` when it has none.
pub fn captions_srt(p: &Project, r: &Resolved) -> Option<String> {
    let mut caps: Vec<(f64, f64, &str)> = p
        .clips
        .iter()
        .filter(|c| c.enabled && is_caption_clip(p, c))
        .filter_map(|c| match (&c.source, r.span(&c.id)) {
            (Source::Text(t), Some(s)) if s.end > s.start && !t.text.trim().is_empty() => Some((s.start, s.end, t.text.trim())),
            _ => None,
        })
        .collect();
    if caps.is_empty() {
        return None;
    }
    caps.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out = String::new();
    for (k, (a, b, text)) in caps.iter().enumerate() {
        let body = text.replace("\r\n", "\n").replace('\n', "\r\n");
        out.push_str(&format!("{}\r\n{} --> {}\r\n{}\r\n\r\n", k + 1, srt_time(*a), srt_time(*b), body));
    }
    Some(out)
}

fn kind_name(c: &Clip) -> String {
    match &c.source {
        Source::Media { .. } => "Media".into(),
        Source::Text(t) => format!("Text: {}", crate::util::short(t.text.lines().next().unwrap_or(""), 40)),
        Source::Solid { color } => format!("Solid {}", color.to_hex()),
        Source::Shape(_) => "Shape".into(),
        Source::Adjustment => "Adjustment".into(),
    }
}

/// One OTIO track (`kind` "Video" or "Audio") from the clips of `tr` in time order. `audio_of_video`:
/// the sound of a video track's clips (media with audio that is not muted).
#[allow(clippy::too_many_arguments)]
fn otio_track(p: &Project, r: &Resolved, tr: &Track, kind: &str, name: String, audio_of_video: bool, rate: f64, path_of: &dyn Fn(&MediaAsset) -> PathBuf, rep: &mut OtioReport) -> Option<Value> {
    let mut clips: Vec<(&Clip, f64, f64)> = p.clips.iter().filter(|c| c.track == tr.id).filter_map(|c| r.span(&c.id).map(|s| (c, s.start, s.end))).collect();
    clips.sort_by(|a, b| a.1.total_cmp(&b.1));
    // Captions go to the subtitle file, not the timeline.
    let before = clips.len();
    clips.retain(|(c, _, _)| !is_caption_clip(p, c));
    if !audio_of_video {
        rep.captions += before - clips.len();
    }
    if clips.is_empty() && before > 0 {
        return None;
    }
    let media = |c: &Clip| match &c.source {
        Source::Media { asset, .. } => p.media.iter().find(|m| &m.id == asset),
        _ => None,
    };
    if audio_of_video {
        clips.retain(|(c, _, _)| !c.muted && media(c).is_some_and(|m| m.kind != MediaKind::Image && m.info.has_audio));
        if clips.is_empty() {
            return None;
        }
    }
    let frame = 1.0 / rate;
    let mut children: Vec<Value> = Vec::new();
    let mut at = 0.0f64;
    let mut index_of: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (c, start, end) in &clips {
        if *start > at + frame * 0.5 {
            children.push(gap(start - at, rate));
        }
        let dur = end - start;
        let mut meta = json!({"aiframes": {"clip": c.id}});
        if !c.notes.is_empty() {
            meta["aiframes"]["notes"] = json!(c.notes);
        }
        let mut effects: Vec<Value> = Vec::new();
        let (reference, src_start) = match media(c) {
            Some(m) => {
                let mut src = source_time(c, 0.0).unwrap_or(0.0);
                match &c.speed {
                    Param::Value(v) if *v == 0.0 => effects.push(json!({"OTIO_SCHEMA": "FreezeFrame.1", "name": "", "effect_name": "FreezeFrame", "time_scalar": 0.0, "metadata": {}})),
                    Param::Value(v) if (*v - 1.0).abs() > 1e-9 || c.reverse => {
                        let s = if c.reverse { -*v } else { *v };
                        effects.push(json!({"OTIO_SCHEMA": "LinearTimeWarp.1", "name": "", "effect_name": "LinearTimeWarp", "time_scalar": s, "metadata": {}}));
                        if c.reverse {
                            src = source_time(c, c.duration).unwrap_or(src);
                        }
                    }
                    Param::Value(_) => {}
                    Param::Animated { .. } => {
                        let used = (source_time(c, c.duration).unwrap_or(src) - src).abs();
                        let avg = if dur > 0.0 { used / dur } else { 1.0 };
                        effects.push(json!({"OTIO_SCHEMA": "LinearTimeWarp.1", "name": "", "effect_name": "LinearTimeWarp", "time_scalar": avg, "metadata": {}}));
                        rep.notes.push(format!("{}: speed ramp exported as its average speed ({:.2}×)", c.id, avg));
                    }
                }
                let available = if m.info.duration > 0.0 { range(0.0, m.info.duration, rate) } else { Value::Null };
                (
                    json!({"OTIO_SCHEMA": "ExternalReference.1", "name": m.name, "target_url": file_url(&path_of(m)), "available_range": available, "metadata": {}}),
                    src,
                )
            }
            None => {
                rep.placeholders += 1;
                meta["aiframes"]["kind"] = json!(kind_name(c));
                if let Source::Text(t) = &c.source {
                    meta["aiframes"]["text"] = json!(t.text);
                }
                (json!({"OTIO_SCHEMA": "MissingReference.1", "name": "", "available_range": null, "metadata": {}}), 0.0)
            }
        };
        let name = if c.name.is_empty() { media(c).map(|m| if m.name.is_empty() { Path::new(&m.path).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| m.path.clone()) } else { m.name.clone() }).unwrap_or_else(|| kind_name(c)) } else { c.name.clone() };
        index_of.insert(c.id.as_str(), children.len());
        children.push(json!({
            "OTIO_SCHEMA": "Clip.1",
            "name": name,
            "source_range": range(src_start, dur, rate),
            "media_reference": reference,
            "effects": effects,
            "markers": [],
            "metadata": meta,
            "enabled": c.enabled,
        }));
        rep.clips += 1;
        at = *end;
    }
    // Transitions sit between the two clips they join, centred on the cut.
    let mut inserts: Vec<(usize, Value)> = Vec::new();
    for t in &p.transitions {
        let (Some(&a), Some(&b)) = (index_of.get(t.from.as_str()), index_of.get(t.to.as_str())) else { continue };
        if b != a + 1 {
            continue;
        }
        let half = t.duration / 2.0;
        let ty = if matches!(t.kind.as_str(), "crossfade" | "dissolve" | "cross_dissolve") { "SMPTE_Dissolve" } else { "Custom_Transition" };
        if ty == "Custom_Transition" && !audio_of_video {
            rep.notes.push(format!("{}: \"{}\" transition exported as a custom transition (most editors show a dissolve)", t.id, t.kind));
        }
        inserts.push((b, json!({"OTIO_SCHEMA": "Transition.1", "name": t.kind, "transition_type": ty, "in_offset": rt(half, rate), "out_offset": rt(half, rate), "metadata": {"aiframes": {"transition": t.id, "kind": t.kind}}})));
        // A video clip's sound crossfades too; count the transition once.
        if !audio_of_video {
            rep.transitions += 1;
        }
    }
    inserts.sort_by_key(|x| std::cmp::Reverse(x.0));
    for (i, v) in inserts {
        children.insert(i, v);
    }
    rep.tracks += 1;
    Some(json!({"OTIO_SCHEMA": "Track.1", "name": name, "kind": kind, "children": children, "source_range": null, "effects": [], "markers": [], "metadata": {"aiframes": {"track": tr.id}}, "enabled": !tr.hidden && !tr.muted}))
}

/// The project as an OTIO timeline. `path_of` gives each media file's absolute path.
pub fn to_otio(p: &Project, r: &Resolved, path_of: &dyn Fn(&MediaAsset) -> PathBuf) -> (Value, OtioReport) {
    let rate = p.settings.fps;
    let mut rep = OtioReport::default();
    let mut video = Vec::new();
    let mut audio = Vec::new();
    let mut vn = 0;
    let mut an = 0;
    // Video tracks bottom to top (OTIO stacks the first child lowest), then audio tracks.
    for tr in &p.tracks {
        match tr.kind {
            TrackKind::Video => {
                vn += 1;
                let label = if tr.name.is_empty() { format!("V{vn}") } else { tr.name.clone() };
                if let Some(t) = otio_track(p, r, tr, "Video", label.clone(), false, rate, path_of, &mut rep) {
                    video.push(t);
                }
                if let Some(t) = otio_track(p, r, tr, "Audio", format!("{label} sound"), true, rate, path_of, &mut rep) {
                    audio.push(t);
                }
            }
            TrackKind::Audio => {
                an += 1;
                let label = if tr.name.is_empty() { format!("A{an}") } else { tr.name.clone() };
                if let Some(t) = otio_track(p, r, tr, "Audio", label, false, rate, path_of, &mut rep) {
                    audio.push(t);
                }
            }
        }
    }
    // Video clips' sound tracks counted their clips again; report each clip once.
    rep.clips = p.clips.iter().filter(|c| r.span(&c.id).is_some() && !is_caption_clip(p, c)).count();
    let mut markers = Vec::new();
    for m in &p.markers {
        let Some(t) = r.marker(&m.id) else { continue };
        let name = if m.name.is_empty() { m.id.clone() } else { m.name.clone() };
        let mut meta = json!({"aiframes": {"marker": m.id, "kind": format!("{:?}", m.kind).to_lowercase()}});
        if !m.note.is_empty() {
            meta["aiframes"]["note"] = json!(m.note);
        }
        markers.push(json!({"OTIO_SCHEMA": "Marker.2", "name": name, "marked_range": range(t, 0.0, rate), "color": marker_color(m.kind), "comment": m.note, "metadata": meta}));
        rep.markers += 1;
    }
    let effects_used = p.clips.iter().filter(|c| !c.effects.is_empty()).count();
    if effects_used > 0 {
        rep.notes.push(format!("effects on {effects_used} clip(s) are not exported"));
    }
    if rep.placeholders > 0 {
        rep.notes.push(format!("{} text/shape/solid/adjustment clip(s) exported as offline placeholders named after their content", rep.placeholders));
    }
    let children: Vec<Value> = video.into_iter().chain(audio).collect();
    let name = if p.name.is_empty() { "AIFrames timeline".to_string() } else { p.name.clone() };
    let tl = json!({
        "OTIO_SCHEMA": "Timeline.1",
        "name": name,
        "global_start_time": rt(0.0, rate),
        "tracks": {"OTIO_SCHEMA": "Stack.1", "name": "tracks", "children": children, "source_range": null, "effects": [], "markers": markers, "metadata": {}, "enabled": true},
        "metadata": {"aiframes": {"width": p.settings.width, "height": p.settings.height, "fps": rate}},
    });
    (tl, rep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use crate::{apply, resolve, Op};

    fn op(p: &mut Project, v: Value) {
        apply(p, &Op::from_json(&v).unwrap()).unwrap();
    }

    #[test]
    fn cut_with_gap_transition_marker_and_placeholder() {
        let mut p = Project::default();
        p.settings.fps = 30.0;
        p.tracks = vec![track("v1", TrackKind::Video, "main"), track("v2", TrackKind::Video, ""), track("a1", TrackKind::Audio, "")];
        p.media = vec![asset("mv", "clips/shot a.mp4", MediaKind::Video, 20.0, true), asset("mw", "vo.wav", MediaKind::Audio, 30.0, true)];
        op(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","in_point":2.0,"start":0,"duration":3}}));
        op(&mut p, json!({"op":"add_clip","clip":{"id":"c2","track":"v1","asset":"mv","in_point":10.0,"start":3,"duration":2,"speed":2}}));
        op(&mut p, json!({"op":"add_transition","from":"c1","to":"c2","kind":"crossfade","duration":0.5}));
        op(&mut p, json!({"op":"add_clip","clip":{"id":"t1","track":"v2","text":"Hello there","start":1,"duration":2}}));
        op(&mut p, json!({"op":"add_clip","clip":{"id":"vo","track":"a1","asset":"mw","start":0.5,"duration":4}}));
        op(&mut p, json!({"op":"add_marker","time":2.0,"name":"Hook","kind":"chapter"}));
        let r = resolve(&p);
        let (v, rep) = to_otio(&p, &r, &|m: &MediaAsset| PathBuf::from(format!("C:/proj/{}", m.path)));
        assert_eq!((rep.clips, rep.placeholders, rep.transitions, rep.markers), (4, 1, 1, 1), "{rep:?}");
        let tracks = v["tracks"]["children"].as_array().unwrap();
        let names: Vec<(&str, &str)> = tracks.iter().map(|t| (t["name"].as_str().unwrap(), t["kind"].as_str().unwrap())).collect();
        assert_eq!(names, vec![("V1", "Video"), ("V2", "Video"), ("V1 sound", "Audio"), ("A1", "Audio")]);
        let v1 = tracks[0]["children"].as_array().unwrap();
        assert_eq!(v1.iter().map(|c| c["OTIO_SCHEMA"].as_str().unwrap()).collect::<Vec<_>>(), vec!["Clip.1", "Transition.1", "Clip.1"]);
        assert_eq!(v1[0]["source_range"]["start_time"]["value"], json!(60.0), "in point 2 s at 30 fps");
        assert_eq!(v1[0]["source_range"]["duration"]["value"], json!(90.0));
        assert_eq!(v1[0]["media_reference"]["target_url"], "file:///C:/proj/clips/shot%20a.mp4");
        assert_eq!(v1[1]["in_offset"]["value"], json!(8.0), "half of 0.5 s, rounded to frames");
        assert_eq!(v1[2]["effects"][0]["time_scalar"], json!(2.0));
        // The text clip is a placeholder after a 1 s gap.
        let v2 = tracks[1]["children"].as_array().unwrap();
        assert_eq!(v2[0]["OTIO_SCHEMA"], "Gap.1");
        assert_eq!(v2[1]["media_reference"]["OTIO_SCHEMA"], "MissingReference.1");
        assert_eq!(v2[1]["name"], "Text: Hello there");
        assert_eq!(v2[1]["metadata"]["aiframes"]["text"], "Hello there");
        // The voice starts after a 0.5 s gap.
        let a1 = tracks[3]["children"].as_array().unwrap();
        assert_eq!(a1[0]["source_range"]["duration"]["value"], json!(15.0));
        let m = &v["tracks"]["markers"][0];
        assert_eq!((m["name"].as_str(), m["color"].as_str(), m["marked_range"]["start_time"]["value"].as_f64()), (Some("Hook"), Some("GREEN"), Some(60.0)));
    }

    #[test]
    fn captions_go_to_subtitles_not_the_timeline() {
        let mut p = Project::default();
        p.settings.fps = 30.0;
        p.tracks = vec![track("v1", TrackKind::Video, "main"), track("v2", TrackKind::Video, "captions"), track("a1", TrackKind::Audio, "")];
        p.media = vec![asset("mv", "a.mp4", MediaKind::Video, 20.0, false)];
        op(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0,"duration":5}}));
        op(&mut p, json!({"op":"add_clip","clip":{"id":"cap1","track":"v2","text":"Hello there","start":0.5,"duration":1.2}}));
        op(&mut p, json!({"op":"add_clip","clip":{"id":"cap2","track":"v2","text":"Line one\nline two","start":2,"duration":1}}));
        op(&mut p, json!({"op":"add_clip","clip":{"id":"title","track":"v1","text":"Title","start":5,"duration":1}}));
        let r = resolve(&p);
        let (v, rep) = to_otio(&p, &r, &|m: &MediaAsset| PathBuf::from(&m.path));
        assert_eq!((rep.captions, rep.placeholders, rep.clips), (2, 1, 2), "{rep:?}");
        let names: Vec<&str> = v["tracks"]["children"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["V1", "A1"], "the captions track is left out");
        let srt = captions_srt(&p, &r).unwrap();
        assert_eq!(srt, "1\r\n00:00:00,500 --> 00:00:01,700\r\nHello there\r\n\r\n2\r\n00:00:02,000 --> 00:00:03,000\r\nLine one\r\nline two\r\n\r\n");
        let mut q = p.clone();
        q.clips.retain(|c| !c.id.starts_with("cap"));
        assert!(captions_srt(&q, &resolve(&q)).is_none());
    }

    #[test]
    fn urls_escape_spaces_and_unicode() {
        assert_eq!(file_url(Path::new("C:\\A B\\é.mov")), "file:///C:/A%20B/%C3%A9.mov");
        assert_eq!(file_url(Path::new("/home/x/a.mp4")), "file:///home/x/a.mp4");
    }
}
