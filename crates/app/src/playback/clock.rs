//! The transport clock (pure logic, no threads).
//!
//! While playing at 1x forward with a working audio device, the time is the audio position: the
//! number of samples the device has actually played (video follows the audio). Otherwise (shuttle
//! speeds, reverse, no device) it is the wall clock scaled by the rate.

/// J/K/L shuttle and Space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shuttle {
    /// L: play forward; again doubles the speed up to 8x (from reverse: 1x forward).
    Forward,
    /// J: play backward; again doubles up to -8x.
    Reverse,
    /// K: stop.
    Stop,
    /// Space: play 1x / pause.
    Toggle,
}

pub const MAX_SHUTTLE: f64 = 8.0;

/// The new signed rate (0 = paused) after a shuttle key.
pub fn shuttle(rate: f64, s: Shuttle) -> f64 {
    match s {
        Shuttle::Forward if rate <= 0.0 => 1.0,
        Shuttle::Forward => (rate * 2.0).min(MAX_SHUTTLE),
        Shuttle::Reverse if rate >= 0.0 => -1.0,
        Shuttle::Reverse => (rate * 2.0).max(-MAX_SHUTTLE),
        Shuttle::Stop => 0.0,
        Shuttle::Toggle if rate != 0.0 => 0.0,
        Shuttle::Toggle => 1.0,
    }
}

/// The integer speed this rate is played with sound at (1x, and 2x/4x shuttle); `None` = silent
/// (pause, reverse, 8x).
pub fn audio_speed(rate: f64) -> Option<u32> {
    [1u32, 2, 4].into_iter().find(|k| (rate - *k as f64).abs() < 1e-9)
}

/// True when this rate is played with sound (and the audio device is the master clock).
pub fn audible(rate: f64) -> bool {
    audio_speed(rate).is_some()
}

#[derive(Clone, Debug)]
pub struct Clock {
    rate: f64,
    origin_t: f64,
    origin_wall: f64,
    last: f64,
}

impl Clock {
    pub fn new(t: f64) -> Self {
        Clock { rate: 0.0, origin_t: t, origin_wall: 0.0, last: t }
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    pub fn playing(&self) -> bool {
        self.rate != 0.0
    }

    /// Starts (or restarts) playback at `t` with a signed `rate`.
    pub fn start(&mut self, t: f64, wall: f64, rate: f64) {
        self.rate = rate;
        self.origin_t = t;
        self.origin_wall = wall;
        self.last = t;
    }

    /// Pauses at `t`.
    pub fn stop(&mut self, t: f64) {
        self.rate = 0.0;
        self.origin_t = t;
        self.last = t;
    }

    /// Re-bases the wall clock at the current time (used when the audio master stalls).
    pub fn rebase(&mut self, wall: f64) {
        let t = self.last;
        let r = self.rate;
        self.start(t, wall, r);
    }

    /// The playhead at wall time `wall`. `audio_pos` = seconds of sound played since `start`
    /// when the audio device is the master. Never moves against the playing direction.
    pub fn time(&mut self, wall: f64, audio_pos: Option<f64>) -> f64 {
        if self.rate == 0.0 {
            return self.origin_t;
        }
        let t = match audio_pos {
            // The device plays `rate` seconds of timeline per second of sound.
            Some(a) if audible(self.rate) => self.origin_t + a.max(0.0) * self.rate,
            _ => self.origin_t + (wall - self.origin_wall).max(0.0) * self.rate,
        };
        let t = if self.rate > 0.0 { t.max(self.last) } else { t.min(self.last) };
        self.last = t;
        t
    }
}

/// What playback does when it reaches the edge of its range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bound {
    Continue,
    /// Pause here.
    Stop(f64),
    /// Jump here and keep playing (loop).
    Wrap(f64),
}

