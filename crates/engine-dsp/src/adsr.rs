/// Where an [`Adsr`] is in its run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Silent and finished. A voice pool may take this one back.
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

/// Attack–decay–sustain–release envelope, in linear segments.
///
/// Linear rather than exponential, deliberately. An exponential segment is the
/// more analogue shape, but a one-pole decay never actually reaches zero: it
/// needs a threshold to stop at, and a voice left sitting at 1e-30 is a
/// denormal, which this engine has already measured the cost of. Linear
/// segments land on their endpoints exactly and in exactly the time asked for,
/// so "this voice is finished" is a fact rather than a guess. Curve shaping can
/// be layered on later without changing that.
///
/// Every transition starts from the level the envelope is actually at, so a
/// note released during its attack, or retriggered while still sounding, moves
/// from where it was instead of jumping. A jump is a click.
#[derive(Debug, Clone)]
pub struct Adsr {
    stage: Stage,
    level: f32,
    sustain: f32,
    attack_samples: f32,
    decay_samples: f32,
    release_samples: f32,
    /// Fixed when the release begins, since it runs from wherever the level is.
    release_step: f32,
}

impl Adsr {
    /// Times are in seconds; `sustain` is a level in `0..=1`.
    ///
    /// A time shorter than one sample applies at once, which is what makes a
    /// zero-attack click of a percussive patch possible without a special case.
    pub fn new(attack_s: f32, decay_s: f32, sustain: f32, release_s: f32, sample_rate: f32) -> Self {
        Self {
            stage: Stage::Idle,
            level: 0.0,
            sustain: sustain.clamp(0.0, 1.0),
            attack_samples: samples(attack_s, sample_rate),
            decay_samples: samples(decay_s, sample_rate),
            release_samples: samples(release_s, sample_rate),
            release_step: 0.0,
        }
    }

    /// Starts, or restarts, the envelope.
    ///
    /// Retriggering does not reset the level to zero: the attack climbs from
    /// wherever the envelope was, at the same rate, so a repeated note is
    /// shorter rather than clicking.
    pub fn note_on(&mut self) {
        self.stage = Stage::Attack;
    }

    /// Releases the note, from whatever level it has reached.
    pub fn note_off(&mut self) {
        if self.stage == Stage::Idle {
            return;
        }
        self.release_step = if self.release_samples < 1.0 { self.level } else { self.level / self.release_samples };
        self.stage = Stage::Release;
    }

    /// Returns the current level, then advances by one sample.
    ///
    /// Same shape as the smoothers: read, then step.
    #[inline]
    pub fn next_value(&mut self) -> f32 {
        let out = self.level;
        match self.stage {
            Stage::Idle | Stage::Sustain => {}
            Stage::Attack => {
                self.level += 1.0 / self.attack_samples;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                self.level -= (1.0 - self.sustain) / self.decay_samples;
                if self.level <= self.sustain {
                    self.level = self.sustain;
                    // Nothing to sustain means the note is over when its decay
                    // is, so the voice can be handed back instead of being held
                    // by something inaudible.
                    self.stage = if self.sustain > 0.0 { Stage::Sustain } else { Stage::Idle };
                }
            }
            Stage::Release => {
                self.level -= self.release_step;
                if self.level <= 0.0 {
                    self.level = 0.0;
                    self.stage = Stage::Idle;
                }
            }
        }
        out
    }

    /// Whether this envelope is still making sound, and so still owns its voice.
    pub fn is_active(&self) -> bool {
        self.stage != Stage::Idle
    }

