# Third-party notices

AIFrames is MIT-licensed (see [LICENSE](LICENSE)). It uses the third-party software below.

## External programs (not bundled)

| What | How it is used | Licence |
|---|---|---|
| FFmpeg / ffprobe | Found on `PATH` (or `AIFRAMES_FFMPEG` / `AIFRAMES_FFPROBE`) and run as separate processes to decode and encode media | LGPL-2.1+ or GPL-2+, depending on the build you install |
| OpenAI Whisper (optional) | Found on `PATH` (or `AIFRAMES_WHISPER`) and run as a separate process for transcription | MIT |
| Claude Code CLI (optional) | Run by the AI panel when you ask it to edit | Its own terms |

## Cargo dependencies

Most dependencies are MIT and/or Apache-2.0. The others:

| Licence | Crates | What it means |
|---|---|---|
| BSD-2-Clause | `arrayref` | Keep the copyright notice |
| BSD-3-Clause | `tiny-skia`, `tiny-skia-path`, `bindgen` (build-time); `moxcms`, `pxfm` (BSD-3-Clause OR Apache-2.0) | Keep the copyright notices |
| Unicode-3.0 | `icu_*`, `litemap`, `potential_utf`, `tinystr`, `writeable`, `yoke*`, `zerofrom*`, `zerotrie`, `zerovec*`; `unicode-ident`, `unicode-properties` (also MIT/Apache) | Permissive; keep the notice |
| Zlib | `zlib-rs`, `slotmap`, `foldhash` | Permissive |
| BSL-1.0 | `clipboard-win`, `error-code` | Permissive |
| ISC | `libloading` | Permissive |
| OFL-1.1 + Ubuntu Font Licence (with MIT/Apache) | `epaint_default_fonts` (fonts embedded through egui) | Fonts may be bundled and redistributed; they can't be sold on their own |
| Apache-2.0 OR GPL-2.0-only | `self_cell` | Used under Apache-2.0 |

Run `cargo metadata` (or a tool such as `cargo-about`) for the full list with versions.
