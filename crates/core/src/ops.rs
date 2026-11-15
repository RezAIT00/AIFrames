//! Typed, serialisable edit operations. Every change to a project — by a person in the GUI or by an
//! AI over MCP — is an [`Op`]; [`apply`] validates it and either applies it completely or leaves the
//! project untouched. [`apply_batch`] is atomic over many ops.
//!
//! JSON: `{"op":"move_clip","clip":"c3","start":12.0}`. Unknown fields are rejected (a typo is an
//! error, never silently ignored).
//!
//! Besides each op's own validation, every op is checked after it runs and refused if it would:
//! create an unresolved anchor (unknown target, cycle, word cut out), make two clips overlap on a
//! track, change anything on a locked track, place a clip before 0 s, or leave a clip invalid (past
//! its media's end, on the wrong kind of track, unknown effect, …).
//!
//! Magnetic timeline semantics: clips anchored `after` another clip follow it, so trimming or deleting
//! a clip moves everything chained after it. `ripple` options additionally shift absolutely placed
//! clips later on the same track.

pub(crate) mod captions;
mod edit;
pub(crate) mod prims;

use crate::model::*;
use crate::resolve::{resolve, Resolved, EPS};
use crate::util;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

pub use captions::CaptionStyle;

/// Shortest clip an op may create (seconds).
pub const MIN_DURATION: f64 = 0.001;
/// Highest playback speed accepted.
pub const MAX_SPEED: f64 = 100.0;
/// Default extra margin removed around deleted words by `delete_words` (seconds).
pub const DEFAULT_WORD_PAD: f64 = 0.04;

fn yes() -> bool {
    true
}
fn is_true(v: &bool) -> bool {
    *v
}
fn is_false(v: &bool) -> bool {
    !*v
}
fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

/// `Option<Option<T>>` for fields where `null` means "clear" and a missing field means "unchanged".
mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<T: Serialize, S: Serializer>(v: &Option<Option<T>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(inner) => inner.serialize(s),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
}

/// Which end of a clip a trim moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    Start,
    End,
}

/// What `set_speed` keeps constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SpeedKeep {
    /// Same source range: the clip gets shorter when faster (default).
    #[default]
    Source,
    /// Same timeline duration: more or less of the source is used.
    Duration,
}

/// An effect for `add_effect`: a kind name (`"blur"`, defaults for everything) or a full effect object.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum EffectArg {
    Kind(String),
    Full(Effect),
}

impl<'de> Deserialize<'de> for EffectArg {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        const HELP: &str = "an effect is a kind name (\"blur\") or an object {\"kind\":\"blur\",\"params\":{\"radius\":8}}";
        match Value::deserialize(d)? {
            Value::String(s) => Ok(EffectArg::Kind(s)),
            v @ Value::Object(_) => serde_json::from_value::<Effect>(v).map(EffectArg::Full).map_err(|e| serde::de::Error::custom(format!("invalid effect: {e}; {HELP}"))),
            other => Err(serde::de::Error::custom(format!("{HELP}, not {other}"))),
        }
    }
}

/// One entry of `move_clips`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipMove {
    pub clip: Id,
    pub start: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<Id>,
}

