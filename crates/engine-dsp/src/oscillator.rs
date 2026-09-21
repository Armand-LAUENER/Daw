use std::f64::consts::TAU;

/// The shape an [`Osc`] produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waveform {
    Sine,
    Saw,
    Square,
}

/// Phase-accumulator oscillator.
///
/// The phase is kept normalised in `[0, 1)` as an `f64` so that it stays
/// precise over hours of playback. Because the phase is continuous, the
/// frequency can change between any two samples without a discontinuity
/// in the waveform.
///
/// Saw and square are band-limited with PolyBLEP. Taking the shapes literally
/// is cheaper but wrong: a jump between two samples carries harmonics past
/// Nyquist, which fold back down as tones that are not related to the note.
/// A naive saw at 5 kHz puts energy at 3 kHz, where it has no business being,
/// and no filter downstream can separate that from the note afterwards.
#[derive(Debug, Clone)]
pub struct Osc {
    phase: f64,
    inv_sample_rate: f64,
}

impl Osc {
    pub fn new(sample_rate: f32) -> Self {
        assert!(sample_rate > 0.0, "sample rate must be positive");
        Self { phase: 0.0, inv_sample_rate: 1.0 / f64::from(sample_rate) }
    }

    /// Returns the current sample, then advances the phase by one sample at `freq_hz`.
    #[inline]
    pub fn next_sample(&mut self, freq_hz: f32, shape: Waveform) -> f32 {
        let dt = f64::from(freq_hz) * self.inv_sample_rate;
        let t = self.phase;
        let out = match shape {
            Waveform::Sine => (t * TAU).sin(),
            // The naive ramp, minus the correction for the step at the wrap.
            Waveform::Saw => 2.0 * t - 1.0 - poly_blep(t, dt),
            // Two steps per cycle, half a turn apart: one up, one down.
            Waveform::Square => {
                let naive = if t < 0.5 { 1.0 } else { -1.0 };
                naive + poly_blep(t, dt) - poly_blep((t + 0.5).fract(), dt)
            }
        };
        self.phase += dt;
        self.phase -= self.phase.floor();
        out as f32
    }

    pub fn reset(&mut self) {
        self.phase = 0.0;
    }
}

/// The band-limited correction for a step, as a function of how far the phase
/// is from it.
///
/// A step between two samples is infinitely sharp, which no sample rate can
/// carry. This subtracts the difference between that step and one that fits,
/// over the one sample either side of the discontinuity — cheap, and enough to
/// take the worst of the folding away.
fn poly_blep(t: f64, dt: f64) -> f64 {
    if dt <= 0.0 {
        return 0.0;
    }
    if t < dt {
        // Just after the step.
        let t = t / dt;
        t + t - t * t - 1.0
    } else if t > 1.0 - dt {
        // Just before the next one.
        let t = (t - 1.0) / dt;
        t * t + t + t + 1.0
    } else {
        0.0
    }
}

/// Sine oscillator, the plain case of [`Osc`].
#[derive(Debug, Clone)]
pub struct SineOsc(Osc);

impl SineOsc {
    pub fn new(sample_rate: f32) -> Self {
        Self(Osc::new(sample_rate))
    }

    /// Returns the current sample, then advances the phase by one sample at `freq_hz`.
    #[inline]
    pub fn next_sample(&mut self, freq_hz: f32) -> f32 {
        self.0.next_sample(freq_hz, Waveform::Sine)
    }

    pub fn reset(&mut self) {
        self.0.reset();
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
        let phase = osc.0.phase;
        assert!(phase < 1e-6 || phase > 1.0 - 1e-6, "phase = {phase}");
    }

    /// Magnitude of `samples` at `freq_hz`, by correlating against a sine and a
    /// cosine there. A whole DFT would answer the same question at every
    /// frequency; this asks it at the one that matters.
    fn magnitude_at(samples: &[f32], freq_hz: f32) -> f32 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, s) in samples.iter().enumerate() {
            let angle = std::f64::consts::TAU * f64::from(freq_hz) * i as f64 / f64::from(SR);
            re += f64::from(*s) * angle.cos();
            im += f64::from(*s) * angle.sin();
        }
        (2.0 * (re * re + im * im).sqrt() / samples.len() as f64) as f32
    }

    #[test]
    fn a_saw_sweeps_the_whole_range() {
        let mut osc = Osc::new(SR);
        let samples: Vec<f32> = (0..SR as usize / 100).map(|_| osc.next_sample(100.0, Waveform::Saw)).collect();
        let (lo, hi) = samples.iter().fold((f32::MAX, f32::MIN), |(l, h), s| (l.min(*s), h.max(*s)));
        assert!(lo < -0.95 && hi > 0.95, "saw spans {lo}..{hi}");
    }

    #[test]
    fn a_square_sits_at_its_two_levels() {
        let mut osc = Osc::new(SR);
        // Away from the two steps, where the correction is zero, it is flat.
        let samples: Vec<f32> = (0..480).map(|_| osc.next_sample(100.0, Waveform::Square)).collect();
        let flat = &samples[20..200];
        assert!(flat.iter().all(|s| (*s - 1.0).abs() < 1e-6), "the high half is not flat");
    }

    #[test]
    fn a_saw_keeps_its_fundamental() {
        // Band-limiting must remove what is folding back, not the note.
        let mut osc = Osc::new(SR);
        let samples: Vec<f32> = (0..48_000).map(|_| osc.next_sample(5_000.0, Waveform::Saw)).collect();
        let fundamental = magnitude_at(&samples, 5_000.0);
        // A saw's fundamental is 2/pi of its amplitude.
        assert!((fundamental - 2.0 / std::f32::consts::PI).abs() < 0.05, "fundamental {fundamental}");
    }

    #[test]
    fn band_limiting_takes_the_folded_harmonics_down() {
        // A 5 kHz saw's ninth harmonic lands at 45 kHz, which at 48 kHz folds
        // back to 3 kHz — a frequency no harmonic of 5 kHz occupies, so
        // whatever is there arrived by aliasing and nothing else.
        let mut naive_phase = 0.0f64;
        let naive: Vec<f32> = (0..48_000)
            .map(|_| {
                let out = 2.0 * naive_phase - 1.0;
                naive_phase = (naive_phase + 5_000.0 / f64::from(SR)).fract();
                out as f32
            })
            .collect();

        let mut osc = Osc::new(SR);
        let limited: Vec<f32> = (0..48_000).map(|_| osc.next_sample(5_000.0, Waveform::Saw)).collect();

        let (before, after) = (magnitude_at(&naive, 3_000.0), magnitude_at(&limited, 3_000.0));
        let improvement = 20.0 * (before / after).log10();
        assert!(improvement > 6.0, "only {improvement:.1} dB down: {before} -> {after}");
    }

    #[test]
    fn every_shape_stays_within_unit_amplitude() {
        // PolyBLEP overshoots slightly at a step; it must not run away.
        for shape in [Waveform::Sine, Waveform::Saw, Waveform::Square] {
            let mut osc = Osc::new(SR);
            let worst = (0..100_000).fold(0.0f32, |w, _| w.max(osc.next_sample(1_234.5, shape).abs()));
            assert!(worst <= 1.2, "{shape:?} reached {worst}");
        }
    }
}
