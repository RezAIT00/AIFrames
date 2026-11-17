//! Field-level documentation of the project JSON, for AIs and tooling ([`schema_docs`]).
//!
//! Every object type lists its fields with `type`, `required`, `default` (when optional), `units`,
//! `meaning` and an `example`; value grammars (time/anchor, param, ease, colour) list their forms, and
//! enums list their values. The unit tests check that every example deserialises and that the
//! documented fields are exactly the fields the model accepts.

use serde_json::{json, Map, Value};

/// A required field.
fn req(ty: &str, units: &str, meaning: &str, example: Value) -> Value {
    let mut v = json!({"type": ty, "required": true, "meaning": meaning, "example": example});
    if !units.is_empty() {
        v["units"] = json!(units);
    }
    v
}

/// An optional field (omitted from files when it has its default).
fn opt(ty: &str, default: Value, units: &str, meaning: &str, example: Value) -> Value {
    let mut v = json!({"type": ty, "required": false, "default": default, "meaning": meaning, "example": example});
    if !units.is_empty() {
        v["units"] = json!(units);
    }
    v
}

fn fields(list: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in list {
        m.insert(k.to_string(), v);
    }
    Value::Object(m)
}

fn ty(about: &str, f: Value, example: Value) -> Value {
    json!({"about": about, "fields": f, "example": example})
}

fn text_fields() -> Vec<(&'static str, Value)> {
    vec![
        ("text", req("string", "", "The text. Words are split at spaces (captions join a multi-part word with a no-break space U+00A0); `\\n` breaks lines.", json!("Let's go"))),
        ("font", opt("string", json!("Segoe UI"), "", "Font family name (installed or in the project folder).", json!("Inter"))),
        ("size", opt("number", json!(72.0), "px", "Em size in project pixels.", json!(64.0))),
        ("weight", opt("integer", json!(700), "", "Font weight 100..900 (400 regular, 700 bold).", json!(800))),
        ("italic", opt("boolean", json!(false), "", "Italic style.", json!(true))),
        ("color", opt("color", json!("#FFFFFF"), "", "Fill colour of the glyphs.", json!("#FFD60A"))),
        ("align", opt("enum align", json!("center"), "", "Alignment of the lines inside the text box.", json!("left"))),
        ("line_height", opt("number", json!(1.15), "× size", "Line spacing as a multiple of `size`.", json!(1.2))),
        ("letter_spacing", opt("number", json!(0.0), "px", "Extra space between letters.", json!(2.0))),
        ("max_width", opt("number | null", Value::Null, "px", "Wrap width; null = no wrapping.", json!(1496.0))),
        ("background", opt("text_box | null", Value::Null, "", "Box drawn behind the text.", json!({"color": "#000000AA", "padding": 16, "radius": 8}))),
        ("stroke", opt("stroke | null", Value::Null, "", "Outline around the glyphs.", json!({"color": "#000000", "width": 4}))),
        ("shadow", opt("shadow | null", Value::Null, "", "Drop shadow of the glyphs.", json!({"color": "#00000099", "blur": 8, "x": 0, "y": 4}))),
        ("reveal", opt("reveal", json!({"kind": "none", "duration": 0.0}), "", "Built-in entrance animation.", json!({"kind": "pop_words", "duration": 0.6}))),
        (
            "highlight",
            opt(
                "highlight | null",
                Value::Null,
                "",
                "Karaoke/caption word highlighting: one active window per word of `text`.",
                json!({"color": "#FFD60A", "words": [[0.0, 0.32], [0.32, 0.7]]}),
            ),
        ),
    ]
}

fn shape_fields() -> Vec<(&'static str, Value)> {
    vec![
        ("kind", req("enum shape_kind", "", "What to draw.", json!("rect"))),
        ("width", req("number", "px", "Box width (lines/arrows: x of the vector from start to end point).", json!(400.0))),
        ("height", req("number", "px", "Box height (lines/arrows: y of that vector).", json!(120.0))),
        ("fill", opt("color | null", Value::Null, "", "Fill colour; null = no fill.", json!("#E63946"))),
        ("stroke", opt("stroke | null", Value::Null, "", "Outline.", json!({"color": "#FFFFFF", "width": 6}))),
        ("radius", opt("number", json!(0.0), "px", "Corner radius of rectangles.", json!(12.0))),
        ("draw", opt("param", json!(1.0), "fraction 0..1", "Portion of the outline drawn (animate 0→1 for a draw-on).", json!({"keys": [{"t": 0, "v": 0, "ease": "out"}, {"t": 0.5, "v": 1}]}))),
    ]
}

