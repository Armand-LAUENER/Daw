//! Musical time, stored exactly (decision D1 in the plan).
//!
//! The model never holds seconds or frames. Positions are whole ticks at a
//! fixed resolution, and the tempo is a whole number of thousandths of a beat
//! per minute: both are integers, so a project saved and reopened, or diffed,
//! never differs by a rounding error. Frames exist only once the engine turns
//! a tick into one, for a given sample rate, in `project-engine`.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Ticks per quarter note.
///
/// 960 = 2⁶·3·5: duplets down to 1/256 notes, triplets and quintuplets all land
/// on whole ticks, which is what lets a quantised note sit exactly on its grid.
pub const PPQ: u64 = 960;

/// A position or a length on the timeline, in ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tick(pub u64);

/// Beats per minute, in thousandths.
///
/// An integer so the value typed is the value stored: 128.5 BPM is 128 500,
/// exactly, where a float would store its nearest neighbour and a diff would
/// one day show the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tempo(u32);

impl Tempo {
    /// Slower than anything that is still felt as a pulse.
    pub const MIN: Tempo = Tempo(20_000);
    /// Faster than any tempo written down, and below where a tick at 960 PPQ
    /// would be shorter than a sample at 48 kHz.
    pub const MAX: Tempo = Tempo(999_000);

    /// From thousandths of a beat per minute. `None` outside [`MIN`](Self::MIN)
    /// ..= [`MAX`](Self::MAX).
    pub fn from_millis(millis: u32) -> Option<Self> {
        (Self::MIN.0..=Self::MAX.0).contains(&millis).then_some(Self(millis))
    }

    /// From beats per minute, rounded to the nearest thousandth.
    pub fn from_bpm(bpm: f64) -> Option<Self> {
        if !bpm.is_finite() {
            return None;
        }
        Self::from_millis((bpm * 1000.0).round().clamp(0.0, f64::from(u32::MAX)) as u32)
    }

    pub fn millis(self) -> u32 {
        self.0
    }

    pub fn bpm(self) -> f64 {
        f64::from(self.0) / 1000.0
    }
}

impl Default for Tempo {
    fn default() -> Self {
        Self(120_000)
    }
}

impl fmt::Display for Tempo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.bpm())
    }
}

/// A time signature: how many beats a bar holds, and which note is one beat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Meter {
    beats: u8,
    unit: u8,
}

impl Meter {
    /// `unit` is the note value of a beat — 4 for quarters, 8 for eighths —
    /// and must be a power of two from 1 to 32. `beats` is 1 to 32.
    pub fn new(beats: u8, unit: u8) -> Option<Self> {
        let valid = (1..=32).contains(&beats) && unit.is_power_of_two() && unit <= 32;
        valid.then_some(Self { beats, unit })
    }

    pub fn beats(self) -> u8 {
        self.beats
    }

    pub fn unit(self) -> u8 {
        self.unit
    }

    /// One beat, in ticks.
    pub fn beat_length(self) -> Tick {
        Tick(PPQ * 4 / u64::from(self.unit))
    }

    /// One bar, in ticks.
    pub fn bar_length(self) -> Tick {
        Tick(self.beat_length().0 * u64::from(self.beats))
    }

    /// Where a tick falls, counted the way musicians count: bar and beat from
    /// one, and the ticks into the beat from zero.
    pub fn locate(self, tick: Tick) -> (u64, u64, u64) {
        let (bar, beat) = (self.bar_length().0, self.beat_length().0);
        let into_bar = tick.0 % bar;
        (tick.0 / bar + 1, into_bar / beat + 1, into_bar % beat)
    }

    fn is_valid(self) -> bool {
        Self::new(self.beats, self.unit).is_some()
    }
}

impl Default for Meter {
    fn default() -> Self {
        Self { beats: 4, unit: 4 }
    }
}

impl fmt::Display for Meter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.beats, self.unit)
    }
}

/// A tempo taking effect at a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TempoChange {
    pub at: Tick,
    pub tempo: Tempo,
}

/// How the tempo moves through a project: a list of changes, each holding
/// until the next. Steps, not ramps.
///
/// The first change is always at tick 0, so every tick has a tempo; the
/// changes are in order and no two share a tick. A change is named by its
/// place in the list, which dragging it never alters — it stops at its
/// neighbours — so a drag stays one gesture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TempoMap(Vec<TempoChange>);

impl TempoMap {
    /// One tempo from the start.
    pub fn constant(tempo: Tempo) -> Self {
        Self(vec![TempoChange { at: Tick(0), tempo }])
    }

    pub fn changes(&self) -> &[TempoChange] {
        &self.0
    }

    /// The tempo the project starts at.
    pub fn start(&self) -> Tempo {
        self.0[0].tempo
    }

    /// The tempo in effect at a tick.
    pub fn tempo_at(&self, tick: Tick) -> Tempo {
        let index = self.0.partition_point(|c| c.at <= tick).saturating_sub(1);
        self.0[index].tempo
    }

    /// Whether this map keeps its promises: a change at 0, in order, no two
    /// at one tick, every tempo in range.
    pub fn is_valid(&self) -> bool {
        self.0.first().is_some_and(|c| c.at == Tick(0))
            && self.0.windows(2).all(|w| w[0].at < w[1].at)
            && self.0.iter().all(|c| Tempo::from_millis(c.tempo.millis()).is_some())
    }

    pub(crate) fn with_tempo(&self, index: usize, tempo: Tempo) -> Option<Self> {
        let mut changes = self.0.clone();
        changes.get_mut(index)?.tempo = tempo;
        Some(Self(changes))
    }

