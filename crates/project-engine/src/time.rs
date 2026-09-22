//! Musical time turned into frames, and back (decision D1 in the plan).
//!
//! The model counts in ticks, the engine in frames, and this is the only place
//! that converts between them. Every conversion is computed from zero in whole
//! numbers — ticks × sample rate × 60 000 over tempo × PPQ — rather than by
//! adding up durations, so the ten-thousandth bar is exactly where it should
//! be instead of wherever a sum of rounded lengths drifted to.

use project_model::{Meter, PPQ, Tempo, Tick};

/// A tempo at a sample rate: enough to place a tick on the engine's timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub tempo: Tempo,
    pub sample_rate: u32,
}

impl Timing {
    pub fn new(tempo: Tempo, sample_rate: u32) -> Self {
        Self { tempo, sample_rate }
    }

    /// Frames per `tick` ticks is `tick × rate × 60 000 / (millibpm × PPQ)`.
    /// The numerator and the denominator, as exact integers.
    fn ratio(self) -> (u128, u128) {
        (u128::from(self.sample_rate) * 60_000, u128::from(self.tempo.millis()) * u128::from(PPQ))
    }

    /// The first frame that is not before a tick.
    ///
    /// Rounded up, not to the nearest: a note placed on a beat must never
    /// sound before it, and the frame it lands on must read back as that beat
    /// rather than the tick just before.
    pub fn frame_of(self, tick: Tick) -> u64 {
        let (num, den) = self.ratio();
        (u128::from(tick.0) * num).div_ceil(den).try_into().unwrap_or(u64::MAX)
    }

    /// The tick a frame falls in, rounded down: a playhead is in a beat until
    /// it has reached the next one, never before.
    pub fn tick_of(self, frame: u64) -> Tick {
        let (num, den) = self.ratio();
        Tick((u128::from(frame) * den / num).try_into().unwrap_or(u64::MAX))
    }
}

/// Where a playhead is, both ways a person reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// From one.
    pub bar: u64,
    /// From one.
    pub beat: u64,
    /// Into the beat, from zero.
    pub tick: u64,
    /// Since the start of the timeline.
    pub millis: u64,
}

impl Position {
    /// Where a timeline frame is, for a project's tempo and meter.
    pub fn of(frame: u64, timing: Timing, meter: Meter) -> Self {
        let (bar, beat, tick) = meter.locate(timing.tick_of(frame));
        let millis = frame * 1000 / u64::from(timing.sample_rate.max(1));
        Self { bar, beat, tick, millis }
    }

    /// `bar.beat.tick`, the way a DAW's counter shows it.
    pub fn musical(&self) -> String {
        format!("{}.{}.{:03}", self.bar, self.beat, self.tick)
    }

    /// `m:ss.mmm`.
    pub fn clock(&self) -> String {
        let (minutes, rest) = (self.millis / 60_000, self.millis % 60_000);
        format!("{minutes}:{:02}.{:03}", rest / 1000, rest % 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn at(bpm: f64, rate: u32) -> Timing {
        Timing::new(Tempo::from_bpm(bpm).unwrap(), rate)
    }

    #[test]
    fn a_quarter_at_120_bpm_is_half_a_second() {
        let timing = at(120.0, 48_000);
        assert_eq!(timing.frame_of(Tick(PPQ)), 24_000);
        assert_eq!(timing.tick_of(24_000), Tick(PPQ));
        assert_eq!(timing.tick_of(23_999), Tick(PPQ - 1), "a playhead reached the beat early");
    }

    #[test]
    fn the_thousandth_bar_is_where_it_should_be() {
        // 128.5 BPM at 44.1 kHz does not divide evenly: a sum of rounded bar
        // lengths would have drifted by now.
        let timing = at(128.5, 44_100);
        let bars = 1_000u64;
        let ticks = Tick(bars * Meter::default().bar_length().0);
        let exact = bars as f64 * 4.0 * 60.0 / 128.5 * 44_100.0;
        assert_eq!(timing.frame_of(ticks), exact.ceil() as u64);
    }

    #[test]
    fn a_position_reads_as_bars_and_as_a_clock() {
        let timing = at(120.0, 48_000);
        let bar_two_beat_three = 48_000 * 2 + 24_000 * 2;
        let position = Position::of(bar_two_beat_three, timing, Meter::default());
        assert_eq!(position.musical(), "2.3.000");
        assert_eq!(position.clock(), "0:03.000");
        assert_eq!(Position::of(48_000 * 61 + 480, timing, Meter::default()).clock(), "1:01.010");
    }

    proptest! {
        #[test]
        fn a_tick_survives_the_round_trip_through_frames(
            millis in 20_000u32..=300_000,
            rate in prop::sample::select(vec![44_100u32, 48_000, 88_200, 96_000]),
            tick in 0u64..10_000_000,
        ) {
            // Up to 300 BPM a tick lasts more than one frame at 44.1 kHz, so a
            // tick placed on a frame must read back as itself.
            let timing = Timing::new(Tempo::from_millis(millis).unwrap(), rate);
            prop_assert_eq!(timing.tick_of(timing.frame_of(Tick(tick))), Tick(tick));
        }

        #[test]
        fn later_ticks_are_never_earlier_frames(
            millis in 20_000u32..=999_000,
            a in 0u64..100_000_000,
            b in 0u64..100_000_000,
        ) {
            let timing = Timing::new(Tempo::from_millis(millis).unwrap(), 48_000);
            let (early, late) = (a.min(b), a.max(b));
            prop_assert!(timing.frame_of(Tick(early)) <= timing.frame_of(Tick(late)));
        }
    }
}
