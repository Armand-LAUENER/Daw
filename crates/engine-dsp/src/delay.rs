//! An echo: the signal again after a while, fed back into itself.
//!
//! One delay line per channel, allocated when it is made for the longest time
//! it will ever be asked for, so nothing is allocated while it plays. The
//! repeats are darkened by a gentle low-pass in the feedback path, as tape and
//! bucket-brigade echoes do, which also keeps a high feedback from building a
//! harsh whistle.
//!
//! A new time glides rather than jumps: reading the line from a different
//! place in one step is a click, while moving the read point smoothly is the
//! pitch bend a tape echo makes when its speed is turned.

use crate::LinearSmoother;

/// The longest echo, in seconds. Four beats at 60 BPM.
pub const MAX_DELAY_S: f32 = 4.0;
/// How long a change of time takes, in milliseconds.
const TIME_GLIDE_MS: f32 = 80.0;
/// Where the feedback's low-pass sits, in Hz.
const FEEDBACK_TONE_HZ: f32 = 5_000.0;
/// Below this, a repeat has decayed to nothing: flushed to zero rather than
/// left to become a denormal, which costs a hundred times a normal number.
const FLUSH: f32 = 1e-15;

#[derive(Debug, Clone)]
pub struct Delay {
    lines: Vec<Vec<f32>>,
    /// Where the next sample is written, the same for every channel.
    write: usize,
    /// The echo's length, in frames, gliding to a new one.
    time: LinearSmoother,
    feedback: f32,
    mix: LinearSmoother,
    /// One low-pass state per channel, in the feedback path.
    tone: Vec<f32>,
    tone_coefficient: f32,
}

impl Delay {
    /// `time_frames` is the echo's length; `feedback` how much of each repeat
    /// comes back, below 1; `mix` how much echo is heard, 0 to 1.
    pub fn new(channels: usize, time_frames: f32, feedback: f32, mix: f32, sample_rate: f32) -> Self {
        let length = (MAX_DELAY_S * sample_rate) as usize + 2;
        let tone_coefficient = (-2.0 * std::f32::consts::PI * FEEDBACK_TONE_HZ / sample_rate).exp();
        Self {
            lines: vec![vec![0.0; length]; channels.max(1)],
            write: 0,
            time: LinearSmoother::with_duration(
                time_frames.clamp(1.0, (length - 2) as f32),
                TIME_GLIDE_MS,
                sample_rate,
            ),
            feedback: feedback.clamp(0.0, 0.95),
            mix: LinearSmoother::with_duration(mix.clamp(0.0, 1.0), 20.0, sample_rate),
            tone: vec![0.0; channels.max(1)],
            tone_coefficient,
        }
    }

    pub fn set_time(&mut self, frames: f32) {
        let longest = (self.lines[0].len() - 2) as f32;
        self.time.set_target(frames.clamp(1.0, longest));
    }

    pub fn set_feedback(&mut self, feedback: f32) {
        self.feedback = feedback.clamp(0.0, 0.95);
    }

    pub fn set_mix(&mut self, mix: f32) {
        self.mix.set_target(mix.clamp(0.0, 1.0));
    }

    /// Processes one frame in place: every channel of it, with the same time.
    #[inline]
    pub fn process_frame(&mut self, frame: &mut [f32]) {
        let time = self.time.next_value();
        let mix = self.mix.next_value();
        let length = self.lines[0].len();
        // Linear interpolation between the two samples around the read point.
        let read = self.write as f32 + length as f32 - time;
        let (whole, fraction) = (read.floor() as usize % length, read.fract());
        let next = (whole + 1) % length;
        for (channel, sample) in frame.iter_mut().enumerate().take(self.lines.len()) {
            let line = &mut self.lines[channel];
            let echo = line[whole] + (line[next] - line[whole]) * fraction;
            let tone = &mut self.tone[channel];
            *tone = echo + self.tone_coefficient * (*tone - echo);
            let mut back = *sample + *tone * self.feedback;
            if back.abs() < FLUSH {
                back = 0.0;
            }
            line[self.write] = back;
            // The dry signal stays whole; the echo is added on top of it.
            *sample += echo * mix;
        }
        self.write = (self.write + 1) % length;
    }

    pub fn reset(&mut self) {
        for line in &mut self.lines {
            line.fill(0.0);
        }
        self.tone.fill(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::SR;

    /// The response of a mono delay to a single impulse, `len` samples long.
    fn impulse(delay: &mut Delay, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let mut frame = [if i == 0 { 1.0 } else { 0.0 }];
                delay.process_frame(&mut frame);
                frame[0]
            })
            .collect()
    }

    fn peak_near(response: &[f32], at: usize) -> f32 {
        response[at.saturating_sub(3)..at + 4].iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    #[test]
    fn the_echo_arrives_after_its_time() {
        let mut delay = Delay::new(1, 1_000.0, 0.0, 1.0, SR);
        let out = impulse(&mut delay, 3_000);
        assert_eq!(out[0], 1.0, "the dry signal is kept");
        assert!(out[1..990].iter().all(|s| s.abs() < 1e-6), "something came out before the echo");
        assert!(peak_near(&out, 1_000) > 0.5, "no echo at 1000 samples");
        assert!(peak_near(&out, 2_000) < 1e-6, "an echo repeated with no feedback");
    }

    #[test]
    fn feedback_repeats_and_decays() {
        // A burst of 200 Hz — well under the feedback's tone filter, so each
        // repeat should come back at the feedback's level. An impulse would
        // not: the filter spreads a single sample out, and its peak with it.
        let mut delay = Delay::new(1, 1_000.0, 0.5, 1.0, SR);
        let out: Vec<f32> = (0..5_000)
            .map(|i| {
                let x = if i < 480 { (i as f32 * 200.0 / SR * std::f32::consts::TAU).sin() } else { 0.0 };
                let mut frame = [x];
                delay.process_frame(&mut frame);
                frame[0]
            })
            .collect();
        let rms = |from: usize| (out[from..from + 480].iter().map(|s| s * s).sum::<f32>() / 480.0).sqrt();
        let (first, second, third) = (rms(1_000), rms(2_000), rms(3_000));
        assert!((second / first - 0.5).abs() < 0.05, "second repeat at {} of the first", second / first);
        assert!((third / second - 0.5).abs() < 0.05, "third repeat at {} of the second", third / second);
    }

    #[test]
    fn a_dry_mix_is_just_the_input() {
        let mut delay = Delay::new(1, 500.0, 0.8, 0.0, SR);
        let out = impulse(&mut delay, 2_000);
        assert_eq!(out[0], 1.0);
        assert!(out[1..].iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn a_long_feedback_tail_ends_in_zeros_not_denormals() {
        let mut delay = Delay::new(1, 100.0, 0.95, 1.0, SR);
        let out = impulse(&mut delay, 400_000);
        assert!(out[390_000..].iter().all(|s| *s == 0.0 || s.is_normal()), "a denormal survived in the tail");
    }
}
