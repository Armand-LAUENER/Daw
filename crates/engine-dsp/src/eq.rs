//! One band of a parametric equaliser, safe to move while it plays.
//!
//! A band is a biquad whose shape is fixed and whose frequency, gain and Q are
//! not. Changing them outright would be a jump in the coefficients, heard as a
//! click or, while a control is dragged, as zipper noise. So each parameter
//! glides — frequency in octaves, because that is how pitch is heard; gain in
//! decibels, for the same reason — and the coefficients are recomputed every
//! few samples and interpolated between, as [`SmoothedBiquad`](crate::SmoothedBiquad)
//! does for a plain filter.
//!
//! The coefficients are shared by every channel; each channel keeps its own
//! delay line, since each carries its own signal.

use crate::biquad::MIN_CUTOFF_HZ;
use crate::{Biquad, Coefficients, DEFAULT_UPDATE_INTERVAL, LinearSmoother};

/// How long a parameter change takes to arrive, in milliseconds.
pub const EQ_RAMP_MS: f32 = 20.0;

/// What a band does to the spectrum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Removes what is below the frequency: a two-pole high-pass.
    LowCut,
    /// Raises or lowers everything below the frequency.
    LowShelf,
    /// Raises or lowers around the frequency.
    Bell,
    /// Raises or lowers everything above the frequency.
    HighShelf,
    /// Removes what is above the frequency: a two-pole low-pass.
    HighCut,
}

impl Shape {
    /// Whether the band's gain means anything. A cut has a slope, not a level.
    pub fn has_gain(self) -> bool {
        matches!(self, Self::LowShelf | Self::Bell | Self::HighShelf)
    }
}

/// The coefficients for a band's settings.
pub fn coefficients(shape: Shape, freq_hz: f32, gain_db: f32, q: f32, sample_rate: f32) -> Coefficients {
    match shape {
        Shape::LowCut => Coefficients::high_pass(freq_hz, q, sample_rate),
        Shape::LowShelf => Coefficients::low_shelf(freq_hz, q, gain_db, sample_rate),
        Shape::Bell => Coefficients::peak(freq_hz, q, gain_db, sample_rate),
        Shape::HighShelf => Coefficients::high_shelf(freq_hz, q, gain_db, sample_rate),
        Shape::HighCut => Coefficients::low_pass(freq_hz, q, sample_rate),
    }
}

/// One band, for a fixed number of channels.
#[derive(Debug, Clone)]
pub struct EqBand {
    shape: Shape,
    /// log2 of the frequency, so a sweep moves evenly through octaves.
    octave: LinearSmoother,
    gain: LinearSmoother,
    q: LinearSmoother,
    filters: Vec<Biquad>,
    current: Coefficients,
    target: Coefficients,
    step: Coefficients,
    gliding: u32,
    until_update: u32,
    /// A parameter is still moving, so the coefficients must keep following.
    moving: bool,
    sample_rate: f32,
}

impl EqBand {
    pub fn new(shape: Shape, freq_hz: f32, gain_db: f32, q: f32, channels: usize, sample_rate: f32) -> Self {
        let coeffs = coefficients(shape, freq_hz, gain_db, q, sample_rate);
        Self {
            shape,
            octave: LinearSmoother::with_duration(octaves(freq_hz), EQ_RAMP_MS, sample_rate),
            gain: LinearSmoother::with_duration(gain_db, EQ_RAMP_MS, sample_rate),
            q: LinearSmoother::with_duration(q, EQ_RAMP_MS, sample_rate),
            filters: vec![Biquad::new(coeffs); channels.max(1)],
            current: coeffs,
            target: coeffs,
            step: coeffs.glide_to(&coeffs, 1),
            gliding: 0,
            until_update: 0,
            moving: false,
            sample_rate,
        }
    }

    pub fn set_freq(&mut self, hz: f32) {
        self.octave.set_target(octaves(hz));
        self.moving = true;
    }

    pub fn set_gain(&mut self, db: f32) {
        self.gain.set_target(db);
        self.moving = true;
    }

    pub fn set_q(&mut self, q: f32) {
        self.q.set_target(q);
        self.moving = true;
    }

    /// Nothing is moving: the coefficients are where they will stay, and a
    /// whole block can be filtered with them.
    pub fn is_settled(&self) -> bool {
        !self.moving && self.gliding == 0
    }

    /// Settled at unity: a bell or a shelf at 0 dB, which changes nothing and
    /// can be skipped.
    pub fn is_flat(&self) -> bool {
        self.is_settled() && self.shape.has_gain() && self.gain.target() == 0.0
    }

    /// Moves the parameters and the coefficients on by one sample. Call once
    /// per frame, before [`EqBand::filter`] on each channel of it.
    #[inline]
    pub fn tick(&mut self) {
        if self.moving {
            let (octave, gain, q) = (self.octave.next_value(), self.gain.next_value(), self.q.next_value());
            if self.until_update == 0 {
                self.target = coefficients(self.shape, octave.exp2(), gain, q, self.sample_rate);
                self.step = self.current.glide_to(&self.target, DEFAULT_UPDATE_INTERVAL);
                self.gliding = DEFAULT_UPDATE_INTERVAL;
                self.until_update = DEFAULT_UPDATE_INTERVAL;
                self.moving = self.octave.is_ramping() || self.gain.is_ramping() || self.q.is_ramping();
            }
            self.until_update -= 1;
        }
        if self.gliding > 0 {
            self.gliding -= 1;
            if self.gliding == 0 {
                self.current = self.target;
            } else {
                self.current.advance(&self.step);
            }
            for filter in &mut self.filters {
                filter.set_coefficients(self.current);
            }
        }
    }