    /// Whether the note has been released and is fading out. A voice pool
    /// steals one of these before it steals a held note.
    pub fn is_releasing(&self) -> bool {
        self.stage == Stage::Release
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    pub fn level(&self) -> f32 {
        self.level
    }

    pub fn set_sustain(&mut self, sustain: f32) {
        self.sustain = sustain.clamp(0.0, 1.0);
    }

    /// Silences the envelope at once, for a voice being reused from scratch.
    pub fn reset(&mut self) {
        self.stage = Stage::Idle;
        self.level = 0.0;
    }
}

/// A duration in samples, never below one, so a step is never a division by zero
/// and a stage shorter than a sample simply happens in that sample.
fn samples(seconds: f32, sample_rate: f32) -> f32 {
    (seconds.max(0.0) * sample_rate).round().max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn run(env: &mut Adsr, n: usize) -> Vec<f32> {
        (0..n).map(|_| env.next_value()).collect()
    }

    /// Largest sample-to-sample step in a run: what a click would show up as.
    fn worst_step(values: &[f32]) -> f32 {
        values.windows(2).fold(0.0f32, |worst, w| worst.max((w[1] - w[0]).abs()))
    }

    #[test]
    fn an_idle_envelope_is_silent() {
        let mut env = Adsr::new(0.01, 0.01, 0.5, 0.01, SR);
        assert!(!env.is_active());
        assert!(run(&mut env, 64).iter().all(|v| *v == 0.0));
    }

    #[test]
    fn the_attack_reaches_full_scale_in_the_time_asked_for() {
        let mut env = Adsr::new(0.01, 1.0, 1.0, 0.01, SR);
        env.note_on();
        let attack = (0.01 * SR) as usize;
        let values = run(&mut env, attack + 1);
        assert_eq!(values[attack], 1.0, "the attack did not land on full scale");
        assert!(values[attack - 1] < 1.0, "it arrived early");
    }

    #[test]
    fn the_decay_lands_on_the_sustain_level() {
        let mut env = Adsr::new(0.0, 0.01, 0.3, 0.01, SR);
        env.note_on();
        run(&mut env, (0.01 * SR) as usize + 8);
        assert_eq!(env.stage(), Stage::Sustain);
        assert!((env.level() - 0.3).abs() < 1e-6, "level {}", env.level());
    }

    #[test]
    fn the_sustain_holds_until_released() {
        let mut env = Adsr::new(0.0, 0.001, 0.4, 0.01, SR);
        env.note_on();
        run(&mut env, 1_000);
        let held = run(&mut env, 48_000);
        assert!(held.iter().all(|v| (*v - 0.4).abs() < 1e-6), "the sustain drifted");
        assert!(env.is_active());
    }

    #[test]
    fn the_release_lands_exactly_on_zero_and_frees_the_voice() {
        // The whole reason for linear segments. A one-pole would leave the
        // level a hair above zero for ever, so "finished" would be a threshold
        // and a silent voice would sit there generating denormals.
        let mut env = Adsr::new(0.0, 0.0, 1.0, 0.01, SR);
        env.note_on();
        run(&mut env, 8);
        env.note_off();
        run(&mut env, (0.01 * SR) as usize + 2);
        assert_eq!(env.level(), 0.0);
        assert_eq!(env.stage(), Stage::Idle);
        assert!(!env.is_active(), "the voice was never handed back");
    }

    #[test]
    fn releasing_mid_attack_carries_on_from_where_it_was() {
        // The classic envelope bug: release from the sustain level rather than
        // the current one, so a key tapped during a slow attack jumps up before
        // it fades. That is a click, not a note.
        let mut env = Adsr::new(0.5, 0.1, 0.8, 0.1, SR);
        env.note_on();
        run(&mut env, 2_000);
        // The level itself, not the last value read: `next_value` reports then
        // advances, so the last reading is already one step behind.
        let at_release = env.level();
        assert!(at_release > 0.0 && at_release < 0.5, "expected to be mid-attack, at {at_release}");

        env.note_off();
        let falling = run(&mut env, 8_000);
        assert_eq!(falling[0], at_release, "the release did not start from the level it was at");
        assert!(worst_step(&falling) < 1e-3, "the release stepped: {}", worst_step(&falling));
    }

    #[test]
    fn retriggering_climbs_from_where_it_was() {
        let attack = 0.02f32;
        let mut env = Adsr::new(attack, 0.05, 0.5, 0.2, SR);
        // The attack's own rate: anything steeper than the segment it is
        // climbing would be the level jumping rather than ramping.
        let steepest = 1.0 / (attack * SR) * 1.001;
        env.note_on();
        run(&mut env, 5_000);
        env.note_off();
        run(&mut env, 2_000);
        let at_retrigger = env.level();

        env.note_on();
        let again = run(&mut env, 2_000);
        assert_eq!(again[0], at_retrigger, "the attack did not climb from the level it was at");
        assert!(worst_step(&again) <= steepest, "the retrigger stepped by {}", worst_step(&again));
    }

    #[test]
    fn a_whole_note_never_steps_more_than_its_steepest_segment() {
        let (attack, decay, release) = (0.005f32, 0.02f32, 0.05f32);
        let mut env = Adsr::new(attack, decay, 0.6, release, SR);
        // The attack crosses full scale in the shortest time, so it is the
        // steepest thing the envelope can legitimately do.
        let steepest = 1.0 / (attack * SR) * 1.001;

        env.note_on();
        let mut values = run(&mut env, 10_000);
        env.note_off();
        values.extend(run(&mut env, 10_000));
        assert!(worst_step(&values) <= steepest, "worst step {}", worst_step(&values));
        assert!(!env.is_active());
    }

    #[test]
    fn a_zero_length_attack_applies_within_a_sample() {
        let mut env = Adsr::new(0.0, 1.0, 1.0, 0.01, SR);
        env.note_on();
        let values = run(&mut env, 3);
        assert_eq!(values[1], 1.0, "{values:?}");
    }

    #[test]
    fn nothing_to_sustain_means_the_note_ends_with_its_decay() {
        // Otherwise a pluck would hold its voice, silent, until something
        // released it — and a pool would run out of voices playing nothing.
        let mut env = Adsr::new(0.0, 0.01, 0.0, 1.0, SR);
        env.note_on();
        run(&mut env, (0.01 * SR) as usize + 4);
        assert_eq!(env.stage(), Stage::Idle);
        assert!(!env.is_active());
    }

    #[test]
    fn releasing_something_idle_does_nothing() {
        let mut env = Adsr::new(0.01, 0.01, 0.5, 0.01, SR);
        env.note_off();
        assert_eq!(env.stage(), Stage::Idle);
        assert!(run(&mut env, 16).iter().all(|v| *v == 0.0));
    }

    #[test]
    fn a_sustain_outside_the_range_is_brought_back_into_it() {
        assert_eq!(Adsr::new(0.0, 0.0, 5.0, 0.0, SR).sustain, 1.0);
        assert_eq!(Adsr::new(0.0, 0.0, -1.0, 0.0, SR).sustain, 0.0);
    }

    #[test]
    fn the_envelope_never_leaves_the_unit_range() {
        let mut env = Adsr::new(0.001, 0.002, 0.7, 0.003, SR);
        for round in 0..50 {
            if round % 2 == 0 {
                env.note_on();
            } else {
                env.note_off();
            }
            assert!(run(&mut env, 97).iter().all(|v| (0.0..=1.0).contains(v)), "left the range in round {round}");
        }
    }
}
