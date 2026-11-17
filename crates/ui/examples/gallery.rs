//! Interactive gallery of every aiframes-ui widget on a rich in-code demo project, with a fake
//! `MediaPreview` (generated placeholder thumbnails and synthetic waveforms). Ops emitted by the
//! widgets are applied with `aiframes_core::Document`, so everything is live (undo/redo included).
//!
//! `cargo run -p aiframes-ui --example gallery -- [--select c3,c4] [--playhead 12.5] [--left media|effects|transcript]
//!  [--right inspector|history] [--zoom 40] [--size 1600x950] [--tl 400] [--tall v1,a1] [--lane path] [--inout]
//!  [--apply '[{"op":"set_speed","clip":"c3","speed":2}]'] [--reveal 30,40,v3] [--rw 330] [--spacing v2] [--no-timeline] [--fx-search text]`

use aiframes_core::*;
use aiframes_media::Waveform;
use aiframes_ui::bin::MediaBin;
use aiframes_ui::effects_browser::EffectsBrowser;
use aiframes_ui::history::HistoryPanel;
use aiframes_ui::inspector::Inspector;
use aiframes_ui::timeline::Timeline;
use aiframes_ui::transcript::TranscriptPanel;
use aiframes_ui::{style, EditCtx, MediaPreview, UiAction};
use eframe::egui::{self, Color32, ColorImage, RichText, TextureHandle, TextureId, TextureOptions};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------------------------
// Fake media previews
// ---------------------------------------------------------------------------------------------

struct FakePreview {
    thumbs: HashMap<Id, Vec<TextureHandle>>,
    waves: HashMap<Id, Arc<Waveform>>,
}

impl MediaPreview for FakePreview {
    fn thumbnail(&self, asset: &MediaAsset, t: f64) -> Option<TextureId> {
        let v = self.thumbs.get(&asset.id)?;
        let i = if asset.info.duration > 0.0 { ((t / asset.info.duration) * v.len() as f64).floor().clamp(0.0, v.len() as f64 - 1.0) as usize } else { 0 };
        Some(v[i].id())
    }
    fn waveform(&self, asset: &MediaAsset) -> Option<Arc<Waveform>> {
        self.waves.get(&asset.id).cloned()
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % 10_000) as f32 / 10_000.0
    }
}

fn hsv(h: f32, s: f32, v: f32) -> [u8; 3] {
    let c = egui::ecolor::Hsva::new(h.rem_euclid(1.0), s, v, 1.0);
    let rgb = Color32::from(c);
    [rgb.r(), rgb.g(), rgb.b()]
}

/// A 128×72 placeholder frame: a sky/ground scene whose colours drift with `i`, plus HUD bars for
/// "gameplay" and a centred mark for the logo.
fn frame_image(kind: &str, i: usize, n: usize) -> ColorImage {
    let (w, h) = (128usize, 72usize);
    let mut px = vec![0u8; w * h * 4];
    let f = i as f32 / n.max(1) as f32;
    for y in 0..h {
        for x in 0..w {
            let fy = y as f32 / h as f32;
            let fx = x as f32 / w as f32;
            let rgb = match kind {
                "gameplay" => {
                    if fy < 0.55 {
                        hsv(0.58 + 0.08 * (f * 6.0).sin(), 0.5, 0.45 + 0.4 * (1.0 - fy))
                    } else {
                        let lane = ((fx * 8.0 + f * 20.0).floor() as i32 % 2) == 0;
                        hsv(0.28 + 0.05 * f, 0.55, if lane { 0.42 } else { 0.32 })
                    }
                }
                "broll" => hsv(0.07 + 0.05 * (f * 4.0).cos(), 0.55, 0.25 + 0.65 * (1.0 - (fx - 0.5).abs()) * (1.0 - fy * 0.6)),
                _ => {
                    let d = ((fx - 0.5).powi(2) + (fy - 0.5).powi(2) * 0.3).sqrt();
                    if d < 0.2 {
                        [245, 245, 245]
                    } else {
                        hsv(0.62, 0.7, 0.55)
                    }
                }
            };
            let o = (y * w + x) * 4;
            px[o..o + 3].copy_from_slice(&rgb);
            px[o + 3] = 255;
        }
    }
    // A moving "sun"/subject and a HUD.
    let cx = (0.15 + 0.7 * f) * w as f32;
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f32 - cx).powi(2) + (y as f32 - 20.0).powi(2)).sqrt();
            let o = (y * w + x) * 4;
            if kind != "logo" && d < 7.0 {
                px[o..o + 3].copy_from_slice(&[255, 214, 90]);
            }
            if kind == "gameplay" && (y < 6 && x < (20 + (i * 7) % 90)) {
                px[o..o + 3].copy_from_slice(&[230, 60, 60]);
            }
        }
    }
    ColorImage::from_rgba_unmultiplied([w, h], &px)
}

