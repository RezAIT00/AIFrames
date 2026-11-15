# AIFrames — architecture and build contract

AIFrames is a fast native Rust (egui) NLE that a person edits like
Premiere/Resolve and an AI edits through MCP while the person watches. It replaces "HTML compiled to
video" workflows (HyperFrames) with a real edit model designed for AI editing.

## Why (the lessons this design encodes)

An investigation of HyperFrames found that it generates video well but edits it badly:

1. **Timing stored twice.** Clip times lived in HTML attributes *and* in absolute GSAP tween positions.
   Moving a clip with the official edit tool left its animations behind; the checker still passed.
   → In AIFrames every animation, effect, fade, text reveal and caption timing is **clip-local**.
   A clip's position is stored exactly once (`Clip::start`).
2. **No intent.** Only absolute seconds, so every real pipeline grew its own plan language
   ("after shot X", "when the voice says *victory*") and compiled to HTML.
   → `Time` is absolute **or anchored**: `after`/`with`/`before` a clip, at a `marker`, at a transcript
   `word`. Chains of `after` make a magnetic storyline (ripple for free).
3. **Silent failures.** Unresolved references silently became 0, a lint error silently disabled checks.
   → `resolve` reports every unresolved/cyclic anchor as an error; ops refuse to create them;
   `validate` explains each finding with a fix (and machine-applicable fix ops where possible).
4. **Weak perception for AI.** Agents only had stills; renders took 25 min.
   → Same renderer for preview, snapshots and export; contact sheets, filmstrips around cuts, loudness
   and silence analysis, transcript search, a text timeline description — all as MCP tools.
5. **Noisy, fragile edits.** Edits reformatted files and stamped ids everywhere.
   → Compact canonical JSON with defaults omitted; ids are short and stable; every change is a typed
   `Op` with a receipt (what moved), atomic batches, dry-run, undo, and author attribution.

## Workspace layout (crates under `crates/`)

| crate | package | owns |
|---|---|---|
| `core/` | `aiframes-core` | model (done: `src/model.rs`), resolve, eval, effects registry, ops, document+history, validate, describe |
| `media/` | `aiframes-media` | ffmpeg/ffprobe discovery, probe, frame decoding + cache, images, audio decode + cache, waveforms, thumbnails, scene/silence/beat detection, loudness, transcription (whisper), encoder pipe |
| `render/` | `aiframes-render` | compositor (frame at t), text + shapes, effects, transitions, audio mixer (fades, automation, ducking, speed), export, snapshots, contact sheets |
| `ui/` | `aiframes-ui` | the egui editing widgets: timeline, inspector, effects browser, transcript editor, history, media bin (API fixed in `ui/src/lib.rs`) |
| `mcp/` | `aiframes-mcp` | `suite_mcp::Backend` impl: AI tools over the open documents; headless CLI commands |
| `app/` | `aiframes-app` (binary `aiframes`) | the GUI: viewer + playback, media bin, inspector, effects, transcript editor, export, history/AI activity, AI prompt panel; `aiframes mcp` and CLI subcommands; file association |

Shared crates under `shared/` (read their sources): `shared/mcp` (`suite-mcp`: Backend, HTTP, stdio proxy,
`launch_gui`), `shared/hub` (`suite-hub`: document registry shared by GUI and MCP), `shared/ui`
(`suite-ui`: egui theme, fonts, recent files), `shared/shell` (file associations).

Dependency direction: `core` ← `media` ← `render` ← {`timeline`, `mcp`} ← `app`. `media` depends on
`core` only for model types (`MediaKind`, `MediaInfo`, `Transcript`). `timeline` depends on `core` and
`media` (thumbnails/waveforms), not on `render`.

## Core API (aiframes-core)

The model is fixed in `core/src/model.rs` (read it — it is the contract). Additions to the model must be
backwards compatible (new fields with serde defaults). Modules and public API:

```rust
// resolve.rs — anchors -> absolute times
pub struct Span { pub start: f64, pub end: f64 }            // timeline seconds, end exclusive
pub struct Resolved {
    pub clips: HashMap<Id, Span>,
    pub markers: HashMap<Id, f64>,
    pub duration: f64,                                       // settings.duration or end of last clip
    pub errors: Vec<ResolveError>,                           // never silent: unknown target, cycle, word cut out...
}
impl Resolved { pub fn span(&self, id: &str) -> Option<Span>; pub fn clips_at(&self, p: &Project, t: f64) -> Vec<&Clip> }
pub fn resolve(p: &Project) -> Resolved;                     // clips with errors are left out of `clips`
// Word anchors: time = clip.start + (word.start - in_point) mapped through the clip's speed (inverse of
// source_time), error if the word is outside the clip's source range.

// eval.rs — sampling (clip-local time everywhere)
pub fn ease(e: &Ease, x: f64) -> f64;                        // x in 0..1
pub fn sample(p: &Param, t_local: f64) -> f64;               // constant before first / after last key
pub fn source_time(clip: &Clip, t_local: f64) -> Option<f64>;  // media clips: in_point + ∫speed (reverse aware); None otherwise
pub fn source_span(clip: &Clip) -> Option<(f64, f64)>;       // source seconds consumed [min,max]
pub fn fade_gain(clip: &Clip, t_local: f64) -> f64;          // fade_in/out envelope 0..1
pub fn local_time_of_source(clip: &Clip, source_t: f64) -> Option<f64>; // inverse (for word anchors)

// effects.rs — data-driven registries (also drive the UI inspector and the MCP `list_effects` tool)
pub struct ParamSpec { pub name: &'static str, pub label: &'static str, pub default: f64, pub min: f64, pub max: f64, pub unit: &'static str, pub help: &'static str }
pub struct EffectSpec { pub kind: &'static str, pub label: &'static str, pub category: &'static str, pub params: &'static [ParamSpec], pub colors: &'static [(&'static str, &'static str)], pub strings: &'static [&'static str], pub help: &'static str }
pub struct TransitionSpec { pub kind: &'static str, pub label: &'static str, pub params: &'static [ParamSpec], pub help: &'static str }
pub static EFFECTS: &[EffectSpec];      // at least: color (exposure, contrast, saturation, temperature, tint, brightness, gamma, highlights, shadows),
                                        // blur, sharpen, vignette, chroma_key, mask_rect, mask_ellipse, drop_shadow, glow, pixelate,
                                        // grain (deterministic), lut (strings: path; intensity), black_white, invert, posterize, letterbox, zoom_blur
pub static TRANSITIONS: &[TransitionSpec]; // crossfade, dip_black, dip_white, wipe_left/right/up/down, slide_left/right/up/down,
                                        // push_left/right, zoom_in, zoom_out, iris, blur_dissolve, whip (motion-blurred push)
pub fn effect_spec(kind: &str) -> Option<&'static EffectSpec>;
pub fn effect_param(e: &Effect, name: &str, t_local: f64) -> f64;   // value or registry default
pub fn effect_color(e: &Effect, name: &str) -> Color;

// ops.rs — every edit is a typed, serialisable Op. JSON: {"op":"move_clip","clip":"c3","start":12.0}
#[serde(tag = "op", rename_all = "snake_case")] pub enum Op { ... }
pub struct OpOutcome { pub created: Vec<Id>, pub changed: Vec<Id>, pub removed: Vec<Id>, pub warnings: Vec<String>, pub summary: String }
pub fn apply(p: &mut Project, op: &Op) -> Result<OpOutcome, EditError>;   // validated; on error p is unchanged
pub fn apply_batch(p: &mut Project, ops: &[Op]) -> Result<Vec<OpOutcome>, EditError>;  // atomic
pub fn new_id(p: &Project, prefix: &str) -> Id;               // "c1", "c2", ... unique across all id spaces

// document.rs — an open project with history
pub struct Receipt { pub revision: u64, pub author: String, pub label: String, pub outcomes: Vec<OpOutcome>,
                     pub moved: Vec<Moved>, pub warnings: Vec<String>, pub new_errors: Vec<String> }
pub struct Moved { pub id: Id, pub before: Option<Span>, pub after: Option<Span> }   // every clip whose resolved span changed
pub struct Document { pub project: Project, pub path: Option<PathBuf>, ... }
impl Document {
    pub fn new(project) / open(path) -> Result / save() / save_as(path)      // canonical pretty JSON, atomic write
    pub fn apply(&mut self, author: &str, label: &str, ops: Vec<Op>) -> Result<Receipt, EditError>;
    pub fn plan(&self, ops: &[Op]) -> Result<Receipt, EditError>;           // dry run, same receipt
    pub fn undo(&mut self) -> Option<Receipt>; pub fn redo(&mut self) -> Option<Receipt>;
    pub fn history(&self) -> &[HistoryEntry];                                 // author, label, time, ops, revision
    pub fn changes_since(&self, revision: u64) -> Vec<&HistoryEntry>;         // "what did the human do since my last turn"
    pub fn check_base(&self, base_revision: u64, ops: &[Op]) -> Result<Option<AppliedOver>, EditError>;  // stale.rs: refuse ops planned
                                                                              // against an older revision when newer changes affect them
    pub fn revision(&self) -> u64; pub fn dirty(&self) -> bool; pub fn resolved(&self) -> &Resolved (cached)
    pub fn project_dir(&self) -> PathBuf; pub fn media_path(&self, asset: &MediaAsset) -> PathBuf  // absolute
}
// History is also appended to `<project>.aifr.history.jsonl` next to the project on save.

// validate.rs
pub enum Severity { Error, Warning, Info }
pub struct Finding { pub severity: Severity, pub code: &'static str, pub message: String, pub at: Option<f64>,
                     pub ids: Vec<Id>, pub fix: Option<String>, pub fix_ops: Vec<Op> }
pub fn validate(p: &Project, r: &Resolved) -> Vec<Finding>;
// codes include: unresolved_anchor, anchor_cycle, overlap_same_track, gap_in_main (black frames), clip_past_media_end,
// missing_media (asset id unknown), keyframe_outside_clip, text_outside_title_safe, content_outside_action_safe,
// transition_not_adjacent, transition_too_long, empty_track, duplicate_id, zero_length_clip, speed_out_of_range,
// unknown_effect, unknown_transition, ducking_track_missing, word_cut_out, past_fixed_duration

// describe.rs — the AI's eyes on structure
pub fn describe(p: &Project, r: &Resolved) -> String;          // compact text: tracks, clips with spans, anchors, sources, notes
pub fn timeline_json(p: &Project, r: &Resolved) -> serde_json::Value;  // clips with absolute start/end, track, source summary, anchor
```

