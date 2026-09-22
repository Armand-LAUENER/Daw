//! A compressor: turns down what goes over a threshold, by a ratio.
//!
//! Feed-forward: the level is measured at the input, turned into a gain
//! reduction in decibels, and that reduction — not the level — is smoothed by
//! the attack and release times. Smoothing the reduction rather than the level
//! is what keeps the release from pumping differently at every ratio.
//!
//! The channels are linked: one reduction, taken from the loudest channel, is
//! applied to all of them, so a compressed stereo image does not lean towards
//! the quieter side.

use crate::LinearSmoother;

/// Width of the soft knee, in dB: the ratio eases in over this range around
/// the threshold instead of switching on at it.
const KNEE_DB: f32 = 6.0;
/// Below this a level is silence, and its decibels would be minus infinity.
const FLOOR_DB: f32 = -120.0;
/// How fast the level detector lets go of a peak, in milliseconds. Short
/// enough to follow a phrase, long enough not to fall to zero between the
/// crests of a low note — which would make the reduction ripple with the
/// waveform instead of following its level.
const DETECTOR_RELEASE_MS: f32 = 10.0;

/// How a compressor is set.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompressorSettings {
    pub threshold_db: f32,
    /// 1 does nothing; 20 is close to limiting.
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    /// Added after, to win back the level the compression took.
    pub makeup_db: f32,
}

/// How much a level over the threshold is reduced by, in dB, with a soft knee.
pub fn reduction_db(level_db: f32, threshold_db: f32, ratio: f32) -> f32 {
    let over = level_db - threshold_db;
    let slope = 1.0 - 1.0 / ratio.max(1.0);
    if over <= -KNEE_DB / 2.0 {
        0.0
    } else if over >= KNEE_DB / 2.0 {
        over * slope
    } else {
        // Quadratic through the knee: continuous in level and in slope.
        let into = over + KNEE_DB / 2.0;
        slope * into * into / (2.0 * KNEE_DB)
    }
}

#[derive(Debug, Clone)]
pub struct Compressor {
    threshold_db: f32,
    ratio: f32,
    attack: f32,
    release: f32,
    makeup: LinearSmoother,
    /// The reduction being applied, in dB, after attack and release.
    reduction: f32,
    /// The input's level: takes a peak at once, lets it go over
    /// [`DETECTOR_RELEASE_MS`].
    envelope: f32,
    detector: f32,
    sample_rate: f32,
}

/// The per-sample coefficient of a one-pole smoother reaching 63 % in `ms`.
fn coefficient(ms: f32, sample_rate: f32) -> f32 {
    (-1.0 / (ms.max(0.01) * 0.001 * sample_rate)).exp()
}

impl Compressor {
    pub fn new(settings: CompressorSettings, sample_rate: f32) -> Self {
        Self {
            threshold_db: settings.threshold_db,
            ratio: settings.ratio,
            attack: coefficient(settings.attack_ms, sample_rate),
            release: coefficient(settings.release_ms, sample_rate),
            makeup: LinearSmoother::with_duration(settings.makeup_db, 20.0, sample_rate),
            reduction: 0.0,
            envelope: 0.0,
            detector: coefficient(DETECTOR_RELEASE_MS, sample_rate),
            sample_rate,
        }
    }

    pub fn set_threshold(&mut self, db: f32) {
        self.threshold_db = db;
    }

    pub fn set_ratio(&mut self, ratio: f32) {
        self.ratio = ratio;
    }

    pub fn set_attack(&mut self, ms: f32) {
        self.attack = coefficient(ms, self.sample_rate);
    }

    pub fn set_release(&mut self, ms: f32) {
        self.release = coefficient(ms, self.sample_rate);
    }

    /// Glided, since a jump in output level is a click.
    pub fn set_makeup(&mut self, db: f32) {
        self.makeup.set_target(db);
    }

    /// The reduction currently applied, in dB, for a meter.
    pub fn reduction(&self) -> f32 {
        self.reduction
    }