fn waveform(kind: &str, duration: f64, words: &[Word], seed: u64) -> Waveform {
    let bps = 200.0;
    let n = (duration * bps) as usize;
    let mut rng = Rng(seed | 1);
    let mut min = Vec::with_capacity(n);
    let mut max = Vec::with_capacity(n);
    let mut rms = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / bps;
        let a = match kind {
            "voice" => {
                let speaking = words.iter().any(|w| t >= w.start && t <= w.end);
                if speaking {
                    0.25 + 0.55 * ((t * 23.0).sin().abs() as f32) * (0.6 + 0.4 * rng.next())
                } else {
                    0.015 * rng.next()
                }
            }
            "music" => {
                let beat = ((t * 2.0).fract() < 0.12) as i32 as f32;
                0.28 + 0.18 * beat + 0.12 * ((t * 0.3).sin() as f32) + 0.08 * rng.next()
            }
            "whoosh" => (1.0 - (t / duration.max(0.01)) as f32).max(0.0) * 0.9 * (0.5 + 0.5 * rng.next()),
            _ => {
                let burst = ((t * 0.7).sin() > 0.6) as i32 as f32;
                0.12 + 0.25 * rng.next() + 0.3 * burst
            }
        };
        max.push(a.min(1.0));
        min.push(-(a * (0.85 + 0.15 * rng.next())).min(1.0));
        rms.push(a * 0.6);
    }
    Waveform { bins_per_sec: bps, duration, min, max, rms }
}

// ---------------------------------------------------------------------------------------------
// Demo project
// ---------------------------------------------------------------------------------------------

const SCRIPT: &str = "Round forty was the hardest fight of the whole run. We had three lives left and the boss was already at full speed. \
Watch the left lane here, because this is where everything went wrong. I placed two towers too late and the shield broke. \
So we sped things up. Ten rounds of farming in about six seconds, then back to real time for the boss. \
And this is the moment. Three hits, one life left, and the victory screen. That was round forty.";

fn transcript() -> Transcript {
    let mut words = Vec::new();
    let mut t = 0.4;
    let mut sentence = 0;
    for (k, raw) in SCRIPT.split_whitespace().enumerate() {
        let len = 0.18 + 0.035 * raw.trim_matches(|c: char| !c.is_alphanumeric()).len() as f64;
        let conf = if k % 23 == 7 { 0.45 } else { 1.0 };
        words.push(Word { text: format!(" {raw}"), start: t, end: t + len, conf });
        t += len + 0.08;
        if raw.ends_with('.') {
            sentence += 1;
            t += if sentence % 2 == 0 { 1.6 } else { 0.55 };
        }
    }
    Transcript { language: "en".into(), words, ..Default::default() }
}

#[allow(clippy::too_many_arguments)]
fn media_op(id: &str, path: &str, kind: MediaKind, duration: f64, w: u32, h: u32, fps: f64, audio: bool, name: &str, notes: &str) -> Op {
    Op::AddMedia {
        id: Some(id.into()),
        path: path.into(),
        kind,
        info: MediaInfo {
            duration,
            width: w,
            height: h,
            fps,
            has_video: kind != MediaKind::Audio,
            has_audio: audio,
            audio_channels: if audio { 2 } else { 0 },
            audio_rate: if audio { 48_000 } else { 0 },
            codec: if kind == MediaKind::Video { "h264".into() } else if kind == MediaKind::Audio { "aac".into() } else { "png".into() },
            rotation: 0,
            ..Default::default()
        },
        name: Some(name.into()),
        notes: Some(notes.into()),
        tags: vec![],
    }
}

fn track(id: &str, kind: TrackKind, role: &str, name: &str) -> Op {
    Op::AddTrack { id: Some(id.into()), kind, name: Some(name.into()), role: Some(role.into()), index: None }
}

fn add(clip: serde_json::Value) -> Op {
    Op::AddClip { clip, overwrite: false }
}

fn word_index(tr: &Transcript, needle: &str, nth: usize) -> usize {
    tr.words.iter().enumerate().filter(|(_, w)| w.text.trim().trim_matches(|c: char| !c.is_alphanumeric()).eq_ignore_ascii_case(needle)).nth(nth).map(|(i, _)| i).unwrap_or(0)
}

