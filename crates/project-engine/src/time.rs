//! Musical time turned into frames, and back (decision D1 in the plan).
//!
//! The model counts in ticks, the engine in frames, and this is the only place
//! that converts between them. Every conversion is computed from zero in whole
//! numbers — ticks × sample rate × 60 000 over tempo × PPQ — rather than by
//! adding up durations, so the ten-thousandth bar is exactly where it should
//! be instead of wherever a sum of rounded lengths drifted to.

use std::sync::Arc;

use project_model::{Meter, PPQ, Tempo, TempoMap, Tick};

/// Fractional bits kept on a segment's starting frame. A tempo change can
/// fall between two frames, so each segment's start is carried in fixed point
/// rather than rounded: the error per change is under 2⁻³² of a frame, which
/// a project would need millions of changes to turn into a single sample.
const FRACTION: u32 = 32;

/// One stretch at one tempo: where it starts, in ticks and in (fixed-point)
/// frames, and how fast it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Segment {
    tick: u64,
    /// Frames since the start, times 2^FRACTION.
    start: u128,
    millis: u32,
}

impl Segment {
    /// `frames = ticks × num / den` within this segment.
    fn ratio(&self, rate: u32) -> (u128, u128) {
        (u128::from(rate) * 60_000, u128::from(self.millis) * u128::from(PPQ))
    }
}

/// A tempo map at a sample rate: enough to place any tick on the engine's
/// timeline, and any frame back in the music.
///
/// Cheap to clone — the segments are shared — since every view that draws
/// time holds one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timing {
    segments: Arc<[Segment]>,
    pub sample_rate: u32,
}

impl Timing {
    pub fn new(map: &TempoMap, sample_rate: u32) -> Self {
        let mut segments: Vec<Segment> = Vec::with_capacity(map.changes().len());
        for change in map.changes() {
            let start = segments.last().map_or(0, |previous: &Segment| {
                let (num, den) = previous.ratio(sample_rate);
                previous.start + ((u128::from(change.at.0 - previous.tick) * num) << FRACTION) / den
            });
            segments.push(Segment { tick: change.at.0, start, millis: change.tempo.millis() });
        }
        Self { segments: segments.into(), sample_rate }
    }

    /// One tempo throughout.
    pub fn constant(tempo: Tempo, sample_rate: u32) -> Self {
        Self::new(&TempoMap::constant(tempo), sample_rate)
    }

    fn at_tick(&self, tick: u64) -> &Segment {
        let index = self.segments.partition_point(|s| s.tick <= tick).saturating_sub(1);
        &self.segments[index]
    }

    /// The first frame that is not before a tick.
    ///
    /// Rounded up, not to the nearest: a note placed on a beat must never
    /// sound before it, and the frame it lands on must read back as that beat
    /// rather than the tick just before.
    pub fn frame_of(&self, tick: Tick) -> u64 {
        let segment = self.at_tick(tick.0);
        let (num, den) = segment.ratio(self.sample_rate);
        let fixed = segment.start + ((u128::from(tick.0 - segment.tick) * num) << FRACTION) / den;
        fixed.div_ceil(1 << FRACTION).try_into().unwrap_or(u64::MAX)
    }

    /// The tick a frame falls in, rounded down: a playhead is in a beat until
    /// it has reached the next one, never before.
    pub fn tick_of(&self, frame: u64) -> Tick {
        let fixed = u128::from(frame) << FRACTION;
        let index = self.segments.partition_point(|s| s.start <= fixed).saturating_sub(1);
        let segment = &self.segments[index];
        let (num, den) = segment.ratio(self.sample_rate);
        let into = (fixed - segment.start) * den / (num << FRACTION);
        Tick(segment.tick + u64::try_from(into).unwrap_or(u64::MAX - segment.tick))
    }