    /// Filters one sample of one channel, with the coefficients of this frame.
    #[inline]
    pub fn filter(&mut self, channel: usize, x: f32) -> f32 {
        match self.filters.get_mut(channel) {
            Some(filter) => filter.process_sample(x),
            None => x,
        }
    }

    /// Filters a whole block of one channel. Only right once settled: the
    /// coefficients do not move inside it.
    pub fn filter_block(&mut self, channel: usize, buf: &mut [f32]) {
        if let Some(filter) = self.filters.get_mut(channel) {
            filter.process_block(buf);
        }
    }

    pub fn reset(&mut self) {
        for filter in &mut self.filters {
            filter.reset();
        }
    }
}

fn octaves(hz: f32) -> f32 {
    hz.max(MIN_CUTOFF_HZ).log2()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{BUTTERWORTH_Q, SR, db, max_step, measured_gain};

    fn gain_of(shape: Shape, freq: f32, gain: f32, q: f32, at: f32) -> f32 {
        let mut band = EqBand::new(shape, freq, gain, q, 1, SR);
        db(measured_gain(
            |x| {
                band.tick();
                band.filter(0, x)
            },
            at,
        ))
    }

    #[test]
    fn a_bell_reaches_its_gain_at_its_centre_and_nothing_far_from_it() {
        assert!((gain_of(Shape::Bell, 1_000.0, 6.0, 1.0, 1_000.0) - 6.0).abs() < 0.1);
        assert!((gain_of(Shape::Bell, 1_000.0, -9.0, 1.0, 1_000.0) + 9.0).abs() < 0.1);
        assert!(gain_of(Shape::Bell, 1_000.0, 6.0, 1.0, 60.0).abs() < 0.2, "a bell leaked far below its centre");
        assert!(gain_of(Shape::Bell, 1_000.0, 6.0, 1.0, 12_000.0).abs() < 0.3, "and far above");
    }

    #[test]
    fn a_shelf_moves_its_side_and_leaves_the_other() {
        assert!((gain_of(Shape::LowShelf, 200.0, 6.0, BUTTERWORTH_Q, 30.0) - 6.0).abs() < 0.2);
        assert!(gain_of(Shape::LowShelf, 200.0, 6.0, BUTTERWORTH_Q, 5_000.0).abs() < 0.1);
        assert!((gain_of(Shape::HighShelf, 4_000.0, -6.0, BUTTERWORTH_Q, 16_000.0) + 6.0).abs() < 0.3);
        assert!(gain_of(Shape::HighShelf, 4_000.0, -6.0, BUTTERWORTH_Q, 200.0).abs() < 0.1);
        // Half the gain at the corner, in decibels.
        assert!((gain_of(Shape::LowShelf, 200.0, 6.0, BUTTERWORTH_Q, 200.0) - 3.0).abs() < 0.2);
    }

    #[test]
    fn a_cut_removes_its_side() {
        assert!((gain_of(Shape::LowCut, 100.0, 0.0, BUTTERWORTH_Q, 100.0) + 3.01).abs() < 0.2);
        assert!(gain_of(Shape::LowCut, 100.0, 0.0, BUTTERWORTH_Q, 25.0) < -20.0);
        assert!(gain_of(Shape::HighCut, 5_000.0, 0.0, BUTTERWORTH_Q, 20_000.0) < -20.0);
    }

    #[test]
    fn the_computed_response_is_the_measured_one() {
        // What the curve on screen draws must be what is heard.
        for (shape, at) in
            [(Shape::Bell, 700.0), (Shape::LowShelf, 90.0), (Shape::HighShelf, 9_000.0), (Shape::LowCut, 60.0)]
        {
            let computed = db(coefficients(shape, 400.0, 5.0, 0.9, SR).magnitude(at, SR));
            let measured = gain_of(shape, 400.0, 5.0, 0.9, at);
            assert!(
                (computed - measured).abs() < 0.1,
                "{shape:?} at {at} Hz: computed {computed}, measured {measured}"
            );
        }
    }

    #[test]
    fn a_band_at_zero_db_is_flat_and_says_so() {
        let band = EqBand::new(Shape::Bell, 1_000.0, 0.0, 1.0, 2, SR);
        assert!(band.is_flat());
        assert!(!EqBand::new(Shape::LowCut, 1_000.0, 0.0, 1.0, 2, SR).is_flat(), "a cut is never flat");
    }

    #[test]
    fn a_gain_dragged_while_playing_does_not_zip() {
        // Jumping a bell from 0 to +12 dB at once steps the output by what the
        // new gain does to the current sample; gliding keeps every step close
        // to what the sine itself moves by.
        let mut band = EqBand::new(Shape::Bell, 1_000.0, 0.0, 1.0, 1, SR);
        let mut n = 0u32;
        let step = max_step(
            |x| {
                n += 1;
                if n == 4_096 {
                    band.set_gain(12.0);
                }
                band.tick();
                band.filter(0, x)
            },
            1_000.0,
            16_384,
            1_024,
        );
        // A full-scale 1 kHz sine at +12 dB moves at most 4 × 0.131 per sample.
        assert!(step < 0.6, "largest step {step} while the gain moved");
        assert!(band.is_settled(), "the band never settled");
    }
}