fn demo() -> (Document, Vec<Id>) {
    let tr = transcript();
    let forty = word_index(&tr, "forty", 0);
    let moment = word_index(&tr, "moment", 0);
    let mut ops = vec![
        Op::SetProject { name: Some("Round 40 recap".into()), notes: Some("Goal: a 75 s recap of the round 40 boss fight for YouTube.\nStyle: plain overlays (no glow), yellow highlights, fast-forward the farming rounds.\nTodo: fix the audio pop at 20 s.".into()) },
        track("v1", TrackKind::Video, "main", "Main"),
        track("v2", TrackKind::Video, "overlay", "B-roll"),
        track("v3", TrackKind::Video, "overlay", "Titles"),
        track("v4", TrackKind::Video, "captions", "Captions"),
        track("a1", TrackKind::Audio, "voice", "Voice-over"),
        track("a2", TrackKind::Audio, "music", "Music"),
        track("a3", TrackKind::Audio, "sfx", "SFX"),
        media_op("gameplay", "assets/gameplay_round40.mp4", MediaKind::Video, 180.0, 1920, 1080, 60.0, true, "gameplay_round40.mp4", "full run, round 38 to the win"),
        media_op("broll", "assets/broll_desk.mp4", MediaKind::Video, 40.0, 1920, 1080, 30.0, false, "broll_desk.mp4", "hands on the keyboard"),
        media_op("logo", "assets/channel_logo.png", MediaKind::Image, 0.0, 1024, 1024, 0.0, false, "channel_logo.png", ""),
        media_op("vo", "assets/voiceover.wav", MediaKind::Audio, 75.0, 0, 0, 0.0, true, "voiceover.wav", "the script, take 3"),
        media_op("music", "assets/music_bed.mp3", MediaKind::Audio, 150.0, 0, 0, 0.0, true, "music_bed.mp3", "calm synth bed"),
        media_op("whoosh", "assets/whoosh.wav", MediaKind::Audio, 1.2, 0, 0, 0.0, true, "whoosh.wav", ""),
        Op::SetTranscript { asset: "vo".into(), transcript: Some(Some(tr.clone())), force: false },
        // Main storyline: a magnetic chain.
        add(json!({"id": "c1", "track": "v1", "name": "Lobby", "asset": "gameplay", "in_point": 12.0, "duration": 8.0, "start": 0.0, "fade_in": 0.6})),
        add(json!({"id": "c2", "track": "v1", "name": "Round 38", "asset": "gameplay", "in_point": 40.0, "duration": 10.0, "start": {"after": "c1"}})),
        add(json!({"id": "c3", "track": "v1", "name": "Farming rounds", "asset": "gameplay", "in_point": 60.0, "duration": 24.0, "start": {"after": "c2"}})),
        Op::SetSpeed { clip: "c3".into(), speed: 4.0, keep: SpeedKeep::Source, linked: true },
        add(json!({"id": "c4", "track": "v1", "name": "Round 40 boss", "asset": "gameplay", "in_point": 110.0, "duration": 16.0, "start": {"after": "c3"}, "fade_out": 1.0})),
        add(json!({"id": "c5", "track": "v1", "name": "Victory", "asset": "gameplay", "in_point": 150.0, "duration": 9.0, "start": {"after": "c4"}})),
        Op::AddTransition { from: "c1".into(), to: "c2".into(), kind: "crossfade".into(), duration: Some(0.5), id: Some("tr1".into()), params: Default::default() },
        Op::AddTransition { from: "c3".into(), to: "c4".into(), kind: "whip".into(), duration: Some(0.3), id: Some("tr2".into()), params: Default::default() },
        // B-roll picture-in-picture with a colour effect.
        add(json!({"id": "b1", "track": "v2", "name": "Desk b-roll", "asset": "broll", "in_point": 2.0, "duration": 5.0, "start": 9.0, "fade_in": 0.4, "fade_out": 0.4,
                   "transform": {"scale": 0.38, "x": 560.0, "y": -250.0}, "effects": [{"kind": "color", "params": {"saturation": 1.3, "contrast": 0.1}}, {"kind": "drop_shadow"}]})),
        add(json!({"id": "logo1", "track": "v2", "name": "Logo", "asset": "logo", "duration": 3.0, "start": 63.0, "transform": {"scale": 0.3}})),
        // Voice, music, sfx.
        add(json!({"id": "vo1", "track": "a1", "name": "Voice-over", "asset": "vo", "start": 1.0, "duration": 70.0})),
        add(json!({"id": "mu1", "track": "a2", "name": "Music bed", "asset": "music", "start": 0.0, "duration": 64.0, "fade_in": 2.0,
                   "volume": {"keys": [{"t": 0.0, "v": 0.7}, {"t": 50.0, "v": 0.5}, {"t": 64.0, "v": 0.0}]}})),
        add(json!({"id": "sfx1", "track": "a3", "name": "Whoosh", "asset": "whoosh", "duration": 1.2, "start": {"before": "c4", "offset": 0.4}})),
        Op::AddDucking { target: "a2".into(), trigger: "a1".into(), amount_db: Some(-12.0), threshold_db: None, attack: None, release: None, id: Some("d1".into()) },
        // Titles anchored to the voice and to the story.
        add(json!({"id": "t1", "track": "v3", "name": "ROUND 40", "start": {"word": {"clip": "vo1", "index": forty}, "offset": -0.1}, "duration": 2.0,
                   "source": {"type": "text", "text": "ROUND 40", "size": 110.0, "weight": 800, "reveal": {"kind": "pop_words", "duration": 0.25}}})),
        add(json!({"id": "t2", "track": "v3", "name": "Lower third", "start": 5.0, "duration": 4.0, "transform": {"x": -520.0, "y": 330.0},
                   "source": {"type": "text", "text": "My Channel\nround 40, hard mode", "size": 46.0, "weight": 600, "align": "left", "background": {"color": "#000000B3", "padding": 18.0, "radius": 4.0}}})),
        add(json!({"id": "t3", "track": "v3", "name": "4× speed", "start": {"with": "c3", "offset": 0.3}, "duration": 4.5, "until": {"after": "c3"}, "transform": {"x": 640.0, "y": -400.0},
                   "source": {"type": "text", "text": "4× speed", "size": 40.0, "weight": 700, "background": {"color": "#000000CC", "padding": 12.0, "radius": 18.0}}})),
        add(json!({"id": "s1", "track": "v3", "name": "Look here", "start": {"word": {"clip": "vo1", "index": moment}}, "duration": 3.0,
                   "source": {"type": "shape", "kind": "ellipse", "width": 320.0, "height": 320.0, "stroke": {"color": "#FFD60A", "width": 8.0}, "draw": {"keys": [{"t": 0.0, "v": 0.0, "ease": "out"}, {"t": 0.35, "v": 1.0}]}}})),
        add(json!({"id": "adj1", "track": "v2", "name": "Victory grade", "start": {"with": "c5"}, "duration": 4.0, "source": {"type": "adjustment"},
                   "effects": [{"kind": "vignette", "params": {"amount": 0.6}}, {"kind": "color", "params": {"exposure": 0.3}}]})),
        // Markers.
        Op::AddMarker { time: Time::At(0.0), name: Some("Intro".into()), kind: Some(MarkerKind::Chapter), id: Some("mk1".into()), color: None, note: None },
        Op::AddMarker { time: Time::Anchored(Anchor { to: AnchorTarget::With("c4".into()), offset: 0.0 }), name: Some("The boss".into()), kind: Some(MarkerKind::Chapter), id: Some("mk2".into()), color: None, note: None },
        Op::AddMarker { time: Time::At(20.0), name: Some("Audio pop".into()), kind: Some(MarkerKind::Todo), id: Some("mk3".into()), color: None, note: Some("click in the VO around here".into()) },
        Op::AddMarker { time: Time::Anchored(Anchor { to: AnchorTarget::With("c5".into()), offset: 0.0 }), name: Some("Victory".into()), kind: Some(MarkerKind::Chapter), id: Some("mk4".into()), color: None, note: None },
    ];
    // Round 6 look tools: the desk b-roll seen through giant "DESK" letters (track matte), turning in
    // 3-D with motion blur; the 4x farming rounds frame-blended; a film burn into the victory.
    ops.extend([
        add(json!({"id": "m1", "track": "v3", "name": "DESK (matte)", "start": 9.0, "duration": 5.0, "source": {"type": "text", "text": "DESK", "size": 320.0, "weight": 900}})),
        Op::SetClip { clip: "b1".into(), patch: json!({"matte": {"clip": "m1", "mode": "alpha"}, "motion_blur": {"shutter": 180, "samples": 12},
            "transform": {"rotation_y": {"keys": [{"t": 0.0, "v": -60.0, "ease": "out"}, {"t": 0.8, "v": 0.0}]}, "perspective": 1400.0}}) },
        Op::SetClip { clip: "c3".into(), patch: json!({"frame_blend": true}) },
        Op::AddTransition { from: "c4".into(), to: "c5".into(), kind: "film_burn".into(), duration: Some(0.8), id: Some("tr3".into()), params: Default::default() },
    ]);
    for (i, t) in [36.0, 36.5, 37.0, 37.5, 38.0].iter().enumerate() {
        ops.push(Op::AddMarker { time: Time::At(*t), name: None, kind: Some(MarkerKind::Beat), id: Some(format!("beat{i}")), color: None, note: None });
    }
    let mut p = Project::default();
    if let Err(e) = apply_batch(&mut p, &ops) {
        panic!("demo project: {e}");
    }
    // Detached audio of the boss fight onto the SFX track would overlap the whoosh; use a new track.
    apply_batch(&mut p, &[Op::AddTrack { id: Some("a4".into()), kind: TrackKind::Audio, name: Some("Game audio".into()), role: Some("sfx".into()), index: None }, Op::DetachAudio { clip: "c4".into(), track: "a4".into(), stream: None, new_id: Some("c4a".into()) }]).expect("detach");
    // Captions (word-anchored karaoke) as the AI would make them.
    let mut doc = Document::new(p);
    doc.apply("ai", "Generate captions", vec![Op::GenerateCaptions { clips: vec!["vo1".into()], track: "v4".into(), style: None, max_words: Some(4), max_chars: None, pause: None, replace: false }]).expect("captions");
    // A person recolours one caption and deletes another (regenerating keeps both decisions).
    let caps: Vec<Id> = {
        let r = doc.resolved();
        let mut v: Vec<(f64, Id)> = doc.project.clips.iter().filter(|c| c.track == "v4").filter_map(|c| Some((r.span(&c.id)?.start, c.id.clone()))).collect();
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        v.into_iter().map(|(_, id)| id).collect()
    };
    if caps.len() > 6 {
        doc.apply("user", "Caption colour", vec![Op::SetClip { clip: caps[2].clone(), patch: json!({"source": {"color": "#FFD60A"}}) }]).expect("recolour");
        doc.apply("user", "Delete caption", vec![Op::DeleteClip { clip: caps[5].clone(), ripple: false, linked: true }]).expect("delete caption");
    }
    doc.apply("user", "Trim the lobby", vec![Op::TrimClip { clip: "c1".into(), edge: Edge::End, to: None, delta: Some(-0.5), ripple: None }]).expect("trim");
    let r = doc.apply("ai", "Punch-in on the boss", vec![
        Op::AddKeyframe { clip: "c4".into(), path: "transform.scale".into(), t: 0.0, v: 1.0, ease: Some(Ease::InOut) },
        Op::AddKeyframe { clip: "c4".into(), path: "transform.scale".into(), t: 2.0, v: 1.18, ease: None },
        Op::AddEffect { clip: "c4".into(), effect: EffectArg::Kind("shake".into()), index: None },
    ]).expect("punch");
    let ai: Vec<Id> = r.outcomes.iter().flat_map(|o| o.changed.iter().chain(o.created.iter()).cloned()).collect();
    doc.apply("user", "Rename chapter", vec![Op::SetMarker { marker: "mk1".into(), time: None, name: Some("Intro".into()), kind: None, color: None, note: Some("YouTube chapter 1".into()) }]).expect("marker");
    // A broken anchor (as left by a hand-edited file) to show the error state.
    let mut broken: Clip = serde_json::from_value(json!({"id": "x1", "track": "v3", "name": "Old callout", "start": {"after": "deleted_clip"}, "duration": 2.0, "source": {"type": "text", "text": "old"}})).unwrap();
    broken.notes = "anchored to a clip that no longer exists".into();
    doc.project.clips.push(broken);
    doc.refresh();
    (doc, ai)
}

