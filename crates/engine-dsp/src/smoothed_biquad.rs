use crate::biquad::MIN_CUTOFF_HZ;
use crate::{Biquad, Coefficients, FilterKind, LinearSmoother};

/// Samples between coefficient updates while the cutoff moves.
///
/// Recomputing coefficients costs a sine and a cosine, too much to pay on every
/// sample; holding them too long turns a sweep into audible steps. At 48 kHz, 32
/// samples is 0.67 ms, so a 10 ms ramp is cut into fifteen updates — small enough
/// that each step disappears into the signal, as the tests measure.
pub const DEFAULT_UPDATE_INTERVAL: u32 = 32;

/// A [`Biquad`] whose cutoff can be modulated from the control thread without
/// zipper noise.
///
/// The cutoff is ramped by a [`LinearSmoother`] instead of jumping, and the
/// coefficients are refreshed every [`DEFAULT_UPDATE_INTERVAL`] samples rather
/// than on every one. Once the ramp has landed on its target the updates stop,
/// so a filter nobody is touching costs exactly one biquad per sample.
///
/// The ramp runs in octaves, not in hertz. A linear sweep in hertz is not linear
/// in anything the filter cares about: going from 20 kHz to 20 Hz, it would spend
/// nine tenths of the ramp above 1 kHz and cross the whole audible range in the
/// last fraction of a millisecond — a jump in disguise. `b0` changes in proportion
/// to itself per octave, so equal steps in `log2(cutoff)` are equal relative
/// changes, which is also how a filter is modulated musically.
#[derive(Debug, Clone)]
pub struct SmoothedBiquad {
    filter: Biquad,
    /// Ramps `log2(cutoff in Hz)`, see the note on the type.
    cutoff: LinearSmoother,
    /// Where the current glide is heading, and the per-sample step to get there.
    target: Coefficients,
    step: Coefficients,
    current: Coefficients,
    /// Samples left in the glide towards `target`.
    gliding: u32,
    kind: FilterKind,
    q: f32,
    sample_rate: f32,
    update_interval: u32,
    until_update: u32,
    /// Whether the cutoff ramp still has ground to cover.
    stale: bool,
}

impl SmoothedBiquad {
    /// `ramp_ms` is how long the cutoff takes to reach a new target.
    pub fn new(kind: FilterKind, cutoff_hz: f32, q: f32, sample_rate: f32, ramp_ms: f32) -> Self {
        let coeffs = Coefficients::new(kind, cutoff_hz, q, sample_rate);
        Self {
            filter: Biquad::new(coeffs),
            cutoff: LinearSmoother::with_duration(octaves(cutoff_hz), ramp_ms, sample_rate),
            target: coeffs,
            step: coeffs.glide_to(&coeffs, 1),
            current: coeffs,
            gliding: 0,
            kind,
            q,
            sample_rate,
            update_interval: DEFAULT_UPDATE_INTERVAL,
            until_update: 0,
            stale: false,
        }
    }

    /// Overrides [`DEFAULT_UPDATE_INTERVAL`], to trade CPU against step size.
    pub fn with_update_interval(mut self, samples: u32) -> Self {
        assert!(samples > 0, "update interval must be at least one sample");
        self.update_interval = samples;
        self
    }

    /// Starts a ramp towards `hz`. Cheap enough to call on every control message.
    pub fn set_cutoff(&mut self, hz: f32) {
        self.cutoff.set_target(octaves(hz));
        self.stale = true;
    }

    /// Q is not ramped: it shifts the resonance rather than the whole response,
    /// and the audible artefact it causes is far smaller than a cutoff jump.
    /// The change lands at the next coefficient update.
    pub fn set_q(&mut self, q: f32) {
        self.q = q;
        self.stale = true;
    }

    /// The cutoff the ramp is heading for, in Hz.
    pub fn cutoff(&self) -> f32 {
        self.cutoff.target().exp2()
    }

