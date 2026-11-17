//! Tool definitions (names, descriptions, JSON Schemas, annotations), the server instructions and the
//! reference texts served as resources.

use aiframes_core::validate::CODES;
use aiframes_core::{EFFECTS, OPS, TRANSITIONS};
use serde_json::{json, Map, Value as J};

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// Reads a project.
    Read,
    /// An undoable edit through `edit` plumbing (adds plan/author/label).
    Edit,
    /// Changes something else about a project (save, live state, jobs).
    Write,
    /// Not tied to a project, read-only.
    Global,
    /// Not tied to a project, changes state.
    GlobalWrite,
}
use Kind::*;

fn tool(name: &str, title: &str, desc: &str, props: J, required: &[&str], kind: Kind) -> J {
    let mut p: Map<String, J> = props.as_object().cloned().unwrap_or_default();
    if !matches!(kind, Global | GlobalWrite) {
        p.entry("project").or_insert(json!({
            "type": ["string", "integer"],
            "description": "Project id (from list_projects), file name or path. Default: your session's project (the one you last created, opened or named)."
        }));
    }
    if kind == Edit {
        p.entry("plan").or_insert(json!({"type": "boolean", "description": "Dry run: return the receipt (what would move, warnings, new findings) without changing anything. Default false."}));
        p.entry("label").or_insert(json!({"type": "string", "description": "History label shown to the person (default: a summary of the ops)."}));
        p.entry("author").or_insert(json!({"type": "string", "description": "Author recorded in the history (default \"ai\")."}));
    }
    let read_only = matches!(kind, Read | Global);
    json!({
        "name": name,
        "title": title,
        "description": desc,
        "inputSchema": {"type": "object", "properties": J::Object(p), "required": required},
        "annotations": {
            "title": title,
            "readOnlyHint": read_only,
            "destructiveHint": false,
            "idempotentHint": read_only,
            "openWorldHint": false,
        }
    })
}

fn s(d: &str) -> J {
    json!({"type": "string", "description": d})
}
fn b(d: &str) -> J {
    json!({"type": "boolean", "description": d})
}
fn n(d: &str) -> J {
    json!({"type": "number", "description": d})
}
fn i(d: &str) -> J {
    json!({"type": "integer", "description": d})
}
fn e(values: &[&str], d: &str) -> J {
    json!({"type": "string", "enum": values, "description": d})
}
fn strs(d: &str) -> J {
    json!({"type": "array", "items": {"type": "string"}, "description": d})
}
fn obj(d: &str) -> J {
    json!({"type": "object", "description": d})
}
fn time(d: &str) -> J {
    json!({"type": ["number", "string"], "description": format!("{d} Timeline seconds, or a clip id (its middle; \"c3@start\" / \"c3@end\" = its first / last frame), or a marker id.")})
}
fn times(d: &str) -> J {
    json!({"type": ["number", "string", "array"], "items": {"type": ["number", "string"]}, "description": format!("{d} Each: timeline seconds, a clip id (its middle; \"c3@start\"/\"c3@end\"), or a marker id. A list, a single value, or \"1,2.5\".")})
}
fn range() -> J {
    json!({"type": "array", "items": {"type": "number"}, "minItems": 2, "maxItems": 2, "description": "[start, end] timeline seconds."})
}

/// `asset` / `file` for the seeing tools: look at a media file directly (source seconds).
fn preview_props(mut props: J) -> J {
    props["asset"] = s("Look at this media asset (id from list_media) instead of the timeline: times are SOURCE seconds of the file. Nothing is added to the timeline.");
    props["file"] = s("Look at any media file (path; not imported) instead of the timeline: times are source seconds.");
    props
}

/// `background` / `wait_seconds` for analyses that run as jobs on long media.
fn job_props(mut props: J) -> J {
    props["background"] = b("Run as a background job and return its id at once (default: only media longer than 2 minutes runs as a job).");
    props["wait_seconds"] = n("For a job: wait up to this long for its result (default 25, max 300); if it is still running you get its id to follow with jobs.");
    props
}

fn src_props(extra: J) -> J {
    let mut m = json!({
        "clip": s("A media clip id: analyse its media and map results onto the timeline through this clip."),
        "asset": s("A media asset id (list_media), when no clip is given."),
    });
    if let (Some(o), Some(x)) = (m.as_object_mut(), extra.as_object()) {
        for (k, v) in x {
            o.insert(k.clone(), v.clone());
        }
    }
    m
}

