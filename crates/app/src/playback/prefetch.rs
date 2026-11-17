//! Progressive audio: tells the media pool which sound will be needed next (decoded around that
//! source time first) and answers "is it there yet?" for the "Buffering audio…" state.
//!
//! `MediaPool::prefetch_audio` may probe a file the first time it sees it, so the calls run on a
//! small worker thread; readiness checks only use `AudioData::ready_at` (non-blocking).

use super::Snapshot;
use aiframes_core::{MediaKind, Source};
use aiframes_media::{AudioData, MediaPool};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The sample rate the renderer mixes (and decodes media audio) at.
pub fn mix_rate(snap: &Snapshot) -> u32 {
    snap.project.settings.sample_rate.clamp(8000, 192_000)
}

/// Media sound heard in `[t, t + window]`: (file, sound stream, source seconds where it starts being
/// needed). Disabled or muted clips and clips on muted tracks are left out.
pub fn audio_sources(snap: &Snapshot, t: f64, window: f64) -> Vec<(PathBuf, u32, f64)> {
    let p = &snap.project;
    let mut out: Vec<(PathBuf, u32, f64)> = Vec::new();
    for c in &p.clips {
        let Source::Media { asset, stream, .. } = &c.source else { continue };
        if !c.enabled || c.muted || p.tracks.iter().any(|tr| tr.id == c.track && tr.muted) {
            continue;
        }
        let Some(m) = p.media.iter().find(|m| &m.id == asset) else { continue };
        let sound = m.kind == MediaKind::Audio || (m.kind == MediaKind::Video && m.info.has_audio);
        if !sound {
            continue;
        }
        let Some(span) = snap.resolved.span(&c.id) else { continue };
        if span.end <= t || span.start > t + window {
            continue;
        }
        let local = (t.max(span.start) - span.start).max(0.0);
        let Some(src) = aiframes_core::source_time(c, local) else { continue };
        let path = crate::previews::media_path(&snap.dir, m);
        if !out.iter().any(|(q, k, s)| *q == path && k == stream && (s - src).abs() < 0.5) {
            out.push((path, *stream, src));
        }
    }
    out
}

type Known = Arc<Mutex<HashMap<(PathBuf, u32), Option<Arc<AudioData>>>>>;
/// (sources, sample rate) to decode.
type Request = (Vec<(PathBuf, u32, f64)>, u32);

pub struct Prefetcher {
    tx: Option<Sender<Request>>,
    /// Files asked for so far: the decoder handle, or `None` when the file has no usable sound.
    known: Known,
}

impl Prefetcher {
    pub fn new(pool: Arc<MediaPool>) -> Prefetcher {
        let (tx, rx) = channel::<Request>();
        let known: Known = Arc::new(Mutex::new(HashMap::new()));
        let k = known.clone();
        let spawned = std::thread::Builder::new().name("aiframes-audio-prefetch".into()).spawn(move || {
            while let Ok((sources, rate)) = rx.recv() {
                for (path, stream, t) in sources {
                    let r = pool.prefetch_audio_stream(&path, rate, stream, t).ok();
                    lock(&k).insert((path, stream), r);
                }
            }
        });
        Prefetcher { tx: spawned.ok().map(|_| tx), known }
    }

    /// Starts decoding these sources (non-blocking).
    pub fn request(&self, sources: Vec<(PathBuf, u32, f64)>, rate: u32) {
        if sources.is_empty() {
            return;
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send((sources, rate));
        }
    }

    /// True when `len` seconds of every source can be read without waiting. Files that turned out
    /// to have no usable sound count as ready; files not looked at yet do not.
    pub fn ready(&self, sources: &[(PathBuf, u32, f64)], len: f64) -> bool {
        let k = lock(&self.known);
        sources.iter().all(|(p, stream, t)| match k.get(&(p.clone(), *stream)) {
            Some(Some(a)) => a.ready_at(*t, len),
            Some(None) => true,
            None => false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Snapshot {
        let project: aiframes_core::Project = serde_json::from_value(serde_json::json!({
            "format": 1,
            "media": [
                {"id": "m1", "path": "a.mp4", "kind": "video", "info": {"duration": 60.0, "has_video": true, "has_audio": true}},
                {"id": "m2", "path": "b.mp4", "kind": "video", "info": {"duration": 60.0, "has_video": true, "has_audio": false}},
                {"id": "m3", "path": "c.wav", "kind": "audio", "info": {"duration": 60.0, "has_audio": true}}
            ],
            "tracks": [{"id": "v1", "kind": "video"}, {"id": "a1", "kind": "audio"}, {"id": "a2", "kind": "audio", "muted": true}],
            "clips": [
                {"id": "c1", "track": "v1", "start": 0.0, "duration": 10.0, "source": {"type": "media", "asset": "m1", "in_point": 20.0}},
                {"id": "c2", "track": "v1", "start": 10.0, "duration": 10.0, "source": {"type": "media", "asset": "m2"}},
                {"id": "c3", "track": "a1", "start": 5.0, "duration": 10.0, "source": {"type": "media", "asset": "m3"}, "speed": 2.0},
                {"id": "c4", "track": "a2", "start": 0.0, "duration": 10.0, "source": {"type": "media", "asset": "m3"}}
            ]
        }))
        .unwrap();
        let resolved = aiframes_core::resolve(&project);
        Snapshot { doc: 1, revision: 0, epoch: 0, project, resolved, dir: PathBuf::from("C:/p") }
    }

    #[test]
    fn sources_at_a_time() {
        let s = snap();
        let a = audio_sources(&s, 2.0, 0.0);
        assert_eq!(a, vec![(PathBuf::from("C:/p/a.mp4"), 0, 22.0)], "in-point applied; muted track and silent video skipped");
        let a = audio_sources(&s, 2.0, 4.0);
        assert_eq!(a.len(), 2, "the window reaches c3 at 5 s: {a:?}");
        assert_eq!(a[1], (PathBuf::from("C:/p/c.wav"), 0, 0.0));
        let a = audio_sources(&s, 7.0, 0.0);
        assert!(a.contains(&(PathBuf::from("C:/p/c.wav"), 0, 4.0)), "2x speed: 2 s into the clip = 4 s of source: {a:?}");
        assert!(audio_sources(&s, 30.0, 1.0).is_empty());
        assert_eq!(mix_rate(&s), 48_000);
    }

    #[test]
    fn each_sound_stream_is_its_own_source() {
        let mut s = snap();
        let base = s.project.clips[0].clone();
        let mut mic = base.clone();
        mic.id = "c9".into();
        mic.track = "a1".into();
        if let Source::Media { stream, .. } = &mut mic.source {
            *stream = 1;
        }
        s.project.clips.push(mic);
        s.resolved = aiframes_core::resolve(&s.project);
        let a = audio_sources(&s, 2.0, 0.0);
        assert_eq!(a, vec![(PathBuf::from("C:/p/a.mp4"), 0, 22.0), (PathBuf::from("C:/p/a.mp4"), 1, 22.0)], "two streams of one file: {a:?}");
    }
}
