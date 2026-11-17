# AIFrames

An AI-first video editor written in Rust. It edits like Premiere or Resolve, and it has a built-in MCP
server, so Claude can cut your video while you watch: every AI edit lands in the open window, flashes on the
timeline, and is one Ctrl+Z away.

It exists because HTML-to-video tools generate well but edit badly (timing stored twice, absolute seconds
everywhere, silent failures). AIFrames stores an edit the way a person thinks about it — see
[ARCHITECTURE.md](ARCHITECTURE.md):

- **Placement by intent.** A clip starts at a time *or* after / with / before another clip, at a marker, or
  when a word is spoken; it can end the same way (`until`). Trim a shot and everything after it follows;
  cut a word from the voice-over and the captions and b-roll on that word follow.
- **Clip-local animation.** Keyframes, fades, effects and caption timings belong to their clip, so moving a
  clip moves its animation. Nothing is written twice.
- **Loud failures.** A reference that can't be resolved is an error with a fix, never a silent 0.

## Install (Windows)

In PowerShell (no admin needed):

```powershell
irm https://github.com/RezAIT00/AIFrames/releases/latest/download/Install-AIFrames.ps1 | iex
```

This installs the latest release to `%LOCALAPPDATA%\Programs\AIFrames`, adds a Start menu shortcut, registers
`.aifr` files, installs ffmpeg with winget if it's missing, and keeps AIFrames up to date from this
repository's [releases](https://github.com/RezAIT00/AIFrames/releases): at logon, every 15 minutes, and each
time you start it from the Start menu (never while it's open). The log is in
`%LOCALAPPDATA%\AIFrames-Updater\update.log`. To remove AIFrames, its shortcut, file type, scheduled task and
updater (ffmpeg stays), run in PowerShell:

```powershell
& ([scriptblock]::Create((irm https://github.com/RezAIT00/AIFrames/releases/latest/download/Install-AIFrames.ps1))) -Uninstall
```

**Publishing an update:** on GitHub open **Actions → Release → Run workflow** and enter a new version such
as `v0.2.0` (or push a tag like `v0.2.0`). The workflow builds `aiframes.exe` and publishes the release;
installed copies pick it up on their next check.

## The editor

- **Timeline**: video and audio tracks with filmstrips (sharp on HiDPI screens, stable while zooming) and
  waveforms (peak and RMS, scaled by clip volume, dB scale by default so quiet sound stays visible), multi-track audio (a file with several sound
  streams, like an OBS recording with game, mic and chat, comes in as the picture plus one linked audio clip per
  stream, each on its own track with its own mute, volume and waveform; the inspector picks a clip's stream), snapping, Select / Razor / Slip / Slide /
  Ripple / Roll tools, live ghost previews of every drag, keyframe lanes and volume lines on the clips,
  transitions on cuts, markers and YouTube chapters, anchor lines showing what follows what, track
  mute/solo/lock/hide and drag-to-reorder, Set spacing / Close gaps.
- **Media bin and Source monitor**: poster frames (never a black leader) decoded at the screen's real pixel
  size, hover scrubbing, waveforms for sound files. Double-click a file to open it in the Source monitor beside
  the program viewer: play it (J/K/L, frame steps), mark in/out (I/O, remembered per file) and Insert (`,`,
  later clips move right) or Overwrite (`.`) the range at the timeline playhead, or drag the picture onto a
  track (Ctrl = insert), like Premiere. The AI does the same with `source_monitor`.
- **Viewer**: smooth playback with the sound as master clock (J/K/L shuttle with 2×/4× audio, scrub audio,
  frame steps, loop in/out), on-canvas move/scale/rotate handles, safe-area guides, render warnings. Preview
  resolution Auto / Full / ½ / ¼: Auto follows the viewer size and steps down while playback drops frames
  (the quality menu then reads e.g. "Auto 50%"); a frame-rate badge turns amber when frames are dropped.
  Upcoming cuts and transitions open their decoders a second early; HEVC, AV1, VP9 and 4K H.264 decode on the
  GPU (D3D11VA, CPU fallback).
- **Inspector**: everything about the selection, every animatable value with keyframe controls, text (system
  fonts, outline, shadow, pill background, reveals, karaoke highlight) and shapes with draw-on, project and
  export settings.
- **Effects browser**: every effect, transition, title and shape preset as a thumbnail (grid or list with
  descriptions) rendered by the real renderer from the selected clip at the playhead (or a sample picture
  when nothing is selected); transitions animate on hover. Hovering an item previews it live in the
  viewer — on the cut for transitions — and nothing is changed until you click or drop it.
- **Effect controls**: 26 effects and 31 transitions from one registry (the same one `list_effects` gives the
  AI). Adding an effect (Effects tab, drop on a clip, right-click > Add effect, the AI) selects the clip and opens
  the new effect; the Effects tab docks the selected clip's controls under the browser. Every parameter has a
  slider over its useful range in its units (%, px, °, EV, ×, Hz, s) plus a value to scrub or type, keyframes,
  and reset; dragging previews live in the viewer and is one undo step. Enable/bypass, drag to reorder,
  copy/paste effects, Attributes > paste attributes onto other clips, a file picker for LUTs. Transitions on
  the timeline select and configure the same way.
- **Transcript**: Whisper transcription (or import `words.json` / SRT / VTT sidecars), click a word to seek,
  select words and press Delete to cut them out of the video, captions that stay synced to the voice.
  Word cuts are measured on the audio: each edge moves onto the quietest point near the word times (which
  recognisers place loosely), so cuts land in a real pause instead of clipping a word (a brief dip inside
  running speech, like the gap in a "p", doesn't count). `audit_joins` checks every cut as heard — cuts on
  speech or in running speech, words cut through, a speaker cut off or picked up mid-sentence, words played
  twice, the pause left — and `trim_to_words` names the nearest sentence end when a line stops mid-thought.
- **History**: every change with its author (you, AI, CLI); click to go back.
- **Review**: edits the AI proposes instead of applying (`suggest`) when your taste decides — which take,
  whether a pause is a beat — each with what it does and why; play, apply (one undoable step) or skip.
- **Export**: H.264 / HEVC / AV1 on NVENC (software fallback), ProRes, GIF, PNG sequence, WAV; YouTube,
  4K, Shorts (1080×1920, cover) and square presets from any project size; loudness normalised to −14 LUFS
  with the true peak verified on the delivered file. File > Export Timeline (`export_otio`) writes an
  OpenTimelineIO file to finish the cut in DaVinci Resolve or another editor.
- **AI panel**: type what you want; Claude Code runs against this window, its tool calls stream into the
  log, the timeline follows its edits, and a summary lists what it added, changed and removed.

## Use it with AI

```powershell
claude mcp add aiframes -- "%LOCALAPPDATA%\Programs\AIFrames\aiframes.exe" mcp
# or straight to a running window:
claude mcp add --transport http aiframes http://127.0.0.1:47675/mcp
```

49 tools. Typical loop: `changes_since` (what the person did) → `describe_project` → preview footage with
`contact_sheet {asset}` → `edit` with `plan: true`, then for real → `validate` → `snapshot` / `filmstrip`
to look → `export`. `preview_effects` shows a clip with each effect or transition added (one labelled
sheet; nothing is applied), so looks can be compared before choosing one. Edits carry `base_revision` (the revision they were planned against): if the person
changed something they depend on in the meantime, the edit is refused with what changed instead of
landing in the wrong place. Text-based editing (`get_transcript`, `delete_words`, `remove_silences`,
`generate_captions`), analysis (`detect_scenes`, `detect_beats`, `analyze_audio`) and `schema` (every field
of every type) are there too. Each AI session keeps to its own project, so other windows and tabs are safe.

## Command line

`aiframes help` lists everything: `new`, `import`, `describe`, `timeline`, `validate`, `apply ops.json`,
`snapshot`, `contact-sheet`, `render`, `transcribe`, `chapters`, `history`, `ops` (all 60 edit operations
with examples), `demo <dir>` (sample media and a project using most features). When the window has the
project open, CLI commands run in the window instead of writing the file behind its back.

## Build & check

Needs ffmpeg on PATH (Whisper optional, for transcription).

```powershell
cargo build --release                          # target\release\aiframes.exe
cargo test
target\release\aiframes.exe --selftest         # builds, plays, edits and exports a generated project
target\release\aiframes.exe --bench-playback p.aifr --cores 4   # viewer playback, seek, scrub, step and loop
                                               # numbers (and wrong-frame check) on a few cores
target\release\aiframes.exe demo out\demo      # then open out\demo\demo.aifr
scripts\install.ps1                            # install for this user and register .aifr
```

Crates: `core` (model, anchors, 60 ops, history, validation), `media` (ffmpeg decode/encode, audio cache,
analysis, Whisper), `render` (compositor, text, effects, transitions, mixer, export), `ui` (timeline and
panels), `mcp` (tools, CLI, shared GUI/AI services), `app` (the window), all under `crates/`. `shared/` holds the
generic MCP server framework, document hub, egui theme helpers and Windows file-association code.

## License

MIT, see [LICENSE](LICENSE). Third-party licenses: [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
