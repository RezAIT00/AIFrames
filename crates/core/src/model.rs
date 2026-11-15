//! The AIFrames project model: the single source of truth for an edit.
//!
//! Design rules (they are why AIFrames exists; see `ARCHITECTURE.md`):
//! * **One clock per fact.** A clip's position is stored once (`Clip::start`). Everything that belongs
//!   to a clip — keyframes, effects, fades, text reveals, caption word timings — is in *clip-local*
//!   seconds, so moving or retiming a clip carries all of it along. Nothing is ever written twice.
//! * **Intent over coordinates.** `Time` can be absolute or an anchor (after / with / before a clip, at a
//!   marker, at a spoken word). Chains of `after` anchors make a magnetic storyline: trim one clip and
//!   everything after it follows.
//! * **Loud failures.** An anchor that cannot be resolved is an error reported by `resolve`, never a
//!   silent 0.
//! * **Compact, stable JSON.** Defaults are omitted, a constant `Param` is a bare number, ids are
//!   short and stable, so an AI can read and diff a project file.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

/// Stable, human-readable identifier (`"v1"`, `"c12"`, `"intro_title"`). Never re-stamped.
pub type Id = String;

/// Current project file format version.
pub const FORMAT_VERSION: u32 = 1;

/// Project file extension (without the dot).
pub const PROJECT_EXT: &str = "aifr";

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}
fn is_true(v: &bool) -> bool {
    *v
}
fn yes() -> bool {
    true
}
fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

// ---------------------------------------------------------------------------------------------
// Project
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub format: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub settings: Settings,
    /// Free-text brief for collaborators and AIs: goal, audience, style rules, todo.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<MediaAsset>,
    /// Ordered. For video tracks the order is the stacking order: index 0 is the bottom layer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<Track>,
    /// Flat list; order is irrelevant (display and rendering use resolved times).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clips: Vec<Clip>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transitions: Vec<Transition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub markers: Vec<Marker>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ducking: Vec<Ducking>,
    /// Word-level transcripts keyed by media asset id, in the asset's *source* seconds.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub transcripts: BTreeMap<Id, Transcript>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub export: ExportSettings,
    /// Highest number handed out per id prefix (`"c"` → 12), so ids of deleted items are never
    /// reissued. Maintained by the ops.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub id_counters: BTreeMap<String, u64>,
    /// Per voice clip (the deleted captions' anchor clip), the source-time ranges `[start, end]` of
    /// generated captions a person deleted; `generate_captions` does not recreate them for that clip
    /// unless `replace: true`. (Keys naming a media asset, written by earlier versions, apply to every
    /// clip of that media.)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub caption_exclusions: BTreeMap<Id, Vec<[f64; 2]>>,
    /// Edits proposed for review (usually by an AI) that have not been applied: applying one runs its
    /// ops as one step and removes it. Oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<Suggestion>,
}

/// An edit proposed for a person to review instead of applied directly (see [`Project::suggestions`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suggestion {
    pub id: Id,
    /// One sentence: what applying it does.
    pub label: String,
    /// Why, and what to listen or look for before deciding.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// Timeline seconds where its effect can be seen or heard (previews seek there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// The edit, applied as one undoable step.
    pub ops: Vec<crate::ops::Op>,
    /// Who proposed it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
}

