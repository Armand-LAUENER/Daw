//! A track's compressor, echo and room, as the project stores them.
//!
//! Each is off until switched on, so a track sounds as its source and
//! equaliser make it until someone decides otherwise. Ranges are kept to what
//! sounds; a file holding a value outside them is refused like any other
//! damage.

use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

use crate::{PPQ, Tick};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CompSettings {
    pub on: bool,
    /// dB.
    pub threshold: f32,
    /// 1 does nothing, 20 is close to limiting.
    pub ratio: f32,
    /// Milliseconds.
    pub attack: f32,
    /// Milliseconds.
    pub release: f32,
    /// dB added after, to win back the level taken.
    pub makeup: f32,
}

impl CompSettings {
    pub const THRESHOLD: RangeInclusive<f32> = -60.0..=0.0;
    pub const RATIO: RangeInclusive<f32> = 1.0..=20.0;
    pub const ATTACK: RangeInclusive<f32> = 0.1..=200.0;
    pub const RELEASE: RangeInclusive<f32> = 5.0..=2_000.0;
    pub const MAKEUP: RangeInclusive<f32> = 0.0..=24.0;

    pub fn is_valid(&self) -> bool {
        Self::THRESHOLD.contains(&self.threshold)
            && Self::RATIO.contains(&self.ratio)
            && Self::ATTACK.contains(&self.attack)
            && Self::RELEASE.contains(&self.release)
            && Self::MAKEUP.contains(&self.makeup)
    }
}

impl Default for CompSettings {
    fn default() -> Self {
        Self { on: false, threshold: -18.0, ratio: 3.0, attack: 10.0, release: 120.0, makeup: 0.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DelaySettings {
    pub on: bool,
    /// Musical, not in seconds: the echo stays on the beat when the tempo
    /// changes (decision D1).
    pub time: Tick,
    /// How much of each repeat comes back, below 1 so it always dies away.
    pub feedback: f32,
    /// How much echo is heard over the dry sound, 0 to 1.
    pub mix: f32,
}

impl DelaySettings {
    /// From a sixty-fourth note to a whole bar of 4/4.
    pub const TIME: RangeInclusive<u64> = PPQ / 16..=PPQ * 4;
    pub const FEEDBACK: RangeInclusive<f32> = 0.0..=0.95;
    pub const MIX: RangeInclusive<f32> = 0.0..=1.0;

    pub fn is_valid(&self) -> bool {
        Self::TIME.contains(&self.time.0) && Self::FEEDBACK.contains(&self.feedback) && Self::MIX.contains(&self.mix)
    }
}

impl Default for DelaySettings {
    fn default() -> Self {
        Self { on: false, time: Tick(PPQ / 2), feedback: 0.35, mix: 0.25 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReverbSettings {
    pub on: bool,
    /// 0 a small room, 1 a hall.
    pub size: f32,
    /// 0 bright, 1 dull.
    pub damping: f32,
    /// How much of the room is heard over the dry sound, 0 to 1.
    pub mix: f32,
}

impl ReverbSettings {
    pub const UNIT: RangeInclusive<f32> = 0.0..=1.0;

    pub fn is_valid(&self) -> bool {
        Self::UNIT.contains(&self.size) && Self::UNIT.contains(&self.damping) && Self::UNIT.contains(&self.mix)
    }
}

impl Default for ReverbSettings {
    fn default() -> Self {
        Self { on: false, size: 0.5, damping: 0.5, mix: 0.2 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_valid_and_off() {
        assert!(CompSettings::default().is_valid() && !CompSettings::default().on);
        assert!(DelaySettings::default().is_valid() && !DelaySettings::default().on);
        assert!(ReverbSettings::default().is_valid() && !ReverbSettings::default().on);
    }

    #[test]
    fn values_outside_what_sounds_are_invalid() {
        assert!(!CompSettings { ratio: 0.5, ..Default::default() }.is_valid());
        assert!(!DelaySettings { feedback: 1.0, ..Default::default() }.is_valid(), "a delay that never dies");
        assert!(!DelaySettings { time: Tick(0), ..Default::default() }.is_valid());
        assert!(!ReverbSettings { mix: f32::NAN, ..Default::default() }.is_valid());
    }
}
