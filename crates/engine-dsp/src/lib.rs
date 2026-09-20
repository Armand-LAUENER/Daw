//! Allocation-free DSP building blocks.
//!
//! Every type here is safe to use from the audio thread: no allocation,
//! no locking, no I/O, constant time per sample.

mod biquad;
mod oscillator;
mod smoother;

pub use biquad::{Biquad, Coefficients};
pub use oscillator::SineOsc;
pub use smoother::LinearSmoother;