// ---------------------------------------------------------------------------------------------
// The app
// ---------------------------------------------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum Left {
    Media,
    Effects,
    Transcript,
}

#[derive(PartialEq, Clone, Copy)]
enum Right {
    Inspector,
    History,
}

struct Gallery {
    doc: Document,
    preview: FakePreview,
    selection: Vec<Id>,
    playhead: f64,
    playing: bool,
    in_point: Option<f64>,
    out_point: Option<f64>,
    ai_highlight: Vec<Id>,
    ai_since: f64,
    status: String,
    applied: Vec<u64>,
    undone: Vec<u64>,
    timeline: Timeline,
    inspector: Inspector,
    effects: EffectsBrowser,
    transcript: TranscriptPanel,
    history: HistoryPanel,
    bin: MediaBin,
    left: Left,
    right: Right,
    last_time: f64,
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

impl Gallery {
    fn new(cc: &eframe::CreationContext) -> Self {
        style::apply_dark(&cc.egui_ctx);
        let (doc, ai) = demo();
        let mut thumbs = HashMap::new();
        for (id, kind, n) in [("gameplay", "gameplay", 36), ("broll", "broll", 12), ("logo", "logo", 1)] {
            let v: Vec<TextureHandle> = (0..n).map(|i| cc.egui_ctx.load_texture(format!("{id}-{i}"), frame_image(kind, i, n), TextureOptions::LINEAR)).collect();
            thumbs.insert(id.to_string(), v);
        }
        let words = doc.project.transcripts.get("vo").map(|t| t.words.clone()).unwrap_or_default();
        let mut waves = HashMap::new();
        waves.insert("vo".to_string(), Arc::new(waveform("voice", 75.0, &words, 7)));
        waves.insert("music".to_string(), Arc::new(waveform("music", 150.0, &[], 11)));
        waves.insert("whoosh".to_string(), Arc::new(waveform("whoosh", 1.2, &[], 3)));
        waves.insert("gameplay".to_string(), Arc::new(waveform("game", 180.0, &[], 5)));
        let mut g = Gallery {
            doc,
            preview: FakePreview { thumbs, waves },
            selection: vec![],
            playhead: 12.5,
            playing: false,
            in_point: None,
            out_point: None,
            ai_highlight: ai,
            ai_since: 0.0,
            status: "Ready — every edit below is an op in the document's history.".into(),
            applied: vec![],
            undone: vec![],
            timeline: Timeline::default(),
            inspector: Inspector::default(),
            effects: EffectsBrowser::default(),
            transcript: TranscriptPanel::default(),
            history: HistoryPanel::default(),
            bin: MediaBin::default(),
            left: Left::Media,
            right: Right::Inspector,
            last_time: 0.0,
        };
        g.applied = g.doc.history().iter().filter(|e| e.kind == HistoryKind::Edit).map(|e| e.revision).collect();
        if let Some(s) = arg("--select") {
            g.selection = s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
        }
        if let Some(t) = arg("--playhead").and_then(|s| s.parse().ok()) {
            g.playhead = t;
        }
        if let Some(z) = arg("--zoom").and_then(|s| s.parse().ok()) {
            g.timeline.zoom = z;
            g.timeline.fit_all();
        }
        match arg("--left").as_deref() {
            Some("effects") => g.left = Left::Effects,
            Some("transcript") => g.left = Left::Transcript,
            _ => {}
        }
        if arg("--right").as_deref() == Some("history") {
            g.right = Right::History;
        }
        if let Some(q) = arg("--fx-search") {
            g.effects.search = q;
        }
        if let Some(t) = arg("--tall") {
            for id in t.split(',') {
                g.timeline.track_heights.insert(id.trim().to_string(), 130.0);
            }
        }
        if let Some(path) = arg("--lane") {
            if let Some(clip) = g.selection.first().cloned() {
                g.timeline.show_lane(&clip, &path);
            }
        }
        // Ops to apply as the person at startup (for scripted before/after screenshots).
        if let Some(json) = arg("--apply") {
            match serde_json::from_str::<Vec<Op>>(&json) {
                Ok(ops) => match g.doc.apply("user", "--apply", ops) {
                    Ok(r) => {
                        g.applied.push(r.revision);
                        g.status = r.outcomes.iter().map(|o| o.summary.clone()).collect::<Vec<_>>().join("; ");
                    }
                    Err(e) => g.status = format!("--apply refused: {e}"),
                },
                Err(e) => g.status = format!("--apply: bad ops JSON: {e}"),
            }
        }
        // "--reveal 30,40[,v3,a2]": a time range and optional tracks to bring into view.
        if let Some(r) = arg("--reveal") {
            let parts: Vec<&str> = r.split(',').collect();
            if let (Some(a), Some(b)) = (parts.first().and_then(|x| x.parse().ok()), parts.get(1).and_then(|x| x.parse().ok())) {
                let tracks: Vec<Id> = parts.iter().skip(2).map(|x| x.trim().to_string()).collect();
                g.timeline.reveal(a, b, &tracks);
            }
        }
        // "--spacing [track]" opens the timeline's Set spacing dialog (as the app's menu does).
        if std::env::args().any(|a| a == "--spacing") {
            g.timeline.open_spacing(arg("--spacing").filter(|t| !t.starts_with("--")));
        }
        if std::env::args().any(|a| a == "--inout") {
            g.in_point = Some(9.0);
            g.out_point = Some(21.0);
        }
        g
    }

