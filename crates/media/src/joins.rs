//! Measuring cuts in speech: where a cut can go so it lands in silence, and how a join sounds.
//!
//! Word timings from speech recognition are off by tens of milliseconds, so a cut placed by word times
//! alone can clip the end of a word or leave a breath of the next one. These functions read the
//! [`Waveform`] (5 ms RMS bins) instead: [`cut_edges`] moves each edge of a word cut onto the quietest
//! point near where the word times put it, and [`edge_level`] / [`classify`] say what plays at a point
//! (silence, low sound or speech), relative to the recording's own room tone ([`room_floor`]).
//!
//! All times are source seconds of the recording.

use crate::waveform::Waveform;
use aiframes_core::Word;

/// Level reported for digital silence (dBFS).
pub const FLOOR_DB: f64 = -120.0;
/// Half-width of the window a point's level is read over (seconds): the loudest bin within it counts.
pub const EDGE_WINDOW: f64 = 0.020;
/// Above the room floor by less than this (dB) is silence.
pub const SILENCE_ABOVE_FLOOR: f64 = 8.0;
/// Above the room floor by at least this (dB) is speech (or other foreground sound).
pub const SPEECH_ABOVE_FLOOR: f64 = 20.0;
/// Furthest (seconds) an edge is moved from where the word times put it.
pub const MAX_REACH: f64 = 0.5;
/// Cost of moving an edge away from where the word times put it (dB per second): 12 dB per 100 ms, so a
/// point 20 dB quieter is worth moving about 170 ms for.
const MOVE_COST_DB_PER_S: f64 = 120.0;
/// Shortest quiet (seconds) that is a pause. Shorter dips are inside running speech: the closure of a
/// "p" or "t", or the joint between two words said in one breath. A cut there is heard as a cut-off even
/// when the level at the cut itself is low.
pub const MIN_PAUSE: f64 = 0.1;
/// Longest stretch [`quiet_run`] measures (seconds).
const QUIET_SCAN: f64 = 2.0;

/// What plays at a point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelClass {
    /// Room tone: a clean place for a cut.
    Silence,
    /// Breath, tail of a word, background: usually fine, worth a listen.
    Low,
    /// Speech or other foreground sound: a cut here is heard.
    Speech,
}

impl LevelClass {
    pub fn as_str(self) -> &'static str {
        match self {
            LevelClass::Silence => "silence",
            LevelClass::Low => "low",
            LevelClass::Speech => "speech",
        }
    }
}

/// RMS of one bin in dBFS.
fn bin_db(w: &Waveform, i: usize) -> f64 {
    let r = w.rms.get(i).copied().unwrap_or(0.0) as f64;
    if r <= 1e-6 {
        FLOOR_DB
    } else {
        (20.0 * r.log10()).max(FLOOR_DB)
    }
}

fn bin_of(w: &Waveform, t: f64) -> i64 {
    (t * w.bins_per_sec).floor() as i64
}

/// Loudest RMS level (dBFS) within `[t0, t1]`; [`FLOOR_DB`] outside the audio.
pub fn level_db(w: &Waveform, t0: f64, t1: f64) -> f64 {
    if w.is_empty() || !t0.is_finite() || !t1.is_finite() {
        return FLOOR_DB;
    }
    let (a, b) = (bin_of(w, t0.min(t1)).max(0), bin_of(w, t0.max(t1)).min(w.len() as i64 - 1));
    (a..=b).map(|i| bin_db(w, i as usize)).fold(FLOOR_DB, f64::max)
}

/// The level at a point as a cut hears it: the loudest bin within ±[`EDGE_WINDOW`].
pub fn edge_level(w: &Waveform, t: f64) -> f64 {
    level_db(w, t - EDGE_WINDOW, t + EDGE_WINDOW)
}

/// How far either side of a passage [`room_floor`] looks for the room's own sound (seconds): far enough
/// to reach a pause even in a fast exchange or a scored cold open.
const FLOOR_REACH: f64 = 20.0;
/// The loudest a room floor can be (dBFS). A "floor" above this is programme sound (continuous talk,
/// a music bed), not the room: judged against it, speech would pass for silence.
pub const MAX_ROOM_FLOOR: f64 = -50.0;

