use std::f64::consts::PI;

/// Lowest cutoff accepted, in Hz. Below this the filter is inaudible anyway.
pub(crate) const MIN_CUTOFF_HZ: f32 = 1.0;
/// Highest cutoff as a fraction of the sample rate. The bilinear transform maps
/// the analogue prototype onto `[0, Nyquist)`, and the coefficients degenerate as
/// the cutoff approaches Nyquist, so it is kept short of it.
const MAX_CUTOFF_RATIO: f32 = 0.49;
/// Lowest Q accepted. Q divides the RBJ `alpha` term, so it may never reach zero.
const MIN_Q: f32 = 0.01;

/// Which of the RBJ responses a set of coefficients describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterKind {
    LowPass,
    HighPass,
}

/// Biquad coefficients, already normalised by `a0`.
///
/// Computed with the RBJ cookbook formulas. The intermediate maths is done in
/// `f64` — the coefficients are a difference of terms close to each other at low
/// cutoffs, which loses too many bits in `f32` — and stored in `f32`, the format
/// the filter runs in.
///
/// `cutoff_hz` and `q` are clamped to the usable range, so any finite parameter
/// yields a stable filter. Non-finite parameters are the caller's responsibility:
/// `engine-io` rejects them at the control boundary, before they reach the DSP.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coefficients {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Coefficients {
    /// Dispatches to [`Coefficients::low_pass`] or [`Coefficients::high_pass`],
    /// for callers that carry the kind as a value.
    pub fn new(kind: FilterKind, cutoff_hz: f32, q: f32, sample_rate: f32) -> Self {
        match kind {
            FilterKind::LowPass => Self::low_pass(cutoff_hz, q, sample_rate),
            FilterKind::HighPass => Self::high_pass(cutoff_hz, q, sample_rate),
        }
    }

    /// Two-pole low-pass. `q` of `1/sqrt(2)` gives the maximally flat (Butterworth)
    /// response, -3 dB at the cutoff; higher values resonate around it.
    pub fn low_pass(cutoff_hz: f32, q: f32, sample_rate: f32) -> Self {
        let (cos_w0, alpha) = rbj_terms(cutoff_hz, q, sample_rate);
        let b = (1.0 - cos_w0) / 2.0;
        Self::normalised(b, 1.0 - cos_w0, b, 1.0 + alpha, -2.0 * cos_w0, 1.0 - alpha)
    }

    /// Two-pole high-pass, mirror of [`Coefficients::low_pass`].
    pub fn high_pass(cutoff_hz: f32, q: f32, sample_rate: f32) -> Self {
        let (cos_w0, alpha) = rbj_terms(cutoff_hz, q, sample_rate);
        let b = (1.0 + cos_w0) / 2.0;
        Self::normalised(b, -(1.0 + cos_w0), b, 1.0 + alpha, -2.0 * cos_w0, 1.0 - alpha)
    }

    /// The per-sample increment that walks `self` onto `target` in `samples`.
    ///
    /// Coefficients are recomputed only every so many samples, because each one
    /// costs a sine and a cosine; gliding between those points costs five adds
    /// and removes the staircase they would otherwise leave in the output.
    pub(crate) fn glide_to(&self, target: &Self, samples: u32) -> Self {
        let n = samples as f32;
        Self {
            b0: (target.b0 - self.b0) / n,
            b1: (target.b1 - self.b1) / n,
            b2: (target.b2 - self.b2) / n,
            a1: (target.a1 - self.a1) / n,
            a2: (target.a2 - self.a2) / n,
        }
    }

    /// Adds one step of a glide produced by [`Coefficients::glide_to`].
    #[inline]
    pub(crate) fn advance(&mut self, step: &Self) {
        self.b0 += step.b0;
        self.b1 += step.b1;
        self.b2 += step.b2;
        self.a1 += step.a1;
        self.a2 += step.a2;
    }

    fn normalised(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        let inv_a0 = 1.0 / a0;
        Self {
            b0: (b0 * inv_a0) as f32,
            b1: (b1 * inv_a0) as f32,
            b2: (b2 * inv_a0) as f32,
            a1: (a1 * inv_a0) as f32,
            a2: (a2 * inv_a0) as f32,
        }
    }
}