    /// Filters one sample, advancing the cutoff ramp with it.
    #[inline]
    pub fn process_sample(&mut self, x: f32) -> f32 {
        if self.stale {
            let cutoff = self.cutoff.next_value();
            if self.until_update == 0 {
                self.target = Coefficients::new(self.kind, cutoff.exp2(), self.q, self.sample_rate);
                self.step = self.current.glide_to(&self.target, self.update_interval);
                self.gliding = self.update_interval;
                self.until_update = self.update_interval;
                // Landing exactly on the target is what lets the updates stop:
                // `cutoff` is only the target once the smoother has finished.
                self.stale = cutoff != self.cutoff.target();
            }
            self.until_update -= 1;
        }
        if self.gliding > 0 {
            self.gliding -= 1;
            // The last step snaps, so that a settled filter holds exactly the
            // coefficients of its target rather than an accumulation of steps.
            if self.gliding == 0 {
                self.current = self.target;
            } else {
                self.current.advance(&self.step);
            }
            self.filter.set_coefficients(self.current);
        }
        self.filter.process_sample(x)
    }

    /// Clears the delay line and snaps the cutoff to its target, as when starting
    /// playback at a new position: there is no previous audio to glide away from.
    pub fn reset(&mut self) {
        let target = self.cutoff.target();
        self.current = Coefficients::new(self.kind, target.exp2(), self.q, self.sample_rate);
        self.target = self.current;
        self.filter.reset();
        self.filter.set_coefficients(self.current);
        self.cutoff = LinearSmoother::new(target, 0);
        self.until_update = 0;
        self.gliding = 0;
        self.stale = false;
    }
}

