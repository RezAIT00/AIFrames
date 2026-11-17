//! Files with several sound streams through the AI side: import lists and places every stream,
//! media_info / list_media / get_clip / describe_project say which stream is where, analyses take a
//! stream, and the schema documents it all.

mod common;

use aiframes_mcp::AiframesBackend;
use common::*;
use serde_json::{json, Value as J};
use suite_mcp::Backend;

fn clips_of(b: &AiframesBackend, asset: &str) -> Vec<J> {
    let t = call(b, "get_timeline", json!({}));
    t["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|tr| tr["clips"].as_array().unwrap().iter().map(move |c| (tr["id"].clone(), c.clone())))
        .filter(|(_, c)| c["source"]["asset"] == asset)
        .map(|(t, mut c)| {
            c["on"] = t;
            c
        })
        .collect()
}

#[test]
fn import_places_every_stream_and_the_ai_sees_them() {
    let dir = scratch("streams-ai");
    let b = AiframesBackend::new(services());
    call(&b, "new_project", json!({"preset": "720p", "path": path_str(&dir.join("s.aifr"))}));

    // media_info on the file before importing it lists the streams.
    let info = call(&b, "media_info", json!({"file": path_str(&obs_video())}));
    let st = info["audio_streams"].as_array().unwrap();
    assert_eq!(st.len(), 3, "{info}");
    assert_eq!(st.iter().map(|s| s["title"].as_str().unwrap()).collect::<Vec<_>>(), vec!["Game", "Mic", "Discord"]);
    assert_eq!((st[1]["index"].clone(), st[1]["channels"].clone(), st[1]["rate"].clone()), (json!(1), json!(1), json!(48000)));
    assert!(info["streams_note"].as_str().unwrap().contains("detach_streams"), "{info}");

    // Import + append: the picture plus one linked audio clip per stream, each on its own track.
    let r = call(&b, "import_media", json!({"paths": [path_str(&obs_video())], "place": "append"}));
    let asset = r["imported"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(r["imported"][0]["audio_streams"].as_array().unwrap().len(), 3, "{r}");
    let clips = clips_of(&b, &asset);
    assert_eq!(clips.len(), 4, "{clips:?}");
    let mut sound: Vec<(i64, String)> = clips.iter().filter(|c| c["on"].as_str().unwrap().starts_with('a')).map(|c| (c["source"]["stream"].as_i64().unwrap(), c["on"].as_str().unwrap().to_string())).collect();
    sound.sort();
    assert_eq!(sound.iter().map(|s| s.0).collect::<Vec<_>>(), vec![0, 1, 2]);
    let tracks: std::collections::HashSet<&String> = sound.iter().map(|s| &s.1).collect();
    assert_eq!(tracks.len(), 3, "each stream on its own track: {sound:?}");

    // list_media / describe_project / get_clip show the streams and which clip plays which.
    let lm = call(&b, "list_media", json!({}));
    assert_eq!(lm["media"][0]["audio_streams"].as_array().unwrap().len(), 3);
    let d = call(&b, "describe_project", json!({}));
    let text = d.to_string();
    assert!(text.contains("+3 audio streams") && text.contains("stream 1/3 \\\"Mic\\\""), "{text}");
    let mic = clips.iter().find(|c| c["source"]["stream"] == 1).unwrap()["id"].as_str().unwrap().to_string();
    let gc = call(&b, "get_clip", json!({"clip": mic}));
    assert_eq!(gc["stream"]["index"], 1);
    assert_eq!(gc["stream"]["of"], 3);
    assert!(gc["stream"]["label"].as_str().unwrap().starts_with("Mic"), "{gc}");
    let mi = call(&b, "media_info", json!({"asset": asset}));
    assert_eq!(mi["clip_streams"].as_array().unwrap().len(), 4, "{mi}");

    // The AI changes one clip's stream with a typed op (one undoable step), refused past the file's streams.
    let game = clips.iter().find(|c| c["source"]["stream"] == 0 && c["on"].as_str().unwrap().starts_with('a')).unwrap()["id"].as_str().unwrap().to_string();
    let r = call(&b, "edit", json!({"ops": [{"op": "set_audio_stream", "clip": game, "stream": 2}]}));
    assert!(r["warnings"].to_string().contains("heard twice"), "two clips now play stream 2: {r}");
    let e = fails(&b, "edit", json!({"ops": [{"op": "set_audio_stream", "clip": game, "stream": 7}]}));
    assert!(e.contains("3 sound streams"), "{e}");
    call(&b, "undo", json!({}));

    // Analyses take a stream: the mic stream has the speech pattern's silences, the game stream none.
    let quiet = call(&b, "detect_silence", json!({"clip": mic, "min_length": 0.5, "wait_seconds": 60}));
    let quiet = if quiet["silences"].is_array() { quiet } else { quiet["job_state"]["result"].clone() };
    assert_eq!(quiet["stream"], 1, "{quiet}");
    assert!(quiet["count"].as_u64().unwrap() >= 2, "mic silences: {quiet}");
    let loud = call(&b, "detect_silence", json!({"asset": asset, "stream": 0, "min_length": 0.5, "wait_seconds": 60}));
    let loud = if loud["silences"].is_array() { loud } else { loud["job_state"]["result"].clone() };
    assert_eq!(loud["count"], 0, "game stream never goes quiet: {loud}");
    let e = fails(&b, "detect_silence", json!({"asset": asset, "stream": 3}));
    assert!(e.contains("not stream 3"), "{e}");

    // streams: "first" places the old way; a list picks streams.
    let dir2 = scratch("streams-ai-2");
    call(&b, "new_project", json!({"preset": "720p", "path": path_str(&dir2.join("t.aifr"))}));
    let r = call(&b, "import_media", json!({"paths": [path_str(&obs_video())], "place": "append", "streams": "first"}));
    let a2 = r["imported"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(clips_of(&b, &a2).len(), 1);
    let r = call(&b, "edit", json!({"ops": [{"op": "detach_streams", "clip": clips_of(&b, &a2)[0]["id"], "streams": [1]}]}));
    assert_eq!(r["created"].as_array().unwrap().len(), 1, "one linked clip; a free audio track was used: {r}");
    let e = fails(&b, "import_media", json!({"paths": [path_str(&video())], "place": "append", "streams": "some"}));
    assert!(e.contains("'streams'"), "{e}");
}

#[test]
fn schema_and_ops_document_the_stream_fields() {
    let b = AiframesBackend::new(services());
    let s = call(&b, "schema", json!({"type": "source"})).to_string();
    assert!(s.contains("stream") && s.contains("set_audio_stream"), "{s}");
    let mi = call(&b, "schema", json!({"type": "media_info"})).to_string();
    assert!(mi.contains("audio_streams"), "{mi}");
    let ops = call(&b, "list_ops", json!({})).to_string();
    for name in ["detach_streams", "set_audio_stream"] {
        assert!(ops.contains(name), "list_ops lacks {name}");
    }
    let tools = b.tools(false);
    let find = |n: &str| tools.iter().find(|t| t["name"] == n).map(|t| t.to_string()).unwrap_or_default();
    assert!(find("import_media").contains("streams"));
    assert!(find("transcribe").contains("stream"));
    assert!(find("detect_silence").contains("stream"));
}