/// The two terms every RBJ filter is built from: `cos(w0)` and `alpha`.
fn rbj_terms(cutoff_hz: f32, q: f32, sample_rate: f32) -> (f64, f64) {
    assert!(sample_rate > 0.0, "sample rate must be positive");
    let cutoff = cutoff_hz.clamp(MIN_CUTOFF_HZ, sample_rate * MAX_CUTOFF_RATIO);
    let w0 = 2.0 * PI * f64::from(cutoff) / f64::from(sample_rate);
    let alpha = w0.sin() / (2.0 * f64::from(q.max(MIN_Q)));
    (w0.cos(), alpha)
}

/// Second-order IIR filter in transposed direct form II.
///
/// That form is chosen over direct form I for its two state variables instead of
/// four, and because it keeps the accumulated state at the output scale, which is
/// better behaved in `f32` than the input-scaled delays of direct form I.
///
/// Changing the coefficients leaves the delay line untouched, so the output step
/// it causes is proportional to the change itself — `b0` applies straight to the
/// current input, so a jump in `b0` shows up as a jump of `delta_b0 * x`. Small
/// changes are therefore inaudible, large ones are not: modulate the cutoff
/// through a smoother rather than writing a new value in one go.
#[derive(Debug, Clone)]
pub struct Biquad {
    coeffs: Coefficients,
    z1: f32,
    z2: f32,
}

impl Biquad {
    pub fn new(coeffs: Coefficients) -> Self {
        Self { coeffs, z1: 0.0, z2: 0.0 }
    }

    pub fn set_coefficients(&mut self, coeffs: Coefficients) {
        self.coeffs = coeffs;
    }

    /// Filters one sample.
    #[inline]
    pub fn process_sample(&mut self, x: f32) -> f32 {
        let c = &self.coeffs;
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y
    }

    /// Clears the delay line, as when starting playback at a new position.
    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SineOsc;
    use crate::test_util::{BUTTERWORTH_Q, SR, db, max_step, measured_gain};

    fn gain(filter: &mut Biquad, freq_hz: f32) -> f32 {
        measured_gain(|x| filter.process_sample(x), freq_hz)
    }

    #[test]
    fn low_pass_leaves_the_passband_alone() {
        let mut f = Biquad::new(Coefficients::low_pass(1_000.0, BUTTERWORTH_Q, SR));
        let gain = db(gain(&mut f, 50.0));
        assert!(gain.abs() < 0.1, "passband gain is {gain} dB, expected 0");
    }

    #[test]
    fn low_pass_is_minus_3_db_at_the_cutoff() {
        // The bilinear transform is prewarped at the cutoff, so this point is exact.
        let mut f = Biquad::new(Coefficients::low_pass(1_000.0, BUTTERWORTH_Q, SR));
        let gain = db(gain(&mut f, 1_000.0));
        assert!((gain + 3.01).abs() < 0.2, "cutoff gain is {gain} dB, expected -3.01");
    }

    #[test]
    fn low_pass_rolls_off_at_12_db_per_octave() {
        // Two poles give -12 dB/octave in the stopband. Measured an octave apart,
        // well above the cutoff but far enough below Nyquist that the frequency
        // warping of the bilinear transform stays under a few tenths of a dB.
        let coeffs = Coefficients::low_pass(500.0, BUTTERWORTH_Q, SR);
        let at_2k = db(gain(&mut Biquad::new(coeffs), 2_000.0));
        let at_4k = db(gain(&mut Biquad::new(coeffs), 4_000.0));
        let slope = at_4k - at_2k;
        assert!((slope + 12.0).abs() < 1.0, "slope is {slope} dB/octave, expected -12");
    }

    #[test]
    fn high_pass_blocks_dc() {
        let mut f = Biquad::new(Coefficients::high_pass(1_000.0, BUTTERWORTH_Q, SR));
        for _ in 0..8_192 {
            f.process_sample(1.0);
        }
        let out = f.process_sample(1.0);
        assert!(out.abs() < 1e-4, "DC leaked through at {out}");
    }

    #[test]
    fn high_pass_is_minus_3_db_at_the_cutoff() {
        let mut f = Biquad::new(Coefficients::high_pass(1_000.0, BUTTERWORTH_Q, SR));
        let gain = db(gain(&mut f, 1_000.0));
        assert!((gain + 3.01).abs() < 0.2, "cutoff gain is {gain} dB, expected -3.01");
    }

