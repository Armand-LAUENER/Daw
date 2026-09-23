//! What a track's MIDI clips are played with.
//!
//! A simple subtractive voice: one oscillator shaped by an envelope. Enough to
//! tell a bass from a pad and to hear a part as it will sit; a plugin host
//! (phase 7) is where real instruments come in.

use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wave {
    Sine,
    Saw,
    Square,
}

impl Wave {
    pub const ALL: [Wave; 3] = [Self::Sine, Self::Saw, Self::Square];

    pub fn name(self) -> &'static str {
        match self {
            Self::Sine => "sine",
            Self::Saw => "saw",
            Self::Square => "square",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Instrument {
    pub wave: Wave,
    /// Milliseconds to full level.
    pub attack: f32,
    /// Milliseconds from full level down to the sustain level.
    pub decay: f32,
    /// Level held while a key is down, 0 to 1.
    pub sustain: f32,
    /// Milliseconds to silence after a key is let go.
    pub release: f32,
    /// Each note's share of full scale at the highest velocity.
    pub level: f32,
}

impl Instrument {
    pub const ATTACK: RangeInclusive<f32> = 0.0..=5_000.0;
    pub const DECAY: RangeInclusive<f32> = 1.0..=5_000.0;
    pub const SUSTAIN: RangeInclusive<f32> = 0.0..=1.0;
    pub const RELEASE: RangeInclusive<f32> = 1.0..=10_000.0;
    pub const LEVEL: RangeInclusive<f32> = 0.0..=1.0;

    pub fn is_valid(&self) -> bool {
        Self::ATTACK.contains(&self.attack)
            && Self::DECAY.contains(&self.decay)
            && Self::SUSTAIN.contains(&self.sustain)
            && Self::RELEASE.contains(&self.release)
            && Self::LEVEL.contains(&self.level)
    }
}

impl Default for Instrument {
    /// What every track played before it could choose: a band-limited saw,
    /// quick to speak, soft to let go.
    fn default() -> Self {
        Self { wave: Wave::Saw, attack: 5.0, decay: 150.0, sustain: 0.6, release: 250.0, level: 0.2 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_valid() {
        assert!(Instrument::default().is_valid());
    }

    #[test]
    fn values_outside_what_sounds_are_invalid() {
        assert!(!Instrument { sustain: 1.5, ..Default::default() }.is_valid());
        assert!(!Instrument { release: 0.0, ..Default::default() }.is_valid());
        assert!(!Instrument { level: f32::NAN, ..Default::default() }.is_valid());
    }
}