/// Every tool. `include_launch` adds the stdio-only `launch_gui`.
pub fn definitions(include_launch: bool) -> Vec<J> {
    let mut v = vec![
        // ---------------- projects
        tool("list_projects", "List projects", "Open projects with id, title, path, frame size, duration, clip count, revision, unsaved state, which is active in the window, and `session: true` for the one YOUR tools act on when you omit `project`; `window` tells whether the person's AIFrames window is attached.", json!({}), &[], Global),
        tool(
            "new_project",
            "New project",
            "Create a project (becomes active). Default tracks: v1 Main (role main), v2 Overlay, a1 Voice, a2 Music. Not saved unless `path` is given.",
            json!({
                "preset": e(&["youtube", "4k", "720p", "shorts", "square"], "Frame size: youtube 1920x1080, 4k 3840x2160, 720p, shorts 1080x1920 (vertical), square 1080x1080. Default youtube."),
                "width": i("Frame width px (overrides the preset)."),
                "height": i("Frame height px."),
                "fps": n("Frames per second (default 30)."),
                "name": s("Project name."),
                "notes": s("Brief for collaborators and AIs: goal, audience, style rules."),
                "background": s("Background colour \"#RRGGBB\" (default black)."),
                "tracks": b("Create the default tracks (default true)."),
                "path": s("Save immediately as this .aifr file."),
                "overwrite": b("Allow replacing an existing file at path (default false)."),
            }),
            &[],
            GlobalWrite,
        ),
        tool("open_project", "Open project", "Open a .aifr project file (or find it if already open). Returns counts and open problems; call describe_project next.", json!({"path": s("The .aifr file."), "show": b("Make it the active project and bring the window forward (default true). false = open in the background: the person's active project does not change."), "reload": b("When it is already open: load the file's current version (another program changed it) as one undoable step."), "author": s("Recorded author of a reload (default \"ai\").")}), &["path"], GlobalWrite),
        tool(
            "save_project",
            "Save project",
            "Save the project (atomic write; its history goes to <file>.history.jsonl). Pass `path` for Save As (media paths are rebased); an existing file is only replaced with overwrite:true, and never another open project's file or a media file. A plain save is refused when another program changed the file since it was opened (open_project with reload, or overwrite_external:true after checking). Save only when the person wants the file written.",
            json!({"path": s("Save As this .aifr path (required if never saved; relative = next to the project)."), "overwrite": b("Allow Save As over an existing file (default false)."), "overwrite_external": b("Save even though another program changed the file since it was opened (refused by default; check what changed first, e.g. open_project with reload).")}),
            &[],
            Write,
        ),
        tool("close_project", "Close project", "Close a project. Refuses when there are unsaved changes unless save or discard is true.", json!({"save": b("Save first."), "discard": b("Close without saving."), "path": s("Path for save."), "overwrite": b("With save and path: allow replacing an existing file."), "overwrite_external": b("With save: save even though another program changed the file.")}), &[], Write),
        tool(
            "describe_project",
            "Describe project",
            "The whole edit as compact text lines: settings, notes, media, tracks in the timeline's on-screen order in two sections — \"Video tracks (top layer first)\" (the top layer, drawn over the others, down to V1) and \"Audio tracks\" (A1 is the top audio row) — with every clip as [start–end] timeline seconds, id, source (media@in_point, text, shape...), anchor, effects, animation, notes; transitions, markers, ducking, resolve errors; plus counts of open findings. Start here.",
            json!({
                "from": n("Only clips overlapping this range: start (timeline s)."),
                "to": n("Range end (timeline s)."),
                "max_clips": i("Show at most this many clips per track (the rest is summarised)."),
                "media": b("Include the media list (default true)."),
                "notes": b("Include project and clip notes (default true)."),
            }),
            &[],
            Read,
        ),
        tool("get_timeline", "Timeline JSON", "The timeline as JSON: tracks (top first) with clips (absolute start/end, duration, source summary incl. source_range, anchor, effects), transitions, markers, ducking, media, resolve errors.", json!({"from": n("Only clips/markers overlapping from (timeline s)."), "to": n("…to (timeline s).")}), &[], Read),
        tool("get_clip", "Get clip", "One clip in full: its canonical JSON (every property, keyframes in clip-local seconds), resolved span, source range, media, transcript words inside it (indices + text), transitions and what is anchored to it.", json!({"clip": s("Clip id.")}), &["clip"], Read),
        tool("list_media", "List media", "Media assets: id, file, kind, duration, size, fps, audio, transcript word count, which clips use it, offline files, notes/tags.", json!({}), &[], Read),
        tool(
            "media_info",
            "Media info",
            "What a media file is — a project asset or any file (not imported): kind, duration, size, fps, audio, codec, which clips use it, its transcript or a transcript sidecar next to it. Then look at it with contact_sheet/snapshot {asset|file}.",
            json!({"asset": s("A media id from list_media."), "file": s("Any media file path (absolute, or relative to the project folder).")}),
            &[],
            Read,
        ),
        tool(
            "import_media",
            "Import media",
            "Probe files and add them as media (folders are scanned recursively; files already in the project are skipped). One undoable step. place:\"append\" also appends each file magnetically to the end of a track (pictures → main video track, sound → first audio track). A transcript sidecar next to a file (<name>.words.json, whisper <name>.json, <name>.srt, <name>.vtt) is imported as its word transcript in the same step. transcribe:true starts a Whisper job for each file with audio and no sidecar. To LOOK at footage before using it, don't import-and-place it: use contact_sheet {file} or {asset}.",
            json!({
                "paths": strs("Files and/or folders (absolute, or relative to the project folder)."),
                "place": e(&["none", "append"], "\"append\" = also put each file on the timeline after the last clip of its track. Default \"none\" (media bin only)."),
                "track": s("Track to append to (default: the main video track / first audio track)."),
                "sidecars": b("Import transcript sidecars found next to the files (default true)."),
                "transcribe": b("Start transcription jobs for files with audio and no sidecar (default false)."),
                "language": s("Spoken language code for transcription (e.g. \"en\"; default auto)."),
                "model": s("Whisper model (default \"turbo\")."),
                "prompt": s("Names/jargon to help recognition."),
            }),
            &["paths"],
            Edit,
        ),
        // ---------------- editing
        tool(
            "edit",
            "Edit (ops)",
            "Apply a batch of typed ops atomically as ONE undoable step (all or nothing). Returns the receipt: revision, done (summaries), created/changed/removed ids, moved (every clip whose [start,end] changed), warnings, new_findings (problems this edit introduced), open_errors/open_warnings, duration. On error nothing changes and the message says how to fix it (unknown op names get a suggestion). list_ops shows every op with an example. Use anchors ({\"after\":\"c3\"}, {\"word\":{...}}) rather than absolute times; keyframe times are clip-local. Pass base_revision (the revision you planned against): the person may have edited since, and a stale edit is then refused instead of landing in the wrong place (the error lists what changed and which ops it affects); an edit their changes do not affect still applies, with `applied_over` in the receipt.",
            json!({"ops": {"type": "array", "items": {"type": "object"}, "description": "Ops like {\"op\":\"split_clip\",\"clip\":\"c3\",\"at\":12.4} (at = timeline seconds). Applied in order; later ops see earlier results."}, "base_revision": i("The revision you planned this against (`revision` of your last receipt or describe_project). If the project changed since, the edit is refused, with nothing changed, when it uses absolute timeline seconds and clips moved, or references something that was changed; otherwise it applies and `applied_over` says so. Omit to skip the check.")}),
            &["ops"],
            Edit,
        ),
        tool("undo", "Undo", "Undo the latest edit(s) — whoever made them (the result says when an undone edit was the person's). Undo is itself a history entry.", json!({"steps": i("How many edits (1–100, default 1)."), "author": s("Recorded author (default \"ai\").")}), &[], Write),
        tool("redo", "Redo", "Redo the most recently undone edit(s); like undo, it is recorded in the history.", json!({"steps": i("How many (1–100, default 1)."), "author": s("Recorded author (default \"ai\").")}), &[], Write),
        tool("history", "History", "The edit history (newest last): revision, author (\"user\", \"ai\", ...), label, time, kind (edit/undo/redo), summary; plus what undo/redo would do.", json!({"limit": i("Most recent entries (default 30)."), "since": i("Only entries after this revision."), "by": s("Only entries by this author."), "ops": b("Include each entry's ops (default false).")}), &[], Read),
        tool(
            "changes_since",
            "Changes since",
            "What changed after a revision — the way to see the person's edits since your last receipt (each receipt has `revision`; a plan:true receipt reports the current revision, since it changed nothing). Entries include their ops; by_author counts.",
            json!({"revision": i("The revision you last saw."), "exclude_author": s("Leave out entries by this author (e.g. \"ai\" for only the person's edits)."), "ops": b("Include ops (default true).")}),
            &["revision"],
            Read,
        ),
        tool(
            "validate",
            "Validate",
            "Check the project: every finding has severity, code, message, timeline time, ids, a one-line fix and, when mechanical, fix_ops (ready for edit). apply_fixes:true applies all matching fix_ops as one undoable edit. Codes include unresolved_anchor, overlap_same_track, gap_in_main (black frames), clip_past_media_end, keyframe_outside_clip, text_outside_title_safe, transition_not_adjacent, media_offline, chapter_markers.",
            json!({
                "severity": e(&["error", "warning", "info"], "Lowest severity to list (default info = all)."),
                "codes": strs("Only these codes."),
                "apply_fixes": b("Apply the fix_ops of the listed findings (one undoable edit), then report what is left."),
                "limit": i("Most findings returned (default 60)."),
                "author": s("Author for apply_fixes (default \"ai\")."),
            }),
            &[],
            Write,
        ),
        tool("list_ops", "List ops", "Every edit op with its group, one-line help and a JSON example, plus the formats of times (anchors) and animatable params (clip-local keys).", json!({"group": e(&["project", "media", "tracks", "clips", "effects", "transitions", "markers", "audio", "transcript"], "Only this group."), "op": s("Only this op.")}), &[], Global),
        tool("list_effects", "List effects", "The effect registry: kind, label, category, help, params (name, label, default, min, max = the valid range, unit, help; `slider` [lo, hi] and `curve` (2 = finer near the low end, 0 = logarithmic) where the inspector's slider covers only the useful part or is not linear; named values such as \"0 = auto, 1 = behind\" are in the help), colours and strings. Params can be animated with clip-local keys. The inspector shows exactly these controls.", json!({"category": s("Only this category."), "kind": s("Only this effect.")}), &[], Global),
        tool("list_transitions", "List transitions", "The transition registry: kind, label, help, default duration, params (described like list_effects params) and colours.", json!({}), &[], Global),
        tool("schema", "Schema", "The exact JSON of every project type an op takes or returns (clip, text, shape, transform, effect, marker, ducking, transcript, export, settings, ...), the value forms (time/anchors, until, param/keys, ease, color) and enums: every field with its type, required/default, units, meaning and an example. Call it when an edit fails with \"unknown field\" or \"invalid type\", or before writing set_clip patches or add_clip JSON by hand.", json!({"type": s("A type, form or enum (e.g. \"clip\", \"text\", \"time\", \"param\"; Rust-style names like \"TextSource\" work too); \"all\" = everything; omitted = the index.")}), &[], Global),
        tool("chapters", "YouTube chapters", "Chapter list for a YouTube description from markers of kind \"chapter\" (\"0:00 Intro\" lines), with the rules it breaks (first at 0:00, at least 3, each ≥ 10 s).", json!({}), &[], Read),
        tool("auto_ducking", "Auto ducking", "Make the music duck under the voice: adds a ducking rule (target = track with role music, trigger = role voice, unless given), or changes amount_db of an existing one.", json!({"target": s("Track to lower (default: role music)."), "trigger": s("Track whose sound lowers it (default: role voice)."), "amount_db": n("Reduction in dB while the trigger sounds (negative, default -14).")}), &[], Edit),
        // ---------------- seeing
        tool(
            "snapshot",
            "Snapshot",
            "Render frames of the edit exactly as exported and return them as an image (up to 16 times → one labelled grid). Also lists what is on screen at each time and `warnings` when something renders as a substitute (offline media placeholder, missing font). Small by default (longest side 768 px). With asset/file: frames of that media at SOURCE seconds — look at footage this way, never by putting it on the timeline.",
            preview_props(json!({
                "at": times("When (default: the playhead, or the middle of asset/file; at most 16). With asset/file: source seconds."),
                "scale": n("Size relative to the project (or media) size (0–1; 1 = full resolution)."),
                "guides": b("Draw title/action-safe guides."),
                "draft": b("Faster, lower quality."),
                "save_dir": s("Also write each frame as a PNG into this folder (relative = the project's folder)."),
                "image": b("Return the image (default true; false = no picture: only on_screen/warnings, or the files with save_dir)."),
            })),
            &[],
            Write,
        ),
        tool(
            "contact_sheet",
            "Contact sheet",
            "A labelled grid of frames across the edit (or a range) to see its whole shape at once: `count` evenly spaced frames (default 12), one every `every` seconds, explicit `times`, or `per_clip` (the middle of each clip on a track); at most 60 frames. Frames are labelled mm:ss.ff; `times` gives them in seconds. With asset/file: a sheet of that media file (from/to/times in SOURCE seconds) — the way to scan long footage for shots without touching the timeline.",
            preview_props(json!({
                "from": n("Range start (timeline s, or source s with asset/file; default 0)."),
                "to": n("Range end (default: the end)."),
                "count": i("Number of frames, 1–60 (default 12)."),
                "every": n("Seconds between frames (instead of count; at most 60 frames)."),
                "times": times("Explicit times (at most 60; source seconds with asset/file)."),
                "per_clip": {"type": ["boolean", "string"], "description": "Timeline only: true = one frame per clip of the main track, or a track id."},
                "cols": i("Columns, 1–12 (default 4)."),
                "width": i("Width of each frame px, 64–1920 (default 320)."),
                "save_path": s("Also write the PNG here (relative = the project's folder; never over project files or media)."),
                "image": b("Return the image (default true; false with save_path = file only)."),
            })),
            &[],
            Write,
        ),
        tool(
            "filmstrip",
            "Filmstrip",
            "A row of frames just before and after a cut (or around a time) to judge the edit point, continuity and transitions. Names the outgoing/incoming clips and any transition. With asset/file: frames around `at` (source seconds) of that media, e.g. to find the exact frame a moment happens.",
            preview_props(json!({
                "cut": s("A clip id: the cut INTO this clip (its start)."),
                "edge": e(&["start", "end"], "With cut: \"end\" = the cut out of the clip instead."),
                "at": time("Centre time instead of a cut (source seconds with asset/file)."),
                "frames": i("Frames, 2–16 (default 6: 3 before, 3 from the cut on)."),
                "span": n("Seconds covered (default 1)."),
                "width": i("Width of each frame px, 64–960 (default 240)."),
                "save_path": s("Also write the PNG here (relative = the project's folder)."),
            })),
            &[],
            Write,
        ),
        // ---------------- transcript
        tool(
            "transcribe",
            "Transcribe",
            "Start a Whisper job that transcribes a media file with word timestamps (source seconds); when done the transcript is stored on the asset (an undoable edit). Follow with jobs; then get_transcript / search_transcript. If word timings already exist in a file (whisper JSON, <name>.words.json, SRT/VTT), use import_transcript instead; with a known script, align_transcript fixes spelling.",
            src_props(json!({"language": s("Language code (default auto)."), "model": s("Whisper model (default \"turbo\")."), "prompt": s("Names/jargon to help recognition."), "wait_seconds": n("Block up to this long for the result (default 0 = return at once; max 300, then follow the job with jobs)."), "author": s("Author recorded for the stored transcript (default \"ai\").")})),
            &[],
            Write,
        ),
        tool(
            "import_transcript",
            "Import transcript",
            "Store word timings from a file as the transcript of a media asset (one undoable step): openai-whisper JSON (segments[].words[]), word lists [{word,start,end}] like NN.words.json, SRT or WebVTT (segment timings; words spread over each segment). Without path, the sidecar next to the media is used. Times are source seconds of that media.",
            json!({
                "asset": s("The media asset (id from list_media)."),
                "clip": s("Or a media clip: its asset."),
                "path": s("The transcript file (absolute, relative to the project folder, or to the media's folder). Default: the sidecar next to the media."),
                "force": b("Store it even if it breaks word anchors (they are then listed by validate)."),
            }),
            &[],
            Edit,
        ),
        tool(
            "align_transcript",
            "Align transcript to script",
            "Keep the recognised word timings but take the words' spelling, case and punctuation from the known script (sequence alignment; numbers spoken as words match digits, e.g. \"10\" ↔ \"Ten\"; written forms like 3D, 10, $500 are kept by default, see spelling and kept_written). Reports low_confidence regions (script words heard differently or not at all — timed by interpolation) to check by ear or snapshot. One undoable step; plan:true previews it.",
            json!({
                "asset": s("The media asset whose transcript is aligned."),
                "clip": s("Or a media clip: its asset."),
                "text": s("The script the voice reads (whole text)."),
                "path": s("Or a text file with the script."),
                "from": i("First transcript word index of the section the script covers (default 0)."),
                "to": i("Last word index of that section (default: the last word)."),
                "force": b("Store it even if it breaks word anchors."),
                "spelling": e(&["written", "script"], "How the words are spelled (captions show them): \"written\" (default) takes the script's words, case and punctuation but keeps written forms where the script only spells out what was said (\"three D\" → 3D, \"ten\" → 10, \"five hundred dollars\" → $500, \"fifty percent\" → 50%, \"A I\" → AI); \"script\" = the script verbatim."),
            }),
            &[],
            Edit,
        ),
        tool(
            "get_transcript",
            "Get transcript",
            "A media transcript for reading and text-based editing (with clip: the words that clip plays, unless from/to are given). Default format \"paragraphs\": {from, to (word indices), src [s,e] (source s), t (timeline s where it starts), text}. \"indexed\": the same with [index] before every word. \"words\": {i index, w word, s/e source seconds, t timeline seconds (absent + out:true when cut out), c clip, also: other places when the media plays more than once}. Words already cut from the timeline are marked ~.",
            src_props(json!({
                "format": e(&["paragraphs", "indexed", "words"], "Default paragraphs."),
                "from": i("First word index."),
                "to": i("Last word index."),
                "max_words": i("Cap (default 400 for words/indexed, 5000 for paragraphs); next_from pages on."),
                "pause": n("Seconds of silence that start a new paragraph (default 1)."),
            })),
            &[],
            Read,
        ),
        tool(
            "search_transcript",
            "Search transcript",
            "Find a word or phrase in transcripts (case and punctuation ignored): each match with word indices from–to, text in context, source and timeline times and the clip it plays in (`places` lists every place when the media plays more than once). Feed indices to delete_words or word anchors.",
            src_props(json!({"query": s("Word or phrase."), "max": i("Most matches, 1–1000 (default 30)."), "context": i("Words of context each side, 0–50 (default 5).")})),
            &["query"],
            Read,
        ),
        tool(
            "export_otio",
            "Export timeline (.otio)",
            "Write the cut as an OpenTimelineIO file to finish it in DaVinci Resolve (File > Import > Timeline), Premiere or another editor: media clips with their source ranges, gaps, track order, video clips' sound as audio tracks, constant speed and freeze frames, crossfades and markers. Text, shapes, solids and adjustment clips become named offline placeholders; effects, keyframes and transforms do not carry over (the result's notes list what was approximated).",
            json!({"path": s("The .otio file (default: next to the project file)."), "overwrite": b("Replace an existing file (default false).")}),
            &[],
            Write,
        ),
        tool(
            "suggest",
            "Suggest an edit",
            "Propose an edit for the person to approve instead of applying it: use it when their taste decides (which take to keep, whether a pause is a beat or dead air, a cut you are unsure of). The ops are checked against the project now and wait in the window's Suggestions panel, where the person plays the spot as it is (from just before `at`), tries it (applies it and plays it, with Undo to hear it both ways), applies (one undoable step) or dismisses each. Edits they asked for directly go through edit as usual.",
            json!({
                "label": s("One sentence: what applying it does (shown on its card)."),
                "reason": s("Why, and what to listen or look for before deciding."),
                "at": n("Timeline seconds where its effect plays (the preview seeks there)."),
                "ops": {"type": "array", "items": {"type": "object"}, "description": "The edit: the same ops edit takes."},
                "base_revision": i("The revision you planned this against (checked now, like edit's base_revision: a stale proposal is refused)."),
                "show": b("Bring the window's Review tab forward so the person sees it (default true)."),
            }),
            &["label", "ops"],
            Write,
        ),
        tool(
            "show_panel",
            "Show a panel",
            "Bring a side panel of the window forward so the person sees it: review (your suggestions), transcript, history, inspector, effects or ai. suggest already shows review.",
            json!({"panel": s("inspector | effects | transcript | history | review | ai")}),
            &["panel"],
            Write,
        ),
        tool("list_suggestions", "List suggestions", "Edits proposed with suggest that the person has not applied or dismissed yet: id, label, reason, at, op names, author.", json!({}), &[], Read),
        tool(
            "resolve_suggestion",
            "Apply or dismiss a suggestion",
            "Apply a pending suggestion (its ops as one undoable step) or dismiss it. Do this only when the person asks — the Suggestions panel is theirs. suggestion \"all\" applies or dismisses every pending one in one step.",
            json!({"suggestion": s("Suggestion id (\"sg3\"), or \"all\"."), "action": s("\"apply\" (default) or \"dismiss\".")}),
            &["suggestion"],
            Edit,
        ),
        tool(
            "trim_to_words",
            "Trim a clip to words",
            "Make a media clip play exactly transcript words from..to of its media, with both edges moved onto the quietest point near the word times (measured on the audio), so the clip starts and ends in room tone instead of on a stray syllable; a real pause (at least 100 ms of quiet) beats a brief dip inside a word. The clip keeps its place; its in-point and length change. It warns when the words stop the speaker mid-sentence or pick them up mid-sentence (heard as a cut-off however clean the edge), and `suggest` names the nearest sentence end (to) or start (from) to use instead. Use it to assemble or tighten talking clips by their words; not_before/not_after keep the edges inside a shot when the footage cuts away right next to the words. Check the result with audit_joins.",
            json!({
                "clip": s("The media clip to trim."),
                "from": i("First word index to keep (get_transcript / search_transcript)."),
                "to": i("Last word index to keep (inclusive; default from)."),
                "pad": n("Breathing room kept around the words where it does not touch neighbours (default 0.04 s)."),
                "not_before": n("Source seconds the start may not go before (e.g. a shot change)."),
                "not_after": n("Source seconds the end may not go past (e.g. a shot change)."),
            }),
            &["clip", "from"],
            Edit,
        ),
        tool(
            "audit_joins",
            "Audit cuts",
            "Every cut in the edit's sound (where one clip with sound ends and the next begins on a track, and every clip edge whose sound starts out of or stops into silence: the first and last clip, gaps, J- and L-cut voices on their own track; the silent side shows as \"silent\"), measured as heard: each side's level at the edge against the recording's room tone (silence / low / speech), how long the quiet at the edge lasts (pause_ms), the words either side (`heard`), the pause the join leaves, and issues — on_sound (the cut lands on speech: a clipped word or breath), no_pause (the cut sits in a brief dip inside running speech, under 100 ms and not silent: the voice is chopped mid-flow even though the level is low), cuts_word (the transcript puts a word across the edge), cut_off (a clip stops its speaker mid-sentence while the recording goes straight on: heard as a cut-off, even under a fade), starts_mid_sentence (a clip picks its speaker up mid-sentence), repeated_word (the join plays the same word twice). Only a fade or volume ramp of at least 0.2 s counts as taking a voice away; a shorter ramp still cuts. Sentence checks need a punctuated transcript (Whisper's); cuts inside one sentence of one recording (words removed) are judged by their sound only. Run it after delete_words, remove_silences, trim_to_words or any cutting pass, and fix what it flags.",
            json!({
                "from": n("Only joins at or after this timeline second."),
                "to": n("Only joins up to this timeline second."),
                "track": s("Only this track."),
                "issues_only": b("List only joins with issues (counts still cover all)."),
            }),
            &[],
            Read,
        ),
        tool(
            "delete_words",
            "Delete words",
            "Text-based editing: cut transcript words out of the timeline (splits the clip and ripple-deletes their source range plus a small pad; captions anchored to them are re-timed or removed). Cut edges are snapped into measured silence (snap). One undoable step. Indices come from get_transcript / search_transcript. Check the result with audit_joins.",
            json!({
                "clip": s("The clip playing the words (default: found from the words; required when the media plays in more than one place). A picture and its detached sound are cut together."),
                "asset": s("The media whose transcript the indices refer to (when no clip)."),
                "from": i("First word index."),
                "to": i("Last word index (inclusive; default from)."),
                "ranges": {"type": "array", "items": {"type": "array", "items": {"type": "integer"}, "minItems": 1, "maxItems": 2}, "description": "Several [from, to] word-index ranges (whole numbers) in one step."},
                "pad": n("Extra seconds removed around the words where that does not touch neighbours (default 0.04)."),
                "snap": b("Move each cut edge onto the quietest point near the word times, measured on the audio, so cuts land in room tone instead of clipping a word (default true). The receipt's cut_edges says where each edge went, how far it moved and its level."),
            }),
            &[],
            Edit,
        ),
        tool(
            "generate_captions",
            "Generate captions",
            "Create caption text clips from transcripts: one short line each, anchored to its first word (captions follow the voice through every cut) with karaoke word highlighting. Regenerating keeps what the person did: captions they edited stay, captions they deleted are not recreated, and the look of the existing captions is reused; other earlier generated captions of the same media on that track are replaced. replace:true replaces everything and recreates deleted ones.",
            json!({
                "clips": strs("Voice clips to caption (default: media clips with transcripts on audio tracks)."),
                "track": s("Video track for captions (default: role captions; created if missing)."),
                "style": obj("Optional: font, size (px), weight, color, highlight (spoken word colour), background (pill colour or \"transparent\"), y (px from centre, + = down), max_width, uppercase, stroke {color,width}, reveal (\"spoken\" = each word appears as it is said; or another reveal_kind)."),
                "max_words": i("Words per caption (default 5)."),
                "max_chars": i("Characters per caption (default 32; 20 for vertical)."),
                "pause": n("A silence this long starts a new caption (default 0.5 s)."),
                "replace": b("Replace ALL earlier generated captions of this media on the track, including ones the person edited, and recreate ones they deleted (default false: their edits and deletions are kept)."),
            }),
            &[],
            Edit,
        ),
        // ---------------- analysis
        tool(
            "detect_scenes",
            "Detect scenes",
            "Find shot changes in a video (source seconds, and timeline seconds through the clip — `places` when the media plays more than once). markers:true adds a marker at each place; split:true (needs clip) cuts the clip at each change. Edits are one undoable step. Long media runs as a background job.",
            src_props(job_props(json!({"threshold": n("0–1, lower finds more cuts (default 0.3)."), "markers": b("Add markers (default false)."), "split": b("Split the clip at each change (default false).")}))),
            &[],
            Edit,
        ),
        tool(
            "detect_silence",
            "Detect silence",
            "Silent stretches of a media file's audio (source seconds; with clip: only inside it, with timeline times). Long media runs as a background job.",
            src_props(job_props(json!({"noise_db": n("Level below which it is silence, dBFS (default -35)."), "min_length": n("Shortest silence, seconds (default 0.5).")}))),
            &[],
            Read,
        ),
        tool(
            "remove_silences",
            "Remove silences",
            "Tighten speech: cut silences out of media clips (cut_source + ripple), keeping `padding` seconds on each side. One undoable step; use plan:true first to see every cut and what moves. Clips whose media has no sound (pictures, b-roll) are skipped; a picture and its detached sound are cut together so they stay in sync; each clip is cut once even if listed twice. Long media runs as a background job.",
            job_props(json!({
                "clip": s("A media clip."),
                "clips": strs("Several clips."),
                "track": s("Every media clip on this track."),
                "noise_db": n("Silence threshold dBFS (default -35)."),
                "min_length": n("Only silences at least this long inside the clip, seconds (default 0.6)."),
                "padding": n("Silence kept on each side of a cut, seconds (default 0.12)."),
            })),
            &[],
            Edit,
        ),
        tool(
            "detect_beats",
            "Detect beats",
            "Tempo (BPM) and beat times of music (timeline seconds through the clip, every place it plays). markers:true adds beat markers (every Nth beat) to cut on the music. Long media runs as a background job.",
            src_props(job_props(json!({"markers": b("Add markers of kind beat (default false)."), "every": i("Every Nth beat, 1–1000 (default 1; 4 = bars in 4/4)."), "from": n("Only beats after (timeline s)."), "to": n("Only beats before (timeline s).")}))),
            &[],
            Edit,
        ),
        tool(
            "analyze_audio",
            "Analyze audio",
            "Loudness of the rendered mix (or one track, or a source file): integrated LUFS, true peak, LRA, short-term loudness over time, silent stretches, per-track levels and voice-over-music separation, with advice and warnings (offline media). Media audio is fully decoded first. Long ranges run as a background job.",
            job_props(json!({
                "track": s("Only this track (default: the full mix)."),
                "clip": s("Analyse the source file of this media clip instead of the mix."),
                "asset": s("Analyse a source file instead of the mix."),
                "from": n("Range start (timeline s)."),
                "to": n("Range end."),
                "window": n("Seconds between short-term readings (default: range/60, 1–10)."),
                "per_track": b("Also measure each track and voice vs music (default true for short projects)."),
                "silence_db": n("Silence threshold dBFS (default -50)."),
                "silence_min": n("Shortest reported silence, s (default 1)."),
            })),
            &[],
            Read,
        ),
        // ---------------- export and jobs
        tool(
            "export",
            "Export",
            "Render the project to a video file in the background (same renderer as snapshots). Defaults come from the project's export settings (H.264, high, NVENC when available, -14 LUFS, the project's frame size). Size presets set the output size (output_size in the result; another aspect is letterboxed unless fit is cover). Never writes over the project file, its history/autosave or media; an existing file needs overwrite:true. Returns a job id; follow with export_status.",
            json!({
                "path": s("Output file (default: next to the project, named after it; relative = the project's folder)."),
                "preset": e(&["youtube", "1080p", "4k", "720p", "shorts", "square", "hevc", "draft", "prores", "gif", "wav"], "youtube = H.264 at the project size; 1080p 1920x1080, 4k 3840x2160, 720p, shorts 1080x1920, square 1080x1080 (H.264 at that size); hevc, draft, prores, gif, wav = codec only."),
                "width": i("Output width px (16–16384); alone keeps the project's aspect."),
                "height": i("Output height px (16–16384); alone keeps the project's aspect."),
                "fit": e(&["contain", "cover", "stretch", "none"], "How the project frame fits another output aspect: contain letterboxes (default), cover crops to fill."),
                "overwrite": b("Replace an existing file at path (default false; files this session exported may be replaced)."),
                "codec": e(&["h264", "hevc", "av1", "prores", "gif", "png_sequence", "wav"], "Codec."),
                "quality": e(&["draft", "standard", "high", "lossless"], "Quality."),
                "hardware": b("Use the GPU encoder when available (default true)."),
                "loudness_lufs": {"type": ["number", "null"], "description": "Normalise to this integrated loudness (default -14); null = leave the mix as is."},
                "true_peak_db": n("True-peak ceiling dBTP (default -1.5)."),
                "range": range(),
                "wait_seconds": n("Block up to this long for the result (default 0; max 300 — a longer export keeps running: follow it with export_status)."),
                "author": s("Who started it, for the activity log (default \"ai\")."),
            }),
            &[],
            Write,
        ),
        tool("export_status", "Export status", "Progress of an export job (default: the latest): status, progress 0–1, stage/ETA message, and when done the report (path, duration, encoder, loudness, bytes).", json!({"job": i("Job id."), "wait_seconds": n("Block up to this long for it to finish (max 300).")}), &[], Global),
        tool("jobs", "Jobs", "Background jobs (exports, transcriptions), newest first; with job: one job.", json!({"job": i("One job."), "all": b("Include finished jobs (default true)."), "limit": i("Most jobs (default 20)."), "wait_seconds": n("With job: block up to this long (max 300).")}), &[], Global),
        tool("cancel_job", "Cancel job", "Stop a running export or transcription.", json!({"job": i("Job id.")}), &["job"], GlobalWrite),
        // ---------------- live window
        tool("get_state", "Get state", "What the person sees: playhead, playing, selection (what \"this clip\" means) with clip summaries, in/out points, what is on screen at the playhead, whether a window is attached.", json!({}), &[], Read),
        tool("play", "Play", "Start playback in the window (optionally from/to).", json!({"from": time("Start here."), "to": time("Stop here.")}), &[], Write),
        tool("pause", "Pause", "Stop playback in the window.", json!({}), &[], Write),
        tool("seek", "Seek", "Move the playhead (the person sees that frame).", json!({"time": time("Where."), "clip": s("Or: the start of this clip."), "marker": s("Or: this marker.")}), &[], Write),
        tool("select", "Select", "Select clips in the person's timeline to show what you mean.", json!({"clips": strs("Clip ids (empty = clear)."), "seek": b("Also move the playhead to the first clip.")}), &[], Write),
        tool("screenshot_ui", "Screenshot window", "PNG of the AIFrames window exactly as the person sees it (needs the window: live mode or after launch_gui).", json!({}), &[], Global),
    ];
    if include_launch {
        v.push(tool(
            "launch_gui",
            "Open AIFrames window",
            "Start the AIFrames app (if not running) and connect to it so the person can watch every edit live. Projects opened headlessly move into the window.",
            json!({}),
            &[],
            GlobalWrite,
        ));
    }
    v
}