    /// How many frames `length` ticks last when they start at `at` — which
    /// depends on where, once the tempo can change.
    pub fn frames_for(&self, length: Tick, at: Tick) -> u64 {
        self.frame_of(Tick(at.0 + length.0)) - self.frame_of(at)
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
    pub fn of(frame: u64, timing: &Timing, meter: Meter) -> Self {
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
        Timing::constant(Tempo::from_bpm(bpm).unwrap(), rate)
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
        let position = Position::of(bar_two_beat_three, &timing, Meter::default());
        assert_eq!(position.musical(), "2.3.000");
        assert_eq!(position.clock(), "0:03.000");
        assert_eq!(Position::of(48_000 * 61 + 480, &timing, Meter::default()).clock(), "1:01.010");
    }

    /// A map built through commands, the only way one can be made outside
    /// the model: 120 BPM, then 60 from bar 2, then 180 from bar 3.
    fn three_tempos(rate: u32) -> Timing {
        let mut session = project_model::Session::new();
        for (bar, bpm) in [(1u64, 60.0), (2, 180.0)] {
            let tempo = Tempo::from_bpm(bpm).unwrap();
            session.apply(project_model::Command::AddTempoChange { at: Tick(bar * 3_840), tempo }).unwrap();
        }
        Timing::new(session.project().tempo_map(), rate)
    }

    #[test]
    fn each_bar_lasts_as_long_as_its_tempo_says() {
        // A 4/4 bar is 2 s at 120, 4 s at 60, 4/3 s at 180.
        let timing = three_tempos(48_000);
        assert_eq!(timing.frame_of(Tick(3_840)), 96_000);
        assert_eq!(timing.frame_of(Tick(7_680)), 96_000 + 192_000);
        assert_eq!(timing.frame_of(Tick(11_520)), 96_000 + 192_000 + 64_000);
        assert_eq!(timing.frames_for(Tick(960), Tick(3_840)), 48_000, "a beat at 60 BPM is a second");
        // And back: the frame a change lands on reads as its tick.
        assert_eq!(timing.tick_of(288_000), Tick(7_680));
        assert_eq!(timing.tick_of(287_999), Tick(7_679));
    }

    proptest! {
        #[test]
        fn a_tick_survives_the_round_trip_across_tempo_changes(
            tempos in prop::collection::vec(20_000u32..=300_000, 1..6),
            gaps in prop::collection::vec(1u64..20_000, 5),
            rate in prop::sample::select(vec![44_100u32, 48_000, 96_000]),
            tick in 0u64..200_000,
        ) {
            let mut session = project_model::Session::new();
            session.apply(project_model::Command::SetTempo { tempo: Tempo::from_millis(tempos[0]).unwrap() }).unwrap();
            let mut at = 0;
            for (millis, gap) in tempos[1..].iter().zip(&gaps) {
                at += gap;
                let tempo = Tempo::from_millis(*millis).unwrap();
                session.apply(project_model::Command::AddTempoChange { at: Tick(at), tempo }).unwrap();
            }
            let timing = Timing::new(session.project().tempo_map(), rate);
            prop_assert_eq!(timing.tick_of(timing.frame_of(Tick(tick))), Tick(tick));
        }

        #[test]
        fn time_never_runs_backwards_across_a_change(
            slow in 20_000u32..=120_000,
            fast in 120_000u32..=999_000,
            a in 0u64..50_000,
            b in 0u64..50_000,
        ) {
            let mut session = project_model::Session::new();
            session.apply(project_model::Command::SetTempo { tempo: Tempo::from_millis(slow).unwrap() }).unwrap();
            session.apply(project_model::Command::AddTempoChange { at: Tick(10_000), tempo: Tempo::from_millis(fast).unwrap() }).unwrap();
            let timing = Timing::new(session.project().tempo_map(), 48_000);
            let (early, late) = (a.min(b), a.max(b));
            prop_assert!(timing.frame_of(Tick(early)) <= timing.frame_of(Tick(late)));
        }

        #[test]
        fn a_tick_survives_the_round_trip_through_frames(
            millis in 20_000u32..=300_000,
            rate in prop::sample::select(vec![44_100u32, 48_000, 88_200, 96_000]),
            tick in 0u64..10_000_000,
        ) {
            // Up to 300 BPM a tick lasts more than one frame at 44.1 kHz, so a
            // tick placed on a frame must read back as itself.
            let timing = Timing::constant(Tempo::from_millis(millis).unwrap(), rate);
            prop_assert_eq!(timing.tick_of(timing.frame_of(Tick(tick))), Tick(tick));
        }

        #[test]
        fn later_ticks_are_never_earlier_frames(
            millis in 20_000u32..=999_000,
            a in 0u64..100_000_000,
            b in 0u64..100_000_000,
        ) {
            let timing = Timing::constant(Tempo::from_millis(millis).unwrap(), 48_000);
            let (early, late) = (a.min(b), a.max(b));
            prop_assert!(timing.frame_of(Tick(early)) <= timing.frame_of(Tick(late)));
        }
    }
}
