//! Effects buses: shared effects that tracks send to.
//!
//! A bus has no source of its own. It hears what tracks send it, runs that
//! through its effects — wet only, since each track's dry sound already
//! reaches the mix on its own — and returns the result to the master. One
//! reverb shared by every track that wants it sounds like one room, which a
//! reverb on each track does not.

use serde::{Deserialize, Serialize};

use crate::{CompSettings, DelaySettings, ReverbSettings};

/// Identifies a bus for the life of a project. Never reused (decision D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct BusId(pub u32);

crate::id::deserialize_as_key!(BusId);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bus {
    pub id: BusId,
    pub name: String,
    /// The return's level, linear.
    pub gain: f32,
    pub muted: bool,
    /// In this order, like a track's. The delay and the reverb are wet only.
    pub comp: CompSettings,
    pub delay: DelaySettings,
    pub reverb: ReverbSettings,
}

impl Bus {
    /// A new bus is a reverb return, since that is what a bus is usually
    /// made for: the room on, fully wet.
    pub fn new(id: BusId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            gain: 1.0,
            muted: false,
            comp: CompSettings::default(),
            delay: DelaySettings { mix: 1.0, ..DelaySettings::default() },
            reverb: ReverbSettings { on: true, mix: 1.0, ..ReverbSettings::default() },
        }
    }

    pub fn is_valid(&self) -> bool {
        self.gain.is_finite() && self.comp.is_valid() && self.delay.is_valid() && self.reverb.is_valid()
    }
}

/// A send's level is linear, from nothing to the track's full level.
pub const SEND: std::ops::RangeInclusive<f32> = 0.0..=1.0;
