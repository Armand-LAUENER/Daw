//! Property tests: invariants that must hold for any parameter sequence,
//! not just the hand-picked values of the unit tests.

use std::f32::consts::TAU;

use engine_dsp::{LinearSmoother, SineOsc};
use proptest::collection::vec;
use proptest::prelude::*;

const SR: f32 = 48_000.0;

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
}