/// The recording's room tone around `[t0, t1]` (dBFS): the 10th percentile of bin levels over the
/// passage plus 20 s either side, between -90 dB (so digital silence does not make every breath
/// "speech") and [`MAX_ROOM_FLOOR`].
pub fn room_floor(w: &Waveform, t0: f64, t1: f64) -> f64 {
    if w.is_empty() {
        return FLOOR_DB;
    }
    let (a, b) = (bin_of(w, t0.min(t1) - FLOOR_REACH).max(0), bin_of(w, t0.max(t1) + FLOOR_REACH).min(w.len() as i64 - 1));
    let mut v: Vec<f64> = (a..=b).map(|i| bin_db(w, i as usize)).collect();
    if v.is_empty() {
        return FLOOR_DB;
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 10].clamp(-90.0, MAX_ROOM_FLOOR)
}

/// What a level is, given the room floor.
pub fn classify(level: f64, floor: f64) -> LevelClass {
    if level < floor + SILENCE_ABOVE_FLOOR {
        LevelClass::Silence
    } else if level >= floor + SPEECH_ABOVE_FLOOR {
        LevelClass::Speech
    } else {
        LevelClass::Low
    }
}

/// Length (seconds) of the quiet stretch around `t`: the run of bins below speech level (room tone, a
/// breath) that contains it, up to 2 s. 0 when `t` is on speech. Compare with [`MIN_PAUSE`].
pub fn quiet_run(w: &Waveform, t: f64, floor: f64) -> f64 {
    if w.is_empty() || !t.is_finite() {
        return 0.0;
    }
    let quiet = |i: i64| i >= 0 && (i as usize) < w.len() && bin_db(w, i as usize) < floor + SPEECH_ABOVE_FLOOR;
    let i = bin_of(w, t);
    if !quiet(i) {
        return 0.0;
    }
    let max = (QUIET_SCAN * w.bins_per_sec) as i64;
    let (mut a, mut b) = (i, i);
    while i - a < max && quiet(a - 1) {
        a -= 1;
    }
    while b - i < max && quiet(b + 1) {
        b += 1;
    }
    (b - a + 1) as f64 / w.bins_per_sec
}

/// Seconds of continuous quiet (below speech level) just before `t`, up to 2 s: how long a clip ending at
/// `t` has already been silent when it stops.
pub fn quiet_before(w: &Waveform, t: f64, floor: f64) -> f64 {
    if w.is_empty() || !t.is_finite() {
        return 0.0;
    }
    let quiet = |i: i64| i >= 0 && (i as usize) < w.len() && bin_db(w, i as usize) < floor + SPEECH_ABOVE_FLOOR;
    let end = bin_of(w, t);
    let max = (QUIET_SCAN * w.bins_per_sec) as i64;
    let mut a = end;
    while end - a < max && quiet(a - 1) {
        a -= 1;
    }
    (end - a) as f64 / w.bins_per_sec
}

/// Whether a transcript word ends a sentence: its text ends in `.`, `?`, `!` or `…` (closing quotes and
/// brackets aside). Speech recognisers that punctuate (Whisper) mark sentence ends this way.
pub fn ends_sentence(text: &str) -> bool {
    let t = text.trim().trim_end_matches(['"', '\'', ')', ']', '\u{201d}', '\u{2019}']);
    t.ends_with(['.', '?', '!', '\u{2026}'])
}

/// Whether a transcript carries punctuation at all (some recognisers and caption files have none, so a
/// missing full stop says nothing there).
pub fn is_punctuated(words: &[Word]) -> bool {
    !words.is_empty() && words.iter().filter(|w| ends_sentence(&w.text)).count() * 60 >= words.len()
}

/// One measured edge of a cut.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edge {
    /// Where the cut goes (source seconds).
    pub t: f64,
    /// Where word times alone put it.
    pub planned: f64,
    /// Level at the edge (dBFS, loudest within ±20 ms).
    pub db: f64,
    pub class: LevelClass,
    /// Length (seconds) of the quiet the edge sits in ([`quiet_run`]); under [`MIN_PAUSE`] the cut is in
    /// running speech and is heard as a cut-off.
    pub pause: f64,
}