    /// A new change at `at`, or `None` when there is one there already — it
    /// should be changed instead — or `at` is the start.
    pub(crate) fn with_change(&self, at: Tick, tempo: Tempo) -> Option<Self> {
        if at == Tick(0) || self.0.iter().any(|c| c.at == at) {
            return None;
        }
        let mut changes = self.0.clone();
        let index = changes.partition_point(|c| c.at < at);
        changes.insert(index, TempoChange { at, tempo });
        Some(Self(changes))
    }

    /// A change moved to `to`, kept strictly between its neighbours so the
    /// order — and every change's place in it — holds. The first cannot move.
    pub(crate) fn with_moved(&self, index: usize, to: Tick) -> Option<Self> {
        if index == 0 || index >= self.0.len() {
            return None;
        }
        let low = self.0[index - 1].at.0 + 1;
        let high = self.0.get(index + 1).map_or(u64::MAX, |c| c.at.0 - 1);
        let mut changes = self.0.clone();
        changes[index].at = Tick(to.0.clamp(low, high));
        Some(Self(changes))
    }

    /// Without a change. The first cannot go: something has to hold at 0.
    pub(crate) fn without(&self, index: usize) -> Option<Self> {
        if index == 0 || index >= self.0.len() {
            return None;
        }
        let mut changes = self.0.clone();
        changes.remove(index);
        Some(Self(changes))
    }
}

impl Default for TempoMap {
    fn default() -> Self {
        Self::constant(Tempo::default())
    }
}

pub(crate) fn meter_valid(meter: Meter) -> bool {
    meter.is_valid()
}

/// A stretch of the timeline the transport can repeat.
///
/// Ticks, like everything else the model places in time: a loop drawn around
/// two bars stays around those two bars when the tempo changes, instead of
/// drifting as frames would.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopRegion {
    pub start: Tick,
    pub end: Tick,
}

impl LoopRegion {
    /// Whether this is a region at all: `start` strictly before `end`, so the
    /// transport always has somewhere to go back to.
    pub fn is_valid(self) -> bool {
        self.start < self.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typed_tempo_is_stored_exactly() {
        assert_eq!(Tempo::from_bpm(128.5).map(Tempo::millis), Some(128_500));
        assert_eq!(Tempo::from_bpm(140.0).map(Tempo::bpm), Some(140.0));
        assert_eq!(Tempo::from_bpm(f64::NAN), None);
        assert_eq!(Tempo::from_bpm(5.0), None);
        assert_eq!(Tempo::from_bpm(2_000.0), None);
    }

    #[test]
    fn a_meter_counts_in_its_own_beat() {
        let six_eight = Meter::new(6, 8).unwrap();
        assert_eq!(six_eight.beat_length(), Tick(480));
        assert_eq!(six_eight.bar_length(), Tick(2_880));
        assert_eq!(Meter::default().bar_length(), Tick(3_840));
        assert_eq!(Meter::new(4, 3), None, "a beat of a third of a whole note does not exist");
        assert_eq!(Meter::new(0, 4), None);
    }

    fn map(changes: &[(u64, f64)]) -> TempoMap {
        TempoMap(
            changes
                .iter()
                .map(|&(at, bpm)| TempoChange { at: Tick(at), tempo: Tempo::from_bpm(bpm).unwrap() })
                .collect(),
        )
    }

    #[test]
    fn a_tick_takes_the_tempo_of_the_last_change_before_it() {
        let m = map(&[(0, 120.0), (3_840, 90.0), (7_680, 140.0)]);
        assert_eq!(m.tempo_at(Tick(0)).bpm(), 120.0);
        assert_eq!(m.tempo_at(Tick(3_839)).bpm(), 120.0);
        assert_eq!(m.tempo_at(Tick(3_840)).bpm(), 90.0, "a change takes effect on its own tick");
        assert_eq!(m.tempo_at(Tick(99_999)).bpm(), 140.0);
        assert!(m.is_valid());
    }

    #[test]
    fn a_map_keeps_its_promises_through_every_edit() {
        let m = map(&[(0, 120.0), (3_840, 90.0), (7_680, 140.0)]);
        // Added in order, refused on an occupied tick and at the start.
        let added = m.with_change(Tick(1_000), Tempo::default()).unwrap();
        assert!(added.is_valid());
        assert_eq!(added.changes()[1].at, Tick(1_000));
        assert!(m.with_change(Tick(3_840), Tempo::default()).is_none());
        assert!(m.with_change(Tick(0), Tempo::default()).is_none());
        // Moved no further than its neighbours allow.
        assert_eq!(m.with_moved(1, Tick(99_999)).unwrap().changes()[1].at, Tick(7_679));
        assert_eq!(m.with_moved(1, Tick(0)).unwrap().changes()[1].at, Tick(1));
        assert!(m.with_moved(0, Tick(10)).is_none(), "the start moved");
        // Removed, except the start.
        assert_eq!(m.without(1).unwrap().changes().len(), 2);
        assert!(m.without(0).is_none());
    }

    #[test]
    fn a_map_that_breaks_a_promise_is_invalid() {
        assert!(!map(&[(10, 120.0)]).is_valid(), "no tempo at the start");
        assert!(!map(&[(0, 120.0), (500, 90.0), (500, 100.0)]).is_valid(), "two changes on one tick");
        assert!(!map(&[(0, 120.0), (900, 90.0), (500, 100.0)]).is_valid(), "out of order");
    }

    #[test]
    fn a_position_is_counted_from_bar_one_beat_one() {
        let meter = Meter::default();
        assert_eq!(meter.locate(Tick(0)), (1, 1, 0));
        assert_eq!(meter.locate(Tick(959)), (1, 1, 959));
        assert_eq!(meter.locate(Tick(960)), (1, 2, 0));
        assert_eq!(meter.locate(Tick(3_840 + 480)), (2, 1, 480));
    }
}