/// Every edit. See [`OPS`] for a one-line description and a JSON example of each.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    // ----- project -----
    SetSettings {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fps: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sample_rate: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        background: Option<Color>,
        /// Fixed length in seconds; `null` = end with the last clip.
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        duration: Option<Option<f64>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title_safe: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action_safe: Option<f64>,
    },
    SetProject {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notes: Option<String>,
    },
    SetExport {
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        path: Option<Option<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codec: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quality: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hardware: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio_bitrate_kbps: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        loudness_lufs: Option<Option<f64>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        true_peak_db: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        range: Option<Option<[f64; 2]>>,
    },

    /// Output frame size of exports (`null` = the project's size; one side alone keeps the project's
    /// aspect) and how the project frame fits an output of another aspect (`contain` letterboxes,
    /// `cover` crops to fill, `stretch`, `none`).
    SetExportSize {
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        width: Option<Option<u32>>,
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        height: Option<Option<u32>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fit: Option<Fit>,
    },

    // ----- media -----
    AddMedia {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Id>,
        path: String,
        kind: MediaKind,
        #[serde(default, skip_serializing_if = "is_default")]
        info: MediaInfo,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notes: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
    },
    RemoveMedia {
        asset: Id,
        /// Also delete the clips that use it (they leave gaps; dependants keep their times).
        #[serde(default, skip_serializing_if = "is_false")]
        force: bool,
    },
    RelinkMedia {
        asset: Id,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        info: Option<MediaInfo>,
    },
    SetMedia {
        asset: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notes: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tags: Option<Vec<String>>,
    },

    // ----- tracks -----
    AddTrack {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Id>,
        kind: TrackKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        /// Position in the track list (0 = bottom of the video stack); default: on top.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
    },
    RemoveTrack {
        track: Id,
        #[serde(default, skip_serializing_if = "is_false")]
        force: bool,
    },
    MoveTrack {
        track: Id,
        index: usize,
    },
    SetTrack {
        track: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        muted: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hidden: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locked: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        solo: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        volume: Option<f64>,
    },

    // ----- clips -----
    /// `clip` is a partial clip: `track` and a source are required (`"asset":"m1"` / `"text":"Hi"` are
    /// shorthands); `id` defaults to a fresh one, `start` to the end of the track, `duration` to the rest
    /// of the media (3 s for stills, text and generators).
    AddClip {
        clip: Value,
        /// Remove/trim whatever is under the new clip on its track instead of failing.
        #[serde(default, skip_serializing_if = "is_false")]
        overwrite: bool,
    },
    /// Magnetic append: the clip starts `after` the given clip (default: the last clip of the track);
    /// clips that were chained after that clip on this track now follow the new one.
    AppendClip {
        track: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<Id>,
        clip: Value,
    },
    /// Inserts at timeline time `at`, splitting a clip there; with `ripple` (default) later clips on the
    /// track move right, without it the new clip overwrites.
    InsertClip {
        track: Id,
        at: f64,
        clip: Value,
        #[serde(default = "yes", skip_serializing_if = "is_true")]
        ripple: bool,
    },
    MoveClip {
        clip: Id,
        start: Time,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        track: Option<Id>,
        #[serde(default, skip_serializing_if = "is_false")]
        overwrite: bool,
    },
    /// Moves several clips in one step: every move is applied, then the result is checked, so clips
    /// can swap places or move as a group across tracks without intermediate overlaps.
    MoveClips {
        moves: Vec<ClipMove>,
        /// Remove/trim whatever (not itself moved) is under the moved clips.
        #[serde(default, skip_serializing_if = "is_false")]
        overwrite: bool,
    },
    /// Sets (or with `null` clears) a clip's end anchor: `{"after":"c3"}` = ends when c3 ends,
    /// `{"with":"c4"}` = ends when c4 starts, a marker, a word, or absolute seconds. The clip's length
    /// then follows that anchor.
    SetUntil {
        clip: Id,
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        until: Option<Option<Time>>,
    },
    /// `to` = absolute timeline seconds for the edge, or `delta` seconds (start edge: + = later/shorter;
    /// end edge: + = longer). `ripple`: omitted = magnetic (clips anchored after it follow; a clip that is
    /// itself chained `after` another keeps its start when its head is trimmed); `true` = also shift every
    /// later clip on the track; `false` = nothing else moves.
    TrimClip {
        clip: Id,
        edge: Edge,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delta: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ripple: Option<bool>,
    },
    SplitClip {
        clip: Id,
        /// Timeline seconds, strictly inside the clip.
        at: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_id: Option<Id>,
        /// Also split linked clips (detached audio anchored `with` it).
        #[serde(default = "yes", skip_serializing_if = "is_true")]
        linked: bool,
    },
    DeleteClip {
        clip: Id,
        /// Close the gap: dependants re-anchor to the deleted clip's own start anchor and later clips on
        /// the track move left. Without ripple, dependants keep their current times (absolute).
        #[serde(default, skip_serializing_if = "is_false")]
        ripple: bool,
        #[serde(default = "yes", skip_serializing_if = "is_true")]
        linked: bool,
    },
    DuplicateClip {
        clip: Id,
        /// Default: magnetically after the last clip of the destination track.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<Time>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        track: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_id: Option<Id>,
        #[serde(default, skip_serializing_if = "is_false")]
        overwrite: bool,
    },
    /// JSON merge patch (RFC 7386) over the clip's JSON; `null` resets a field to its default. The id
    /// cannot change. The result is fully re-validated.
    SetClip {
        clip: Id,
        patch: Value,
    },
    /// Sets a parameter to a constant `value` or to animation `keys` (clip-local seconds).
    SetParam {
        clip: Id,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        keys: Option<Vec<Key>>,
    },
    AddKeyframe {
        clip: Id,
        path: String,
        /// Clip-local seconds.
        t: f64,
        v: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ease: Option<Ease>,
    },
    RemoveKeyframe {
        clip: Id,
        path: String,
        t: f64,
    },
    /// Moves the media under the clip (in_point += delta) without moving the clip.
    SlipClip {
        clip: Id,
        delta: f64,
        #[serde(default = "yes", skip_serializing_if = "is_true")]
        linked: bool,
    },
    /// Moves the cut between two touching clips by `delta` (left gets longer, right shorter).
    RollEdit {
        left: Id,
        right: Id,
        delta: f64,
    },
    /// Moves a clip between its touching neighbours: the previous one gets longer, the next shorter.
    SlideClip {
        clip: Id,
        delta: f64,
    },
    SetSpeed {
        clip: Id,
        speed: f64,
        #[serde(default, skip_serializing_if = "is_default")]
        keep: SpeedKeep,
        #[serde(default = "yes", skip_serializing_if = "is_true")]
        linked: bool,
    },
    /// Holds the frame at timeline time `at` for `duration` seconds, pushing the rest of the clip later.
    FreezeFrame {
        clip: Id,
        at: f64,
        duration: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_id: Option<Id>,
    },
    /// Creates an audio clip of the video clip's sound on an audio track, anchored `with` the video
    /// clip (linked: splits/trims/deletes of the video follow), and mutes the video clip's own sound.
    DetachAudio {
        clip: Id,
        track: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_id: Option<Id>,
    },
    ReplaceMedia {
        clip: Id,
        asset: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        in_point: Option<f64>,
    },
    /// Removes everything in `[from, to)` on the tracks (default: all unlocked tracks), leaving a gap.
    LiftRange {
        from: f64,
        to: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tracks: Option<Vec<Id>>,
    },
    /// Removes `[from, to)` and closes the gap (ripple) on the tracks (default: all unlocked tracks).
    ExtractRange {
        from: f64,
        to: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tracks: Option<Vec<Id>>,
    },
    /// Moves clips by `delta` seconds (anchored clips keep their anchor and change its offset).
    Nudge {
        clips: Vec<Id>,
        delta: f64,
    },
    /// Changes the gaps between consecutive clips (the listed `clips`, or every clip on `track`, in time
    /// order): `gap` sets every gap, `scale` multiplies them, `min_gap`/`max_gap` clamp them. Clips move
    /// through their anchor offsets (or starts), so everything anchored to them follows. The outcome's
    /// `details.gaps` lists each gap as `{"from": clip before it, "to": clip after it, "before": s,
    /// "after": s}`.
    SetSpacing {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        clips: Vec<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        track: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gap: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scale: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_gap: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_gap: Option<f64>,
    },
    /// Closes the gaps between consecutive clips on a track (only gaps up to `max_gap` seconds when given).
    CloseGaps {
        track: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_gap: Option<f64>,
    },

    // ----- effects -----
    AddEffect {
        clip: Id,
        effect: EffectArg,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
    },
    RemoveEffect {
        clip: Id,
        index: usize,
    },
    /// Merges into effect `index`; a `null` value resets that parameter/colour/string to its default.
    SetEffect {
        clip: Id,
        index: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        params: BTreeMap<String, Option<Param>>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        colors: BTreeMap<String, Option<Color>>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        strings: BTreeMap<String, Option<String>>,
    },
    MoveEffect {
        clip: Id,
        from: usize,
        to: usize,
    },

    // ----- transitions -----
    AddTransition {
        from: Id,
        to: Id,
        kind: String,
        /// Default: the registry's suggested duration for the kind.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Id>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        params: BTreeMap<String, f64>,
    },
    SetTransition {
        transition: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration: Option<f64>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        params: BTreeMap<String, Option<f64>>,
    },
    RemoveTransition {
        transition: Id,
    },
    /// Sets (or with `null` resets to the registry default) colour parameters of a transition, e.g.
    /// `{"color":"#FF8A2B"}` for a film burn.
    SetTransitionColors {
        transition: Id,
        colors: BTreeMap<String, Option<Color>>,
    },

    // ----- markers -----
    AddMarker {
        time: Time,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<MarkerKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<Color>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    SetMarker {
        marker: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        time: Option<Time>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<MarkerKind>,
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        color: Option<Option<Color>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    RemoveMarker {
        marker: Id,
    },

    // ----- review -----
    /// Proposes `ops` for a person to review instead of applying them: they are checked against the
    /// project now and kept in `suggestions` until applied (as one step) or dismissed.
    AddSuggestion {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Id>,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<f64>,
        ops: Vec<Op>,
    },
    /// Removes a suggestion without applying it (dismiss). Applying one is its ops followed by this op
    /// (see [`suggestion_ops`]).
    RemoveSuggestion {
        suggestion: Id,
    },

    // ----- audio -----
    AddDucking {
        target: Id,
        trigger: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        amount_db: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        threshold_db: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attack: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        release: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Id>,
    },
    SetDucking {
        ducking: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        amount_db: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        threshold_db: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attack: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        release: Option<f64>,
    },
    RemoveDucking {
        ducking: Id,
    },

    // ----- transcript and text-based editing -----
    /// Sets (or with `null`, removes) the word transcript of a media asset (source seconds). Refused if
    /// it would break word anchors, unless `force`.
    SetTranscript {
        asset: Id,
        #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
        transcript: Option<Option<Transcript>>,
        #[serde(default, skip_serializing_if = "is_false")]
        force: bool,
    },
    /// Text-based editing: removes words `from..=to` (transcript indices) from the clip by splitting and
    /// ripple-deleting their source range, plus `pad` seconds (default 0.04) on each side where that does
    /// not eat into the neighbouring words. Captions anchored to those words are re-timed or removed.
    /// `edges` (source seconds `[start, end]`) replaces that rule with cut points measured on the audio
    /// (hosts snap them into silence, see `aiframes_media::cut_edges`); each must lie within 0.5 s of
    /// the words' own edges.
    DeleteWords {
        clip: Id,
        from: usize,
        to: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pad: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        edges: Option<[f64; 2]>,
    },
    /// Removes the media source range `[from, to)` (source seconds, e.g. a detected silence) from the clip
    /// by splitting and ripple-deleting.
    CutSource {
        clip: Id,
        from: f64,
        to: f64,
        #[serde(default = "yes", skip_serializing_if = "is_true")]
        linked: bool,
    },
    /// Creates caption text clips on `track` from the transcripts of `clips`, one per short line, each
    /// anchored to its first word (captions follow the voice) with karaoke `highlight` word windows.
    /// Captions people edited or deleted are respected unless `replace: true`; see `details`.
    GenerateCaptions {
        clips: Vec<Id>,
        track: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        style: Option<CaptionStyle>,
        /// Default 5.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_words: Option<usize>,
        /// Default 32 (20 for portrait projects).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_chars: Option<usize>,
        /// A silence at least this long (seconds) starts a new caption. Default 0.5.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pause: Option<f64>,
        /// Default (false): captions generated earlier on `track` for the same media are replaced unless a
        /// person edited them (kept), words whose captions a person deleted are skipped, and the look of
        /// the existing captions is inherited. `true` replaces everything and recreates deleted ones.
        #[serde(default, skip_serializing_if = "is_false")]
        replace: bool,
    },
}

/// Description and example of an op (drives `list_ops`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct OpSpec {
    pub name: &'static str,
    pub group: &'static str,
    pub help: &'static str,
    pub example: &'static str,
}

const fn spec(name: &'static str, group: &'static str, help: &'static str, example: &'static str) -> OpSpec {
    OpSpec { name, group, help, example }
}