    /// The gain for one frame whose loudest channel has absolute value `peak`.
    #[inline]
    pub fn gain_for(&mut self, peak: f32) -> f32 {
        self.envelope = peak.max(self.envelope * self.detector);
        let level = if self.envelope > 1e-6 { (20.0 * self.envelope.log10()).max(FLOOR_DB) } else { FLOOR_DB };
        let wanted = reduction_db(level, self.threshold_db, self.ratio);
        // Attack while the reduction grows, release while it shrinks.
        let k = if wanted > self.reduction { self.attack } else { self.release };
        self.reduction = wanted + k * (self.reduction - wanted);
        db_to_gain(self.makeup.next_value() - self.reduction)
    }
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SineOsc;
    use crate::test_util::{SR, db};

    fn settings(threshold_db: f32, ratio: f32) -> CompressorSettings {
        CompressorSettings { threshold_db, ratio, attack_ms: 5.0, release_ms: 50.0, makeup_db: 0.0 }
    }

    /// Output RMS in dB for a sine of `amplitude`, once the compressor has settled.
    fn level_out(comp: &mut Compressor, amplitude: f32) -> f32 {
        let mut osc = SineOsc::new(SR);
        for _ in 0..48_000 {
            let x = osc.next_sample(1_000.0) * amplitude;
            comp.gain_for(x.abs());
        }
        let (mut sum_in, mut sum_out) = (0.0f64, 0.0f64);
        for _ in 0..48_000 {
            let x = osc.next_sample(1_000.0) * amplitude;
            let y = x * comp.gain_for(x.abs());
            sum_in += f64::from(x * x);
            sum_out += f64::from(y * y);
        }
        db((sum_out / sum_in).sqrt() as f32)
    }

    #[test]
    fn the_curve_is_flat_below_the_knee_and_follows_the_ratio_above_it() {
        assert_eq!(reduction_db(-40.0, -20.0, 4.0), 0.0);
        // 12 dB over at 4:1 comes out 3 dB over: 9 dB of reduction.
        assert!((reduction_db(-8.0, -20.0, 4.0) - 9.0).abs() < 1e-4);
        // At 1:1 nothing is ever reduced.
        assert_eq!(reduction_db(0.0, -20.0, 1.0), 0.0);
    }

    #[test]
    fn the_knee_is_continuous_at_both_edges() {
        let (edge_low, edge_high) = (-20.0 - KNEE_DB / 2.0, -20.0 + KNEE_DB / 2.0);
        assert!(reduction_db(edge_low + 1e-3, -20.0, 4.0).abs() < 1e-3);
        let expected = (edge_high + 20.0) * 0.75;
        assert!((reduction_db(edge_high - 1e-3, -20.0, 4.0) - expected).abs() < 1e-2);
    }

    #[test]
    fn a_quiet_signal_is_left_alone() {
        let mut comp = Compressor::new(settings(-20.0, 4.0), SR);
        assert!(level_out(&mut comp, 0.01).abs() < 0.05, "a signal 20 dB under the threshold was touched");
    }

    #[test]
    fn a_loud_signal_is_turned_down_by_the_ratio() {
        // A sine peaking at -6 dBFS, 14 dB over a -20 dB threshold, at 4:1:
        // the peaks come out 3.5 dB over, 10.5 dB down.
        let mut comp = Compressor::new(settings(-20.0, 4.0), SR);
        let reduced = level_out(&mut comp, 0.5);
        assert!((reduced + 10.5).abs() < 0.8, "reduced by {} dB, expected about 10.5", -reduced);
    }

    #[test]
    fn makeup_gain_is_added_after() {
        let mut comp = Compressor::new(CompressorSettings { makeup_db: 6.0, ..settings(-20.0, 1.0) }, SR);
        assert!((level_out(&mut comp, 0.1) - 6.0).abs() < 0.05);
    }

    #[test]
    fn the_attack_takes_about_as_long_as_it_says() {
        // A step from silence to loud: after one attack time, about 63 % of the
        // final reduction is applied.
        let mut comp = Compressor::new(CompressorSettings { attack_ms: 10.0, ..settings(-20.0, 10.0) }, SR);
        for _ in 0..480 {
            comp.gain_for(1.0);
        }
        let final_reduction = reduction_db(0.0, -20.0, 10.0);
        let fraction = comp.reduction() / final_reduction;
        assert!((fraction - 0.632).abs() < 0.02, "{fraction} of the reduction after one attack time");
    }
}
