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

/// Whether a tempo and a meter read from a file are ones this build accepts.
pub(crate) fn valid(tempo: Tempo, meter: Meter) -> bool {
    Tempo::from_millis(tempo.0).is_some() && meter.is_valid()
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

    #[test]
    fn a_position_is_counted_from_bar_one_beat_one() {
        let meter = Meter::default();
        assert_eq!(meter.locate(Tick(0)), (1, 1, 0));
        assert_eq!(meter.locate(Tick(959)), (1, 1, 959));
        assert_eq!(meter.locate(Tick(960)), (1, 2, 0));
        assert_eq!(meter.locate(Tick(3_840 + 480)), (2, 1, 480));
    }
}