impl Default for Project {
    fn default() -> Self {
        Project {
            format: FORMAT_VERSION,
            name: String::new(),
            settings: Settings::default(),
            notes: String::new(),
            media: Vec::new(),
            tracks: Vec::new(),
            clips: Vec::new(),
            transitions: Vec::new(),
            markers: Vec::new(),
            ducking: Vec::new(),
            transcripts: BTreeMap::new(),
            export: ExportSettings::default(),
            id_counters: BTreeMap::new(),
            caption_exclusions: BTreeMap::new(),
            suggestions: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub width: u32,
    pub height: u32,
    /// Frames per second (29.97 is written as 29.97; the renderer snaps to frame boundaries).
    pub fps: f64,
    #[serde(default = "default_rate")]
    pub sample_rate: u32,
    #[serde(default = "black")]
    pub background: Color,
    /// Fixed length in seconds; `None` = ends where the last clip ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Title-safe and action-safe boxes as a fraction of the frame (guides and validation).
    #[serde(default = "default_title_safe")]
    pub title_safe: f64,
    #[serde(default = "default_action_safe")]
    pub action_safe: f64,
}
fn default_rate() -> u32 {
    48_000
}
fn black() -> Color {
    Color::BLACK
}
fn default_title_safe() -> f64 {
    0.8
}
fn default_action_safe() -> f64 {
    0.9
}
impl Default for Settings {
    fn default() -> Self {
        Settings {
            width: 1920,
            height: 1080,
            fps: 30.0,
            sample_rate: default_rate(),
            background: Color::BLACK,
            duration: None,
            title_safe: default_title_safe(),
            action_safe: default_action_safe(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Media
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    #[default]
    Video,
    Audio,
    Image,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaAsset {
    pub id: Id,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Path relative to the project file's folder when possible, else absolute. Forward slashes.
    pub path: String,
    pub kind: MediaKind,
    #[serde(default)]
    pub info: MediaInfo,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// What is in it ("gameplay, round 40 win", "voice-over part 2"): for humans and AIs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MediaInfo {
    /// Seconds; 0 for still images.
    #[serde(default)]
    pub duration: f64,
    #[serde(default, skip_serializing_if = "is_default")]
    pub width: u32,
    #[serde(default, skip_serializing_if = "is_default")]
    pub height: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fps: f64,
    #[serde(default)]
    pub has_video: bool,
    #[serde(default)]
    pub has_audio: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub audio_channels: u32,
    #[serde(default, skip_serializing_if = "is_default")]
    pub audio_rate: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub codec: String,
    /// Display rotation in degrees from container metadata (0/90/180/270).
    #[serde(default, skip_serializing_if = "is_default")]
    pub rotation: i32,
}

// ---------------------------------------------------------------------------------------------
// Tracks
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    /// Pictures (video, images, text, shapes, adjustment layers). Embedded audio of video clips plays too.
    #[default]
    Video,
    Audio,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Track {
    pub id: Id,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub kind: TrackKind,
    /// Semantic role: "main", "overlay", "captions", "voice", "music", "sfx", or any word.
    /// Used by ducking presets, validation and AI tools.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub muted: bool,
    /// Picture hidden (video tracks).
    #[serde(default, skip_serializing_if = "is_default")]
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub locked: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub solo: bool,
    /// Linear gain for every clip's sound on this track.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub volume: f64,
}
fn one() -> f64 {
    1.0
}
fn is_one(v: &f64) -> bool {
    *v == 1.0
}

// ---------------------------------------------------------------------------------------------
// Time and anchors
// ---------------------------------------------------------------------------------------------

/// A point on the timeline: absolute seconds, or an anchor resolved by `resolve`.
///
/// JSON: `12.5`, `{"after":"c3"}`, `{"after":"c3","offset":0.4}`, `{"with":"c3","offset":1.2}`,
/// `{"before":"c7"}`, `{"marker":"m2"}`, `{"word":{"clip":"vo1","index":17},"offset":-0.1}`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Time {
    At(f64),
    Anchored(Anchor),
}

impl Default for Time {
    fn default() -> Self {
        Time::At(0.0)
    }
}

impl From<f64> for Time {
    fn from(v: f64) -> Self {
        Time::At(v)
    }
}

/// Deserialised strictly: exactly one target key and an optional `offset`; unknown keys are errors.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Anchor {
    #[serde(flatten)]
    pub to: AnchorTarget,
    /// Seconds added after resolving the target (negative = earlier).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub offset: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorTarget {
    /// Starts when that clip ends (magnetic chain).
    After(Id),
    /// Starts when that clip starts.
    With(Id),
    /// Ends when that clip starts (this clip's start = target start − this duration). For markers: ends at it.
    Before(Id),
    /// At a marker's time.
    Marker(Id),
    /// When the `index`-th word of the transcript of `clip`'s media is spoken, through that clip's
    /// in-point and speed. Error if the word is cut out of the clip.
    Word(WordRef),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WordRef {
    pub clip: Id,
    pub index: usize,
    /// `true` = the word's end instead of its start.
    #[serde(default, skip_serializing_if = "is_default")]
    pub end: bool,
    /// The word's text when the anchor was made (filled in by the ops); `validate` warns
    /// (`word_anchor_text_mismatch`) when the transcript word at `index` no longer says this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Animatable parameters
// ---------------------------------------------------------------------------------------------

/// A number that may be animated. JSON: a bare number when constant, else `{"keys":[...]}`.
/// Key times are **clip-local seconds** (0 = the clip's first frame).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Param {
    Value(f64),
    Animated { keys: Vec<Key> },
}

impl Param {
    pub const fn new(v: f64) -> Self {
        Param::Value(v)
    }
    pub fn is_animated(&self) -> bool {
        matches!(self, Param::Animated { keys } if !keys.is_empty())
    }
}

const ANCHOR_HELP: &str = "anchors are {\"after\"|\"with\"|\"before\"|\"marker\": id, \"offset\": seconds} or {\"word\": {\"clip\": id, \"index\": n, \"end\": false}, \"offset\": seconds}";

fn anchor_from_map(map: serde_json::Map<String, serde_json::Value>) -> Result<Anchor, String> {
    const KEYS: [&str; 6] = ["after", "with", "before", "marker", "word", "offset"];
    let mut target: Option<(String, AnchorTarget)> = None;
    let mut offset = 0.0;
    for (k, v) in map {
        let t = match k.as_str() {
            "offset" => {
                offset = v.as_f64().filter(|x| x.is_finite()).ok_or_else(|| format!("anchor `offset` must be a number of seconds, not {v}"))?;
                continue;
            }
            "after" | "with" | "before" | "marker" => {
                let id = v.as_str().ok_or_else(|| format!("anchor `{k}` must be an id string like \"c3\", not {v}"))?.to_string();
                match k.as_str() {
                    "after" => AnchorTarget::After(id),
                    "with" => AnchorTarget::With(id),
                    "before" => AnchorTarget::Before(id),
                    _ => AnchorTarget::Marker(id),
                }
            }
            "word" => AnchorTarget::Word(serde_json::from_value::<WordRef>(v).map_err(|e| format!("invalid word anchor: {e}; {ANCHOR_HELP}"))?),
            other => {
                return Err(format!("unknown anchor key `{other}`.{} Valid keys: {}; {ANCHOR_HELP}", crate::util::did_you_mean(other, KEYS), KEYS.join(", ")));
            }
        };
        if let Some((prev, _)) = &target {
            return Err(format!("an anchor has exactly one target, but this one has both `{prev}` and `{k}`; {ANCHOR_HELP}"));
        }
        target = Some((k, t));
    }
    match target {
        Some((_, to)) => Ok(Anchor { to, offset }),
        None => Err(format!("the anchor names no target; {ANCHOR_HELP}")),
    }
}

impl<'de> Deserialize<'de> for Anchor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::Object(m) => anchor_from_map(m).map_err(serde::de::Error::custom),
            other => Err(serde::de::Error::custom(format!("an anchor must be an object, not {other}; {ANCHOR_HELP}"))),
        }
    }
}

impl<'de> Deserialize<'de> for Time {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::Number(n) => n.as_f64().map(Time::At).ok_or_else(|| serde::de::Error::custom("a time must be a number of seconds")),
            serde_json::Value::Object(m) => anchor_from_map(m).map(Time::Anchored).map_err(serde::de::Error::custom),
            other => Err(serde::de::Error::custom(format!("a time is seconds (12.5) or an anchor object ({{\"after\":\"c3\"}}), not {other}"))),
        }
    }
}

