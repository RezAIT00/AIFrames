mod common;

use aiframes_media::{transcribe, MediaError, TranscribeOptions};
use common::*;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// A real voice-over clip (read-only) saying the REFERENCE sentence, given by the
/// `AIFRAMES_TEST_VOICE_CLIP` environment variable. The test is skipped without it.
fn source_clip() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("AIFRAMES_TEST_VOICE_CLIP")?);
    p.is_file().then_some(p)
}

/// Reference: "Over at Anthropic, Claude is learning to dream."
const REFERENCE: [(&str, f64); 8] =
    [("over", 0.0), ("at", 0.26), ("anthropic", 0.38), ("claude", 1.2), ("is", 1.42), ("learning", 1.52), ("to", 1.72), ("dream", 1.88)];

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

fn check(words: &[aiframes_core::model::Word], shift: f64) {
    assert!(words.len() >= 6 && words.len() <= 10, "{words:?}");
    let mut last = f64::NEG_INFINITY;
    for w in words {
        assert!(w.start >= last && w.end >= w.start, "ordered: {words:?}");
        assert!((0.0..=1.0).contains(&w.conf));
        last = w.start;
    }
    let mut matched = 0;
    for (text, start) in REFERENCE {
        if let Some(w) = words.iter().find(|w| norm(&w.text) == text) {
            assert!((w.start - (start + shift)).abs() < 0.3, "{text}: {} vs {}", w.start, start + shift);
            matched += 1;
        }
    }
    assert!(matched >= 6, "matched {matched} of 8: {words:?}");
}

#[test]
fn transcribes_real_speech_with_word_times() {
    if aiframes_media::tools().unwrap().whisper.is_none() {
        eprintln!("skipping: whisper is not installed");
        return;
    }
    let Some(src) = source_clip() else {
        eprintln!("skipping: no voice-over clip on this machine");
        return;
    };
    let dir = scratch("whisper");
    let clip = dir.join("vo.wav");
    ffmpeg(&["-i", src.to_str().unwrap(), "-c", "copy", clip.to_str().unwrap()]);
    let padded = dir.join("padded.wav");
    ffmpeg(&["-i", clip.to_str().unwrap(), "-af", "adelay=1000:all=1,apad=pad_dur=0.5", padded.to_str().unwrap()]);

    let seen = Arc::new(Mutex::new(Vec::<f32>::new()));
    let s2 = seen.clone();
    let opts = TranscribeOptions { progress: Some(Arc::new(move |f| s2.lock().unwrap().push(f))), ..Default::default() };
    let t = transcribe(&clip, &opts).unwrap();
    assert_eq!(t.language, "en");
    check(&t.words, 0.0);
    let p = seen.lock().unwrap().clone();
    assert!(p.windows(2).all(|w| w[1] >= w[0] - 1e-6), "progress monotonic: {p:?}");
    assert_eq!(p.last().copied(), Some(1.0));

    // Leading silence shifts every word: times are source seconds.
    let opts = TranscribeOptions { language: Some("en".into()), ..Default::default() };
    let t = transcribe(&padded, &opts).unwrap();
    check(&t.words, 1.0);

    // Only a range of the source; word times stay in source seconds.
    let opts = TranscribeOptions { language: Some("en".into()), range: Some([0.8, 3.6]), ..Default::default() };
    let t = transcribe(&padded, &opts).unwrap();
    check(&t.words, 1.0);
    assert!(t.words.iter().all(|w| w.start >= 0.8 - 1e-6 && w.end <= 3.7), "{:?}", t.words);
}

#[test]
fn cancel_and_errors() {
    if aiframes_media::tools().unwrap().whisper.is_none() {
        eprintln!("skipping: whisper is not installed");
        return;
    }
    let cancel = Arc::new(AtomicBool::new(true));
    let opts = TranscribeOptions { cancel: Some(cancel), ..Default::default() };
    let e = transcribe(&burst_wav(), &opts).unwrap_err();
    assert!(matches!(e, MediaError::Cancelled), "{e}");
    let e = transcribe(&cfr(), &TranscribeOptions::default()).unwrap_err();
    assert!(e.to_string().contains("no audio stream"), "{e}");
}
