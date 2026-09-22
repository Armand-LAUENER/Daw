//! A track's equaliser, as the project stores it.
//!
//! Plain settings: a shape and three numbers per band. Frequencies and gains
//! are parameters rather than positions in time, so they are floats (decision
//! D1 is about time); they are kept inside ranges that sound, and a file that
//! holds one outside is refused like any other damage.

use serde::{Deserialize, Serialize};

/// Bands per track. Enough for the usual moves — clean the low end, shape two
/// regions, tame the top — and fixed, so a band keeps its place in a file.
pub const BANDS: usize = 4;

/// What a band does to the spectrum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandShape {
    /// Removes what is below the frequency.
    LowCut,
    /// Raises or lowers everything below the frequency.
    LowShelf,
    /// Raises or lowers around the frequency.
    Bell,
    /// Raises or lowers everything above the frequency.
    HighShelf,
    /// Removes what is above the frequency.
    HighCut,
}

impl BandShape {
    pub const ALL: [BandShape; 5] = [Self::LowCut, Self::LowShelf, Self::Bell, Self::HighShelf, Self::HighCut];

    /// Whether the gain means anything. A cut has a slope, not a level.
    pub fn has_gain(self) -> bool {
        matches!(self, Self::LowShelf | Self::Bell | Self::HighShelf)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::LowCut => "low cut",
            Self::LowShelf => "low shelf",
            Self::Bell => "bell",
            Self::HighShelf => "high shelf",
            Self::HighCut => "high cut",
        }
    }
}

/// One band's settings.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    pub shape: BandShape,
    /// Hertz.
    pub freq: f32,
    /// Decibels. Ignored by the cuts.
    pub gain: f32,
    pub q: f32,
    pub on: bool,
}

impl EqBand {
    /// The audible range.
    pub const FREQ: std::ops::RangeInclusive<f32> = 20.0..=20_000.0;
    /// Beyond this a band is a tone control no longer, and nearer a feedback
    /// hazard than a correction.
    pub const GAIN: std::ops::RangeInclusive<f32> = -24.0..=24.0;
    pub const Q: std::ops::RangeInclusive<f32> = 0.1..=18.0;

    pub fn is_valid(&self) -> bool {
        Self::FREQ.contains(&self.freq) && Self::GAIN.contains(&self.gain) && Self::Q.contains(&self.q)
    }
}

/// A new track's equaliser: two shelves and two bells spread across the
/// spectrum, all at 0 dB — on, but changing nothing until moved.
pub fn default_eq() -> [EqBand; BANDS] {
    let band = |shape, freq| EqBand { shape, freq, gain: 0.0, q: std::f32::consts::FRAC_1_SQRT_2, on: true };
    [
        band(BandShape::LowShelf, 100.0),
        EqBand { q: 1.0, ..band(BandShape::Bell, 500.0) },
        EqBand { q: 1.0, ..band(BandShape::Bell, 2_000.0) },
        band(BandShape::HighShelf, 8_000.0),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_valid_and_flat() {
        assert!(default_eq().iter().all(|b| b.is_valid() && b.gain == 0.0 && b.on));
    }

    #[test]
    fn values_outside_what_sounds_are_invalid() {
        let band = default_eq()[1];
        assert!(!EqBand { freq: 5.0, ..band }.is_valid());
        assert!(!EqBand { gain: 40.0, ..band }.is_valid());
        assert!(!EqBand { q: 0.0, ..band }.is_valid());
        assert!(!EqBand { gain: f32::NAN, ..band }.is_valid(), "NaN is not in any range");
    }
}
