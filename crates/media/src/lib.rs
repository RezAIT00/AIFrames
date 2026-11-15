//! AIFrames media layer (see `ARCHITECTURE.md`, "Media API").
//!
//! Everything goes through the ffmpeg / ffprobe executables (and the whisper CLI for speech) as
//! child processes — never FFI — spawned without console windows on Windows.
//!
//! * [`tools`] finds the executables; [`probe`] tells what a file is.
//! * [`MediaPool`] (shared via `Arc`) serves video frames at any source time from streaming decode
//!   sessions with a byte-bounded frame cache, still images, filmstrip thumbnails, disk-cached audio
//!   ([`AudioData`], decoded progressively in the background so Play is instant on new media) and
//!   waveforms ([`Waveform`]).
//! * Analysis: [`detect_scenes`], [`detect_silence`], [`detect_beats`], [`loudness_of_file`] /
//!   [`loudness_of_pcm`].
//! * [`transcribe`] runs whisper with word timestamps into a `Transcript`.
//! * [`Encoder`] pipes rendered RGBA frames (+ a WAV from [`write_wav`]) into ffmpeg.
//!
//! Time is always *source seconds*: 0 is the start of the file (its container start time), the
//! same origin for video frames, audio samples, scene cuts, silences, beats and transcript words.

mod analysis;
mod audio;
mod cache;
mod decode;
mod encode;
mod error;
mod joins;
mod loudness;
mod pool;
mod probe;
mod progressive;
mod still;
mod tools;
mod transcribe;
mod util;
mod waveform;

pub use analysis::{analyze_beats, beats_of_pcm, detect_beats, detect_scenes, detect_silence, silence_of_pcm, Beats};
pub use audio::{write_wav, AudioData, WavFormat, WavWriter};
pub use decode::HwAccel;
pub use encode::{bitrate_cap_mbps, EncodeOptions, Encoder, Quality, VideoCodec};
pub use error::MediaError;
pub use joins::{classify, cut_edges, edge_level, ends_sentence, is_punctuated, quiet_before, level_db, quiet_run, room_floor, CutEdges, Edge, LevelClass, MIN_PAUSE, SPEECH_ABOVE_FLOOR};
pub use loudness::{loudness_of_file, loudness_of_pcm, Loudness, LoudnessMeter};
pub use pool::{Frame, FrameCaching, MediaPool, PoolStats, DEFAULT_READAHEAD_BYTES};
pub use probe::probe;
pub use tools::{tools, Tools};
pub use transcribe::{transcribe, ProgressFn, TranscribeOptions};
pub use util::{default_cache_dir, fps_rational};
pub use waveform::{Waveform, WAVEFORM_BINS_PER_SEC};

/// Number of region decoders (decoding ahead of the main audio decoder) started in this process.
#[doc(hidden)]
pub fn audio_regions_started() -> u64 {
    progressive::REGIONS_STARTED.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod send_sync {
    fn assert_send_sync<T: Send + Sync>() {}
    #[test]
    fn shared_types_are_send_sync() {
        assert_send_sync::<super::MediaPool>();
        assert_send_sync::<super::AudioData>();
        assert_send_sync::<super::Waveform>();
        assert_send_sync::<super::Frame>();
        assert_send_sync::<super::MediaError>();
        assert_send_sync::<super::TranscribeOptions>();
    }
}