    #[test]
    fn high_pass_rolls_off_at_12_db_per_octave() {
        let coeffs = Coefficients::high_pass(4_000.0, BUTTERWORTH_Q, SR);
        let at_1k = db(gain(&mut Biquad::new(coeffs), 1_000.0));
        let at_500 = db(gain(&mut Biquad::new(coeffs), 500.0));
        let slope = at_500 - at_1k;
        assert!((slope + 12.0).abs() < 1.0, "slope is {slope} dB/octave, expected -12");
    }

    #[test]
    fn resonance_peaks_at_the_cutoff() {
        // At high Q the low-pass overshoots around the cutoff, by roughly Q itself.
        let mut f = Biquad::new(Coefficients::low_pass(1_000.0, 8.0, SR));
        let gain = db(gain(&mut f, 1_000.0));
        assert!((gain - db(8.0)).abs() < 0.5, "resonant peak is {gain} dB, expected ~18");
    }

    #[test]
    fn extreme_parameters_stay_stable() {
        // Cutoffs beyond Nyquist and a zero Q are clamped rather than producing
        // coefficients that would blow the filter up.
        for (cutoff, q) in [(0.0, 0.0), (-100.0, 1.0), (SR, 0.5), (SR * 100.0, 1e-9)] {
            let mut f = Biquad::new(Coefficients::low_pass(cutoff, q, SR));
            let mut osc = SineOsc::new(SR);
            for _ in 0..48_000 {
                let y = f.process_sample(osc.next_sample(1_000.0));
                assert!(y.is_finite() && y.abs() < 100.0, "cutoff {cutoff}, q {q}: output {y}");
            }
        }
    }

    #[test]
    fn reset_clears_the_delay_line() {
        let mut f = Biquad::new(Coefficients::low_pass(1_000.0, BUTTERWORTH_Q, SR));
        for _ in 0..1_000 {
            f.process_sample(1.0);
        }
        f.reset();
        // With an empty delay line the first output is just b0 * x.
        assert_eq!(f.process_sample(0.0), 0.0);
    }

    const PROBE_HZ: f32 = 200.0;
    const RUN_LEN: usize = 4_000;
    /// Samples to skip before measuring, so the filter's own transient is not
    /// counted as a retuning step.
    const SETTLE: usize = 1_000;

    /// Largest sample-to-sample step of the output for a 200 Hz sine, with the
    /// cutoff set to `cutoff_at(t)` at `updates` evenly spaced points, `t` running
    /// from 0 to 1 over the run.
    fn max_output_step(cutoff_at: impl Fn(f32) -> f32, updates: usize) -> f32 {
        let mut f = Biquad::new(Coefficients::low_pass(cutoff_at(0.0), BUTTERWORTH_Q, SR));
        let mut i = 0usize;
        max_step(
            |x| {
                if i.is_multiple_of(RUN_LEN / updates) {
                    let cutoff = cutoff_at(i as f32 / RUN_LEN as f32);
                    f.set_coefficients(Coefficients::low_pass(cutoff, BUTTERWORTH_Q, SR));
                }
                i += 1;
                f.process_sample(x)
            },
            PROBE_HZ,
            RUN_LEN,
            SETTLE,
        )
    }

    /// The step the signal is responsible for on its own, cutoff held still.
    fn signal_step() -> f32 {
        max_output_step(|_| 3_000.0, 1)
    }

    /// How much retuning adds on top of that, sweeping 800 Hz -> 3 kHz.
    fn retuning_step(updates: usize) -> f32 {
        max_output_step(|t| 800.0 + t * 2_200.0, updates) - signal_step()
    }

    #[test]
    fn retuning_steps_shrink_in_proportion_to_the_change() {
        // b0 multiplies the current input, so a jump in b0 steps the output by
        // delta_b0 * x. Splitting the same sweep into ten times more updates must
        // shrink that step by about ten — this is what makes smoothing work.
        let coarse = retuning_step(4);
        let fine = retuning_step(40);
        assert!(fine < coarse / 5.0, "coarse {coarse}, fine {fine}: steps did not shrink");
    }

    #[test]
    fn a_finely_swept_cutoff_is_lost_in_the_signal() {
        // Retuned every 10 samples, the sweep adds under 1% to the motion the
        // signal already has: nothing a listener could pick out as a click.
        let (added, signal) = (retuning_step(400), signal_step());
        assert!(added < signal * 0.01, "retuning adds {added}, signal moves by {signal}");
    }
}
