//! Allocation-free DSP building blocks.
//!
//! Every type here is safe to use from the audio thread: no allocation,
//! no locking, no I/O, constant time per sample.

mod adsr;
mod biquad;
mod oscillator;
mod smoothed_biquad;
mod smoother;
#[cfg(test)]
mod test_util;

pub use adsr::{Adsr, Stage};
pub use biquad::{Biquad, Coefficients, FilterKind};
pub use oscillator::{Osc, SineOsc, Waveform};
pub use smoothed_biquad::{DEFAULT_UPDATE_INTERVAL, SmoothedBiquad};
pub use smoother::LinearSmoother;