impl Edge {
    /// The edge sits in a real pause, not a dip inside running speech.
    pub fn in_pause(&self) -> bool {
        self.pause >= MIN_PAUSE
    }

    /// The edge sits in a brief dip inside running speech: below speech level but not silent, and shorter
    /// than a pause (the gap in a "p", two words said in one breath). Heard as a chop. A brief gap that is
    /// truly silent is a clean place to cut, however short.
    pub fn in_running_speech(&self) -> bool {
        self.class == LevelClass::Low && !self.in_pause()
    }
}

impl Edge {
    /// Milliseconds the edge moved from where the word times put it (signed).
    pub fn moved_ms(&self) -> f64 {
        (self.t - self.planned) * 1000.0
    }
}

/// Where to cut out words `from..=to`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CutEdges {
    pub start: Edge,
    pub end: Edge,
    /// Room tone (dBFS) the edges were classed against.
    pub floor_db: f64,
}

/// An [`Edge`] at `t`.
fn edge_at(w: &Waveform, t: f64, planned: f64, floor: f64) -> Edge {
    let db = edge_level(w, t);
    Edge { t, planned, db, class: classify(db, floor), pause: quiet_run(w, t, floor) }
}

/// The quietest point of `[lo, hi]` near `target`, as an [`Edge`]. A point in a real pause (at least
/// [`MIN_PAUSE`] of quiet) beats any point in a shorter dip: a dip inside running speech (the gap in a
/// "p", two words said in one breath) is quiet for a moment, but a cut there still chops the speaker.
fn best_point(w: &Waveform, lo: f64, hi: f64, target: f64, floor: f64) -> Edge {
    let mut best = (target, f64::INFINITY);
    let mut best_pause = (target, f64::INFINITY);
    if hi > lo {
        let step = 1.0 / w.bins_per_sec;
        let n = ((hi - lo) / step).ceil() as usize;
        for k in 0..=n {
            let t = (lo + k as f64 * step).min(hi);
            // Ranked by what the cut would hear (loudest within ±20 ms). Below silence level, quieter is
            // no better: only the distance counts.
            let level = edge_level(w, t).max(floor + SILENCE_ABOVE_FLOOR * 0.5);
            let cost = level + (t - target).abs() * MOVE_COST_DB_PER_S;
            if cost < best.1 - 1e-9 {
                best = (t, cost);
            }
            if cost < best_pause.1 - 1e-9 && level < floor + SPEECH_ABOVE_FLOOR && quiet_run(w, t, floor) >= MIN_PAUSE {
                best_pause = (t, cost);
            }
        }
    }
    let t = if best_pause.1.is_finite() { best_pause.0 } else { best.0 };
    edge_at(w, t, target, floor)
}