/// Field-level documentation of the project file: object types (fields with type, required, default,
/// units, meaning, example), value forms (time/anchor, param, key/ease, colour) and enum values.
pub fn schema_docs() -> Value {
    let time_meaning = "Timeline seconds or an anchor (see forms.time).";
    let mut types = Map::new();

    types.insert(
        "project".into(),
        ty(
            "A whole project file (.aifr). Defaults are omitted when saved.",
            fields(vec![
                ("format", req("integer", "", "File format version (currently 1).", json!(1))),
                ("name", opt("string", json!(""), "", "Project name.", json!("Cold open"))),
                ("settings", opt("settings", json!({"width": 1920, "height": 1080, "fps": 30.0}), "", "Frame size, frame rate, audio rate, background.", json!({"width": 1920, "height": 1080, "fps": 29.97}))),
                ("notes", opt("string", json!(""), "", "Intent for humans and AIs.", json!("60 s teaser for the launch"))),
                ("media", opt("media[]", json!([]), "", "Imported files.", json!([{"id": "m1", "path": "media/vo.wav", "kind": "audio"}]))),
                ("tracks", opt("track[]", json!([]), "", "Tracks, first = bottom of the picture stack.", json!([{"id": "v1", "kind": "video", "role": "main"}]))),
                ("clips", opt("clip[]", json!([]), "", "Clips on tracks.", json!([{"id": "c1", "track": "v1", "start": 0, "duration": 2, "source": {"type": "solid", "color": "#000000"}}]))),
                ("transitions", opt("transition[]", json!([]), "", "Transitions across cuts.", json!([{"id": "t1", "from": "c1", "to": "c2", "kind": "crossfade", "duration": 0.5}]))),
                ("markers", opt("marker[]", json!([]), "", "Timeline markers (chapters, beats, todos).", json!([{"id": "mk1", "time": 12.0, "name": "drop"}]))),
                ("ducking", opt("ducking[]", json!([]), "", "Sidechain ducking rules.", json!([{"id": "d1", "target": "a2", "trigger": "a1"}]))),
                ("transcripts", opt("map<media id, transcript>", json!({}), "", "Word timings per media asset (source seconds).", json!({"m1": {"words": [{"text": "Hello", "start": 0.12, "end": 0.48}]}}))),
                ("export", opt("export", json!({}), "", "Render settings.", json!({"codec": "h264", "quality": "high"}))),
                ("id_counters", opt("map<prefix, integer>", json!({}), "", "Highest number issued per id prefix, so ids are never reused (kept by the ops).", json!({"c": 42, "t": 3}))),
                (
                    "caption_exclusions",
                    opt(
                        "map<clip id, [number, number][]>",
                        json!({}),
                        "source s",
                        "Per voice clip: source ranges whose captions a person deleted; generate_captions does not recreate them for that clip unless `replace: true` (kept by the ops).",
                        json!({"vo1": [[12.4, 15.0]]}),
                    ),
                ),
                ("suggestions", opt("suggestion[]", json!([]), "", "Edits proposed for review and not yet applied (add_suggestion; applied or dismissed by a person).", json!([{"id": "sg1", "label": "Cut the false start", "ops": [{"op": "cut_source", "clip": "c3", "from": 4.1, "to": 5.0}]}]))),
            ]),
            json!({"format": 1, "name": "demo", "tracks": [{"id": "v1", "kind": "video"}], "clips": [{"id": "c1", "track": "v1", "start": 0, "duration": 3, "source": {"type": "text", "text": "Hi"}}]}),
        ),
    );

    types.insert(
        "suggestion".into(),
        ty(
            "An edit proposed for a person to review: applying it runs its ops as one undoable step and removes it.",
            fields(vec![
                ("id", req("string", "", "Unique id (\"sg1\").", json!("sg1"))),
                ("label", req("string", "", "One sentence: what applying it does.", json!("Cut the false start before \"So today\""))),
                ("reason", opt("string", json!(""), "", "Why, and what to listen or look for before deciding.", json!("The restart is cleaner; check the breath at the join"))),
                ("at", opt("number", json!(null), "s", "Timeline time where its effect plays (previews seek there).", json!(12.4))),
                ("ops", req("op[]", "", "The edit (see the op reference).", json!([{"op": "cut_source", "clip": "c3", "from": 4.1, "to": 5.0}]))),
                ("author", opt("string", json!(""), "", "Who proposed it.", json!("ai"))),
            ]),
            json!({"id": "sg1", "label": "Cut the false start", "at": 12.4, "ops": [{"op": "cut_source", "clip": "c3", "from": 4.1, "to": 5.0}]}),
        ),
    );

    types.insert(
        "settings".into(),
        ty(
            "Project frame and audio settings.",
            fields(vec![
                ("width", req("integer", "px", "Frame width.", json!(1920))),
                ("height", req("integer", "px", "Frame height.", json!(1080))),
                (
                    "fps",
                    req(
                        "number",
                        "frames/s",
                        "Frame rate. 23.976/29.97/59.94 mean the exact NTSC rates 24000/1001, 30000/1001, 60000/1001. Clips on video tracks start and end on whole frames (resolved spans are rounded to the nearest frame; stored times are kept).",
                        json!(29.97),
                    ),
                ),
                ("sample_rate", opt("integer", json!(48000), "Hz", "Audio mix rate.", json!(48000))),
                ("background", opt("color", json!("#000000"), "", "Colour behind all tracks.", json!("#101014"))),
                ("duration", opt("number | null", Value::Null, "s", "Fixed project length; null = ends with the last clip.", json!(60.0))),
                ("title_safe", opt("number", json!(0.8), "fraction 0..1", "Title-safe box (guides, validation, caption placement).", json!(0.8))),
                ("action_safe", opt("number", json!(0.9), "fraction 0..1", "Action-safe box.", json!(0.9))),
            ]),
            json!({"width": 1080, "height": 1920, "fps": 30}),
        ),
    );

    types.insert(
        "media".into(),
        ty(
            "An imported file (video, audio or image).",
            fields(vec![
                ("id", req("string (id)", "", "Stable id clips refer to.", json!("m1"))),
                ("name", opt("string", json!(""), "", "Display name.", json!("Voice-over take 3"))),
                ("path", req("string", "", "File path, relative to the project folder when possible.", json!("media/vo.wav"))),
                ("kind", req("enum media_kind", "", "What the file is.", json!("audio"))),
                ("info", opt("media_info", json!({}), "", "Probed facts (filled in by the app on import).", json!({"duration": 42.5, "has_audio": true, "audio_rate": 48000}))),
                ("tags", opt("string[]", json!([]), "", "Free tags.", json!(["vo"]))),
                ("notes", opt("string", json!(""), "", "Notes about the file.", json!("clean take"))),
            ]),
            json!({"id": "m2", "path": "media/broll.mp4", "kind": "video"}),
        ),
    );

    types.insert(
        "media_info".into(),
        ty(
            "Probed facts about a media file.",
            fields(vec![
                ("duration", opt("number", json!(0.0), "source s", "Length of the file (0 for stills).", json!(42.5))),
                ("width", opt("integer", json!(0), "px", "Picture width.", json!(1920))),
                ("height", opt("integer", json!(0), "px", "Picture height.", json!(1080))),
                ("fps", opt("number", json!(0.0), "frames/s", "Native frame rate.", json!(29.97))),
                ("has_video", opt("boolean", json!(false), "", "Has a picture stream.", json!(true))),
                ("has_audio", opt("boolean", json!(false), "", "Has a sound stream.", json!(true))),
                ("audio_channels", opt("integer", json!(0), "", "Sound channels.", json!(2))),
                ("audio_rate", opt("integer", json!(0), "Hz", "Sound sample rate.", json!(48000))),
                ("codec", opt("string", json!(""), "", "Codec name.", json!("h264"))),
                ("rotation", opt("integer", json!(0), "degrees", "Display rotation from the file's metadata.", json!(90))),
            ]),
            json!({"duration": 12.0, "width": 1920, "height": 1080, "fps": 30, "has_video": true}),
        ),
    );

    types.insert(
        "track".into(),
        ty(
            "A track. Video tracks stack upward (later = on top); audio tracks are mixed.",
            fields(vec![
                ("id", req("string (id)", "", "Stable id.", json!("v2"))),
                ("name", opt("string", json!(""), "", "Display name.", json!("Titles"))),
                ("kind", req("enum track_kind", "", "Pictures or sound.", json!("video"))),
                ("role", opt("string", json!(""), "", "Semantic role: main, overlay, captions, voice, music, sfx or any word (ducking, validation, AI tools).", json!("captions"))),
                ("muted", opt("boolean", json!(false), "", "Silence the track.", json!(true))),
                ("hidden", opt("boolean", json!(false), "", "Hide the track's picture.", json!(true))),
                ("locked", opt("boolean", json!(false), "", "Edits that would change its clips are refused.", json!(true))),
                ("solo", opt("boolean", json!(false), "", "Only solo tracks are heard.", json!(true))),
                ("volume", opt("number", json!(1.0), "linear gain", "Gain for every clip's sound on the track.", json!(0.8))),
            ]),
            json!({"id": "a1", "kind": "audio", "role": "voice"}),
        ),
    );

    types.insert(
        "clip".into(),
        ty(
            "A clip on a track. Keyframes, fades, effects and caption timings inside it are clip-local, so they move with it.",
            fields(vec![
                ("id", req("string (id)", "", "Stable id.", json!("c3"))),
                ("track", req("string (id)", "", "Track it sits on.", json!("v1"))),
                ("name", opt("string", json!(""), "", "Display name.", json!("Hook"))),
                ("start", req("time", "s", time_meaning, json!({"after": "c2"}))),
                (
                    "duration",
                    req(
                        "number",
                        "s",
                        "Timeline length (> 0). With `until` it is derived (kept up to date by the ops). On video tracks the shown span is rounded to whole frames.",
                        json!(4.5),
                    ),
                ),
                (
                    "until",
                    opt(
                        "time | null",
                        Value::Null,
                        "s",
                        "End anchor: the clip ends there, e.g. {\"after\":\"c3\"} = when c3 ends, {\"with\":\"c4\"} = when c4 starts. Overlays use it to follow their base clip's length. Error if it resolves at or before the start.",
                        json!({"after": "c3"}),
                    ),
                ),
                ("source", req("source", "", "What the clip shows/plays (see types.source).", json!({"type": "media", "asset": "m1", "in_point": 2.5}))),
                ("speed", opt("param", json!(1.0), "× realtime", "Playback speed of media (0 = freeze, 4 = fast-forward); animated = speed ramp.", json!(2.0))),
                ("reverse", opt("boolean", json!(false), "", "Play media backwards.", json!(true))),
                ("transform", opt("transform", json!({}), "", "Position, scale, rotation, fit, crop.", json!({"scale": 1.2, "y": -80}))),
                ("opacity", opt("param", json!(1.0), "0..1", "Picture opacity.", json!(0.85))),
                ("volume", opt("param", json!(1.0), "linear gain", "Gain of the clip's sound.", json!(0.5))),
                ("fade_in", opt("number", json!(0.0), "s", "Fade from the clip's start (picture for visual clips, sound for clips with audio).", json!(0.3))),
                ("fade_out", opt("number", json!(0.0), "s", "Fade to the clip's end.", json!(0.3))),
                ("muted", opt("boolean", json!(false), "", "Silence the clip's sound (a video clip's embedded audio).", json!(true))),
                ("blend", opt("enum blend", json!("normal"), "", "How the picture combines with the tracks below.", json!("screen"))),
                ("effects", opt("effect[]", json!([]), "", "Effect stack, applied in order (adjustment clips: to everything below).", json!([{"kind": "blur", "params": {"radius": 8}}]))),
                ("enabled", opt("boolean", json!(true), "", "Disabled clips are skipped by the render.", json!(false))),
                ("tags", opt("string[]", json!([]), "", "Free tags (captions carry \"caption\"; \"edited\" marks captions a person changed).", json!(["broll"]))),
                ("notes", opt("string", json!(""), "", "Intent (\"hook: the win moment\").", json!("b-roll for line 4"))),
                (
                    "motion_blur",
                    opt(
                        "motion_blur | null",
                        Value::Null,
                        "",
                        "Motion blur: each frame averages `samples` renders spread over the shutter (transform, opacity and effects animate within it). null = off.",
                        json!({"shutter": 180, "samples": 12}),
                    ),
                ),
                (
                    "frame_blend",
                    opt("boolean", json!(false), "", "Frame blending (velocity blur): while faster than 1×, each output frame averages every source frame it covers.", json!(true)),
                ),
                (
                    "matte",
                    opt(
                        "matte | null",
                        Value::Null,
                        "",
                        "Track matte: the clip is drawn only through another clip (e.g. footage inside giant text); that clip is then not drawn itself. Evaluated at the same timeline time, whatever track the matte is on.",
                        json!({"clip": "title", "mode": "alpha"}),
                    ),
                ),
            ]),
            json!({"id": "c4", "track": "v2", "start": {"with": "c3", "offset": 0.5}, "duration": 2.0, "until": {"after": "c3"}, "source": {"type": "text", "text": "NEW"}}),
        ),
    );

    let mut variants = Map::new();
    variants.insert(
        "media".into(),
        ty(
            "A video, audio or image asset.",
            fields(vec![
                ("type", req("\"media\"", "", "Variant tag.", json!("media"))),
                ("asset", req("string (media id)", "", "The media asset.", json!("m1"))),
                ("in_point", opt("number", json!(0.0), "source s", "Source time shown at the clip's first frame.", json!(12.25))),
            ]),
            json!({"type": "media", "asset": "m1", "in_point": 3.0}),
        ),
    );
    let mut tf = vec![("type", req("\"text\"", "", "Variant tag.", json!("text")))];
    tf.extend(text_fields());
    variants.insert("text".into(), ty("Text (titles, captions); the fields of types.text.", fields(tf), json!({"type": "text", "text": "Chapter 1", "size": 96})));
    variants.insert(
        "solid".into(),
        ty(
            "A flat colour filling the frame (or a box when transform/crop say so).",
            fields(vec![("type", req("\"solid\"", "", "Variant tag.", json!("solid"))), ("color", req("color", "", "The colour.", json!("#1D3557")))]),
            json!({"type": "solid", "color": "#000000"}),
        ),
    );
    let mut sf = vec![("type", req("\"shape\"", "", "Variant tag.", json!("shape")))];
    sf.extend(shape_fields());
    variants.insert("shape".into(), ty("A vector shape; the fields of types.shape.", fields(sf), json!({"type": "shape", "kind": "arrow", "width": 300, "height": 0, "stroke": {"color": "#FFFFFF", "width": 8}})));
    variants.insert(
        "adjustment".into(),
        ty(
            "Draws nothing; its effects (and opacity as mix) apply to the tracks below during its span.",
            fields(vec![("type", req("\"adjustment\"", "", "Variant tag.", json!("adjustment")))]),
            json!({"type": "adjustment"}),
        ),
    );
    types.insert(
        "source".into(),
        json!({
            "about": "What a clip shows or plays: an object with a `type` tag and the fields of that variant.",
            "variants": Value::Object(variants),
            "example": {"type": "media", "asset": "m1"},
        }),
    );

    types.insert(
        "text".into(),
        ty(
            "The fields of a text source ({\"type\":\"text\", ...}).",
            fields(text_fields()),
            json!({"text": "SUBSCRIBE", "size": 80, "weight": 900, "color": "#FFFFFF", "stroke": {"color": "#000000", "width": 6}}),
        ),
    );
    types.insert(
        "text_box".into(),
        ty(
            "Box behind text.",
            fields(vec![
                ("color", req("color", "", "Box colour.", json!("#000000AA"))),
                ("padding", opt("number", json!(16.0), "px", "Space between text and box edge.", json!(24.0))),
                ("radius", opt("number", json!(0.0), "px", "Corner radius.", json!(10.0))),
            ]),
            json!({"color": "#000000B3", "padding": 20}),
        ),
    );
    types.insert(
        "stroke".into(),
        ty(
            "An outline.",
            fields(vec![("color", req("color", "", "Outline colour.", json!("#000000"))), ("width", req("number", "px", "Outline width.", json!(4.0)))]),
            json!({"color": "#000000", "width": 3}),
        ),
    );
    types.insert(
        "shadow".into(),
        ty(
            "A drop shadow behind text.",
            fields(vec![
                ("color", req("color", "", "Shadow colour (alpha = strength).", json!("#00000099"))),
                ("blur", opt("number", json!(0.0), "px", "Soft edge width (2σ of the Gaussian); 0 = hard.", json!(8.0))),
                ("x", opt("number", json!(0.0), "px", "Offset right.", json!(2.0))),
                ("y", opt("number", json!(0.0), "px", "Offset down.", json!(4.0))),
            ]),
            json!({"color": "#000000AA", "blur": 6, "y": 3}),
        ),
    );
    types.insert(
        "reveal".into(),
        ty(
            "Built-in text entrance.",
            fields(vec![
                ("kind", opt("enum reveal_kind", json!("none"), "", "Animation style; \"spoken\" shows each word as it is said (word windows of `highlight`, as captions have; duration unused).", json!("typewriter"))),
                ("duration", opt("number", json!(0.0), "clip-local s", "Length of the whole reveal from the clip's start.", json!(0.8))),
            ]),
            json!({"kind": "fade_words", "duration": 0.5}),
        ),
    );
    types.insert(
        "highlight".into(),
        ty(
            "Per-word highlight windows (karaoke captions). Window k belongs to word k of `text` (split at spaces).",
            fields(vec![
                ("color", req("color", "", "Colour of the active word.", json!("#FFD60A"))),
                (
                    "words",
                    req(
                        "[number, number][]",
                        "clip-local s",
                        "[start, end] per word, counted from where the clip is shown (its frame-accurate start), so a window may start up to half a frame before 0 to match the voice exactly.",
                        json!([[0.0, 0.4], [0.4, 0.9]]),
                    ),
                ),
                ("background", opt("color | null", Value::Null, "", "Box behind the active word.", json!("#00000080"))),
            ]),
            json!({"color": "#00E5FF", "words": [[0.0, 0.5]]}),
        ),
    );
    types.insert("shape".into(), ty("The fields of a shape source ({\"type\":\"shape\", ...}).", fields(shape_fields()), json!({"kind": "ellipse", "width": 200, "height": 200, "fill": "#E63946"})));

    types.insert(
        "transform".into(),
        ty(
            "How a visual clip sits in the frame. x/y = offset of the content's centre from the frame centre.",
            fields(vec![
                ("x", opt("param", json!(0.0), "px", "Offset right of centre.", json!(-420.0))),
                ("y", opt("param", json!(0.0), "px", "Offset down from centre.", json!(300.0))),
                ("scale", opt("param", json!(1.0), "× fitted size", "Uniform scale on top of `fit`.", json!({"keys": [{"t": 0, "v": 1, "ease": "in_out"}, {"t": 4, "v": 1.1}]}))),
                ("rotation", opt("param", json!(0.0), "degrees", "Clockwise rotation.", json!(-5.0))),
                ("rotation_x", opt("param", json!(0.0), "degrees", "3-D card tilt around the horizontal axis through the anchor (+ = top edge away from the viewer).", json!(25.0))),
                ("rotation_y", opt("param", json!(0.0), "degrees", "3-D card turn around the vertical axis through the anchor (+ = right edge away from the viewer).", json!({"keys": [{"t": 0, "v": -90, "ease": "out"}, {"t": 0.4, "v": 0}]}))),
                ("perspective", opt("number", json!(2000.0), "px", "Focal distance of the 3-D view (smaller = stronger perspective; 0 = orthographic). Not animatable.", json!(1200.0))),
                ("anchor_x", opt("number", json!(0.5), "fraction 0..1", "Pivot for scale/rotation across the content box.", json!(0.0))),
                ("anchor_y", opt("number", json!(0.5), "fraction 0..1", "Pivot for scale/rotation down the content box.", json!(1.0))),
                ("fit", opt("enum fit", json!("contain"), "", "How media fills the frame (text and shapes keep their own size).", json!("cover"))),
                ("flip_x", opt("boolean", json!(false), "", "Mirror horizontally.", json!(true))),
                ("flip_y", opt("boolean", json!(false), "", "Mirror vertically.", json!(true))),
                ("crop", opt("crop", json!({}), "", "Edges cut before fitting.", json!({"left": 0.1, "right": 0.1}))),
            ]),
            json!({"scale": 0.4, "x": 640, "y": -300, "fit": "cover"}),
        ),
    );
    types.insert(
        "crop".into(),
        ty(
            "Fractions of the content cut from each edge, applied before `fit` (media is re-fitted, a solid's box shrinks; text and shapes ignore it).",
            fields(vec![
                ("left", opt("param", json!(0.0), "fraction 0..1", "Cut from the left edge.", json!(0.1))),
                ("top", opt("param", json!(0.0), "fraction 0..1", "Cut from the top edge.", json!(0.05))),
                ("right", opt("param", json!(0.0), "fraction 0..1", "Cut from the right edge.", json!(0.1))),
                ("bottom", opt("param", json!(0.0), "fraction 0..1", "Cut from the bottom edge.", json!(0.05))),
            ]),
            json!({"left": 0.125, "right": 0.125}),
        ),
    );
    types.insert(
        "effect".into(),
        ty(
            "One effect in a clip's stack. `kind` names an effect of the registry (list_effects); missing params take its defaults.",
            fields(vec![
                ("kind", req("string", "", "Registry name: color, blur, sharpen, vignette, chroma_key, mask_rect, drop_shadow, glow, lut, ...", json!("vignette"))),
                ("enabled", opt("boolean", json!(true), "", "Bypass when false.", json!(false))),
                ("params", opt("map<string, param>", json!({}), "per the registry", "Numeric parameters (may be animated). list_effects gives each one's default, valid range min–max (values outside it, or unknown names, are refused), unit (frac = 0–1, shown as % in the inspector; px = project pixels; deg; x = multiplier; stops; s; hz; count = whole number; ratio; toggle = 0/1), named values in its help (\"0 = auto, 1 = behind\"), and `slider`/`curve` where the inspector's slider covers only the useful part of the range or is not linear.", json!({"amount": 0.6}))),
                ("colors", opt("map<string, color>", json!({}), "", "Colour parameters.", json!({"color": "#000000"}))),
                ("strings", opt("map<string, string>", json!({}), "", "String parameters (e.g. a LUT file path).", json!({}))),
            ]),
            json!({"kind": "blur", "params": {"radius": {"keys": [{"t": 0, "v": 20, "ease": "out"}, {"t": 1, "v": 0}]}}}),
        ),
    );
    types.insert(
        "motion_blur".into(),
        ty(
            "Motion blur of a clip (clip.motion_blur).",
            fields(vec![
                ("shutter", opt("number", json!(180.0), "degrees", "Shutter angle 0–720: 180 blurs half a frame of motion, 360 a whole frame.", json!(270.0))),
                ("samples", opt("integer", json!(12), "", "Sub-frame renders averaged per frame, 2–64 (more = smoother, slower).", json!(16))),
            ]),
            json!({"shutter": 180, "samples": 12}),
        ),
    );
    types.insert(
        "matte".into(),
        ty(
            "A track matte (clip.matte): the clip is drawn through another clip.",
            fields(vec![
                ("clip", req("string (clip id)", "", "The matte source: a visual clip. It is not drawn normally while used as a matte.", json!("title"))),
                ("mode", opt("enum matte_mode", json!("alpha"), "", "How the matte lets the clip through.", json!("luma"))),
            ]),
            json!({"clip": "big_text"}),
        ),
    );
    types.insert(
        "transition".into(),
        ty(
            "A transition across the cut between two touching clips on one track, centred on the cut.",
            fields(vec![
                ("id", req("string (id)", "", "Stable id.", json!("t1"))),
                ("from", req("string (clip id)", "", "Outgoing clip.", json!("c1"))),
                ("to", req("string (clip id)", "", "Incoming clip (starts where `from` ends).", json!("c2"))),
                ("kind", req("string", "", "Registry name: crossfade, dip_black, dip_white, wipe_left, slide_left, push_left, zoom_in, iris, blur_dissolve, whip, ...", json!("crossfade"))),
                ("duration", req("number", "s", "Total length, half before and half after the cut.", json!(0.5))),
                ("params", opt("map<string, number>", json!({}), "per the registry", "Transition parameters, described like effect parameters by list_transitions (default, min–max, unit, help, slider, curve).", json!({}))),
                ("colors", opt("map<string, color>", json!({}), "", "Colour parameters (e.g. film_burn's `color`); missing ones take the registry default.", json!({"color": "#FF8A2B"}))),
            ]),
            json!({"id": "t2", "from": "c2", "to": "c3", "kind": "dip_black", "duration": 0.6}),
        ),
    );
    types.insert(
        "marker".into(),
        ty(
            "A timeline marker; clips can anchor to it ({\"marker\":\"mk1\"}).",
            fields(vec![
                ("id", req("string (id)", "", "Stable id.", json!("mk1"))),
                ("time", req("time", "s", time_meaning, json!({"word": {"clip": "vo", "index": 12}}))),
                ("name", opt("string", json!(""), "", "Label (chapter title for chapters).", json!("The reveal"))),
                ("kind", opt("enum marker_kind", json!("marker"), "", "What it marks.", json!("chapter"))),
                ("color", opt("color | null", Value::Null, "", "Display colour.", json!("#FFB703"))),
                ("note", opt("string", json!(""), "", "Note.", json!("tighten this"))),
            ]),
            json!({"id": "mk2", "time": 30.0, "kind": "beat"}),
        ),
    );
    types.insert(
        "ducking".into(),
        ty(
            "Sidechain ducking: lower the `target` track while the `trigger` track has sound.",
            fields(vec![
                ("id", req("string (id)", "", "Stable id.", json!("d1"))),
                ("target", req("string (track id)", "", "Track that gets quieter (music).", json!("a2"))),
                ("trigger", req("string (track id)", "", "Track whose sound triggers it (voice).", json!("a1"))),
                ("amount_db", opt("number", json!(-14.0), "dB", "Gain change while active (negative).", json!(-10.0))),
                ("threshold_db", opt("number", json!(-45.0), "dBFS", "Trigger level above which it counts as active.", json!(-40.0))),
                ("attack", opt("number", json!(0.12), "s", "Time to reach the full reduction (the ducker looks ahead by this much).", json!(0.1))),
                ("release", opt("number", json!(0.45), "s", "Recovery time, after a 0.3 s hold below the threshold.", json!(0.6))),
            ]),
            json!({"id": "d1", "target": "a2", "trigger": "a1", "amount_db": -12}),
        ),
    );
    types.insert(
        "transcript".into(),
        ty(
            "Word timings of one media asset (project.transcripts[asset]).",
            fields(vec![
                ("language", opt("string", json!(""), "", "Language code.", json!("en"))),
                ("words", req("word[]", "", "Words in spoken order.", json!([{"text": "Hello", "start": 0.12, "end": 0.48}]))),
            ]),
            json!({"language": "en", "words": [{"text": "Hi", "start": 0.1, "end": 0.3}]}),
        ),
    );
    types.insert(
        "word".into(),
        ty(
            "One transcribed word.",
            fields(vec![
                ("text", req("string", "", "The word as spoken (may contain spaces: \"three D\").", json!("Hello"))),
                ("start", req("number", "source s", "When the word starts in the media file.", json!(0.12))),
                ("end", req("number", "source s", "When it ends.", json!(0.48))),
                ("conf", opt("number", json!(1.0), "0..1", "Recognition confidence.", json!(0.93))),
            ]),
            json!({"text": "world", "start": 0.5, "end": 0.9}),
        ),
    );
    types.insert(
        "word_ref".into(),
        ty(
            "The target of a word anchor ({\"word\": {...}}): a word of the transcript of a clip's media, through that clip's in-point and speed.",
            fields(vec![
                ("clip", req("string (clip id)", "", "Clip playing the media.", json!("vo"))),
                ("index", req("integer", "", "Word index in the transcript (0-based).", json!(17))),
                ("end", opt("boolean", json!(false), "", "Anchor to the word's end instead of its start.", json!(true))),
                (
                    "text",
                    opt("string | null", Value::Null, "", "The word's text when the anchor was made (filled in by the ops); validate warns (word_anchor_text_mismatch) when the word at `index` changed.", json!("ten")),
                ),
            ]),
            json!({"clip": "vo", "index": 3}),
        ),
    );
    types.insert(
        "key".into(),
        ty(
            "One keyframe of an animated param.",
            fields(vec![
                ("t", req("number", "clip-local s", "Time from the clip's start.", json!(0.5))),
                ("v", req("number", "the param's units", "Value at `t`.", json!(1.2))),
                ("ease", opt("ease", json!("linear"), "", "Easing of the segment from this key to the next one (see forms.ease); an ease on the last key does nothing.", json!("in_out"))),
            ]),
            json!({"t": 0, "v": 0, "ease": "out"}),
        ),
    );
    types.insert(
        "export".into(),
        ty(
            "Render settings (project.export).",
            fields(vec![
                ("path", opt("string | null", Value::Null, "", "Output file, relative to the project folder or absolute.", json!("out/final.mp4"))),
                ("codec", opt("enum export_codec", json!("h264"), "", "Output codec / container.", json!("hevc"))),
                ("quality", opt("enum export_quality", json!("high"), "", "Encoder quality preset.", json!("standard"))),
                ("hardware", opt("boolean", json!(true), "", "Use the GPU encoder when available.", json!(false))),
                ("audio_bitrate_kbps", opt("integer", json!(256), "kbit/s", "Audio bitrate.", json!(320))),
                ("loudness_lufs", opt("number | null", json!(-14.0), "LUFS", "Integrated loudness target (two-pass); null = leave the mix as is.", json!(-16.0))),
                ("true_peak_db", opt("number", json!(-1.5), "dBTP", "True-peak ceiling.", json!(-1.0))),
                ("range", opt("[number, number] | null", Value::Null, "s", "Only render [start, end] of the timeline.", json!([10.0, 20.0]))),
                ("width", opt("integer | null", Value::Null, "px", "Output width; null = project width (or derived from height, keeping the aspect).", json!(1280))),
                ("height", opt("integer | null", Value::Null, "px", "Output height; null = project height (or derived from width).", json!(720))),
                ("fit", opt("enum fit", json!("contain"), "", "How the project frame maps onto an output of another aspect (contain = letterbox).", json!("cover"))),
            ]),
            json!({"path": "out/teaser.mp4", "codec": "h264", "quality": "high", "width": 1080, "height": 1920, "fit": "cover"}),
        ),
    );

    let forms = json!({
        "time": {
            "about": "A point on the timeline: absolute seconds, or an anchor resolved when the project is resolved. Every anchor takes an optional `offset` (seconds, negative = earlier). Exactly one target key per anchor.",
            "forms": [
                {"form": "number", "meaning": "Absolute timeline seconds.", "example": 12.5},
                {"form": "{\"after\": clip}", "meaning": "When that clip ends (magnetic chain).", "example": {"after": "c3"}},
                {"form": "{\"with\": clip}", "meaning": "When that clip starts.", "example": {"with": "c3", "offset": 1.2}},
                {"form": "{\"before\": clip}", "meaning": "As a start: this clip ends when that clip starts (start = its start − this duration). As an `until`: that clip's start.", "example": {"before": "c7"}},
                {"form": "{\"marker\": marker}", "meaning": "At a marker's time.", "example": {"marker": "mk2"}},
                {"form": "{\"word\": word_ref}", "meaning": "When a transcript word of a clip's media is heard (through the clip's in-point and speed); error if the word is cut out of the clip.", "example": {"word": {"clip": "vo", "index": 17, "text": "ten"}, "offset": -0.1}},
            ],
            "frames": "Clips on video tracks are shown on whole frames: their resolved start and end are rounded to the nearest frame of settings.fps (29.97 = 30000/1001). Audio clips keep their exact times. Stored values are not changed.",
        },
        "until": {
            "about": "clip.until uses the time forms as an end point: {\"after\":\"c3\"} = c3's end, {\"with\":\"c3\"} or {\"before\":\"c3\"} = c3's start, a marker, a word, or seconds.",
            "example": {"after": "c3", "offset": -0.5},
        },
        "param": {
            "about": "An animatable number: a bare number when constant, else keyframes with clip-local times.",
            "forms": [
                {"form": "number", "meaning": "Constant value.", "example": 0.8},
                {"form": "{\"keys\": key[]}", "meaning": "Keyframes (types.key), sorted by t; values hold before the first and after the last key.", "example": {"keys": [{"t": 0, "v": 0, "ease": "out"}, {"t": 0.4, "v": 1}]}},
            ],
        },
        "ease": {
            "about": "Easing of a keyframe segment, written on the key where the segment STARTS: {\"t\":0,\"v\":0,\"ease\":\"out\"},{\"t\":1,\"v\":1} eases the move from 0 to 1; an ease on the last key does nothing.",
            "forms": [
                {"form": "\"linear\"", "meaning": "Constant rate (default).", "example": "linear"},
                {"form": "\"hold\"", "meaning": "Keep this key's value until the next key.", "example": "hold"},
                {"form": "\"in\"", "meaning": "Start slow.", "example": "in"},
                {"form": "\"out\"", "meaning": "End slow.", "example": "out"},
                {"form": "\"in_out\"", "meaning": "Slow at both ends.", "example": "in_out"},
                {"form": "\"back_out\"", "meaning": "Overshoot, then settle.", "example": "back_out"},
                {"form": "\"elastic\"", "meaning": "Springy settle.", "example": "elastic"},
                {"form": "{\"bezier\": [x1, y1, x2, y2]}", "meaning": "CSS cubic-bezier.", "example": {"bezier": [0.25, 0.1, 0.25, 1.0]}},
            ],
        },
        "color": {
            "about": "sRGB colour, straight alpha. Written as \"#RRGGBB\" or \"#RRGGBBAA\".",
            "forms": [
                {"form": "\"#RRGGBB\"", "meaning": "Opaque colour.", "example": "#FFD60A"},
                {"form": "\"#RRGGBBAA\"", "meaning": "With alpha (AA = 00 transparent .. FF opaque).", "example": "#000000B3"},
                {"form": "\"#RGB\"", "meaning": "Short form (input only).", "example": "#FFF"},
                {"form": "name", "meaning": "CSS basic names on input: black, white, red, green, lime, blue, yellow, orange, purple, pink, gray, cyan, magenta, transparent.", "example": "white"},
            ],
        },
    });

    let enums = json!({
        "media_kind": ["video", "audio", "image"],
        "track_kind": ["video", "audio"],
        "blend": ["normal", "add", "multiply", "screen", "overlay", "darken", "lighten", "difference"],
        "fit": ["contain", "cover", "stretch", "none"],
        "align": ["left", "center", "right"],
        "reveal_kind": ["none", "typewriter", "fade_words", "pop_words", "slide_up", "fade", "spoken"],
        "shape_kind": ["rect", "ellipse", "line", "arrow"],
        "marker_kind": ["marker", "chapter", "beat", "todo"],
        "matte_mode": ["alpha", "alpha_inverted", "luma", "luma_inverted"],
        "export_codec": ["h264", "hevc", "av1", "prores", "gif", "png_sequence", "wav"],
        "export_quality": ["draft", "standard", "high", "lossless"],
    });

    json!({
        "format": crate::model::FORMAT_VERSION,
        "about": "The AIFrames project JSON. Ids are short, stable strings. Defaults are omitted when saved. Units: s = timeline seconds; clip-local s = seconds from the clip's start (keyframes, fades, highlight windows); source s = seconds in the media file (in_point, transcripts); px = project pixels.",
        "types": Value::Object(types),
        "forms": forms,
        "enums": enums,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use serde::de::DeserializeOwned;
    use std::collections::BTreeSet;

    fn check<T: DeserializeOwned>(v: &Value, what: &str) {
        if let Err(e) = serde_json::from_value::<T>(v.clone()) {
            panic!("{what}: {e}\n{v}");
        }
    }

    /// The field names a struct accepts, from serde's unknown-field message.
    fn real_fields<T: DeserializeOwned>(base: Value) -> BTreeSet<String> {
        let mut v = base;
        v["zz_not_a_field"] = json!(1);
        let e = serde_json::from_value::<T>(v).err().expect("deny_unknown_fields").to_string();
        let list = e.split("expected one of ").nth(1).or_else(|| e.split("expected ").nth(1)).unwrap_or_else(|| panic!("{e}"));
        let list = list.split(" at line").next().unwrap().replace(" or ", ", ");
        list.split(", ").map(|s| s.trim().trim_matches('`').to_string()).filter(|s| !s.is_empty()).collect()
    }

    fn all_fields(doc: &Value) -> Value {
        let mut m = Map::new();
        for (k, f) in doc["fields"].as_object().unwrap() {
            m.insert(k.clone(), f["example"].clone());
        }
        Value::Object(m)
    }

    fn documented(doc: &Value) -> BTreeSet<String> {
        doc["fields"].as_object().unwrap().keys().filter(|k| *k != "type").cloned().collect()
    }

    fn struct_type<T: DeserializeOwned>(docs: &Value, name: &str) {
        let doc = &docs["types"][name];
        assert!(doc.is_object(), "types.{name} missing");
        for (k, f) in doc["fields"].as_object().unwrap() {
            for key in ["type", "required", "meaning", "example"] {
                assert!(f.get(key).is_some(), "types.{name}.{k} has no {key}");
            }
            assert!(f["required"].as_bool() == Some(true) || f.get("default").is_some(), "types.{name}.{k}: optional fields name their default");
        }
        let all = all_fields(doc);
        check::<T>(&all, &format!("types.{name} with every field"));
        check::<T>(&doc["example"], &format!("types.{name}.example"));
        let required: Map<String, Value> = doc["fields"].as_object().unwrap().iter().filter(|(_, f)| f["required"] == json!(true)).map(|(k, f)| (k.clone(), f["example"].clone())).collect();
        check::<T>(&Value::Object(required.clone()), &format!("types.{name} with only the required fields"));
        assert_eq!(documented(doc), real_fields::<T>(Value::Object(required)), "types.{name}: documented fields differ from the model");
    }

    #[test]
    fn every_documented_type_matches_the_model() {
        let d = schema_docs();
        struct_type::<Project>(&d, "project");
        struct_type::<Settings>(&d, "settings");
        struct_type::<MediaAsset>(&d, "media");
        struct_type::<MediaInfo>(&d, "media_info");
        struct_type::<Track>(&d, "track");
        struct_type::<Clip>(&d, "clip");
        struct_type::<TextSource>(&d, "text");
        struct_type::<TextBox>(&d, "text_box");
        struct_type::<Stroke>(&d, "stroke");
        struct_type::<Shadow>(&d, "shadow");
        struct_type::<TextReveal>(&d, "reveal");
        struct_type::<WordHighlight>(&d, "highlight");
        struct_type::<ShapeSource>(&d, "shape");
        struct_type::<Transform>(&d, "transform");
        struct_type::<Crop>(&d, "crop");
        struct_type::<Effect>(&d, "effect");
        struct_type::<Transition>(&d, "transition");
        struct_type::<Marker>(&d, "marker");
        struct_type::<Ducking>(&d, "ducking");
        struct_type::<Transcript>(&d, "transcript");
        struct_type::<Word>(&d, "word");
        struct_type::<WordRef>(&d, "word_ref");
        struct_type::<Key>(&d, "key");
        struct_type::<ExportSettings>(&d, "export");
        struct_type::<MotionBlur>(&d, "motion_blur");
        struct_type::<Matte>(&d, "matte");
        struct_type::<Suggestion>(&d, "suggestion");
        // Every struct type is covered by the list above.
        assert_eq!(d["types"].as_object().unwrap().len(), 28, "a new type needs a check here");
    }

    #[test]
    fn source_variants_match_the_model() {
        let d = schema_docs();
        let vars = d["types"]["source"]["variants"].as_object().unwrap();
        assert_eq!(vars.keys().cloned().collect::<BTreeSet<_>>(), ["adjustment", "media", "shape", "solid", "text"].iter().map(|s| s.to_string()).collect());
        check::<Source>(&d["types"]["source"]["example"], "source example");
        for (name, doc) in vars {
            check::<Source>(&all_fields(doc), &format!("source.{name} with every field"));
            check::<Source>(&doc["example"], &format!("source.{name}.example"));
            if name == "adjustment" {
                continue;
            }
            let mut required: Map<String, Value> = doc["fields"].as_object().unwrap().iter().filter(|(_, f)| f["required"] == json!(true)).map(|(k, f)| (k.clone(), f["example"].clone())).collect();
            required.insert("type".into(), json!(name));
            let real = real_fields::<Source>(Value::Object(required));
            assert_eq!(documented(doc), real, "source.{name}");
        }
        // The text and shape variants document the same fields as types.text / types.shape.
        assert_eq!(documented(&vars["text"]), documented(&d["types"]["text"]));
        assert_eq!(documented(&vars["shape"]), documented(&d["types"]["shape"]));
    }

    #[test]
    fn forms_and_enums_parse() {
        let d = schema_docs();
        for f in d["forms"]["time"]["forms"].as_array().unwrap() {
            check::<Time>(&f["example"], "time form");
        }
        check::<Time>(&d["forms"]["until"]["example"], "until example");
        for f in d["forms"]["param"]["forms"].as_array().unwrap() {
            check::<Param>(&f["example"], "param form");
        }
        for f in d["forms"]["ease"]["forms"].as_array().unwrap() {
            check::<Ease>(&f["example"], "ease form");
        }
        for f in d["forms"]["color"]["forms"].as_array().unwrap() {
            check::<Color>(&f["example"], "color form");
        }
        fn variants<T: DeserializeOwned>(d: &Value, name: &str) {
            let vals: BTreeSet<String> = d["enums"][name].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
            for v in &vals {
                check::<T>(&json!(v), &format!("enums.{name}"));
            }
            let e = serde_json::from_value::<T>(json!("zz_nope")).err().unwrap().to_string();
            let list = e.split("expected one of ").nth(1).or_else(|| e.split("expected ").nth(1)).unwrap().split(" at line").next().unwrap().replace(" or ", ", ");
            let real: BTreeSet<String> = list.split(", ").map(|s| s.trim().trim_matches('`').to_string()).collect();
            assert_eq!(vals, real, "enums.{name}");
        }
        variants::<MediaKind>(&d, "media_kind");
        variants::<TrackKind>(&d, "track_kind");
        variants::<BlendMode>(&d, "blend");
        variants::<Fit>(&d, "fit");
        variants::<Align>(&d, "align");
        variants::<RevealKind>(&d, "reveal_kind");
        variants::<ShapeKind>(&d, "shape_kind");
        variants::<MarkerKind>(&d, "marker_kind");
        variants::<MatteMode>(&d, "matte_mode");
        // Every field type that names an enum or a type refers to one that is documented.
        for (tname, t) in d["types"].as_object().unwrap() {
            let mut fs: Vec<(&String, &Value)> = t.get("fields").and_then(|f| f.as_object()).map(|m| m.iter().collect()).unwrap_or_default();
            if let Some(vs) = t.get("variants").and_then(|v| v.as_object()) {
                for v in vs.values() {
                    fs.extend(v["fields"].as_object().unwrap().iter());
                }
            }
            for (fname, f) in fs {
                let ty = f["type"].as_str().unwrap();
                if let Some(e) = ty.strip_prefix("enum ") {
                    assert!(d["enums"][e].is_array(), "{tname}.{fname}: unknown enum {e}");
                }
                for part in ty.split(['|', '<', '>', ',', '[', ']', ' ']).map(str::trim).filter(|s| !s.is_empty()) {
                    let known = ["string", "number", "integer", "boolean", "null", "map", "id", "(id)", "(media", "(clip", "(track", "id)", "prefix", "enum", "time", "param", "color", "ease", /* edit ops: the op reference */ "op"];
                    let is_lit = part.starts_with('"');
                    assert!(is_lit || known.contains(&part) || d["types"][part].is_object() || d["enums"][part].is_array(), "{tname}.{fname}: type {ty:?} names {part:?}");
                }
            }
        }
    }
}
