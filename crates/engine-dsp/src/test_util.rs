//! Measurement helpers shared by the unit tests of this crate.
//!
//! They treat whatever is under test as a black box — a `FnMut(f32) -> f32` —
//! so the same bench measures a raw filter and a smoothed one.

use crate::SineOsc;

pub(crate) const SR: f32 = 48_000.0;
/// Maximally flat: -3.01 dB at the cutoff, no resonant peak.
pub(crate) const BUTTERWORTH_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

pub(crate) fn db(gain: f32) -> f32 {
    20.0 * gain.log10()
}

/// Magnitude response of `process` at `freq_hz`, measured the way it would be on
/// a bench: drive it with a sine, wait for the transient to decay, then compare
/// output and input RMS over the same window. Measuring the input rather than
/// assuming `1/sqrt(2)` cancels most of the error of a non-integer cycle count.
pub(crate) fn measured_gain(mut process: impl FnMut(f32) -> f32, freq_hz: f32) -> f32 {
    let mut osc = SineOsc::new(SR);
    // A biquad settles in a few hundred samples; this is a wide margin.
    for _ in 0..8_192 {
        process(osc.next_sample(freq_hz));
    }
    let len = ((64.0 * SR / freq_hz) as usize).max(4_096);
    let (mut sum_in, mut sum_out) = (0.0f64, 0.0f64);
    for _ in 0..len {
        let x = osc.next_sample(freq_hz);
        let y = process(x);
        sum_in += f64::from(x) * f64::from(x);
        sum_out += f64::from(y) * f64::from(y);
    }
    (sum_out / sum_in).sqrt() as f32
}

/// Largest sample-to-sample step out of `process`, driven by a `freq_hz` sine for
/// `len` samples. The first `settle` samples are skipped so that the filter's own
/// start-up transient is not counted as a modulation step.
pub(crate) fn max_step(mut process: impl FnMut(f32) -> f32, freq_hz: f32, len: usize, settle: usize) -> f32 {
    let mut osc = SineOsc::new(SR);
    let (mut prev, mut worst) = (0.0f32, 0.0f32);
    for i in 0..len {
        let y = process(osc.next_sample(freq_hz));
        if i > settle {
            worst = worst.max((y - prev).abs());
        }
        prev = y;
    }
    worst
}
