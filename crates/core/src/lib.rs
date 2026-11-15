//! AIFrames core: the project model and everything that can be decided without touching media.
//!
//! * [`model`] — the serialisable project (the single source of truth).
//! * [`resolve`] — anchors (after/with/before a clip, at a marker, at a spoken word) → absolute spans.
//! * [`eval`] — easing, parameter sampling, speed integration (clip-local time everywhere), fades.
//! * [`effects`] — the effect and transition registries (labels, defaults, ranges, help).
//! * [`ops`] — every edit as a typed, serialisable [`Op`]; atomic, validated, with outcomes.
//! * [`document`] — an open project: save/open, history with authors, undo/redo, dry-run receipts.
//! * [`validate`] — findings with explanations and machine-applicable fixes.
//! * [`describe`] — the edit as compact text / JSON for an AI.
//! * [`schema`] — field-level documentation of the project JSON ([`schema_docs`]).
//! * [`words`] — transcript tokens as readable text ("$2" ".11" → "$2.11"; [`join_words`]).
//!
//! See `ARCHITECTURE.md` for the design.

pub mod describe;
pub mod document;
pub mod effects;
pub mod eval;
pub mod model;
pub mod otio;
pub mod ops;
pub mod resolve;
pub mod schema;
pub mod validate;
pub mod words;

mod util;

#[cfg(test)]
mod testutil;

pub use describe::{describe, describe_with, timeline_json, tracks_in_display_order, until_text, DescribeOptions, AUDIO_TRACKS_HEADING, VIDEO_TRACKS_HEADING};
pub use document::{DocError, Document, FileStamp, HistoryEntry, HistoryKind, Moved, Receipt};
pub use effects::{
    clip_param_spec, effect_color, effect_param, effect_spec, transition_color, transition_param, transition_spec, EffectSpec, ParamSpec, TransitionSpec, CLIP_PARAMS, DUCKING_PARAMS, EFFECTS, TRANSITIONS,
};
pub use eval::{ease, fade_gain, local_time_of_source, sample, source_span, source_time};
pub use model::*;
pub use ops::{
    apply, apply_as, apply_batch, apply_batch_as, clip_params, is_ai_author, is_human_author, diff_projects, linked_clips, linked_partners, new_id, param_paths, CaptionStyle, ClipMove, Edge, EditError, EffectArg, Op, OpOutcome,
    OpSpec, SpeedKeep, OPS, SCALAR_PARAM_PATHS, suggestion_ops,
};
pub use resolve::{frame_rate, resolve, resolve_raw, snap_to_frame, ResolveError, ResolveErrorKind, Resolved, Span};
pub use schema::schema_docs;
pub use words::{join_words, word_units};
pub use validate::{validate, Finding, Severity};