/// Range check for time `t` moving at `rate` within `[start, end]`. `last_frame` is the duration
/// of one frame (a reverse loop wraps to the last frame, not past the end).
pub fn bound(t: f64, rate: f64, start: f64, end: f64, looping: bool, last_frame: f64) -> Bound {
    if end <= start {
        return Bound::Stop(start);
    }
    if rate > 0.0 && t >= end {
        if looping {
            Bound::Wrap(start)
        } else {
            Bound::Stop(end)
        }
    } else if rate < 0.0 && t <= start {
        if looping {
            Bound::Wrap((end - last_frame).max(start))
        } else {
            Bound::Stop(start)
        }
    } else {
        Bound::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jkl() {
        assert_eq!(shuttle(0.0, Shuttle::Forward), 1.0);
        assert_eq!(shuttle(1.0, Shuttle::Forward), 2.0);
        assert_eq!(shuttle(2.0, Shuttle::Forward), 4.0);
        assert_eq!(shuttle(4.0, Shuttle::Forward), 8.0);
        assert_eq!(shuttle(8.0, Shuttle::Forward), 8.0);
        assert_eq!(shuttle(-4.0, Shuttle::Forward), 1.0);
        assert_eq!(shuttle(0.0, Shuttle::Reverse), -1.0);
        assert_eq!(shuttle(-1.0, Shuttle::Reverse), -2.0);
        assert_eq!(shuttle(-8.0, Shuttle::Reverse), -8.0);
        assert_eq!(shuttle(2.0, Shuttle::Reverse), -1.0);
        assert_eq!(shuttle(4.0, Shuttle::Stop), 0.0);
        assert_eq!(shuttle(0.0, Shuttle::Toggle), 1.0);
        assert_eq!(shuttle(-2.0, Shuttle::Toggle), 0.0);
        assert!(audible(1.0) && audible(2.0) && audible(4.0) && !audible(8.0) && !audible(-1.0) && !audible(0.0));
        assert_eq!(audio_speed(4.0), Some(4));
    }

    #[test]
    fn wall_clock() {
        let mut c = Clock::new(5.0);
        assert_eq!(c.time(100.0, None), 5.0);
        c.start(5.0, 100.0, 2.0);
        assert!((c.time(101.0, None) - 7.0).abs() < 1e-9);
        c.start(5.0, 100.0, -1.0);
        assert!((c.time(100.5, None) - 4.5).abs() < 1e-9);
        c.stop(4.5);
        assert!(!c.playing());
        assert_eq!(c.time(200.0, None), 4.5);
    }

    #[test]
    fn audio_master_and_monotonic() {
        let mut c = Clock::new(0.0);
        c.start(10.0, 0.0, 1.0);
        // Before the device has played anything the playhead waits at the start.
        assert_eq!(c.time(0.2, Some(0.0)), 10.0);
        assert!((c.time(0.3, Some(0.25)) - 10.25).abs() < 1e-9);
        // A jittery (smaller) audio position never moves the playhead back.
        assert!((c.time(0.31, Some(0.24)) - 10.25).abs() < 1e-9);
        // Shuttle speeds: the sound is played k times faster, so 0.5 s of sound = 1 s of timeline.
        c.start(10.0, 0.0, 2.0);
        assert!((c.time(1.0, Some(0.5)) - 11.0).abs() < 1e-9);
        // Reverse runs on the wall clock.
        c.start(10.0, 0.0, -2.0);
        assert!((c.time(1.0, Some(0.5)) - 8.0).abs() < 1e-9);
        // Rebase continues from the last time on the wall clock.
        c.start(0.0, 0.0, 1.0);
        let t = c.time(0.5, Some(0.4));
        c.rebase(3.0);
        assert!((c.time(3.5, None) - (t + 0.5)).abs() < 1e-9);
    }

    #[test]
    fn bounds() {
        assert_eq!(bound(5.0, 1.0, 0.0, 10.0, false, 0.04), Bound::Continue);
        assert_eq!(bound(10.2, 1.0, 0.0, 10.0, false, 0.04), Bound::Stop(10.0));
        assert_eq!(bound(10.2, 1.0, 2.0, 10.0, true, 0.04), Bound::Wrap(2.0));
        assert_eq!(bound(1.9, -1.0, 2.0, 10.0, false, 0.04), Bound::Stop(2.0));
        assert_eq!(bound(1.9, -2.0, 2.0, 10.0, true, 0.04), Bound::Wrap(9.96));
        assert_eq!(bound(3.0, 1.0, 5.0, 5.0, false, 0.04), Bound::Stop(5.0));
    }
}