impl<'de> Deserialize<'de> for Param {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        const HELP: &str = "a parameter is a number (0.8) or {\"keys\":[{\"t\":0,\"v\":1,\"ease\":\"out\"}]} with clip-local times";
        let v = serde_json::Value::deserialize(d)?;
        match v {
            serde_json::Value::Number(n) => n.as_f64().map(Param::Value).ok_or_else(|| serde::de::Error::custom(HELP)),
            serde_json::Value::Object(mut m) => {
                let keys = m.remove("keys").ok_or_else(|| serde::de::Error::custom(format!("missing `keys`: {HELP}")))?;
                if let Some(k) = m.keys().next() {
                    return Err(serde::de::Error::custom(format!("unknown key `{k}` in an animated parameter: {HELP}")));
                }
                let keys: Vec<Key> = serde_json::from_value(keys).map_err(|e| serde::de::Error::custom(format!("invalid keys: {e}; {HELP}")))?;
                Ok(Param::Animated { keys })
            }
            other => Err(serde::de::Error::custom(format!("{HELP}, not {other}"))),
        }
    }
}

impl From<f64> for Param {
    fn from(v: f64) -> Self {
        Param::Value(v)
    }
}

impl Default for Param {
    fn default() -> Self {
        Param::Value(0.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    /// Clip-local seconds.
    pub t: f64,
    pub v: f64,
    /// Easing of the segment from this key to the next.
    #[serde(default, skip_serializing_if = "is_default")]
    pub ease: Ease,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Ease {
    #[default]
    Linear,
    /// Holds this key's value until the next key.
    Hold,
    In,
    Out,
    InOut,
    /// Overshoots then settles.
    BackOut,
    /// Springy settle.
    Elastic,
    /// CSS-style cubic-bezier(x1, y1, x2, y2).
    Bezier([f64; 4]),
}

fn param_zero() -> Param {
    Param::Value(0.0)
}
fn param_one() -> Param {
    Param::Value(1.0)
}
fn is_param_zero(p: &Param) -> bool {
    *p == Param::Value(0.0)
}
fn is_param_one(p: &Param) -> bool {
    *p == Param::Value(1.0)
}

// ---------------------------------------------------------------------------------------------
// Clips
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clip {
    pub id: Id,
    pub track: Id,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub start: Time,
    /// Timeline seconds (> 0). When `until` is set this is derived from it (kept up to date by the ops).
    pub duration: f64,
    /// Optional end anchor: the clip ends at this time (`{"after":"c3"}` = when c3 ends, `{"with":"c4"}`
    /// = when c4 starts, a marker, a word, or absolute seconds), so overlays follow their base clip's
    /// length. `duration` is then derived during resolve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Time>,
    pub source: Source,
    /// Playback speed of media (1 = normal, 0 = freeze frame, 4 = fast-forward). May be animated
    /// (speed ramp); the source position is the integral of speed over clip-local time.
    #[serde(default = "param_one", skip_serializing_if = "is_param_one")]
    pub speed: Param,
    /// Play media backwards (from `in_point` + consumed source down to `in_point`).
    #[serde(default, skip_serializing_if = "is_default")]
    pub reverse: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub transform: Transform,
    #[serde(default = "param_one", skip_serializing_if = "is_param_one")]
    pub opacity: Param,
    /// Linear gain of the clip's sound (audio clips and video clips with sound).
    #[serde(default = "param_one", skip_serializing_if = "is_param_one")]
    pub volume: Param,
    /// Seconds. Fades picture (opacity) for visual clips and sound for clips with audio.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fade_in: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fade_out: f64,
    /// Silence this clip's sound (a video clip's embedded audio).
    #[serde(default, skip_serializing_if = "is_default")]
    pub muted: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub blend: BlendMode,
    /// Applied in order. For `Source::Adjustment` they apply to everything beneath.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Effect>,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Intent ("hook: the win moment", "b-roll for line 4"): for humans and AIs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    /// Motion blur: the picture is the average of `samples` renders spread over the shutter interval
    /// around each frame (transform, opacity and effects animate within it). `None` = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motion_blur: Option<MotionBlur>,
    /// Frame blending (velocity blur): while the clip plays faster than 1×, each output frame averages
    /// every source frame it covers.
    #[serde(default, skip_serializing_if = "is_default")]
    pub frame_blend: bool,
    /// Track matte: this clip is drawn only through another clip (e.g. footage inside giant text). The
    /// matte clip is then not drawn itself (see [`matte_sources`]); it is evaluated at the same
    /// timeline time, whatever track it is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matte: Option<Matte>,
}

/// Motion blur settings of a clip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MotionBlur {
    /// Shutter angle in degrees (0–720): 180 = half a frame of motion is blurred, 360 = a whole frame.
    #[serde(default = "default_shutter")]
    pub shutter: f64,
    /// Sub-frame renders averaged per frame (2–64).
    #[serde(default = "default_blur_samples")]
    pub samples: u32,
}
fn default_shutter() -> f64 {
    180.0
}
fn default_blur_samples() -> u32 {
    12
}
impl Default for MotionBlur {
    fn default() -> Self {
        MotionBlur { shutter: default_shutter(), samples: default_blur_samples() }
    }
}

/// How a matte clip lets this clip through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MatteMode {
    /// Visible where the matte is opaque.
    #[default]
    Alpha,
    /// Visible where the matte is transparent.
    AlphaInverted,
    /// Visible where the matte is bright (its luminance × alpha).
    Luma,
    /// Visible where the matte is dark.
    LumaInverted,
}

/// A track matte: the clip is drawn through `clip`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Matte {
    /// The matte source (a visual clip). It is not drawn normally while it is used as a matte.
    pub clip: Id,
    #[serde(default, skip_serializing_if = "is_default")]
    pub mode: MatteMode,
}

