use std::f64::consts::TAU;

/// Phase-accumulator sine oscillator.
///
/// The phase is kept normalised in `[0, 1)` as an `f64` so that it stays
/// precise over hours of playback. Because the phase is continuous, the
/// frequency can change between any two samples without a discontinuity
/// in the waveform.
#[derive(Debug, Clone)]
pub struct SineOsc {
    phase: f64,
    inv_sample_rate: f64,
}

impl SineOsc {
    pub fn new(sample_rate: f32) -> Self {
        assert!(sample_rate > 0.0, "sample rate must be positive");
        Self {
            phase: 0.0,
            inv_sample_rate: 1.0 / f64::from(sample_rate),
        }
    }

    /// Returns the current sample, then advances the phase by one sample at `freq_hz`.
    #[inline]
    pub fn next_sample(&mut self, freq_hz: f32) -> f32 {
        let out = (self.phase * TAU).sin() as f32;
        self.phase += f64::from(freq_hz) * self.inv_sample_rate;
        self.phase -= self.phase.floor();
        out
    }

    pub fn reset(&mut self) {
        self.phase = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn rising_zero_crossings(samples: &[f32]) -> usize {
        samples.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count()
    }

    #[test]
    fn produces_requested_frequency() {
        let mut osc = SineOsc::new(SR);
        let samples: Vec<f32> = (0..SR as usize).map(|_| osc.next_sample(440.0)).collect();
        let crossings = rising_zero_crossings(&samples);
        assert!((439..=441).contains(&crossings), "got {crossings} cycles in 1 s");
    }

    #[test]
    fn stays_within_unit_amplitude() {
        let mut osc = SineOsc::new(SR);
        assert!((0..100_000).all(|_| osc.next_sample(1234.5).abs() <= 1.0));
    }

    #[test]
    fn frequency_jump_keeps_waveform_continuous() {
        // A continuous sine at f has a max sample-to-sample step of 2*pi*f/sr.
        // After a jump 220 -> 880 Hz, no step may exceed the bound for 880 Hz.
        let mut osc = SineOsc::new(SR);
        let mut prev = osc.next_sample(220.0);
        let max_step = std::f32::consts::TAU * 880.0 / SR;
        for i in 0..10_000 {
            let freq = if i < 5_000 { 220.0 } else { 880.0 };
            let s = osc.next_sample(freq);
            assert!((s - prev).abs() <= max_step * 1.001, "discontinuity at sample {i}");
            prev = s;
        }
    }

    #[test]
    fn phase_does_not_drift_over_long_runs() {
        // 30 minutes at 48 kHz: a 1 Hz sine must still be at phase 0 on whole seconds.
        let mut osc = SineOsc::new(SR);
        for _ in 0..(SR as u64 * 60 * 30) {
            osc.next_sample(1.0);
        }
        assert!(osc.phase < 1e-6 || osc.phase > 1.0 - 1e-6, "phase = {}", osc.phase);
    }
}
