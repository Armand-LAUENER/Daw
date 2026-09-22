//! A track's equaliser as a node: a few bands in series.
//!
//! Frequency, gain and Q of each band are parameters, reached by automation on
//! the graph that is playing and glided by the band itself. A band's shape, and
//! whether it is on, are not: changing either is a different filter, and the
//! graph is rebuilt for it, so the swap's crossfade covers the change instead
//! of a jump in the coefficients.

use engine_dsp::{EqBand, Shape};

use crate::{AudioBuffer, Node};

/// How a band is set up when the node is built.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BandSettings {
    pub shape: Shape,
    pub freq_hz: f32,
    pub gain_db: f32,
    pub q: f32,
    pub on: bool,
}

/// An equaliser: its bands in order, a band that is off simply absent.
pub struct Eq {
    /// One slot per band, so parameter numbers stay tied to the band they
    /// were given for whichever others are off.
    bands: Vec<Option<EqBand>>,
}

impl Eq {
    /// Parameters per band: frequency, gain, Q. Band `n`'s frequency is
    /// parameter `n * PARAMS_PER_BAND + PARAM_FREQ`.
    pub const PARAMS_PER_BAND: u32 = 3;
    pub const PARAM_FREQ: u32 = 0;
    pub const PARAM_GAIN: u32 = 1;
    pub const PARAM_Q: u32 = 2;

    pub fn new(bands: &[BandSettings], channels: usize, sample_rate: f32) -> Self {
        let bands = bands
            .iter()
            .map(|b| b.on.then(|| EqBand::new(b.shape, b.freq_hz, b.gain_db, b.q, channels, sample_rate)))
            .collect();
        Self { bands }
    }

    /// The parameter number of one of a band's settings.
    pub fn param(band: usize, which: u32) -> u32 {
        band as u32 * Self::PARAMS_PER_BAND + which
    }
}

impl Node for Eq {
    fn set_param(&mut self, param: u32, value: f32) {
        let band = (param / Self::PARAMS_PER_BAND) as usize;
        let Some(Some(band)) = self.bands.get_mut(band) else { return };
        match param % Self::PARAMS_PER_BAND {
            Self::PARAM_FREQ => band.set_freq(value),
            Self::PARAM_GAIN => band.set_gain(value),
            _ => band.set_q(value),
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        let (frames, channels) = (buffer.frames(), buffer.channels());
        for band in self.bands.iter_mut().flatten() {
            if band.is_flat() {
                // A bell or shelf at 0 dB changes nothing.
                continue;
            }
            if band.is_settled() {
                for channel in 0..channels {
                    band.filter_block(channel, buffer.channel_mut(channel));
                }
            } else {
                // Moving: the coefficients change sample by sample, and every
                // channel of a frame must see the same ones.
                for frame in 0..frames {
                    band.tick();
                    for channel in 0..channels {
                        let sample = &mut buffer.channel_mut(channel)[frame];
                        *sample = band.filter(channel, *sample);
                    }
                }
            }
        }
    }

    fn reset(&mut self) {
        for band in self.bands.iter_mut().flatten() {
            band.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn bell(gain_db: f32, on: bool) -> BandSettings {
        BandSettings { shape: Shape::Bell, freq_hz: 1_000.0, gain_db, q: 1.0, on }
    }

    /// RMS out over RMS in for a sine through the node, in dB, after settling.
    fn gain_db(eq: &mut Eq, freq: f32) -> f32 {
        let (mut phase, step) = (0.0f32, freq / SR);
        let (mut sum_in, mut sum_out) = (0.0f64, 0.0f64);
        for block in 0..64 {
            let mut data: Vec<f32> = (0..512)
                .map(|_| {
                    phase = (phase + step).fract();
                    (phase * std::f32::consts::TAU).sin()
                })
                .collect();
            let input = data.clone();
            eq.process(&mut AudioBuffer::new(&mut data, 1));
            if block >= 16 {
                sum_in += input.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>();
                sum_out += data.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>();
            }
        }
        (10.0 * (sum_out / sum_in).log10()) as f32
    }

    #[test]
    fn bands_in_series_add_up() {
        let mut eq = Eq::new(&[bell(6.0, true), bell(-3.0, true)], 1, SR);
        assert!((gain_db(&mut eq, 1_000.0) - 3.0).abs() < 0.1);
    }

    #[test]
    fn a_band_that_is_off_is_not_heard_and_keeps_the_others_numbers() {
        let mut eq = Eq::new(&[bell(6.0, false), bell(-3.0, true)], 1, SR);
        assert!((gain_db(&mut eq, 1_000.0) + 3.0).abs() < 0.1);
        // Band 1's gain is still parameter 4, band 0 being off or not.
        eq.set_param(Eq::param(1, Eq::PARAM_GAIN), 0.0);
        assert!(gain_db(&mut eq, 1_000.0).abs() < 0.1);
        // And a parameter for the band that is off goes nowhere.
        eq.set_param(Eq::param(0, Eq::PARAM_GAIN), 12.0);
        assert!(gain_db(&mut eq, 1_000.0).abs() < 0.1);
    }

    #[test]
    fn a_frequency_moved_by_automation_arrives() {
        let mut eq = Eq::new(&[bell(9.0, true)], 1, SR);
        eq.set_param(Eq::param(0, Eq::PARAM_FREQ), 4_000.0);
        assert!((gain_db(&mut eq, 4_000.0) - 9.0).abs() < 0.2);
        assert!(gain_db(&mut eq, 1_000.0) < 3.0, "the old centre is still boosted");
    }
}
