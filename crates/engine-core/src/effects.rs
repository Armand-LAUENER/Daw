//! A track's dynamics and space: compressor, echo and room, as nodes.
//!
//! Each wraps a DSP block from `engine-dsp` and exposes its settings as
//! parameters, reached by automation on the graph that is playing. Whether an
//! effect is in the chain at all is not a parameter: switching one on or off
//! rebuilds the graph, and the swap's crossfade covers the change.

use engine_dsp::{Compressor, CompressorSettings, Delay, Reverb};

use crate::{AudioBuffer, Node};

/// The most channels a frame-at-a-time effect handles. The graph is stereo;
/// this leaves room without reaching for the allocator.
const MAX_CHANNELS: usize = 8;

/// Runs `f` on each frame of a planar buffer, gathered into a small array and
/// scattered back — for effects that must see every channel of a frame at once.
#[inline]
fn per_frame(buffer: &mut AudioBuffer<'_>, mut f: impl FnMut(&mut [f32])) {
    let (frames, channels) = (buffer.frames(), buffer.channels().min(MAX_CHANNELS));
    let mut frame = [0.0f32; MAX_CHANNELS];
    for at in 0..frames {
        for (channel, slot) in frame.iter_mut().enumerate().take(channels) {
            *slot = buffer.channel_mut(channel)[at];
        }
        f(&mut frame[..channels]);
        for (channel, value) in frame.iter().enumerate().take(channels) {
            buffer.channel_mut(channel)[at] = *value;
        }
    }
}

/// A compressor, its channels linked.
pub struct Compress(Compressor);

impl Compress {
    pub const PARAM_THRESHOLD: u32 = 0;
    pub const PARAM_RATIO: u32 = 1;
    pub const PARAM_ATTACK: u32 = 2;
    pub const PARAM_RELEASE: u32 = 3;
    pub const PARAM_MAKEUP: u32 = 4;

    pub fn new(settings: CompressorSettings, sample_rate: f32) -> Self {
        Self(Compressor::new(settings, sample_rate))
    }
}

impl Node for Compress {
    fn set_param(&mut self, param: u32, value: f32) {
        match param {
            Self::PARAM_THRESHOLD => self.0.set_threshold(value),
            Self::PARAM_RATIO => self.0.set_ratio(value),
            Self::PARAM_ATTACK => self.0.set_attack(value),
            Self::PARAM_RELEASE => self.0.set_release(value),
            Self::PARAM_MAKEUP => self.0.set_makeup(value),
            _ => {}
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        let comp = &mut self.0;
        per_frame(buffer, |frame| {
            // The loudest channel decides, so the image does not lean.
            let peak = frame.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            let gain = comp.gain_for(peak);
            for sample in frame {
                *sample *= gain;
            }
        });
    }
}

/// An echo, its time in frames.
pub struct Echo(Delay);

impl Echo {
    pub const PARAM_TIME: u32 = 0;
    pub const PARAM_FEEDBACK: u32 = 1;
    pub const PARAM_MIX: u32 = 2;

    pub fn new(channels: usize, time_frames: f32, feedback: f32, mix: f32, sample_rate: f32) -> Self {
        Self(Delay::new(channels, time_frames, feedback, mix, sample_rate))
    }
}

impl Node for Echo {
    fn set_param(&mut self, param: u32, value: f32) {
        match param {
            Self::PARAM_TIME => self.0.set_time(value),
            Self::PARAM_FEEDBACK => self.0.set_feedback(value),
            Self::PARAM_MIX => self.0.set_mix(value),
            _ => {}
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        let delay = &mut self.0;
        per_frame(buffer, |frame| delay.process_frame(frame));
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

/// A room.
pub struct Room(Reverb);

impl Room {
    pub const PARAM_SIZE: u32 = 0;
    pub const PARAM_DAMPING: u32 = 1;
    pub const PARAM_MIX: u32 = 2;

    pub fn new(channels: usize, size: f32, damping: f32, mix: f32, sample_rate: f32) -> Self {
        Self(Reverb::new(channels, size, damping, mix, sample_rate))
    }
}

impl Node for Room {
    fn set_param(&mut self, param: u32, value: f32) {
        match param {
            Self::PARAM_SIZE => self.0.set_size(value),
            Self::PARAM_DAMPING => self.0.set_damping(value),
            Self::PARAM_MIX => self.0.set_mix(value),
            _ => {}
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        let reverb = &mut self.0;
        per_frame(buffer, |frame| reverb.process_frame(frame));
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    #[test]
    fn the_compressor_reduces_both_sides_by_the_same_amount() {
        // Loud on the left, quiet on the right: linked, the right is turned
        // down as much as the left, so the balance between them holds.
        let settings =
            CompressorSettings { threshold_db: -20.0, ratio: 4.0, attack_ms: 1.0, release_ms: 50.0, makeup_db: 0.0 };
        let mut node = Compress::new(settings, SR);
        let mut data = [vec![0.8f32; 4_800], vec![0.1f32; 4_800]].concat();
        node.process(&mut AudioBuffer::new(&mut data, 2));
        let (left, right) = (data[4_799], data[9_599]);
        assert!(left < 0.8 * 0.5, "the loud side was not reduced: {left}");
        assert!((left / right - 8.0).abs() < 1e-3, "the balance moved: {}", left / right);
    }

    #[test]
    fn the_echo_repeats_each_channel_separately() {
        let mut node = Echo::new(2, 100.0, 0.0, 1.0, SR);
        let mut data = vec![0.0f32; 400];
        data[0] = 1.0; // left impulse only
        node.process(&mut AudioBuffer::new(&mut data, 2));
        assert!(data[100] > 0.5, "no echo on the left");
        assert!(data[200..].iter().all(|s| s.abs() < 1e-6), "the left echo reached the right");
    }

    #[test]
    fn the_room_adds_a_tail_after_the_sound() {
        let mut node = Room::new(2, 0.6, 0.5, 0.5, SR);
        let mut data = vec![0.0f32; 2 * 24_000];
        data[0] = 1.0;
        data[24_000] = 1.0;
        node.process(&mut AudioBuffer::new(&mut data, 2));
        assert!(data[5_000..24_000].iter().any(|s| s.abs() > 1e-5), "no tail on the left");
    }
}
