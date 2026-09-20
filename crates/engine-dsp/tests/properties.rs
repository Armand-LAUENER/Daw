//! Property tests: invariants that must hold for any parameter sequence,
//! not just the hand-picked values of the unit tests.

use std::f32::consts::TAU;

use engine_dsp::{Biquad, Coefficients, LinearSmoother, SineOsc};
use proptest::collection::vec;
use proptest::prelude::*;

const SR: f32 = 48_000.0;
/// Maximally flat: -3.01 dB at the cutoff, whatever the cutoff.
const BUTTERWORTH_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Magnitude response at `freq_hz`, from the output/input RMS ratio once the
/// filter's transient has decayed. Fixed lengths keep the cost per case flat.
fn gain_at(filter: &mut Biquad, freq_hz: f32) -> f32 {
    let mut osc = SineOsc::new(SR);
    for _ in 0..8_192 {
        filter.process_sample(osc.next_sample(freq_hz));
    }
    let (mut sum_in, mut sum_out) = (0.0f64, 0.0f64);
    for _ in 0..16_384 {
        let x = osc.next_sample(freq_hz);
        let y = filter.process_sample(x);
        sum_in += f64::from(x) * f64::from(x);
        sum_out += f64::from(y) * f64::from(y);
    }
    (sum_out / sum_in).sqrt() as f32
}

proptest! {
    #[test]
    fn sine_stays_within_unit_range(freqs in vec(0.0f32..=SR / 2.0, 1..32), len in 1usize..512) {
        let mut osc = SineOsc::new(SR);
        for f in freqs {
            for _ in 0..len {
                let s = osc.next_sample(f);
                prop_assert!(s.is_finite() && s.abs() <= 1.0, "sample {s} at {f} Hz");
            }
        }
    }

    #[test]
    fn sine_is_continuous_across_frequency_changes(freqs in vec(0.0f32..4_000.0, 2..32), len in 1usize..256) {
        // |sin(a + d) - sin(a)| <= d, with d the phase step of the fastest frequency.
        let max_step = TAU * freqs.iter().copied().fold(0.0, f32::max) / SR * 1.001 + 1e-6;
        let mut osc = SineOsc::new(SR);
        let mut prev = osc.next_sample(freqs[0]);
        for f in freqs {
            for _ in 0..len {
                let s = osc.next_sample(f);
                prop_assert!((s - prev).abs() <= max_step, "step {} > {max_step}", (s - prev).abs());
                prev = s;
            }
        }
    }

    #[test]
    fn smoother_lands_exactly_on_target(start in -1e4f32..1e4, target in -1e4f32..1e4, len in 0u32..2_000) {
        let mut s = LinearSmoother::new(start, len);
        s.set_target(target);
        for _ in 0..len {
            s.next_value();
        }
        prop_assert!(!s.is_ramping());
        prop_assert_eq!(s.next_value(), target);
    }

    #[test]
    fn smoother_ramp_is_monotonic_and_bounded(start in -1e4f32..1e4, target in -1e4f32..1e4, len in 1u32..2_000) {
        let (lo, hi) = (start.min(target), start.max(target));
        let eps = (hi - lo) * 1e-4 + 1e-3;
        let dir = (target - start).signum();
        let mut s = LinearSmoother::new(start, len);
        s.set_target(target);
        let mut prev = s.next_value();
        for _ in 0..len {
            let v = s.next_value();
            prop_assert!(v >= lo - eps && v <= hi + eps, "{v} outside [{lo}, {hi}]");
            prop_assert!((v - prev) * dir >= -eps, "ramp went backwards: {prev} -> {v}");
            prev = v;
        }
    }

    #[test]
    fn smoother_retargeting_never_jumps(
        start in -1e3f32..1e3,
        moves in vec((-1e3f32..1e3, 0u32..600), 1..16),
        len in 1u32..500,
    ) {
        // Every step is a fraction 1/len of a distance inside the visited range.
        let lo = moves.iter().map(|m| m.0).fold(start, f32::min);
        let hi = moves.iter().map(|m| m.0).fold(start, f32::max);
        let max_step = (hi - lo) / len as f32 * 1.001 + 1e-3;
        let mut s = LinearSmoother::new(start, len);
        let mut prev = s.next_value();
        for (target, hold) in moves {
            s.set_target(target);
            for _ in 0..hold {
                let v = s.next_value();
                prop_assert!((v - prev).abs() <= max_step, "jump {prev} -> {v}");
                prev = v;
            }
        }
    }

    #[test]
    fn biquad_never_produces_non_finite_samples(
        params in vec((1.0f32..20_000.0, 0.1f32..10.0, 0usize..64), 1..32),
        low_pass in any::<bool>(),
    ) {
        // Whatever the control thread throws at it — including cutoffs past
        // Nyquist, which the coefficients clamp — the filter must never hand the
        // device a NaN or an infinity. Note that this says nothing about level:
        // retuning abruptly injects a transient, and a filter tuned near DC holds
        // it for seconds, so repeated jumps can pile up tens of dB. Bounded output
        // is a property of *smooth* modulation, covered by the next test.
        let mut f = Biquad::new(Coefficients::low_pass(1_000.0, BUTTERWORTH_Q, SR));
        let mut osc = SineOsc::new(SR);
        for (cutoff, q, hold) in params {
            f.set_coefficients(if low_pass {
                Coefficients::low_pass(cutoff, q, SR)
            } else {
                Coefficients::high_pass(cutoff, q, SR)
            });
            for _ in 0..hold {
                let y = f.process_sample(osc.next_sample(1_000.0));
                prop_assert!(y.is_finite(), "cutoff {cutoff}, q {q}: output {y}");
            }
        }
    }

    #[test]
    fn a_smoothly_modulated_biquad_stays_bounded(
        targets in vec((20.0f32..20_000.0, 1u32..480), 1..16),
        q in 0.1f32..10.0,
    ) {
        // The intended way to modulate: the cutoff goes through a smoother and the
        // coefficients are recomputed every sample. A 1 kHz unit sine can then only
        // come out at the resonant gain, which is about Q; the bound leaves a factor
        // of three for the sweep's own transient.
        let mut cutoff = LinearSmoother::with_duration(1_000.0, 10.0, SR);
        let mut f = Biquad::new(Coefficients::low_pass(1_000.0, q, SR));
        let mut osc = SineOsc::new(SR);
        for (target, hold) in targets {
            cutoff.set_target(target);
            for _ in 0..hold {
                f.set_coefficients(Coefficients::low_pass(cutoff.next_value(), q, SR));
                let y = f.process_sample(osc.next_sample(1_000.0));
                prop_assert!(y.abs() < 3.0 * q.max(1.0), "q {q}, cutoff {target}: output {y}");
            }
        }
    }

    #[test]
    fn butterworth_is_minus_3_db_at_any_cutoff(cutoff in 200.0f32..8_000.0, low_pass in any::<bool>()) {
        // The bilinear transform is prewarped at the cutoff, so this holds across
        // the range, not just where the unit tests happen to sample it.
        let coeffs = if low_pass {
            Coefficients::low_pass(cutoff, BUTTERWORTH_Q, SR)
        } else {
            Coefficients::high_pass(cutoff, BUTTERWORTH_Q, SR)
        };
        let db = 20.0 * gain_at(&mut Biquad::new(coeffs), cutoff).log10();
        prop_assert!((db + 3.01).abs() < 0.2, "{db} dB at {cutoff} Hz, expected -3.01");
    }
}