    fn handle(&mut self, actions: Vec<UiAction>) {
        for a in actions {
            match a {
                UiAction::Seek(t) => self.playhead = t.max(0.0),
                UiAction::Select(ids) => self.selection = ids,
                UiAction::Apply { label, ops } => match self.doc.apply("user", &label, ops) {
                    Ok(r) => {
                        self.applied.push(r.revision);
                        self.undone.clear();
                        let moved = r.moved.len();
                        self.status = format!("{label}{}{}", if moved > 0 { format!(" — {moved} clip(s) moved") } else { String::new() }, if r.warnings.is_empty() { String::new() } else { format!(" — {}", r.warnings.join("; ")) });
                        let live: Vec<Id> = self.selection.iter().filter(|id| exists(&self.doc.project, id)).cloned().collect();
                        self.selection = live;
                    }
                    Err(e) => self.status = format!("Refused: {e}"),
                },
                UiAction::Undo => self.undo(),
                UiAction::Redo => self.redo(),
                UiAction::RestoreRevision(rev) => {
                    if self.applied.contains(&rev) {
                        while self.applied.last().is_some_and(|r| *r != rev) {
                            self.undo();
                        }
                    } else if self.undone.contains(&rev) {
                        while !self.applied.contains(&rev) && !self.undone.is_empty() {
                            self.redo();
                        }
                    }
                    self.status = format!("Restored to after revision {rev}");
                }
                UiAction::TogglePlay => self.playing = !self.playing,
                UiAction::SetIn(t) => self.in_point = t,
                UiAction::SetOut(t) => self.out_point = t,
                UiAction::Import(_) => self.status = "Import: the app shell opens a file dialog here.".into(),
                UiAction::Transcribe(id) | UiAction::TranscribeStream(id, _) => {
                    self.status = format!("Transcribe {id}: the app runs Whisper and applies set_transcript.");
                }
                UiAction::Status(s) => self.status = s,
                UiAction::AskAi { prompt, ids } => self.status = format!("AI prompt pre-filled: “{prompt}” ({})", ids.join(", ")),
                UiAction::RevealMedia(id) => self.status = format!("Reveal {id} in Explorer (app shell)."),
                UiAction::RelinkMedia(id) => self.status = format!("Relink {id}: file dialog (app shell)."),
                UiAction::Preview(_) => {}
                UiAction::PickEffectFile { clip, index, name } => self.status = format!("Pick a file for {name} of effect {index} on {clip} (app shell)."),
                UiAction::OpenSource(id) => self.status = format!("Open {id} in the Source monitor (app shell)."),
            }
        }
    }