/// The `instructions` string returned by initialize.
pub fn instructions(mode: &str) -> String {
    format!(
        r##"AIFrames is a non-linear video editor (Premiere/Resolve-like) built for AI editing. These tools edit projects {mode}. Every edit is a typed op: atomic, validated, undoable, attributed to you ("ai") and visible to the person (changed clips flash in their timeline; your edits appear in their history and AI log).

Work loop
1. Look: list_projects / describe_project (the whole edit as text: video tracks from the top layer down to V1, then audio tracks with A1 first — the order the timeline shows them; clips as [start–end] timeline seconds with id, source, anchor, effects, notes; open problems). get_clip / get_timeline for details; get_state for what the person is looking at (their selection is what "this clip" means).
2. Edit: edit {{ops:[...]}} — one array = one atomic, undoable step. Use plan:true first for anything large (same receipt and findings, nothing changes). list_ops shows every op with an example (and the clip parameter ranges); list_effects / list_transitions the registries. move_clips moves several clips at once (swap two shots, move a group to another track) without intermediate overlaps.
3. Check: read the receipt (moved, warnings, new_findings, open_errors) and run validate (each finding has a fix and often fix_ops; apply_fixes:true applies them). After tightening, speeding up or trimming, re-check the brief's constraints (total length range, moments that must land on a word or screen, e.g. a fast-forward ending on the win screen) with describe_project and a snapshot — tightening silently moves them.
4. See: snapshot (frames → image; what is on screen; warnings when media is offline), contact_sheet (the whole edit at a glance), filmstrip (frames around a cut). Look after every few edits.
5. Deliver: save_project when asked, export (background job → export_status; presets 1080p/4k/shorts/square set the output size, or set_export_size in the project), chapters for the YouTube description, analyze_audio for levels (auto_ducking puts music under the voice).
Analyses of long media (detect_*, remove_silences, analyze_audio) run as background jobs: when a result has a job id and status "running", call jobs {{"job": id, "wait_seconds": 60}} for the result.

The model
- A clip's position is stored once: `start` is seconds or an ANCHOR — {{"after":"c3"}} (starts when c3 ends; chains of after make a magnetic storyline: trim one clip and everything after follows), {{"with":"c3","offset":1.2}}, {{"before":"c7"}}, {{"marker":"mk1"}}, {{"word":{{"clip":"c5","index":17}}}} (when word 17 of c5's transcript is spoken; follows cuts and speed). Prefer anchors to absolute times: a title anchored to its shot, b-roll to the word it illustrates, a hit to a beat marker.
- Everything inside a clip is CLIP-LOCAL: keyframe t (0 = the clip's first frame), fades, effect animation, text reveals, caption word timings. Moving or retiming a clip carries all of it. Never convert keys to absolute times.
- A key's `ease` shapes the move from THAT key to the next one: {{"keys":[{{"t":0,"v":1.4,"ease":"out"}},{{"t":0.3,"v":1}}]}} is an eased slam-in; an ease on the last key does nothing (validate flags it as ease_on_last_key).
- Timeline seconds (clip spans, markers, snapshot times) differ from source seconds (in_point, transcripts, detect_* results, i.e. time in the media file). Tools that analyse media also give timeline times.
- Ids are short and stable: tracks v1/a1, media m1, clips c1, markers mk1, transitions tr1, duckings d1; you may choose ids ("intro_title") in add ops. Video tracks stack bottom (v1) to top. Track roles (main, overlay, captions, voice, music) drive defaults.
- When the person's taste decides (which take to keep, whether a pause is a beat, a cut you are unsure of), propose it with suggest instead of applying it: it waits in their Suggestions panel to preview, apply or dismiss. Apply what they asked for directly.
- Nothing fails silently: an op that would break an anchor, overlap clips on a track, touch a locked track or run past the media is refused with how to fix it, and nothing changes.

- Overlays that should last as long as the shot under them (a "4x" badge on a sped-up clip, a lower third on an interview) end with an end anchor: `until` (set_until) instead of a fixed duration, so they follow every retime. Pauses between lines: set_spacing / close_gaps rather than moving clips one by one. schema {{type}} shows the exact JSON of any type (Clip, TextSource, ...) when an edit says "unknown field" or "invalid type".

Footage: look at source media with contact_sheet / snapshot / filmstrip {{asset}} (or {{file}} for a file not imported yet; times are SOURCE seconds) and media_info — NEVER put footage on the person's timeline just to look at it. Find a moment with a coarse contact_sheet over the whole file, then a finer one over the stretch, then filmstrip at the exact second.

Text-based editing: word timings that already exist (whisper JSON, <name>.words.json, SRT/VTT next to the media) are imported automatically by import_media, or explicitly with import_transcript — never type transcripts by hand. Otherwise transcribe (job); with the script at hand, align_transcript gives the words the script's spelling, case and punctuation while keeping the timings — but keeps written forms (3D, 10, $500) where a script for a voice spells them out (\"three D\", \"ten\"); check its kept_written and low_confidence regions. Then get_transcript (paragraphs; "indexed"/"words" for exact indices) or search_transcript (phrase → word indices + timeline time) → delete_words (ripple cut, edges snapped into silence; captions follow) → audit_joins (every cut measured: clipped words, doubled words, pauses) → generate_captions (karaoke captions anchored to words). remove_silences tightens pauses (plan:true first). detect_scenes / detect_beats can add markers to cut on.

Working with the person: they may edit at the same time. Start EVERY turn with changes_since {{revision}} (the revision of your last receipt) and keep what they changed. Each receipt has a revision (a plan's is the current one). Pass that revision as base_revision on every edit and suggest: if they changed something in between that your ops depend on (clips moved under absolute seconds, a clip you reference was changed), the edit is refused with what changed instead of landing in the wrong place — then changes_since, re-read and resend with the new revision. Project rule: tools without `project` act on YOUR session's project — the one you last created, opened or named (list_projects marks it session:true) — not on whatever tab the window shows (other programs and the person open projects too); only a session without one falls back to the window's active project. Every result names the project it acted on. undo undoes the latest edit whoever made it (the result says so) — don't undo their work unless asked. Saving and exporting are explicit; existing files are only replaced with overwrite:true — never overwrite files they did not ask for. If save_project says another program changed the file, find out what changed (open_project with reload:true loads it) before ever using overwrite_external. Relative output paths (export, save_dir, save_path) are relative to the project's folder. Unknown tool arguments are errors (with the valid list) — read them, don't guess."##
    )
}

/// Markdown reference of every op.
pub fn ops_reference() -> String {
    let mut s = String::from("# AIFrames ops\n\nPass ops to the `edit` tool as one array (atomic, one undo step).\n\n");
    s.push_str(&format!("- {}\n- {}\n", crate::tools::time_help(), crate::tools::param_help()));
    let mut group = "";
    for o in OPS {
        if o.group != group {
            group = o.group;
            s.push_str(&format!("\n## {group}\n\n"));
        }
        s.push_str(&format!("- **{}** — {}\n  `{}`\n", o.name, o.help, o.example));
    }
    s
}

/// Markdown reference of effects and transitions.
pub fn effects_reference() -> String {
    let mut s = String::from("# AIFrames effects\n\nAdd with `{\"op\":\"add_effect\",\"clip\":\"c3\",\"effect\":{\"kind\":\"blur\",\"params\":{\"radius\":8}}}`. Params can be animated with clip-local keys.\n");
    let mut cat = "";
    for e in EFFECTS {
        if e.category != cat {
            cat = e.category;
            s.push_str(&format!("\n## {cat}\n\n"));
        }
        s.push_str(&format!("- **{}** ({}) — {}\n", e.kind, e.label, e.help));
        for p in e.params {
            s.push_str(&format!("  - `{}` default {} ({}–{}{}) {}\n", p.name, p.default, p.min, p.max, if p.unit.is_empty() { String::new() } else { format!(" {}", p.unit) }, p.help));
        }
        for (c, d) in e.colors {
            s.push_str(&format!("  - colour `{c}` default {d}\n"));
        }
        for st in e.strings {
            s.push_str(&format!("  - string `{st}`\n"));
        }
    }
    s.push_str("\n# Transitions\n\n`{\"op\":\"add_transition\",\"from\":\"c3\",\"to\":\"c4\",\"kind\":\"crossfade\",\"duration\":0.5}` — the clips must touch on one track.\n\n");
    for t in TRANSITIONS {
        s.push_str(&format!("- **{}** ({}, default {} s) — {}\n", t.kind, t.label, t.default_duration, t.help));
    }
    s
}

/// Markdown reference of validation codes.
pub fn validation_reference() -> String {
    let mut s = String::from("# AIFrames validation codes\n\n");
    for (code, sev, help) in CODES {
        s.push_str(&format!("- **{code}** ({sev:?}) — {help}\n"));
    }
    s
}