/// Every op with a one-line help and a JSON example.
pub static OPS: &[OpSpec] = &[
    spec("set_settings", "project", "Frame size, fps, sample rate, background, fixed duration (null = auto), safe areas.", r#"{"op":"set_settings","width":1080,"height":1920,"fps":30}"#),
    spec("set_project", "project", "Project name and the notes/brief for collaborators.", r#"{"op":"set_project","name":"Round 40 recap","notes":"Fast cuts, captions on"}"#),
    spec("set_export", "project", "Export defaults: path, codec (h264|hevc|av1|prores|gif|png_sequence|wav), quality, hardware, loudness.", r#"{"op":"set_export","codec":"hevc","quality":"high","loudness_lufs":-14}"#),
    spec("set_export_size", "project", "Export frame size (null = project size; one side keeps the aspect) and fit: contain letterboxes, cover crops to fill.", r#"{"op":"set_export_size","width":1080,"height":1920,"fit":"cover"}"#),
    spec("add_media", "media", "Registers a media file (normally via the import tool, which probes `info`).", r#"{"op":"add_media","path":"assets/gameplay.mp4","kind":"video","info":{"duration":120.0,"width":1920,"height":1080,"fps":60,"has_video":true,"has_audio":true}}"#),
    spec("remove_media", "media", "Removes a media asset; `force` also deletes the clips that use it.", r#"{"op":"remove_media","asset":"m2","force":true}"#),
    spec("relink_media", "media", "Points an asset at a new file path (optionally with new probe info).", r#"{"op":"relink_media","asset":"m1","path":"D:/footage/gameplay.mp4"}"#),
    spec("set_media", "media", "Renames an asset or sets its notes/tags.", r#"{"op":"set_media","asset":"m1","notes":"round 40 win at 31 s","tags":["gameplay"]}"#),
    spec("add_track", "tracks", "Adds a video or audio track (default on top) with an optional role (main, overlay, captions, voice, music, sfx).", r#"{"op":"add_track","kind":"video","role":"captions","name":"Captions"}"#),
    spec("remove_track", "tracks", "Removes a track; `force` also deletes its clips and duckings.", r#"{"op":"remove_track","track":"v3","force":true}"#),
    spec("move_track", "tracks", "Moves a track to an index (video stacking order: 0 = bottom).", r#"{"op":"move_track","track":"v2","index":0}"#),
    spec("set_track", "tracks", "Name, role, mute, hide, lock, solo, volume of a track.", r#"{"op":"set_track","track":"a2","volume":0.5}"#),
    spec("add_clip", "clips", "Adds a clip from partial JSON (track + source required; shorthands asset/text).", r#"{"op":"add_clip","clip":{"track":"v2","text":"VICTORY!","start":{"word":{"clip":"c3","index":17}},"duration":1.5}}"#),
    spec("append_clip", "clips", "Appends a clip magnetically after the last clip of a track (or after `after`).", r#"{"op":"append_clip","track":"v1","clip":{"asset":"m2","in_point":31.2,"duration":4}}"#),
    spec("insert_clip", "clips", "Inserts a clip at a time, splitting what is there and pushing later clips right (ripple).", r#"{"op":"insert_clip","track":"v1","at":12.0,"clip":{"asset":"m3","duration":2.5}}"#),
    spec("move_clip", "clips", "Moves a clip to an absolute time or an anchor, optionally to another track.", r#"{"op":"move_clip","clip":"c7","start":{"after":"c6","offset":0.3},"track":"v2"}"#),
    spec("set_until", "clips", "Ends a clip at an anchor (after/with a clip, a marker, a word) so it follows its base clip's length; null clears it.", r#"{"op":"set_until","clip":"badge","until":{"after":"c3"}}"#),
    spec("move_clips", "clips", "Moves several clips at once (atomic: they may swap places or move as a group across tracks).", r#"{"op":"move_clips","moves":[{"clip":"c3","start":12.0,"track":"v2"},{"clip":"c4","start":{"after":"c3"},"track":"v2"}]}"#),
    spec("trim_clip", "clips", "Moves a clip's start or end edge (to = absolute seconds, or delta).", r#"{"op":"trim_clip","clip":"c3","edge":"end","delta":-1.5}"#),
    spec("split_clip", "clips", "Splits a clip at a timeline time; the left part keeps the id.", r#"{"op":"split_clip","clip":"c3","at":14.2}"#),
    spec("delete_clip", "clips", "Deletes a clip; `ripple` closes the gap.", r#"{"op":"delete_clip","clip":"c4","ripple":true}"#),
    spec("duplicate_clip", "clips", "Copies a clip (default: appended to the end of its track).", r#"{"op":"duplicate_clip","clip":"c2","start":30.0,"track":"v2"}"#),
    spec("set_clip", "clips", "JSON merge patch over a clip (escape hatch; fully re-validated).", r#"{"op":"set_clip","clip":"c5","patch":{"name":"hook","blend":"screen","notes":"the win moment"}}"#),
    spec("set_param", "clips", "Sets opacity, volume, speed, transform.x|y|scale|rotation|rotation_x|rotation_y|anchor_x|anchor_y|perspective, transform.crop.left|top|right|bottom, fade_in, fade_out, source.draw or effects.N.<param> to a value or keys.", r#"{"op":"set_param","clip":"c5","path":"transform.scale","keys":[{"t":0,"v":1},{"t":2,"v":1.2,"ease":"in_out"}]}"#),
    spec("add_keyframe", "clips", "Adds (or replaces) one key at clip-local time t; its ease shapes the move from this key to the next one.", r#"{"op":"add_keyframe","clip":"c5","path":"opacity","t":0.5,"v":1,"ease":"out"}"#),
    spec("remove_keyframe", "clips", "Removes the key at clip-local time t (within half a frame).", r#"{"op":"remove_keyframe","clip":"c5","path":"opacity","t":0.5}"#),
    spec("slip_clip", "clips", "Shifts the media under a clip (in_point += delta) without moving it.", r#"{"op":"slip_clip","clip":"c3","delta":0.5}"#),
    spec("roll_edit", "clips", "Moves the cut between two touching clips.", r#"{"op":"roll_edit","left":"c3","right":"c4","delta":0.25}"#),
    spec("slide_clip", "clips", "Moves a clip between its neighbours, which absorb the change.", r#"{"op":"slide_clip","clip":"c4","delta":-0.5}"#),
    spec("set_speed", "clips", "Constant playback speed; keep \"source\" (duration changes) or \"duration\".", r#"{"op":"set_speed","clip":"c3","speed":2,"keep":"source"}"#),
    spec("freeze_frame", "clips", "Holds the frame at a timeline time for a duration, pushing the rest later.", r#"{"op":"freeze_frame","clip":"c3","at":15.0,"duration":1.5}"#),
    spec("detach_audio", "clips", "Moves a video clip's sound to a linked audio clip on an audio track.", r#"{"op":"detach_audio","clip":"c3","track":"a1"}"#),
    spec("replace_media", "clips", "Swaps the media of a clip, keeping its timing and effects.", r#"{"op":"replace_media","clip":"c3","asset":"m4","in_point":2.0}"#),
    spec("lift_range", "clips", "Removes everything in [from, to) on tracks, leaving a gap.", r#"{"op":"lift_range","from":10.0,"to":12.5,"tracks":["v1","a1"]}"#),
    spec("extract_range", "clips", "Removes [from, to) on tracks and closes the gap.", r#"{"op":"extract_range","from":10.0,"to":12.5}"#),
    spec("nudge", "clips", "Moves clips by delta seconds.", r#"{"op":"nudge","clips":["c7","c8"],"delta":0.1}"#),
    spec("set_spacing", "clips", "Retimes the pauses between consecutive clips (gap, scale, min_gap, max_gap); anchored clips follow.", r#"{"op":"set_spacing","track":"a1","scale":0.7,"min_gap":0.4}"#),
    spec("close_gaps", "clips", "Closes the gaps between consecutive clips on a track (up to max_gap).", r#"{"op":"close_gaps","track":"v1","max_gap":0.5}"#),
    spec("add_effect", "effects", "Adds an effect (kind name or full object) to a clip's stack.", r#"{"op":"add_effect","clip":"c3","effect":{"kind":"color","params":{"saturation":1.2,"contrast":0.1}}}"#),
    spec("remove_effect", "effects", "Removes effect `index` from a clip.", r#"{"op":"remove_effect","clip":"c3","index":0}"#),
    spec("set_effect", "effects", "Changes an effect's params/colors/strings/enabled (null = default).", r#"{"op":"set_effect","clip":"c3","index":0,"params":{"radius":{"keys":[{"t":0,"v":20},{"t":0.5,"v":0}]}}}"#),
    spec("move_effect", "effects", "Reorders a clip's effect stack.", r#"{"op":"move_effect","clip":"c3","from":1,"to":0}"#),
    spec("add_transition", "transitions", "Adds a transition across the cut between two touching clips.", r#"{"op":"add_transition","from":"c3","to":"c4","kind":"crossfade","duration":0.5}"#),
    spec("set_transition", "transitions", "Changes a transition's kind, duration or params.", r#"{"op":"set_transition","transition":"tr1","kind":"whip","duration":0.3}"#),
    spec("set_transition_colors", "transitions", "Sets colour parameters of a transition (null = registry default).", r##"{"op":"set_transition_colors","transition":"tr1","colors":{"color":"#FF8A2B"}}"##),
    spec("remove_transition", "transitions", "Removes a transition.", r#"{"op":"remove_transition","transition":"tr1"}"#),
    spec("add_marker", "markers", "Adds a marker (marker, chapter, beat, todo) at a time or anchor.", r#"{"op":"add_marker","time":42.0,"name":"Round 40","kind":"chapter"}"#),
    spec("set_marker", "markers", "Changes a marker.", r#"{"op":"set_marker","marker":"mk1","time":{"with":"c5"},"note":"move here"}"#),
    spec("remove_marker", "markers", "Removes a marker (anchored items keep their times).", r#"{"op":"remove_marker","marker":"mk1"}"#),
    spec(
        "add_suggestion",
        "review",
        "Proposes ops for a person to approve instead of applying them (checked now, applied later as one step).",
        r#"{"op":"add_suggestion","label":"Tighten the pause before the punchline to 0.3 s","reason":"Listen: the beat may be intentional","at":42.5,"ops":[{"op":"cut_source","clip":"c3","from":61.2,"to":61.9}]}"#,
    ),
    spec("remove_suggestion", "review", "Dismisses a suggestion without applying it.", r#"{"op":"remove_suggestion","suggestion":"sg1"}"#),
    spec("add_ducking", "audio", "Lowers a track while another has sound (e.g. music under voice).", r#"{"op":"add_ducking","target":"a2","trigger":"a1","amount_db":-12}"#),
    spec("set_ducking", "audio", "Changes a ducking rule.", r#"{"op":"set_ducking","ducking":"d1","amount_db":-18}"#),
    spec("remove_ducking", "audio", "Removes a ducking rule.", r#"{"op":"remove_ducking","ducking":"d1"}"#),
    spec("set_transcript", "transcript", "Sets or removes (null) an asset's word transcript (source seconds).", r#"{"op":"set_transcript","asset":"m1","transcript":{"language":"en","words":[{"text":"Hello","start":0.5,"end":0.9}]}}"#),
    spec("delete_words", "transcript", "Cuts transcript words from..=to out of a clip (text-based editing).", r#"{"op":"delete_words","clip":"c3","from":12,"to":14}"#),
    spec("cut_source", "transcript", "Cuts a source-time range (e.g. a silence) out of a clip.", r#"{"op":"cut_source","clip":"c3","from":31.2,"to":32.0}"#),
    spec("generate_captions", "transcript", "Creates word-anchored karaoke captions for clips on a track.", r#"{"op":"generate_captions","clips":["c3"],"track":"v3","max_words":4}"#),
];

impl Op {
    /// The op's JSON name (`"move_clip"`).
    pub fn name(&self) -> &'static str {
        match self {
            Op::SetSettings { .. } => "set_settings",
            Op::SetProject { .. } => "set_project",
            Op::SetExport { .. } => "set_export",
            Op::SetExportSize { .. } => "set_export_size",
            Op::AddMedia { .. } => "add_media",
            Op::RemoveMedia { .. } => "remove_media",
            Op::RelinkMedia { .. } => "relink_media",
            Op::SetMedia { .. } => "set_media",
            Op::AddTrack { .. } => "add_track",
            Op::RemoveTrack { .. } => "remove_track",
            Op::MoveTrack { .. } => "move_track",
            Op::SetTrack { .. } => "set_track",
            Op::AddClip { .. } => "add_clip",
            Op::AppendClip { .. } => "append_clip",
            Op::InsertClip { .. } => "insert_clip",
            Op::MoveClip { .. } => "move_clip",
            Op::MoveClips { .. } => "move_clips",
            Op::SetUntil { .. } => "set_until",
            Op::TrimClip { .. } => "trim_clip",
            Op::SplitClip { .. } => "split_clip",
            Op::DeleteClip { .. } => "delete_clip",
            Op::DuplicateClip { .. } => "duplicate_clip",
            Op::SetClip { .. } => "set_clip",
            Op::SetParam { .. } => "set_param",
            Op::AddKeyframe { .. } => "add_keyframe",
            Op::RemoveKeyframe { .. } => "remove_keyframe",
            Op::SlipClip { .. } => "slip_clip",
            Op::RollEdit { .. } => "roll_edit",
            Op::SlideClip { .. } => "slide_clip",
            Op::SetSpeed { .. } => "set_speed",
            Op::FreezeFrame { .. } => "freeze_frame",
            Op::DetachAudio { .. } => "detach_audio",
            Op::ReplaceMedia { .. } => "replace_media",
            Op::LiftRange { .. } => "lift_range",
            Op::ExtractRange { .. } => "extract_range",
            Op::Nudge { .. } => "nudge",
            Op::SetSpacing { .. } => "set_spacing",
            Op::CloseGaps { .. } => "close_gaps",
            Op::AddEffect { .. } => "add_effect",
            Op::RemoveEffect { .. } => "remove_effect",
            Op::SetEffect { .. } => "set_effect",
            Op::MoveEffect { .. } => "move_effect",
            Op::AddTransition { .. } => "add_transition",
            Op::SetTransition { .. } => "set_transition",
            Op::RemoveTransition { .. } => "remove_transition",
            Op::SetTransitionColors { .. } => "set_transition_colors",
            Op::AddMarker { .. } => "add_marker",
            Op::SetMarker { .. } => "set_marker",
            Op::RemoveMarker { .. } => "remove_marker",
            Op::AddSuggestion { .. } => "add_suggestion",
            Op::RemoveSuggestion { .. } => "remove_suggestion",
            Op::AddDucking { .. } => "add_ducking",
            Op::SetDucking { .. } => "set_ducking",
            Op::RemoveDucking { .. } => "remove_ducking",
            Op::SetTranscript { .. } => "set_transcript",
            Op::DeleteWords { .. } => "delete_words",
            Op::CutSource { .. } => "cut_source",
            Op::GenerateCaptions { .. } => "generate_captions",
        }
    }

    /// Parses one op from JSON with a helpful error (unknown op names get a suggestion).
    pub fn from_json(v: &Value) -> Result<Op, EditError> {
        serde_json::from_value::<Op>(v.clone()).map_err(|e| {
            let name = v.get("op").and_then(Value::as_str).unwrap_or("");
            let hint = if name.is_empty() {
                " Every op needs an \"op\" field, e.g. {\"op\":\"move_clip\",\"clip\":\"c3\",\"start\":12.0}.".to_string()
            } else if op_spec(name).is_none() {
                util::did_you_mean(name, OPS.iter().map(|s| s.name))
            } else if name == "set_export" && ["width", "height", "fit"].iter().any(|k| v.get(*k).is_some()) {
                format!(" The output frame size is set with set_export_size: {}", op_spec("set_export_size").map(|s| s.example).unwrap_or(""))
            } else if name == "move_clip" && v.get("moves").is_some() {
                format!(" To move several clips at once use move_clips: {}", op_spec("move_clips").map(|s| s.example).unwrap_or(""))
            } else {
                format!(" Example: {}", op_spec(name).map(|s| s.example).unwrap_or(""))
            };
            EditError::new(format!("invalid op: {e}.{hint}"))
        })
    }
}

/// The spec of an op by name.
pub fn op_spec(name: &str) -> Option<&'static OpSpec> {
    OPS.iter().find(|s| s.name == name)
}

// ---------------------------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------------------------

/// What an op did. `created`/`changed`/`removed` list ids of every kind (clips, tracks, media,
/// markers, transitions, duckings; an asset id is listed as changed when its transcript changed).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpOutcome {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created: Vec<Id>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<Id>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<Id>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub summary: String,
    /// Structured results of some ops (`set_spacing`/`close_gaps`: `gaps`; `generate_captions`:
    /// `created`, `replaced`, `kept`, `skipped`); `null` otherwise.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub details: Value,
}

/// Why an edit was refused. The message says what is wrong and how to fix it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EditError {
    pub message: String,
    /// The op's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    /// Position of the failing op in a batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
}

impl EditError {
    pub fn new(message: impl Into<String>) -> Self {
        EditError { message: message.into(), op: None, index: None }
    }
    pub fn with_op(mut self, op: &str) -> Self {
        if self.op.is_none() {
            self.op = Some(op.to_string());
        }
        self
    }
    pub fn at(mut self, index: usize) -> Self {
        self.index = Some(index);
        self
    }
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.index, &self.op) {
            (Some(i), Some(op)) => write!(f, "op #{i} ({op}): {}", self.message),
            (Some(i), None) => write!(f, "op #{i}: {}", self.message),
            (None, Some(op)) => write!(f, "{op}: {}", self.message),
            (None, None) => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for EditError {}

pub(crate) fn fail<T>(message: impl Into<String>) -> Result<T, EditError> {
    Err(EditError::new(message))
}

// ---------------------------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------------------------

/// Every id in the project with what it names.
pub fn all_ids(p: &Project) -> impl Iterator<Item = (&str, &'static str)> {
    p.media
        .iter()
        .map(|m| (m.id.as_str(), "media asset"))
        .chain(p.tracks.iter().map(|t| (t.id.as_str(), "track")))
        .chain(p.clips.iter().map(|c| (c.id.as_str(), "clip")))
        .chain(p.transitions.iter().map(|t| (t.id.as_str(), "transition")))
        .chain(p.markers.iter().map(|m| (m.id.as_str(), "marker")))
        .chain(p.ducking.iter().map(|d| (d.id.as_str(), "ducking")))
        .chain(p.suggestions.iter().map(|s| (s.id.as_str(), "suggestion")))
}

/// The ops that apply suggestion `id`: its own ops, then removing it — one step for [`apply_batch`].
pub fn suggestion_ops(p: &Project, id: &str) -> Result<Vec<Op>, EditError> {
    let s = p.suggestions.iter().find(|s| s.id == id).ok_or_else(|| {
        let known: Vec<&str> = p.suggestions.iter().map(|s| s.id.as_str()).collect();
        EditError::new(if known.is_empty() { format!("there is no suggestion {id}: none are pending.") } else { format!("there is no suggestion {id} (pending: {}).", known.join(", ")) })
    })?;
    let mut ops = s.ops.clone();
    ops.push(Op::RemoveSuggestion { suggestion: id.to_string() });
    Ok(ops)
}

/// Allocates fresh ids (`prefix` + number above every existing one, so deleted ids are never reused)
/// and validates caller-supplied ones. Ids are unique across all id spaces.
pub struct IdGen {
    used: HashMap<String, &'static str>,
    next: HashMap<String, u64>,
    counters: BTreeMap<String, u64>,
}

/// Splits an id into its prefix and trailing number (`"mk12"` → `("mk", 12)`).
pub fn id_number(id: &str) -> Option<(&str, u64)> {
    let digits = id.bytes().rev().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits == id.len() || digits > 18 {
        return None;
    }
    let (prefix, n) = id.split_at(id.len() - digits);
    Some((prefix, n.parse().ok()?))
}

impl IdGen {
    pub fn new(p: &Project) -> Self {
        IdGen { used: all_ids(p).map(|(i, k)| (i.to_string(), k)).collect(), next: HashMap::new(), counters: p.id_counters.clone() }
    }

    /// A fresh id with the prefix: clips `c`, video/audio tracks `v`/`a`, media `m`, markers `mk`,
    /// transitions `tr`, duckings `d`.
    pub fn fresh(&mut self, prefix: &str) -> Id {
        let used = &self.used;
        let high = self.counters.get(prefix).copied().unwrap_or(0);
        let n = self.next.entry(prefix.to_string()).or_insert_with(|| {
            used.keys()
                .filter_map(|id| id.strip_prefix(prefix))
                .filter(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) && rest.len() < 18)
                .filter_map(|rest| rest.parse::<u64>().ok())
                .max()
                .unwrap_or(0)
                .max(high)
                + 1
        });
        loop {
            let id = format!("{prefix}{n}");
            *n += 1;
            if !self.used.contains_key(&id) {
                self.used.insert(id.clone(), "new");
                return id;
            }
        }
    }

    /// (added) An id for a new piece of clip `base_id` (the right half of a split): a named clip keeps its
    /// name, numbered as parts (`vo` → `vo-2`, `vo-2` → `vo-3`), so the pieces stay readable; generic
    /// ids (`c12`) get the next `c` id as before. Numbers of deleted pieces are not reissued.
    pub fn fresh_piece(&mut self, base_id: &str) -> Id {
        if matches!(id_number(base_id), Some(("c", _))) {
            return self.fresh("c");
        }
        let base = match base_id.rsplit_once('-') {
            Some((b, n)) if !b.is_empty() && !n.is_empty() && n.len() < 18 && n.bytes().all(|x| x.is_ascii_digit()) => b,
            _ => base_id,
        };
        if base.len() > 56 || valid_id(base).is_err() {
            return self.fresh("c");
        }
        let prefix = format!("{base}-");
        let mut n = self.counters.get(&prefix).copied().unwrap_or(0).max(1) + 1;
        loop {
            let id = format!("{prefix}{n}");
            if !self.used.contains_key(&id) {
                self.used.insert(id.clone(), "new");
                return id;
            }
            n += 1;
        }
    }

    /// Ids handed out by [`IdGen::fresh`] or reserved by [`IdGen::claim`].
    pub fn created(&self) -> impl Iterator<Item = &str> {
        self.used.iter().filter(|(_, k)| **k == "new").map(|(i, _)| i.as_str())
    }

    /// Reserves a caller-supplied id, or explains why it cannot be used.
    pub fn claim(&mut self, id: &str) -> Result<(), EditError> {
        valid_id(id)?;
        if let Some(kind) = self.used.get(id) {
            return fail(format!("the id {id:?} is already used by a {kind}; ids are unique across the whole project. Choose another id or omit it to get a fresh one."));
        }
        self.used.insert(id.to_string(), "new");
        Ok(())
    }
}

/// A fresh id (`"c1"`, `"c2"`, …) unique across all id spaces of the project.
pub fn new_id(p: &Project, prefix: &str) -> Id {
    IdGen::new(p).fresh(prefix)
}

/// Ids are 1–64 characters of `A-Z a-z 0-9 _ - .`.
pub fn valid_id(id: &str) -> Result<(), EditError> {
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.') {
        return fail(format!("invalid id {id:?}: use 1–64 letters, digits, '_', '-' or '.' (e.g. \"intro_title\")."));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Applying
// ---------------------------------------------------------------------------------------------

/// Applies one op. Validated; on error the project is unchanged.
pub fn apply(p: &mut Project, op: &Op) -> Result<OpOutcome, EditError> {
    apply_as(p, op, "")
}

/// [`apply`] by a named author (`"ai"`, `"user"`, …): caption edits by people ([`is_human_author`]) are
/// remembered so `generate_captions` keeps them.
pub fn apply_as(p: &mut Project, op: &Op, author: &str) -> Result<OpOutcome, EditError> {
    let mut v = apply_batch_as(p, std::slice::from_ref(op), author).map_err(|mut e| {
        e.index = None;
        e
    })?;
    Ok(v.remove(0))
}

/// Applies ops in order, atomically: either all succeed or the project is unchanged.
///
/// Each op validates its own inputs as it runs, but the whole-project checks (overlaps, unresolved
/// anchors, media ranges, locked tracks, …) run once on the final state against the original, so a
/// later op may repair what an earlier one left (e.g. `[set_transcript, move_clip]`). A remaining
/// problem names the first op after which it appeared.
pub fn apply_batch(p: &mut Project, ops: &[Op]) -> Result<Vec<OpOutcome>, EditError> {
    apply_batch_as(p, ops, "")
}

/// [`apply_batch`] by a named author.
pub fn apply_batch_as(p: &mut Project, ops: &[Op], author: &str) -> Result<Vec<OpOutcome>, EditError> {
    let original = p.clone();
    let r0 = resolve(&original);
    let mut work = p.clone();
    let mut outs = Vec::with_capacity(ops.len());
    let mut first_seen: HashMap<String, usize> = HashMap::new();
    for (i, op) in ops.iter().enumerate() {
        let (out, issues) = apply_one(&mut work, op, author).map_err(|e| e.with_op(op.name()).at(i))?;
        for is in issues {
            first_seen.entry(is.key).or_insert(i);
        }
        outs.push(out);
    }
    let rf = resolve(&work);
    let d = diff(&original, &work);
    let remaining = issues(&original, &r0, &work, &rf, &d);
    if let Some(first) = remaining.first() {
        let i = first_seen.get(&first.key).copied().unwrap_or(ops.len().saturating_sub(1));
        return Err(EditError::new(issue_message(&remaining)).with_op(ops.get(i).map(|o| o.name()).unwrap_or("")).at(i));
    }
    *p = work;
    Ok(outs)
}

/// Applies an op to `p` in place and reports the whole-project problems it introduced (relative to the
/// state before it). On error `p` may be partly modified (callers work on a copy).
fn apply_one(p: &mut Project, op: &Op, author: &str) -> Result<(OpOutcome, Vec<Issue>), EditError> {
    let before = p.clone();
    let rb = resolve(&before);
    let caps = captions::collect_caps(&before, &rb);
    let mut cx = Cx {
        p,
        ids: IdGen::new(&before),
        warnings: Vec::new(),
        summary: String::new(),
        made: Vec::new(),
        notes: Vec::new(),
        author: author.to_string(),
        details: Value::Null,
    };
    cx.run(op)?;
    let (retimed, dropped) = cx.resync_captions(&before, &rb, &caps)?;
    cx.drop_broken_transitions(&before, &rb);
    cx.drop_lost_mattes(&before);
    cx.normalize_until();
    cx.fill_word_texts(&before);
    if is_human_author(&cx.author) {
        cx.note_human_caption_changes(op, &before, &caps, &dropped);
    }
    let Cx { p, ids, mut warnings, mut summary, made, notes, details, .. } = cx;
    // Remember the highest number used per id prefix so deleted ids are never handed out again.
    for id in ids.created() {
        if let Some((prefix, n)) = id_number(id) {
            let e = p.id_counters.entry(prefix.to_string()).or_insert(0);
            if n > *e {
                *e = n;
            }
        }
    }
    if !retimed.is_empty() {
        summary.push_str(&format!("; re-timed captions {}", util::join_limited(&retimed, 12)));
    }
    if !dropped.is_empty() {
        summary.push_str(&format!("; removed captions {}", util::join_limited(&dropped, 12)));
    }
    let ra = resolve(p);
    let mut d = diff(&before, p);
    let found = issues(&before, &rb, p, &ra, &d);
    d.created.sort_by_key(|id| made.iter().position(|m| m == id).unwrap_or(usize::MAX));
    // Re-anchoring notes are written against the final project, so they name the new anchor.
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut lines: Vec<String> = Vec::new();
    for (id, why) in &notes {
        let line = prims::reanchor_note(p, &ra, id, why);
        match (seen.get(id.as_str()), line) {
            (Some(&k), Some(l)) => lines[k] = l,
            (None, Some(l)) => {
                seen.insert(id, lines.len());
                lines.push(l);
            }
            _ => {}
        }
    }
    warnings.extend(lines);
    // Never name ids that no longer exist: warnings must only mention live items, summaries must not
    // mention pieces that were created and removed inside the op.
    let alive: HashSet<&str> = all_ids(p).map(|(i, _)| i).collect();
    let mut gone_any: HashMap<String, &'static str> = all_ids(&before).filter(|(i, _)| !alive.contains(i)).map(|(i, k)| (i.to_string(), k)).collect();
    let gone_new: HashMap<String, &'static str> = ids.created().filter(|i| !alive.contains(i)).map(|i| (i.to_string(), "clip")).collect();
    gone_any.extend(gone_new.iter().map(|(k, v)| (k.clone(), *v)));
    let warnings = warnings.iter().map(|w| scrub_ids(w, &gone_any)).collect();
    let summary = scrub_ids(&summary, &gone_new);
    Ok((OpOutcome { created: d.created, changed: d.changed, removed: d.removed, warnings, summary, details }, found))
}

/// Replaces id-like whole words (containing a digit, `_`, `-` or `.`) that are in `gone` by
/// "a removed <kind>". Plain words are left alone even if an id has the same spelling (`a`, `hook`).
pub(crate) fn scrub_ids(text: &str, gone: &HashMap<String, &'static str>) -> String {
    if gone.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut tok = String::new();
    let flush = |tok: &mut String, out: &mut String| {
        if tok.is_empty() {
            return;
        }
        let core = tok.trim_end_matches(['.', '-']);
        let id_like = core.bytes().any(|b| b.is_ascii_digit() || b == b'_' || b == b'-' || b == b'.');
        match gone.get(core).filter(|_| id_like) {
            Some(kind) => {
                out.push_str("a removed ");
                out.push_str(kind);
                out.push_str(&tok[core.len()..]);
            }
            None => out.push_str(tok),
        }
        tok.clear();
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' || ch == '-' {
            tok.push(ch);
        } else {
            flush(&mut tok, &mut out);
            out.push(ch);
        }
    }
    flush(&mut tok, &mut out);
    out
}

/// What changed between two versions of a project, as an outcome (ids created/changed/removed of every
/// kind; empty summary and warnings).
pub fn diff_projects(before: &Project, after: &Project) -> OpOutcome {
    let d = diff(before, after);
    OpOutcome { created: d.created, changed: d.changed, removed: d.removed, warnings: Vec::new(), summary: String::new(), details: Value::Null }
}

/// Clips linked to `clip` in either direction: its detached-audio followers (same media, in-point,
/// duration and speed, anchored `with` it at offset 0) and, when `clip` is itself such a follower, its
/// leader and the leader's other followers. Split, trim, slip, speed, delete and cut keep these in sync.
pub fn linked_partners(p: &Project, clip: &str) -> Vec<Id> {
    let mut out = prims::linked_of(p, clip);
    if let Some(c) = p.clips.iter().find(|c| c.id == clip) {
        if let Time::Anchored(Anchor { to: AnchorTarget::With(leader), .. }) = &c.start {
            let followers = prims::linked_of(p, leader);
            if followers.iter().any(|f| f == clip) {
                out.push(leader.clone());
                out.extend(followers.into_iter().filter(|f| f != clip && !out.contains(f)).collect::<Vec<_>>());
            }
        }
    }
    out
}

/// The followers an edit of `clip` carries along (one direction of [`linked_partners`]).
pub fn linked_clips(p: &Project, clip: &str) -> Vec<Id> {
    prims::linked_of(p, clip)
}

/// Paths `set_param` accepts with `value` only (not animatable).
pub const SCALAR_PARAM_PATHS: &[&str] = &["fade_in", "fade_out", "transform.anchor_x", "transform.anchor_y", "transform.perspective"];

/// Every animatable parameter path of a clip that `set_param` / `add_keyframe` / `remove_keyframe`
/// accept: opacity, volume, speed, transform.x|y|scale|rotation|rotation_x|rotation_y, transform.crop.left|top|right|bottom,
/// source.draw (shapes) and `effects.N.<param>` for every registry parameter of each known effect
/// (whether set or at its default). See also [`SCALAR_PARAM_PATHS`].
pub fn param_paths(c: &Clip) -> Vec<String> {
    let mut v: Vec<String> = [
        "opacity",
        "volume",
        "speed",
        "transform.x",
        "transform.y",
        "transform.scale",
        "transform.rotation",
        "transform.rotation_x",
        "transform.rotation_y",
        "transform.crop.left",
        "transform.crop.top",
        "transform.crop.right",
        "transform.crop.bottom",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Source::Shape(_) = c.source {
        v.push("source.draw".into());
    }
    for (i, e) in c.effects.iter().enumerate() {
        if let Some(spec) = crate::effects::effect_spec(&e.kind) {
            v.extend(spec.params.iter().map(|p| format!("effects.{i}.{}", p.name)));
        }
    }
    v
}

/// The clip's animatable parameters that have a value, with their paths (effect parameters only when
/// set on the effect; missing ones use the registry default).
pub fn clip_params(c: &Clip) -> Vec<(String, &Param)> {
    let t = &c.transform;
    let mut v: Vec<(String, &Param)> = vec![
        ("opacity".into(), &c.opacity),
        ("volume".into(), &c.volume),
        ("speed".into(), &c.speed),
        ("transform.x".into(), &t.x),
        ("transform.y".into(), &t.y),
        ("transform.scale".into(), &t.scale),
        ("transform.rotation".into(), &t.rotation),
        ("transform.rotation_x".into(), &t.rotation_x),
        ("transform.rotation_y".into(), &t.rotation_y),
        ("transform.crop.left".into(), &t.crop.left),
        ("transform.crop.top".into(), &t.crop.top),
        ("transform.crop.right".into(), &t.crop.right),
        ("transform.crop.bottom".into(), &t.crop.bottom),
    ];
    if let Source::Shape(s) = &c.source {
        v.push(("source.draw".into(), &s.draw));
    }
    for (i, e) in c.effects.iter().enumerate() {
        for (n, p) in &e.params {
            v.push((format!("effects.{i}.{n}"), p));
        }
    }
    v
}

/// Working state of one op.
pub(crate) struct Cx<'a> {
    pub p: &'a mut Project,
    pub ids: IdGen,
    pub warnings: Vec<String>,
    pub summary: String,
    /// Ids of the op's main new items, in creation order (listed first in `OpOutcome::created`).
    pub made: Vec<Id>,
    /// `(id, why)`: items whose anchor had to change; turned into warnings that name the final anchor.
    pub notes: Vec<(Id, String)>,
    /// Who made the edit ("" when unknown); edits by people ([`is_human_author`]) mark captions as edited.
    pub author: String,
    /// Structured results for `OpOutcome::details`.
    pub details: Value,
}

/// True for authors that are AI agents ("ai", "ai:…", "claude…", "…agent…", "plan").
pub fn is_ai_author(author: &str) -> bool {
    let a = author.trim().to_ascii_lowercase();
    a == "ai" || a.starts_with("ai:") || a.starts_with("ai-") || a.starts_with("claude") || a.contains("agent") || a == "plan"
}

/// True for authors that are people: `"user"` (what the app's UI uses), `"human"`, `"person"`, or
/// `"user:<name>"` / `"human:<name>"`. Their caption edits and deletions are remembered (the `edited`
/// tag, `caption_exclusions`) so `generate_captions` keeps them. Every other author (AIs, `"cli"`,
/// `"demo"`, scripts, `""`) is treated as automation.
pub fn is_human_author(author: &str) -> bool {
    let a = author.trim().to_ascii_lowercase();
    matches!(a.as_str(), "user" | "human" | "person") || a.starts_with("user:") || a.starts_with("human:")
}

impl Cx<'_> {
    pub fn warn(&mut self, w: impl Into<String>) {
        self.warnings.push(w.into());
    }

    /// Clips whose matte clip was removed by this op are drawn without a matte again (with a warning).
    pub(crate) fn drop_lost_mattes(&mut self, before: &Project) {
        let ids: HashSet<Id> = self.p.clips.iter().map(|c| c.id.clone()).collect();
        let existed: HashSet<&str> = before.clips.iter().map(|c| c.id.as_str()).collect();
        let mut lost: Vec<Id> = Vec::new();
        for c in self.p.clips.iter_mut() {
            if c.matte.as_ref().is_some_and(|m| !ids.contains(&m.clip) && existed.contains(m.clip.as_str())) {
                c.matte = None;
                lost.push(c.id.clone());
            }
        }
        for id in lost {
            self.warn(format!("{id}'s matte clip was removed, so {id} is drawn without a matte now."));
        }
    }

    /// Removes transitions whose clips touched before this op but no longer do (they would render
    /// across a gap or an unrelated cut).
    pub(crate) fn drop_broken_transitions(&mut self, before: &Project, rb: &Resolved) {
        if self.p.transitions.is_empty() {
            return;
        }
        let touching = |p: &Project, r: &Resolved, t: &Transition| -> bool {
            let (Some(a), Some(b)) = (p.clips.iter().find(|c| c.id == t.from), p.clips.iter().find(|c| c.id == t.to)) else { return false };
            a.track == b.track && matches!((r.span(&a.id), r.span(&b.id)), (Some(x), Some(y)) if (x.end - y.start).abs() <= 1e-3)
        };
        let ok_before: HashSet<&str> = before.transitions.iter().filter(|t| touching(before, rb, t)).map(|t| t.id.as_str()).collect();
        let ra = resolve(self.p);
        let mut gone: Vec<(Id, String)> = Vec::new();
        for t in &self.p.transitions {
            if ok_before.contains(t.id.as_str()) && !touching(self.p, &ra, t) {
                let both = self.p.clips.iter().any(|c| c.id == t.from) && self.p.clips.iter().any(|c| c.id == t.to);
                let why = if both {
                    format!("removed a {} transition between {} and {}: they no longer touch.", t.kind, t.from, t.to)
                } else {
                    format!("removed a {} transition whose clip was removed.", t.kind)
                };
                gone.push((t.id.clone(), why));
            }
        }
        for (id, why) in gone {
            self.p.transitions.retain(|t| t.id != id);
            self.warn(why);
        }
    }
}

#[derive(Default)]
struct Diff {
    created: Vec<Id>,
    changed: Vec<Id>,
    removed: Vec<Id>,
    /// Clip ids created or changed.
    clips_touched: Vec<Id>,
}

fn diff_list<T: PartialEq>(b: &[T], a: &[T], id: impl Fn(&T) -> &str, d: &mut Diff, clips: bool) {
    let bm: HashMap<&str, &T> = b.iter().map(|x| (id(x), x)).collect();
    let am: HashSet<&str> = a.iter().map(&id).collect();
    for x in a {
        match bm.get(id(x)) {
            None => {
                d.created.push(id(x).to_string());
                if clips {
                    d.clips_touched.push(id(x).to_string());
                }
            }
            Some(old) if *old != x => {
                d.changed.push(id(x).to_string());
                if clips {
                    d.clips_touched.push(id(x).to_string());
                }
            }
            _ => {}
        }
    }
    for x in b {
        if !am.contains(id(x)) {
            d.removed.push(id(x).to_string());
        }
    }
}

fn diff(b: &Project, a: &Project) -> Diff {
    let mut d = Diff::default();
    diff_list(&b.media, &a.media, |x| &x.id, &mut d, false);
    diff_list(&b.tracks, &a.tracks, |x| &x.id, &mut d, false);
    diff_list(&b.clips, &a.clips, |x| &x.id, &mut d, true);
    diff_list(&b.transitions, &a.transitions, |x| &x.id, &mut d, false);
    diff_list(&b.markers, &a.markers, |x| &x.id, &mut d, false);
    diff_list(&b.ducking, &a.ducking, |x| &x.id, &mut d, false);
    diff_list(&b.suggestions, &a.suggestions, |x| &x.id, &mut d, false);
    let keys: HashSet<&String> = b.transcripts.keys().chain(a.transcripts.keys()).collect();
    let mut keys: Vec<&String> = keys.into_iter().collect();
    keys.sort();
    for k in keys {
        if b.transcripts.get(k) != a.transcripts.get(k) && !d.changed.contains(k) && !d.created.contains(k) && !d.removed.contains(k) {
            d.changed.push(k.clone());
        }
    }
    d
}

/// Overlapping clip pairs per track (ordered by id) with their spans.
pub(crate) fn overlaps(p: &Project, r: &Resolved) -> HashMap<(Id, Id), (Id, crate::resolve::Span, crate::resolve::Span)> {
    let mut by_track: HashMap<&str, Vec<(&str, crate::resolve::Span)>> = HashMap::new();
    for c in &p.clips {
        if let Some(s) = r.span(&c.id) {
            by_track.entry(c.track.as_str()).or_default().push((c.id.as_str(), s));
        }
    }
    let mut out = HashMap::new();
    for (track, mut v) in by_track {
        v.sort_by(|a, b| a.1.start.total_cmp(&b.1.start));
        for i in 0..v.len() {
            for j in i + 1..v.len() {
                if v[j].1.start >= v[i].1.end - EPS {
                    break;
                }
                if v[i].1.overlaps(&v[j].1) {
                    let (a, b) = if v[i].0 < v[j].0 { (v[i], v[j]) } else { (v[j], v[i]) };
                    out.insert((a.0.to_string(), b.0.to_string()), (track.to_string(), a.1, b.1));
                }
            }
        }
    }
    out
}

/// A whole-project problem found after an edit: a stable key (to tell which op introduced it), its kind
/// and the message.
#[derive(Clone, Debug)]
pub(crate) struct Issue {
    key: String,
    kind: &'static str,
    message: String,
}

fn issue(key: String, kind: &'static str, message: String) -> Issue {
    Issue { key, kind, message }
}

/// Two times printed with just enough decimals (2–6) to tell them apart.
pub(crate) fn distinct_secs(a: f64, b: f64) -> (String, String) {
    for d in 2..=6 {
        let (x, y) = (format!("{a:.d$}"), format!("{b:.d$}"));
        if x != y || (a - b).abs() < 1e-9 {
            return (x, y);
        }
    }
    (format!("{a:.6}"), format!("{b:.6}"))
}

/// The message for the first kind of problem in `issues`.
fn issue_message(issues: &[Issue]) -> String {
    let Some(first) = issues.first() else { return String::new() };
    let same: Vec<&Issue> = issues.iter().filter(|i| i.kind == first.kind).collect();
    match first.kind {
        "anchor" => {
            let roots: Vec<&str> = same.iter().filter(|i| !i.key.starts_with("blocked:")).map(|i| i.message.as_str()).collect();
            let msgs: Vec<&str> = if roots.is_empty() { same.iter().map(|i| i.message.as_str()).collect() } else { roots.clone() };
            let mut m = format!("this edit would leave unresolved anchors, so nothing was changed: {}", util::join_limited(&msgs, 4));
            let blocked = same.len() - roots.len();
            if blocked > 0 && !roots.is_empty() {
                m.push_str(&format!(" ({blocked} more item(s) anchored to those would become unplaceable.)"));
            }
            m
        }
        "overlap" => {
            let mut m = first.message.clone();
            if same.len() > 1 {
                m.push_str(&format!(" ({} more overlapping pair(s).)", same.len() - 1));
            }
            m
        }
        _ => first.message.clone(),
    }
}

/// Every whole-project problem `after` has that `before` did not, most important first.
fn issues(before: &Project, rb: &Resolved, after: &Project, ra: &Resolved, d: &Diff) -> Vec<Issue> {
    let mut out: Vec<Issue> = Vec::new();
    // 1. No new unresolved anchors: an item that could be placed before must still be placeable (items
    //    that were already broken may change how they are broken, unless this edit changed their time).
    let old: HashMap<&str, (crate::resolve::ResolveErrorKind, Option<&str>)> = rb.errors.iter().map(|e| (e.id.as_str(), (e.kind, e.target.as_deref()))).collect();
    let time_of = |p: &Project, id: &str| -> Option<(Time, Option<Time>)> {
        p.clips.iter().find(|c| c.id == id).map(|c| (c.start.clone(), c.until.clone())).or_else(|| p.markers.iter().find(|m| m.id == id).map(|m| (m.time.clone(), None)))
    };
    for e in &ra.errors {
        let new = match old.get(e.id.as_str()) {
            None => true,
            Some(&(k, t)) => (k != e.kind || t != e.target.as_deref()) && time_of(before, &e.id) != time_of(after, &e.id),
        };
        if new {
            let prefix = if e.kind == crate::resolve::ResolveErrorKind::Blocked { "blocked:" } else { "anchor:" };
            out.push(issue(format!("{prefix}{}", e.id), "anchor", e.message.clone()));
        }
    }
    // 1b. Every number stays finite (a project that cannot be saved is never produced).
    if let Some(what) = prims::first_non_finite(after) {
        if prims::first_non_finite(before).is_none() {
            out.push(issue("nonfinite".into(), "values", format!("this edit would make {what} not a finite number; use finite values.")));
        }
    }
    // 2. Created/changed clips must be valid (only problems the clip did not already have count).
    let before_clips: HashMap<&str, &Clip> = before.clips.iter().map(|c| (c.id.as_str(), c)).collect();
    let after_clips: HashMap<&str, &Clip> = after.clips.iter().map(|c| (c.id.as_str(), c)).collect();
    for id in &d.clips_touched {
        let Some(c) = after_clips.get(id.as_str()) else { continue };
        let probs = prims::clip_problems(after, c);
        if probs.is_empty() {
            continue;
        }
        let old: HashSet<String> = before_clips.get(id.as_str()).map(|b| prims::clip_problems(before, b).into_iter().map(|(k, _)| k).collect()).unwrap_or_default();
        for (k, m) in probs.into_iter().filter(|(k, _)| !old.contains(k)) {
            out.push(issue(format!("clip:{id}:{k}"), "clip", m));
        }
    }
    // 3. No new overlaps on a track (frame-accurate on video tracks).
    let ob = overlaps(before, rb);
    let oa = overlaps(after, ra);
    let mut new_overlaps: Vec<_> = oa.iter().filter(|(k, _)| !ob.contains_key(*k)).collect();
    new_overlaps.sort_by(|a, b| a.0.cmp(b.0));
    for ((a, b), (track, sa, sb)) in new_overlaps {
        let ((first, s1), (second, s2)) = if sa.start <= sb.start { ((a, sa), (b, sb)) } else { ((b, sb), (a, sa)) };
        let (e1, b2) = distinct_secs(s1.end, s2.start);
        let (b1, e2) = (distinct_secs(s1.start, s1.end).0, distinct_secs(s2.end, s2.start).0);
        out.push(issue(
            format!("overlap:{a}:{b}"),
            "overlap",
            format!(
                "{first} [{b1}–{e1}] would overlap {second} [{b2}–{e2}] on track {track} by {} s. Trim or move one of them, put the clip on another track, use overwrite: true (add_clip/move_clip/duplicate_clip) or insert_clip to push later clips right.",
                distinct_secs(s1.end - s2.start, 0.0).0
            ),
        ));
    }
    // 4. Locked tracks do not change.
    let locked: HashSet<&str> = before.tracks.iter().filter(|t| t.locked).map(|t| t.id.as_str()).collect();
    if !locked.is_empty() {
        let unlock = |t: &str| format!("unlock it first with {{\"op\":\"set_track\",\"track\":\"{t}\",\"locked\":false}}");
        for c in &before.clips {
            if !locked.contains(c.track.as_str()) {
                continue;
            }
            let key = format!("locked:{}", c.id);
            match after_clips.get(c.id.as_str()) {
                None => out.push(issue(key, "locked", format!("track {} is locked, but this edit would delete {}; {}.", c.track, c.id, unlock(&c.track)))),
                Some(a) if *a != c => out.push(issue(key, "locked", format!("track {} is locked, but this edit would change {}; {}.", c.track, c.id, unlock(&c.track)))),
                Some(_) => {
                    let (sb, sa) = (rb.span(&c.id), ra.span(&c.id));
                    let moved = match (sb, sa) {
                        (Some(x), Some(y)) => (x.start - y.start).abs() > EPS || (x.end - y.end).abs() > EPS,
                        (None, None) => false,
                        _ => true,
                    };
                    if moved {
                        out.push(issue(
                            key,
                            "locked",
                            format!(
                                "track {} is locked, but this edit would move {} (it is anchored {}); {}, or re-anchor {} so it does not depend on what you are editing.",
                                c.track,
                                c.id,
                                match &c.start {
                                    Time::Anchored(a) => crate::resolve::describe_anchor(a),
                                    Time::At(_) => "absolutely".into(),
                                },
                                unlock(&c.track),
                                c.id
                            ),
                        ));
                    }
                }
            }
        }
        for c in &after.clips {
            if locked.contains(c.track.as_str()) && !before_clips.contains_key(c.id.as_str()) {
                out.push(issue(format!("locked:{}", c.id), "locked", format!("track {} is locked, so {} cannot be added to it; {}.", c.track, c.id, unlock(&c.track))));
            }
            if let Some(b) = before_clips.get(c.id.as_str()) {
                if b.track != c.track && locked.contains(c.track.as_str()) {
                    out.push(issue(format!("locked:{}", c.id), "locked", format!("track {} is locked, so {} cannot be moved onto it; {}.", c.track, c.id, unlock(&c.track))));
                }
            }
        }
    }
    // 5. Nothing new starts before 0.
    for c in &after.clips {
        let Some(sa) = ra.span(&c.id) else { continue };
        if sa.start < -EPS {
            let was = rb.span(&c.id).map(|s| (s.start - sa.start).abs() <= EPS).unwrap_or(false);
            if !was {
                out.push(issue(
                    format!("negative:{}", c.id),
                    "negative",
                    format!("{} would start at {} s, before the start of the timeline. Move it later or reduce the edit (e.g. a smaller delta).", c.id, util::secs(sa.start)),
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