    fn undo(&mut self) {
        if let Some(r) = self.doc.undo() {
            if let Some(rev) = self.applied.pop() {
                self.undone.push(rev);
            }
            self.status = r.label;
        }
    }

    fn redo(&mut self) {
        if let Some(r) = self.doc.redo() {
            if let Some(rev) = self.undone.pop() {
                self.applied.push(rev);
            }
            self.status = r.label;
        }
    }

    fn simulate_ai(&mut self, now: f64) {
        let ops = vec![
            Op::AddEffect { clip: "c2".into(), effect: EffectArg::Kind("color".into()), index: None },
            Op::Nudge { clips: vec!["b1".into()], delta: 0.5 },
            Op::AddMarker { time: Time::At(self.playhead), name: Some("AI: check pacing".into()), kind: Some(MarkerKind::Todo), id: None, color: None, note: None },
        ];
        match self.doc.apply("ai", "Warm up round 38, retime b-roll", ops) {
            Ok(r) => {
                self.applied.push(r.revision);
                self.ai_highlight = r.outcomes.iter().flat_map(|o| o.changed.iter().chain(o.created.iter()).cloned()).collect();
                self.ai_since = now;
                self.status = "The AI edited 3 things (glowing).".into();
            }
            Err(e) => self.status = format!("AI edit refused: {e}"),
        }
    }
}

fn exists(p: &Project, id: &str) -> bool {
    p.clips.iter().any(|c| c.id == id) || p.markers.iter().any(|m| m.id == id) || p.transitions.iter().any(|t| t.id == id) || p.tracks.iter().any(|t| t.id == id) || p.media.iter().any(|m| m.id == id)
}

impl eframe::App for Gallery {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let now = ctx.input(|i| i.time);
        let dt = (now - self.last_time).clamp(0.0, 0.1);
        self.last_time = now;
        if self.playing {
            self.playhead += dt;
            if self.playhead > self.doc.resolved().duration {
                self.playhead = 0.0;
            }
            ctx.request_repaint();
        }
        let flash = (1.0 - (now - self.ai_since) / 2.5).clamp(0.0, 1.0) as f32;
        if flash > 0.0 {
            ctx.request_repaint();
        }
        // Global undo/redo.
        let (undo, redo) = ctx.input_mut(|i| (i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z), i.consume_key(egui::Modifiers::COMMAND, egui::Key::Y)));
        if undo {
            self.undo();
        }
        if redo {
            self.redo();
        }