/// Hertz to octaves, the domain the cutoff is ramped in. The floor matches the
/// one the coefficients apply, and keeps `log2` away from zero and negatives;
/// it also turns a NaN into a usable value rather than poisoning the ramp.
fn octaves(hz: f32) -> f32 {
    hz.max(MIN_CUTOFF_HZ).log2()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SineOsc;
    use crate::test_util::{BUTTERWORTH_Q, SR, db, max_step, measured_gain};

    const PROBE_HZ: f32 = 200.0;
    const RAMP_MS: f32 = 10.0;

    fn low_pass(cutoff_hz: f32, ramp_ms: f32) -> SmoothedBiquad {
        SmoothedBiquad::new(FilterKind::LowPass, cutoff_hz, BUTTERWORTH_Q, SR, ramp_ms)
    }

    /// Largest output step while the cutoff is moved from 800 Hz to 3 kHz halfway
    /// through, with coefficients refreshed every `interval` samples. A `ramp_ms`
    /// of zero makes the cutoff jump in one go instead of gliding.
    fn step_of_a_sweep(interval: u32, ramp_ms: f32) -> f32 {
        let mut f = low_pass(800.0, ramp_ms).with_update_interval(interval);
        let mut i = 0usize;
        max_step(
            |x| {
                if i == 2_000 {
                    f.set_cutoff(3_000.0);
                }
                i += 1;
                f.process_sample(x)
            },
            PROBE_HZ,
            4_000,
            1_000,
        )
    }

    /// A 200 Hz unit sine moves by this much per sample on its own. Modulation
    /// that stays under it cannot be picked out of the signal.
    fn signal_step() -> f32 {
        std::f32::consts::TAU * PROBE_HZ / SR
    }

    #[test]
    fn ramping_the_cutoff_still_beats_jumping_it() {
        // The coefficient glide already softens a cutoff written in one go, so
        // this is no longer the difference between a click and nothing. What the
        // ramp adds is the last stretch: only with it does the sweep disappear
        // into the signal.
        let jumped = step_of_a_sweep(DEFAULT_UPDATE_INTERVAL, 0.0);
        let ramped = step_of_a_sweep(DEFAULT_UPDATE_INTERVAL, RAMP_MS);
        assert!(ramped < jumped, "jumped {jumped}, ramped {ramped}");
        assert!(jumped > signal_step(), "a jump should still show: {jumped}");
    }

    #[test]
    fn a_ramped_sweep_is_lost_in_the_signal() {
        // Ramped, the sweep must not add anything a listener could pick out.
        let worst = step_of_a_sweep(DEFAULT_UPDATE_INTERVAL, RAMP_MS);
        assert!(worst < signal_step() * 1.05, "worst step {worst}, signal moves by {}", signal_step());
    }

    #[test]
    fn the_glide_takes_the_sting_out_of_the_update_interval() {
        // Without interpolation the interval was a direct trade of CPU against
        // step size. Gliding between updates takes that away: from 8 samples to
        // 512, a sixty-fold change, the worst step barely moves.
        let short = step_of_a_sweep(8, RAMP_MS);
        let long = step_of_a_sweep(512, RAMP_MS);
        assert!(long < short * 1.5, "interval 8 stepped {short}, interval 512 stepped {long}");
    }

    #[test]
    fn the_ramp_lands_on_the_requested_cutoff() {
        // After the ramp, the response must be the one a filter built at 3 kHz has.
        let mut f = low_pass(800.0, RAMP_MS);
        f.set_cutoff(3_000.0);
        for _ in 0..(RAMP_MS * 1e-3 * SR) as usize + DEFAULT_UPDATE_INTERVAL as usize {
            f.process_sample(0.0);
        }
        let at_cutoff = db(measured_gain(|x| f.process_sample(x), 3_000.0));
        assert!((at_cutoff + 3.01).abs() < 0.2, "landed on {at_cutoff} dB, expected -3.01");
    }

    #[test]
    fn a_settled_filter_matches_a_plain_biquad_sample_for_sample() {
        // Once the ramp has landed the updates stop, so the output must be exactly
        // what the raw filter produces — no residual drift from the smoother.
        let mut smoothed = low_pass(1_000.0, RAMP_MS);
        let mut plain = Biquad::new(Coefficients::low_pass(1_000.0, BUTTERWORTH_Q, SR));
        let mut osc = SineOsc::new(SR);
        for i in 0..10_000 {
            let x = osc.next_sample(PROBE_HZ);
            assert_eq!(smoothed.process_sample(x), plain.process_sample(x), "diverged at sample {i}");
        }
    }

    #[test]
    fn reset_snaps_to_the_target_instead_of_gliding() {
        let mut f = low_pass(200.0, RAMP_MS);
        f.set_cutoff(5_000.0);
        f.process_sample(0.0);
        f.reset();
        // The cutoff is carried in octaves, so it comes back a few ulps off the
        // hertz it went in as — far below anything audible, but not bit-identical.
        assert!((f.cutoff() - 5_000.0).abs() < 0.01, "cutoff came back as {}", f.cutoff());
        let mut plain = Biquad::new(Coefficients::low_pass(f.cutoff(), BUTTERWORTH_Q, SR));
        let mut osc = SineOsc::new(SR);
        for i in 0..1_000 {
            let x = osc.next_sample(PROBE_HZ);
            assert_eq!(f.process_sample(x), plain.process_sample(x), "diverged at sample {i}");
        }
    }

    #[test]
    fn a_full_range_sweep_stays_bounded() {
        // The pathological case increment 1 found — a filter driven to the bottom
        // of its range and back — is harmless once the cutoff is ramped.
        let mut f = low_pass(20_000.0, RAMP_MS);
        let mut osc = SineOsc::new(SR);
        for i in 0..200_000 {
            if i % 2_000 == 0 {
                f.set_cutoff(if (i / 2_000) % 2 == 0 { 20.0 } else { 20_000.0 });
            }
            let y = f.process_sample(osc.next_sample(1_000.0));
            assert!(y.is_finite() && y.abs() < 4.0, "output {y} at sample {i}");
        }
    }
}