**Ops (minimum set).** Every op validates its inputs (unknown ids, locked tracks, negative durations,
overlaps) and returns a clear error naming the problem and how to fix it.

- Project: `set_settings`, `set_project` (name, notes), `set_export`
- Media: `add_media {id?, path, kind, info, name?, notes?}`, `remove_media {asset, force}`, `relink_media {asset, path, info?}`, `set_media {asset, name?, notes?, tags?}`
- Tracks: `add_track {id?, kind, name?, role?, index?}`, `remove_track {track, force}`, `move_track {track, index}`, `set_track {track, name?, role?, muted?, hidden?, locked?, solo?, volume?}`
- Clips: `add_clip {clip: partial Clip JSON; id optional; duration defaults to remaining media / 3 s for stills/text}`,
  `append_clip {track, after?: clip, clip}` (anchors after the last clip of the track = magnetic),
  `insert_clip {track, at, clip, ripple: true}` (splits/pushes what is there),
  `move_clip {clip, start: Time, track?}`, `trim_clip {clip, edge: "start"|"end", to?: abs seconds, delta?: seconds, ripple?: bool}`
  (trimming the start advances in_point by delta×speed), `split_clip {clip, at: timeline seconds, new_id?}` (keys/effects split correctly,
  anchors pointing at the original's end move to the right half), `delete_clip {clip, ripple: bool}` (ripple: dependants re-anchor to the
  deleted clip's own start anchor; non-ripple: dependants get absolute times), `duplicate_clip {clip, start?, track?}`,
  `set_clip {clip, patch: JSON merge patch over the clip}` (generic escape hatch; re-validated), `set_param {clip, path, value?|keys?}`
  (paths: `opacity`, `volume`, `speed`, `transform.x|y|scale|rotation`, `transform.crop.left|...`, `effects.N.<param>`, `source.draw`),
  `add_keyframe {clip, path, t, v, ease?}`, `remove_keyframe {clip, path, t}`, `slip_clip {clip, delta}` (moves in_point only),
  `roll_edit {left, right, delta}`, `slide_clip {clip, delta}`, `set_speed {clip, speed, keep: "duration"|"source"}`, `freeze_frame {clip, at, duration}`,
  `detach_audio {clip, track}` (creates an audio clip anchored `with` the video clip and mutes the video's sound),
  `replace_media {clip, asset, in_point?}`, `lift_range {from, to, tracks?}` and `extract_range {from, to, tracks?}` (ripple),
  `nudge {clips, delta}`
- Effects: `add_effect {clip, effect, index?}`, `remove_effect {clip, index}`, `set_effect {clip, index, enabled?, params?, colors?, strings?}`, `move_effect`
- Transitions: `add_transition {from, to, kind, duration, id?}`, `set_transition`, `remove_transition`
- Markers: `add_marker {time, name?, kind?, id?}`, `set_marker`, `remove_marker`
- Audio: `add_ducking {target, trigger, amount_db?...}`, `set_ducking`, `remove_ducking`
- Transcript: `set_transcript {asset, transcript}`, `delete_words {clip, from, to}` (text-based editing: removes those words' source
  range from the clip by splitting and ripple-deleting, keeping a small configurable pad), `generate_captions {clips, track, style?, max_words?, max_chars?}`
  (creates text clips anchored to words so captions follow the voice; karaoke `highlight` filled in)

## Media API (aiframes-media)

```rust
pub struct Tools { pub ffmpeg: PathBuf, pub ffprobe: PathBuf, pub whisper: Option<PathBuf> }
pub fn tools() -> Result<&'static Tools, MediaError>;   // env AIFRAMES_FFMPEG / AIFRAMES_FFPROBE / AIFRAMES_WHISPER, then PATH
pub fn probe(path: &Path) -> Result<(MediaKind, MediaInfo), MediaError>;
pub struct Frame { pub width: u32, pub height: u32, pub rgba: Arc<Vec<u8>> /* straight RGBA8 */, pub time: f64 }
pub struct MediaPool;  // Send + Sync, shared via Arc by renderer, UI and MCP
impl MediaPool {
    pub fn new(cache_bytes: usize) -> Arc<Self>;
    pub fn video_frame(&self, path: &Path, t: f64, max: Option<(u32, u32)>) -> Result<Frame, MediaError>; // frame displayed at source time t
    pub fn image(&self, path: &Path, max: Option<(u32, u32)>) -> Result<Frame, MediaError>;
    pub fn audio(&self, path: &Path, rate: u32) -> Result<Arc<AudioData>, MediaError>;   // decoded once to a disk cache, stereo f32
    pub fn waveform(&self, path: &Path) -> Result<Arc<Waveform>, MediaError>;            // min/max peaks, ~200 bins/s
    pub fn thumbnail(&self, path: &Path, t: f64, height: u32) -> Result<Frame, MediaError>;
}
impl AudioData { pub fn rate(&self) -> u32; pub fn duration(&self) -> f64; pub fn read(&self, start_sec: f64, out: &mut [f32]) /* stereo interleaved, zeros outside */ }
pub fn detect_scenes(path: &Path, threshold: f64) -> Result<Vec<f64>, MediaError>;
pub fn detect_silence(path: &Path, noise_db: f64, min_len: f64) -> Result<Vec<[f64; 2]>, MediaError>;
pub fn detect_beats(path: &Path) -> Result<Vec<f64>, MediaError>;
pub struct Loudness { pub integrated_lufs: f64, pub true_peak_db: f64, pub lra: f64, pub max_momentary_lufs: f64 }
pub fn loudness_of_pcm(stereo: &[f32], rate: u32) -> Loudness;   pub fn loudness_of_file(path: &Path) -> Result<Loudness, MediaError>;
pub fn transcribe(path: &Path, opts: &TranscribeOptions) -> Result<Transcript, MediaError>;   // word timestamps; progress callback
pub struct Encoder;  // ffmpeg child: raw RGBA frames on stdin + an audio WAV -> mp4/mov/gif; NVENC when asked and available
impl Encoder { pub fn start(opts: &EncodeOptions) -> Result<Self, MediaError>; pub fn push(&mut self, rgba: &[u8]) -> Result<(), MediaError>; pub fn finish(self) -> Result<(), MediaError> }
```
Sequential frame access (playback, export) must be fast: keep an ffmpeg decode session streaming forward and only reseek on
backward/large jumps; LRU frame cache bounded by bytes. Never block the UI thread: the UI calls these from worker threads.

## Render API (aiframes-render)

```rust
pub struct Image { pub width: u32, pub height: u32, pub rgba: Vec<u8> }   // straight RGBA8
pub struct FrameOptions { pub scale: f32 /* 1 = project size */, pub guides: bool, pub draft: bool }
pub struct Renderer;  // Send + Sync; caches fonts, glyphs, LUTs
impl Renderer {
    pub fn new(pool: Arc<MediaPool>) -> Self;
    pub fn frame(&self, doc_dir: &Path, p: &Project, r: &Resolved, t: f64, o: &FrameOptions) -> Result<Image, RenderError>;
    pub fn mix(&self, doc_dir: &Path, p: &Project, r: &Resolved, t0: f64, out: &mut [f32]) -> Result<(), RenderError>; // stereo interleaved at settings.sample_rate
}
pub fn export(doc_dir: &Path, p: &Project, pool: Arc<MediaPool>, s: &ExportSettings, out: &Path,
              progress: &mut dyn FnMut(ExportProgress) -> bool /* false = cancel */) -> Result<ExportReport, RenderError>;
pub fn contact_sheet(r: &Renderer, doc_dir: &Path, p: &Project, res: &Resolved, times: &[f64], cols: u32, thumb_w: u32) -> Result<Image, RenderError>; // labelled
pub fn encode_png(img: &Image) -> Vec<u8>;
```
The same `frame` function serves preview (scaled), snapshots and export, so what you see is what you get.

## Status and where the real APIs are

- `core` and `media` are **implemented and tested** (78 + 58 tests). Their real public APIs are in `core/src/lib.rs` (re-exports)
  and the module files, and `media/src/lib.rs`. Track ids are `v1`/`a1`, media `m1`, markers `mk1`, clips `c1`, transitions `tr1`.
  `Document` has `undo_by(author)`/`redo_by(author)`, `refresh()`, `validate()` (adds `media_offline`), `describe()`.
  `core::OPS` lists every op with help and a JSON example; `Op::from_json` gives near-miss suggestions.
- `render/src/lib.rs`, `ui/src/lib.rs` and `mcp/src/lib.rs` are **orchestrator stubs whose public signatures are the contract**
  (types: `Image`, `FrameOptions`, `Renderer`, `export`, `contact_sheet`; `EditCtx`, `UiAction`, `MediaPreview`, `MediaDrag`, widget
  structs; `Services`, `Hub`, `ProjectDoc`, `LiveState`, `Jobs`, `actions::*`, `start_http`, `run_stdio`, `cli`). Keep them; add freely.

## Editing widgets (aiframes-ui) — the earlier sketch below is superseded by `ui/src/lib.rs`

```rust
pub struct TimelineState { pub zoom: f64 /* px per second */, pub scroll: f64, pub snapping: bool, pub tool: Tool, ... }
pub enum Tool { Select, Razor, Slip, Ripple, Roll }
pub trait MediaPreview { fn thumbnail(&self, asset: &MediaAsset, t: f64) -> Option<egui::TextureId>; fn waveform(&self, asset: &MediaAsset) -> Option<Arc<Waveform>>; }
pub struct TimelineCtx<'a> { pub project: &'a Project, pub resolved: &'a Resolved, pub playhead: f64, pub selection: &'a [Id], pub preview: &'a dyn MediaPreview, pub ai_highlight: &'a [Id] }
pub enum TimelineAction { Seek(f64), Select(Vec<Id>), Apply { label: String, ops: Vec<Op> }, OpenClip(Id), DropMedia { asset: Id, track: Option<Id>, at: f64 }, ContextMenu(..) }
impl TimelineState { pub fn ui(&mut self, ui: &mut egui::Ui, cx: &TimelineCtx) -> Vec<TimelineAction> }
```
Edits are emitted as `Op`s (never mutate the project directly) so the human and the AI share one history.

## MCP and CLI (aiframes-mcp)

`AppSpec { id: "aiframes", name: "AIFrames", title: "AIFrames Video Editor", default_port: 47675, doc_noun: "project", open_tool: "open_project", save_tool: "save_project" }`.
Tools (all JSON in/out, every edit returns the receipt; `author` = "ai" unless given):
`list_projects`, `new_project`, `open_project`, `save_project`, `close_project`, `describe_project` (text timeline + notes + open findings),
`get_timeline` (JSON), `get_clip`, `import_media` (probe + add, folder import, optional transcribe), `list_media`, `edit` (batch of ops, `plan: true` = dry run, `base_revision` = refuse it if the person's newer changes affect it),
`undo`, `redo`, `history` / `changes_since`, `validate`, `snapshot` (PNG frames at times — returned as images), `contact_sheet`, `filmstrip` (frames around a cut),
`list_effects` / `list_transitions` / `list_ops` (schemas with defaults), `transcribe`, `get_transcript` / `search_transcript` (word indices + timeline times),
`delete_words`, `generate_captions`, `detect_scenes`, `detect_silence` + `remove_silences` (plan/apply), `detect_beats` (+ markers), `analyze_audio` (loudness over time,
peaks, per-track levels), `export` (background job with progress; `export_status`), `chapters` (YouTube chapter text from chapter markers),
`play` / `seek` / `select` / `screenshot_ui` (live GUI only), `launch_gui` (headless).
CLI (same code, headless): `aiframes new|import|describe|validate|apply ops.json|snapshot --at 1,2|contact-sheet|render -o out.mp4|transcribe|mcp`.

## GUI (aiframes-app)

Dark, dense, Premiere/Resolve-like, using `suite-ui`. Panels: media bin (import dialog, drag-drop from Explorer, thumbnails, info,
notes), program viewer (play/pause, JKL shuttle, frame step, in/out, safe-area guides, zoom, on-canvas transform handles for the
selected clip), timeline (the widget), inspector (clip properties with keyframe toggles + mini curve, effects stack from the registry,
text editing, transitions, audio), transcript editor (select words → delete = ripple cut; click word = seek), history panel (every
entry with its author; AI entries highlighted; undo), AI panel (prompt box that runs Claude Code headless against this window's
MCP endpoint, streaming its progress; AI-changed clips flash in the timeline), export dialog (presets: YouTube 1080p, 4K, Shorts
1080×1920, ProRes, GIF; loudness target; progress + cancel), project settings. Autosave. Keyboard shortcuts: Space, J/K/L,
I/O, S or Ctrl+K split, Q/W ripple trim, Delete/Shift+Delete ripple delete, Ctrl+Z/Y, +/- zoom, arrows frame step, M marker.