        let history = self.doc.history().to_vec();
        let mut actions = Vec::new();
        {
            let project = &self.doc.project;
            let resolved = self.doc.resolved();
            let cx = EditCtx {
                project,
                resolved,
                playhead: self.playhead,
                playing: self.playing,
                selection: &self.selection,
                preview: &self.preview,
                ai_highlight: &self.ai_highlight,
                ai_flash: flash,
                history: &history,
                revision: self.doc.revision(),
                can_undo: self.doc.can_undo(),
                can_redo: self.doc.can_redo(),
                in_point: self.in_point,
                out_point: self.out_point,
            };
            egui::Panel::top("gallery_top").exact_size(30.0).show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(RichText::new("AIFrames").strong().color(style::ACCENT));
                    ui.label(RichText::new(format!("{}  ·  widget gallery", project.name)).weak());
                    ui.separator();
                    if ui.add_enabled(cx.can_undo, egui::Button::new("Undo")).on_hover_text("Undo (Ctrl+Z)").clicked() {
                        actions.push(UiAction::Undo);
                    }
                    if ui.add_enabled(cx.can_redo, egui::Button::new("Redo")).on_hover_text("Redo (Ctrl+Y)").clicked() {
                        actions.push(UiAction::Redo);
                    }
                    if ui.button(if self.playing { "Pause" } else { "Play" }).clicked() {
                        actions.push(UiAction::TogglePlay);
                    }
                    if ui.button(RichText::new("Simulate AI edit").color(style::AI)).clicked() {
                        actions.push(UiAction::Status("__ai__".into()));
                    }
                });
            });
            egui::Panel::bottom("gallery_status").exact_size(22.0).show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(RichText::new(aiframes_ui::util::plain(&self.status)).small());
                });
            });
            if !std::env::args().any(|a| a == "--no-timeline") {
            egui::Panel::bottom("gallery_timeline").resizable(true).default_size(arg("--tl").and_then(|s| s.parse().ok()).unwrap_or(400.0)).size_range(200.0..=700.0).frame(egui::Frame::NONE.fill(style::rgb(0x15, 0x16, 0x18))).show(ui, |ui| {
                actions.extend(self.timeline.ui(ui, &cx));
            });
            }
            egui::Panel::left("gallery_left").resizable(true).default_size(330.0).size_range(240.0..=520.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.left, Left::Media, "Media");
                    ui.selectable_value(&mut self.left, Left::Effects, "Effects");
                    ui.selectable_value(&mut self.left, Left::Transcript, "Transcript");
                });
                ui.separator();
                actions.extend(match self.left {
                    Left::Media => self.bin.ui(ui, &cx),
                    Left::Effects => self.effects.ui(ui, &cx),
                    Left::Transcript => self.transcript.ui(ui, &cx),
                });
            });
            egui::Panel::right("gallery_right").resizable(true).default_size(arg("--rw").and_then(|s| s.parse().ok()).unwrap_or(360.0)).size_range(260.0..=560.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.right, Right::Inspector, "Inspector");
                    ui.selectable_value(&mut self.right, Right::History, "History");
                });
                ui.separator();
                actions.extend(match self.right {
                    Right::Inspector => self.inspector.ui(ui, &cx),
                    Right::History => self.history.ui(ui, &cx),
                });
            });
            egui::CentralPanel::default().show(ui, |ui| viewer(ui, &cx));
        }
        if actions.iter().any(|a| matches!(a, UiAction::Status(s) if s == "__ai__")) {
            actions.retain(|a| !matches!(a, UiAction::Status(s) if s == "__ai__"));
            self.simulate_ai(now);
        }
        self.handle(actions);
    }
}