/// Ids of clips that some clip uses as its matte: the renderer does not draw them normally.
pub fn matte_sources(p: &Project) -> std::collections::HashSet<Id> {
    p.clips.iter().filter_map(|c| c.matte.as_ref().map(|m| m.clip.clone())).collect()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
pub enum Source {
    /// A video, audio or image asset. `in_point` = source seconds shown at the clip's first frame.
    Media {
        asset: Id,
        #[serde(default, skip_serializing_if = "is_zero")]
        in_point: f64,
    },
    Text(TextSource),
    /// Full-frame flat colour (or a sized box when `transform`/mask says so).
    Solid { color: Color },
    Shape(ShapeSource),
    /// Draws nothing itself; its `effects` (and opacity as mix) apply to the composite of the
    /// tracks below it during its time range.
    Adjustment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BlendMode {
    #[default]
    Normal,
    Add,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    Difference,
}

/// How a visual clip sits in the frame. Position is the offset of the content's centre from the
/// frame's centre in project pixels (+x right, +y down).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transform {
    #[serde(default = "param_zero", skip_serializing_if = "is_param_zero")]
    pub x: Param,
    #[serde(default = "param_zero", skip_serializing_if = "is_param_zero")]
    pub y: Param,
    /// Uniform scale on top of `fit` (1 = fitted size).
    #[serde(default = "param_one", skip_serializing_if = "is_param_one")]
    pub scale: Param,
    /// Degrees, clockwise.
    #[serde(default = "param_zero", skip_serializing_if = "is_param_zero")]
    pub rotation: Param,
    /// 3-D card tilt around the horizontal axis in degrees (+ = top edge away from the viewer).
    #[serde(default = "param_zero", skip_serializing_if = "is_param_zero")]
    pub rotation_x: Param,
    /// 3-D card turn around the vertical axis in degrees (+ = right edge away from the viewer).
    #[serde(default = "param_zero", skip_serializing_if = "is_param_zero")]
    pub rotation_y: Param,
    /// Focal distance of the 3-D view in pixels (smaller = stronger perspective); 0 = orthographic.
    #[serde(default = "default_perspective", skip_serializing_if = "is_default_perspective")]
    pub perspective: f64,
    /// Pivot for scale/rotation as a fraction of the content box (0.5,0.5 = centre).
    #[serde(default = "half", skip_serializing_if = "is_half")]
    pub anchor_x: f64,
    #[serde(default = "half", skip_serializing_if = "is_half")]
    pub anchor_y: f64,
    /// How media fills the frame. Text and shapes ignore `fit` (and `crop`): they always draw at their
    /// own size.
    #[serde(default, skip_serializing_if = "is_default")]
    pub fit: Fit,
    #[serde(default, skip_serializing_if = "is_default")]
    pub flip_x: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub flip_y: bool,
    /// Fractions of the content cut from each edge (0..1). Applied before `fit`: media is re-fitted after
    /// cropping, a solid's box just shrinks. Text and shapes ignore crop.
    #[serde(default, skip_serializing_if = "is_default")]
    pub crop: Crop,
}
fn half() -> f64 {
    0.5
}
fn default_perspective() -> f64 {
    2000.0
}
fn is_default_perspective(v: &f64) -> bool {
    *v == 2000.0
}
fn is_half(v: &f64) -> bool {
    *v == 0.5
}
impl Default for Transform {
    fn default() -> Self {
        Transform {
            x: param_zero(),
            y: param_zero(),
            scale: param_one(),
            rotation: param_zero(),
            rotation_x: param_zero(),
            rotation_y: param_zero(),
            perspective: default_perspective(),
            anchor_x: 0.5,
            anchor_y: 0.5,
            fit: Fit::default(),
            flip_x: false,
            flip_y: false,
            crop: Crop::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Crop {
    #[serde(default = "param_zero", skip_serializing_if = "is_param_zero")]
    pub left: Param,
    #[serde(default = "param_zero", skip_serializing_if = "is_param_zero")]
    pub top: Param,
    #[serde(default = "param_zero", skip_serializing_if = "is_param_zero")]
    pub right: Param,
    #[serde(default = "param_zero", skip_serializing_if = "is_param_zero")]
    pub bottom: Param,
}
impl Default for Crop {
    fn default() -> Self {
        Crop { left: param_zero(), top: param_zero(), right: param_zero(), bottom: param_zero() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    /// Whole content visible, letterboxed (media default).
    #[default]
    Contain,
    /// Fills the frame, cropping overflow.
    Cover,
    /// Stretches to the frame.
    Stretch,
    /// Native pixel size (text and shapes always use their own size).
    None,
}

// ---------------------------------------------------------------------------------------------
// Text and shapes
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextSource {
    pub text: String,
    #[serde(default = "default_font")]
    pub font: String,
    /// Pixels (em size at project resolution).
    #[serde(default = "default_text_size")]
    pub size: f64,
    #[serde(default = "default_weight")]
    pub weight: u16,
    #[serde(default, skip_serializing_if = "is_default")]
    pub italic: bool,
    #[serde(default = "white")]
    pub color: Color,
    #[serde(default, skip_serializing_if = "is_default")]
    pub align: Align,
    #[serde(default = "default_line_height")]
    pub line_height: f64,
    /// Extra pixels between letters.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub letter_spacing: f64,
    /// Wrap width in pixels; `None` = no wrapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<TextBox>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke: Option<Stroke>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<Shadow>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub reveal: TextReveal,
    /// Karaoke/caption highlighting: per-word active windows in clip-local seconds, matched to the
    /// whitespace-separated words of `text` in order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlight: Option<WordHighlight>,
}
fn default_font() -> String {
    "Segoe UI".into()
}
fn default_text_size() -> f64 {
    72.0
}
fn default_weight() -> u16 {
    700
}
fn white() -> Color {
    Color::WHITE
}
fn default_line_height() -> f64 {
    1.15
}

impl Default for TextSource {
    fn default() -> Self {
        TextSource {
            text: String::new(),
            font: default_font(),
            size: default_text_size(),
            weight: default_weight(),
            italic: false,
            color: Color::WHITE,
            align: Align::default(),
            line_height: default_line_height(),
            letter_spacing: 0.0,
            max_width: None,
            background: None,
            stroke: None,
            shadow: None,
            reveal: TextReveal::default(),
            highlight: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextBox {
    pub color: Color,
    #[serde(default = "default_padding")]
    pub padding: f64,
    #[serde(default)]
    pub radius: f64,
}
fn default_padding() -> f64 {
    16.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stroke {
    pub color: Color,
    pub width: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shadow {
    pub color: Color,
    /// Soft edge width in px (2σ of the Gaussian); 0 = hard shadow.
    #[serde(default)]
    pub blur: f64,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
}

/// Built-in text entrance. `duration` is the whole reveal in seconds from the clip's start.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TextReveal {
    #[serde(default)]
    pub kind: RevealKind,
    #[serde(default)]
    pub duration: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RevealKind {
    #[default]
    None,
    Typewriter,
    FadeWords,
    PopWords,
    SlideUp,
    Fade,
    /// Each word appears as it is spoken (when its `highlight.words` window starts) and stays: the
    /// line types itself in with the voice. Needs word windows (generated captions have them);
    /// `duration` is not used.
    Spoken,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WordHighlight {
    pub color: Color,
    /// `[start, end]` clip-local seconds per word.
    pub words: Vec<[f64; 2]>,
    /// Optional box behind the active word.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Color>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeSource {
    pub kind: ShapeKind,
    /// Box size in pixels (for lines/arrows: the vector from start to end point is (width, height)).
    pub width: f64,
    pub height: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<Color>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stroke: Option<Stroke>,
    /// Corner radius for rectangles.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub radius: f64,
    /// 0..1 portion of the outline drawn (draw-on animation for boxes, circles and arrows).
    #[serde(default = "param_one", skip_serializing_if = "is_param_one")]
    pub draw: Param,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShapeKind {
    #[default]
    Rect,
    Ellipse,
    Line,
    Arrow,
}

// ---------------------------------------------------------------------------------------------
// Effects and transitions (data-driven; parameter specs live in `crate::effects`)
// ---------------------------------------------------------------------------------------------

/// One effect in a clip's stack. `kind` names an entry of the effect registry
/// (`crate::effects::EFFECTS`); missing params take the registry default.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Effect {
    pub kind: String,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, Param>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub colors: BTreeMap<String, Color>,
    /// String parameters (e.g. a LUT file path).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub strings: BTreeMap<String, String>,
}

/// A transition across the cut between two clips on the same track (`from` ends where `to` starts).
/// It is centred on the cut; media handles beyond the clip edges are used, else edge frames are held.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transition {
    pub id: Id,
    pub from: Id,
    pub to: Id,
    /// Registry name (`crate::effects::TRANSITIONS`): "crossfade", "dip_black", "wipe_left", ...
    pub kind: String,
    pub duration: f64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, f64>,
    /// Colour parameters (e.g. a film burn's tint); missing ones take the registry default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub colors: BTreeMap<String, Color>,
}

// ---------------------------------------------------------------------------------------------
// Markers, ducking, transcripts, export
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Marker {
    pub id: Id,
    pub time: Time,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "is_default")]
    pub kind: MarkerKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Color>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MarkerKind {
    #[default]
    Marker,
    /// Becomes a YouTube chapter.
    Chapter,
    /// A music beat (from beat detection).
    Beat,
    /// Something to fix.
    Todo,
}

/// Sidechain ducking: lower `target` track(s) while `trigger` track has sound.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ducking {
    pub id: Id,
    pub target: Id,
    pub trigger: Id,
    /// Gain reduction while the trigger is active (negative dB).
    #[serde(default = "default_duck_db")]
    pub amount_db: f64,
    /// Trigger level (dBFS) above which it counts as active.
    #[serde(default = "default_duck_threshold")]
    pub threshold_db: f64,
    /// Seconds to reach the full reduction. The ducker looks ahead by `attack`, so the target is already
    /// down when the trigger becomes active.
    #[serde(default = "default_attack")]
    pub attack: f64,
    /// Seconds to recover, starting after the trigger has stayed below the threshold for 0.3 s (hold).
    #[serde(default = "default_release")]
    pub release: f64,
}
fn default_duck_db() -> f64 {
    -14.0
}
fn default_duck_threshold() -> f64 {
    -45.0
}
fn default_attack() -> f64 {
    0.12
}
fn default_release() -> f64 {
    0.45
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Transcript {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub language: String,
    pub words: Vec<Word>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Word {
    pub text: String,
    /// Source seconds.
    pub start: f64,
    pub end: f64,
    /// Recognition confidence 0..1 (omitted when 1).
    #[serde(default = "one_f32", skip_serializing_if = "is_one_f32")]
    pub conf: f32,
}
fn is_one_f32(v: &f32) -> bool {
    *v == 1.0
}
fn one_f32() -> f32 {
    1.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportSettings {
    /// Output file (relative to the project folder or absolute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// "h264" | "hevc" | "av1" | "prores" | "gif" | "png_sequence" | "wav".
    #[serde(default = "default_codec")]
    pub codec: String,
    /// "draft" | "standard" | "high" | "lossless".
    #[serde(default = "default_quality")]
    pub quality: String,
    /// Use the GPU encoder (NVENC) when available.
    #[serde(default = "yes")]
    pub hardware: bool,
    #[serde(default = "default_abr")]
    pub audio_bitrate_kbps: u32,
    /// Integrated loudness target in LUFS (two-pass normalisation); `None` = leave the mix as is.
    #[serde(default = "default_lufs")]
    pub loudness_lufs: Option<f64>,
    #[serde(default = "default_tp")]
    pub true_peak_db: f64,
    /// Optional `[start, end]` timeline seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<[f64; 2]>,
    /// Output frame width in pixels; `None` = the project's width (or derived from `height`, keeping the
    /// project's aspect). See [`ExportSettings::output_size`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Output frame height in pixels; `None` = the project's height (or derived from `width`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// How the project frame maps onto an output of another aspect ratio: `contain` = the whole frame,
    /// letterboxed/pillarboxed with the background colour (default); `cover` = fill the output, cropping
    /// the overflow; `stretch` = distort to fill; `none` = 1:1 pixels, centred.
    #[serde(default, skip_serializing_if = "is_default")]
    pub fit: Fit,
}
fn default_codec() -> String {
    "h264".into()
}
fn default_quality() -> String {
    "high".into()
}
fn default_abr() -> u32 {
    256
}
fn default_lufs() -> Option<f64> {
    Some(-14.0)
}
fn default_tp() -> f64 {
    -1.5
}
impl Default for ExportSettings {
    fn default() -> Self {
        ExportSettings {
            path: None,
            codec: default_codec(),
            quality: default_quality(),
            hardware: true,
            audio_bitrate_kbps: default_abr(),
            loudness_lufs: default_lufs(),
            true_peak_db: default_tp(),
            range: None,
            width: None,
            height: None,
            fit: Fit::Contain,
        }
    }
}

impl ExportSettings {
    /// The exported frame size for project settings `s`: both sides when both are set; one side alone
    /// scales the other to keep the project's aspect (rounded to an even number of pixels); neither =
    /// the project's size.
    pub fn output_size(&self, s: &Settings) -> (u32, u32) {
        let even = |v: f64| ((v / 2.0).round() * 2.0).max(2.0) as u32;
        let (pw, ph) = (s.width.max(1) as f64, s.height.max(1) as f64);
        match (self.width.filter(|w| *w > 0), self.height.filter(|h| *h > 0)) {
            (Some(w), Some(h)) => (w, h),
            (Some(w), None) => (w, even(w as f64 * ph / pw)),
            (None, Some(h)) => (even(h as f64 * pw / ph), h),
            (None, None) => (s.width, s.height),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------------------------

/// Straight (non-premultiplied) sRGB colour, components 0..1. JSON: `"#RRGGBB"` or `"#RRGGBBAA"`
/// (CSS names like `"white"` are accepted on input).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const BLACK: Color = Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
    pub const WHITE: Color = Color { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
    pub const TRANSPARENT: Color = Color { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };

    pub fn rgba8(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: a as f32 / 255.0 }
    }

    pub fn to_rgba8(self) -> [u8; 4] {
        let c = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        [c(self.r), c(self.g), c(self.b), c(self.a)]
    }

    pub fn to_hex(self) -> String {
        let [r, g, b, a] = self.to_rgba8();
        if a == 255 {
            format!("#{r:02X}{g:02X}{b:02X}")
        } else {
            format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
        }
    }

    /// Parses `#RGB`, `#RRGGBB`, `#RRGGBBAA` or a basic CSS colour name.
    pub fn parse(s: &str) -> Option<Color> {
        let s = s.trim();
        if !s.is_ascii() {
            return None;
        }
        if let Some(h) = s.strip_prefix('#') {
            let v = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
            return match h.len() {
                3 => {
                    let d = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).ok().map(|x| x * 17);
                    Some(Color::rgba8(d(0)?, d(1)?, d(2)?, 255))
                }
                6 => Some(Color::rgba8(v(0)?, v(2)?, v(4)?, 255)),
                8 => Some(Color::rgba8(v(0)?, v(2)?, v(4)?, v(6)?)),
                _ => None,
            };
        }
        let named = match s.to_ascii_lowercase().as_str() {
            "black" => (0, 0, 0, 255),
            "white" => (255, 255, 255, 255),
            "red" => (255, 0, 0, 255),
            "green" => (0, 128, 0, 255),
            "lime" => (0, 255, 0, 255),
            "blue" => (0, 0, 255, 255),
            "yellow" => (255, 255, 0, 255),
            "orange" => (255, 165, 0, 255),
            "purple" => (128, 0, 128, 255),
            "pink" => (255, 192, 203, 255),
            "gray" | "grey" => (128, 128, 128, 255),
            "cyan" => (0, 255, 255, 255),
            "magenta" => (255, 0, 255, 255),
            "transparent" => (0, 0, 0, 0),
            _ => return None,
        };
        Some(Color::rgba8(named.0, named.1, named.2, named.3))
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Color::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("invalid colour {s:?} (use \"#RRGGBB\" or \"#RRGGBBAA\")")))
    }
}