/// Edges for cutting words `from..=to` out of a recording: start from the rule `delete_words` uses
/// (`pad` seconds of extra margin, never into the neighbouring words), then move each edge to the
/// quietest point between the middles of the words either side of it (recognisers place word edges
/// loosely and often close the pauses between words). `None` when the indices are out of range or there is no audio.
pub fn cut_edges(w: &Waveform, words: &[Word], from: usize, to: usize, pad: f64) -> Option<CutEdges> {
    if w.is_empty() || from > to || to >= words.len() {
        return None;
    }
    let ws = words[from].start;
    let we = words[from..=to].iter().map(|x| x.end).fold(f64::MIN, f64::max);
    let mid = |x: &Word| (x.start + x.end) / 2.0;
    let prev = from.checked_sub(1).map(|i| &words[i]);
    let next = words.get(to + 1);
    let prev_end = prev.map(|x| x.end).unwrap_or(f64::NEG_INFINITY);
    let next_start = next.map(|x| x.start).unwrap_or(f64::INFINITY);
    let floor = room_floor(w, ws, we);

    let plan_a = (ws - pad).max(prev_end + pad).min(ws).max(0.0);
    let plan_b = (we + pad).min(next_start - pad).max(we).min(w.duration);
    // Recognisers often run a word on until the next one starts, so the real pause can be anywhere
    // between the middles of the two words: search there, and let the cost of moving keep edges close.
    // Start edge: from the middle of the previous word (or MAX_REACH before) to the middle of the first
    // cut word.
    let lo_a = match prev {
        Some(p) => mid(p),
        None => ws - MAX_REACH,
    }
    .max(plan_a - MAX_REACH)
    .max(0.0);
    let hi_a = mid(&words[from]).min(we);
    // End edge: from the middle of the last cut word to the middle of the next word (or MAX_REACH after).
    let last = &words[to];
    let lo_b = mid(last).max(ws);
    let hi_b = match next {
        Some(n) => mid(n),
        None => we + MAX_REACH,
    }
    .min(plan_b + MAX_REACH)
    .min(w.duration);

    let start = best_point(w, lo_a.min(plan_a), hi_a.max(plan_a), plan_a, floor);
    let mut end = best_point(w, lo_b.min(plan_b), hi_b.max(plan_b), plan_b, floor);
    if end.t <= start.t {
        end = edge_at(w, plan_b.max(start.t + 0.001), plan_b, floor);
    }
    Some(CutEdges { start, end, floor_db: floor })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioData;

    /// 48 kHz stereo: low room noise everywhere, a 0.3-amplitude tone during each `(start, end)`.
    pub(crate) fn speech(len: f64, words: &[(f64, f64)]) -> Waveform {
        let rate = 48_000usize;
        let n = (len * rate as f64) as usize;
        let mut s = Vec::with_capacity(n * 2);
        let mut seed = 1u32;
        for i in 0..n {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.002;
            let t = i as f64 / rate as f64;
            let on = words.iter().any(|(a, b)| t >= *a && t < *b);
            let v = if on { 0.3 * ((t * 220.0 * std::f64::consts::TAU).sin() as f32) } else { 0.0 } + noise;
            s.push(v);
            s.push(v);
        }
        Waveform::from_audio(&AudioData::from_samples("x", rate as u32, s))
    }

    fn word(text: &str, start: f64, end: f64) -> Word {
        Word { text: text.into(), start, end, conf: 1.0 }
    }

    #[test]
    fn classes_follow_the_room_floor() {
        let w = speech(4.0, &[(1.0, 1.5)]);
        let floor = room_floor(&w, 0.0, 4.0);
        assert!(floor < -50.0, "{floor}");
        assert_eq!(classify(edge_level(&w, 0.5), floor), LevelClass::Silence);
        assert_eq!(classify(edge_level(&w, 1.2), floor), LevelClass::Speech);
        // 20 ms either side of the word's edge still hears it.
        assert_eq!(classify(edge_level(&w, 1.515), floor), LevelClass::Speech);
        assert_eq!(classify(edge_level(&w, 1.6), floor), LevelClass::Silence);
    }

    #[test]
    fn edges_move_off_speech_when_word_times_are_late() {
        // Real speech: "one" 0.5–0.9, "two" 1.0–1.4, "three" 1.5–1.9. The transcript is 50 ms late on
        // "two" and runs it into "three" (no gap), as recognisers do.
        let w = speech(3.0, &[(0.5, 0.9), (1.0, 1.4), (1.5, 1.9)]);
        let words = vec![word("one", 0.55, 0.93), word("two", 1.05, 1.5), word("three", 1.5, 1.95)];
        let c = cut_edges(&w, &words, 1, 1, 0.04).unwrap();
        // Word times alone would cut at 1.5 (inside "three"'s first 20 ms → speech).
        assert_eq!(c.end.planned, 1.5);
        assert_eq!(classify(edge_level(&w, 1.5), c.floor_db), LevelClass::Speech);
        assert_eq!(c.start.class, LevelClass::Silence, "{c:?}");
        assert_eq!(c.end.class, LevelClass::Silence, "{c:?}");
        assert!(c.end.t > 1.42 && c.end.t < 1.48, "{c:?}");
        assert!(c.start.t > 0.92 && c.start.t < 0.98, "{c:?}");
    }

    #[test]
    fn a_real_pause_beats_a_dip_inside_a_word() {
        // "principles" is said 1.0–1.45 with a 50 ms closure (the "p") at 1.27–1.32; a real pause follows
        // (1.45–1.65), then "and". The transcript ends "principles" early, at 1.25, right by the closure.
        let w = speech(3.0, &[(1.0, 1.27), (1.32, 1.45), (1.65, 1.95)]);
        let words = vec![word("first", 0.5, 0.95), word("principles", 1.0, 1.25), word("and", 1.65, 1.95)];
        let floor = room_floor(&w, 0.0, 3.0);
        assert!((quiet_before(&w, 1.6, floor) - 0.15).abs() < 0.011, "silent since the word ended at 1.45");
        assert_eq!(quiet_before(&w, 1.2, floor), 0.0, "still talking");
        assert!(quiet_run(&w, 1.295, floor) < MIN_PAUSE, "the closure is a dip, not a pause");
        assert!(quiet_run(&w, 1.55, floor) >= MIN_PAUSE);
        assert_eq!(quiet_run(&w, 1.1, floor), 0.0, "on speech");
        let c = cut_edges(&w, &words, 0, 1, 0.04).unwrap();
        assert!(c.end.t > 1.46 && c.end.t < 1.64, "the cut goes in the pause after the word, not in its closure: {c:?}");
        assert!(c.end.in_pause(), "{c:?}");
    }

    #[test]
    fn running_speech_has_no_pause_to_cut_in() {
        // "know" runs straight into "and" with only a 30 ms dip: there is no clean place to stop.
        let w = speech(3.0, &[(1.0, 1.4), (1.43, 1.8)]);
        let words = vec![word("know,", 1.0, 1.4), word("and", 1.43, 1.8)];
        let c = cut_edges(&w, &words, 0, 0, 0.04).unwrap();
        assert!(!c.end.in_pause(), "{c:?}");
    }

    #[test]
    fn continuous_talk_is_not_mistaken_for_room_tone() {
        // 30 s of unbroken loud sound (a scored cold open, a fast exchange): its quietest tenth is still
        // programme sound, so the floor stays at the cap and the sound reads as speech, not silence.
        let w = speech(30.0, &[(0.0, 30.0)]);
        let floor = room_floor(&w, 10.0, 11.0);
        assert_eq!(floor, MAX_ROOM_FLOOR);
        assert_eq!(classify(edge_level(&w, 10.5), floor), LevelClass::Speech);
        assert_eq!(quiet_run(&w, 10.5, floor), 0.0);
    }

    #[test]
    fn sentence_ends_by_punctuation() {
        for t in ["right?", "it.", "wow!", "so…", " immaculate.\"", "isn't.)"] {
            assert!(ends_sentence(t), "{t}");
        }
        for t in ["know,", "and", "principles", "Mr", "U.S.-"] {
            assert!(!ends_sentence(t), "{t}");
        }
        let plain: Vec<Word> = (0..100).map(|i| word("so", i as f64, i as f64 + 0.5)).collect();
        assert!(!is_punctuated(&plain));
        let mut said = plain.clone();
        said[10].text = "so.".into();
        said[50].text = "so?".into();
        assert!(is_punctuated(&said));
    }

    #[test]
    fn clean_pauses_keep_the_planned_edges() {
        let w = speech(3.0, &[(0.5, 0.9), (1.2, 1.5), (1.8, 2.2)]);
        let words = vec![word("one", 0.5, 0.9), word("two", 1.2, 1.5), word("three", 1.8, 2.2)];
        let c = cut_edges(&w, &words, 1, 1, 0.04).unwrap();
        assert!(c.start.moved_ms().abs() < 1.0 && c.end.moved_ms().abs() < 1.0, "{c:?}");
        assert_eq!((c.start.class, c.end.class), (LevelClass::Silence, LevelClass::Silence));
        assert!(cut_edges(&w, &words, 2, 3, 0.04).is_none());
    }
}