/// A stand-in program monitor: the topmost picture clip's thumbnail, text clips, safe areas.
fn viewer(ui: &mut egui::Ui, cx: &EditCtx) {
    let p = cx.project;
    let avail = ui.available_rect_before_wrap().shrink(8.0);
    let aspect = p.settings.width as f32 / p.settings.height as f32;
    let mut size = egui::vec2(avail.width(), avail.width() / aspect);
    if size.y > avail.height() - 24.0 {
        size = egui::vec2((avail.height() - 24.0) * aspect, avail.height() - 24.0);
    }
    let frame = egui::Rect::from_center_size(egui::pos2(avail.center().x, avail.top() + size.y * 0.5), size);
    let painter = ui.painter_at(avail);
    painter.rect_filled(frame, 0.0, Color32::BLACK);
    let k = frame.width() / p.settings.width as f32;
    for c in cx.resolved.clips_at(p, cx.playhead) {
        let Some(tr) = p.tracks.iter().find(|t| t.id == c.track) else { continue };
        if tr.kind != TrackKind::Video || tr.hidden || !c.enabled {
            continue;
        }
        let start = cx.resolved.span(&c.id).map(|s| s.start).unwrap_or(0.0);
        let local = cx.playhead - start;
        let op = eval::visual_opacity(c, local) as f32;
        let sc = eval::sample(&c.transform.scale, local) as f32;
        let center = frame.center() + egui::vec2(eval::sample(&c.transform.x, local) as f32 * k, eval::sample(&c.transform.y, local) as f32 * k);
        match &c.source {
            Source::Media { asset, .. } => {
                let Some(a) = p.media.iter().find(|m| &m.id == asset) else { continue };
                let src = eval::source_time(c, local).unwrap_or(0.0);
                if let Some(tex) = cx.preview.thumbnail(a, src) {
                    let ar = if a.info.height > 0 { a.info.width as f32 / a.info.height as f32 } else { aspect };
                    let mut s = egui::vec2(frame.width(), frame.width() / ar);
                    if s.y > frame.height() {
                        s = egui::vec2(frame.height() * ar, frame.height());
                    }
                    let r = egui::Rect::from_center_size(center, s * sc);
                    painter.with_clip_rect(frame).image(tex, r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::from_white_alpha((op * 255.0) as u8));
                }
            }
            Source::Text(t) => {
                let col = style::color32(t.color).gamma_multiply(op);
                let g = painter.layout(t.text.clone(), egui::FontId::proportional((t.size as f32 * k * sc).max(6.0)), col, t.max_width.map(|w| w as f32 * k).unwrap_or(frame.width()));
                let r = egui::Rect::from_center_size(center, g.size());
                if let Some(b) = &t.background {
                    painter.rect_filled(r.expand(b.padding as f32 * k), (b.radius as f32 * k) as u8, style::color32(b.color).gamma_multiply(op));
                }
                painter.galley(r.min, g, col);
            }
            Source::Shape(s) => {
                let r = egui::Rect::from_center_size(center, egui::vec2(s.width as f32, s.height as f32) * k * sc);
                let st = s.stroke.as_ref().map(|x| egui::Stroke::new(x.width as f32 * k, style::color32(x.color).gamma_multiply(op))).unwrap_or(egui::Stroke::NONE);
                match s.kind {
                    ShapeKind::Ellipse => {
                        painter.circle_stroke(r.center(), r.width() * 0.5, st);
                    }
                    _ => {
                        painter.rect_stroke(r, 4.0, st, egui::StrokeKind::Middle);
                    }
                }
            }
            Source::Solid { color } => {
                painter.rect_filled(frame, 0.0, style::color32(*color).gamma_multiply(op));
            }
            Source::Adjustment => {}
        }
    }
    // Safe areas.
    let ts = p.settings.title_safe as f32;
    let a_s = p.settings.action_safe as f32;
    for f in [ts, a_s] {
        painter.rect_stroke(egui::Rect::from_center_size(frame.center(), frame.size() * f), 0.0, egui::Stroke::new(1.0, Color32::from_white_alpha(28)), egui::StrokeKind::Middle);
    }
    painter.text(
        egui::pos2(frame.left(), frame.bottom() + 6.0),
        egui::Align2::LEFT_TOP,
        format!("{}   ·   {}×{} @ {} fps   ·   program monitor (stand-in)", aiframes_ui::util::timecode(cx.playhead, p.settings.fps), p.settings.width, p.settings.height, p.settings.fps),
        egui::FontId::monospace(11.0),
        ui.visuals().weak_text_color(),
    );
}

fn main() -> eframe::Result {
    let size = arg("--size").and_then(|s| s.split_once('x').and_then(|(w, h)| Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?)))).unwrap_or((1600.0, 1000.0));
    let options = eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_inner_size([size.0, size.1]).with_title("AIFrames widget gallery"), ..Default::default() };
    eframe::run_native("AIFrames widget gallery", options, Box::new(|cc| Ok(Box::new(Gallery::new(cc)))))
}
