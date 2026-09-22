//! A room: Freeverb, Jezar's public-domain design.
//!
//! Eight comb filters in parallel make the dense tail, each a delay line fed
//! back through a low-pass so the highs die first, as they do in a real room;
//! four all-passes in series then smear their echoes into a wash. The right
//! channel's lines are a few samples longer than the left's, which is what
//! makes the tail wide rather than a mono reverb panned centre.
//!
//! The line lengths are Freeverb's, given at 44.1 kHz and scaled to the rate
//! in use, so the room is the same size at any rate.

use crate::LinearSmoother;

const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASSES: [usize; 4] = [556, 441, 341, 225];
/// How much longer the right channel's lines are, in samples at 44.1 kHz.
const SPREAD: usize = 23;
const ALLPASS_FEEDBACK: f32 = 0.5;
/// The input is scaled down so eight combs summed stay near full scale.
const INPUT_GAIN: f32 = 0.015;
/// Below this a sample is gone; flushed so the tail never becomes denormals.
const FLUSH: f32 = 1e-15;

#[derive(Debug, Clone)]
struct Comb {
    line: Vec<f32>,
    at: usize,
    store: f32,
}

impl Comb {
    #[inline]
    fn process(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let out = self.line[self.at];
        self.store = out + damp * (self.store - out);
        if self.store.abs() < FLUSH {
            self.store = 0.0;
        }
        self.line[self.at] = x + self.store * feedback;
        self.at = (self.at + 1) % self.line.len();
        out
    }
}

#[derive(Debug, Clone)]
struct AllPass {
    line: Vec<f32>,
    at: usize,
}

impl AllPass {
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let delayed = self.line[self.at];
        let mut stored = x + delayed * ALLPASS_FEEDBACK;
        if stored.abs() < FLUSH {
            stored = 0.0;
        }
        self.line[self.at] = stored;
        self.at = (self.at + 1) % self.line.len();
        delayed - x
    }
}

/// One channel's filters.
#[derive(Debug, Clone)]
struct Tank {
    combs: Vec<Comb>,
    allpasses: Vec<AllPass>,
}

impl Tank {
    fn new(extra: usize, sample_rate: f32) -> Self {
        let scale = |n: usize| (((n + extra) as f32) * sample_rate / 44_100.0).round().max(1.0) as usize;
        Self {
            combs: COMBS.iter().map(|&n| Comb { line: vec![0.0; scale(n)], at: 0, store: 0.0 }).collect(),
            allpasses: ALLPASSES.iter().map(|&n| AllPass { line: vec![0.0; scale(n)], at: 0 }).collect(),
        }
    }

    #[inline]
    fn process(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let mut out: f32 = self.combs.iter_mut().map(|c| c.process(x, feedback, damp)).sum();
        for allpass in &mut self.allpasses {
            out = allpass.process(out);
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct Reverb {
    tanks: Vec<Tank>,
    size: f32,
    damping: f32,
    mix: LinearSmoother,
}

impl Reverb {
    /// `size` 0 to 1, from a small room to a hall; `damping` 0 to 1, from
    /// bright to dull; `mix` 0 to 1, how much of the room is heard.
    pub fn new(channels: usize, size: f32, damping: f32, mix: f32, sample_rate: f32) -> Self {
        Self {
            tanks: (0..channels.max(1)).map(|c| Tank::new(if c % 2 == 1 { SPREAD } else { 0 }, sample_rate)).collect(),
            size: size.clamp(0.0, 1.0),
            damping: damping.clamp(0.0, 1.0),
            mix: LinearSmoother::with_duration(mix.clamp(0.0, 1.0), 20.0, sample_rate),
        }
    }

    pub fn set_size(&mut self, size: f32) {
        self.size = size.clamp(0.0, 1.0);
    }

    pub fn set_damping(&mut self, damping: f32) {
        self.damping = damping.clamp(0.0, 1.0);
    }

    pub fn set_mix(&mut self, mix: f32) {
        self.mix.set_target(mix.clamp(0.0, 1.0));
    }

    /// Processes one frame in place. Every channel is fed the same sum, as a
    /// room hears a source rather than two speakers.
    #[inline]
    pub fn process_frame(&mut self, frame: &mut [f32]) {
        // Freeverb's mapping: size 0..1 to comb feedback 0.7..0.98, damping to
        // at most 0.4 of low-pass.
        let feedback = 0.7 + 0.28 * self.size;
        let damp = 0.4 * self.damping;
        let mix = self.mix.next_value();
        let input = frame.iter().sum::<f32>() / frame.len().max(1) as f32 * INPUT_GAIN;
        let tanks = self.tanks.len();
        for (channel, sample) in frame.iter_mut().enumerate() {
            let tank = &mut self.tanks[channel % tanks];
            *sample += tank.process(input, feedback, damp) * mix;
        }
    }

    pub fn reset(&mut self) {
        for tank in &mut self.tanks {
            for comb in &mut tank.combs {
                comb.line.fill(0.0);
                comb.store = 0.0;
            }
            for allpass in &mut tank.allpasses {
                allpass.line.fill(0.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::SR;

    /// The stereo response to one impulse, as (left, right) per frame.
    fn impulse(reverb: &mut Reverb, len: usize) -> Vec<(f32, f32)> {
        (0..len)
            .map(|i| {
                let x = if i == 0 { 1.0 } else { 0.0 };
                let mut frame = [x, x];
                reverb.process_frame(&mut frame);
                (frame[0], frame[1])
            })
            .collect()
    }

    fn energy(samples: &[(f32, f32)]) -> f64 {
        samples.iter().map(|(l, r)| f64::from(l * l + r * r)).sum()
    }

    #[test]
    fn a_click_leaves_a_tail_that_dies_away() {
        let mut reverb = Reverb::new(2, 0.5, 0.5, 1.0, SR);
        let out = impulse(&mut reverb, (SR * 6.0) as usize);
        let second = |n: usize| energy(&out[(n * SR as usize)..((n + 1) * SR as usize)]);
        assert!(second(0) > 0.0, "no tail at all");
        assert!(second(1) < second(0) && second(2) < second(1), "the tail did not decay");
        assert!(second(5) < second(0) * 1e-6, "still ringing after five seconds");
    }

    #[test]
    fn a_bigger_room_rings_longer() {
        let tail = |size: f32| {
            let mut reverb = Reverb::new(2, size, 0.5, 1.0, SR);
            energy(&impulse(&mut reverb, (SR * 3.0) as usize)[SR as usize * 2..])
        };
        assert!(tail(0.9) > tail(0.2) * 10.0, "size made little difference to the tail");
    }

    #[test]
    fn the_two_sides_differ() {
        // The spread is what makes the room wide; identical sides would be mono.
        let mut reverb = Reverb::new(2, 0.5, 0.5, 1.0, SR);
        let out = impulse(&mut reverb, 20_000);
        assert!(out[2_000..].iter().any(|(l, r)| (l - r).abs() > 1e-4));
    }

    #[test]
    fn a_dry_mix_is_just_the_input() {
        let mut reverb = Reverb::new(2, 0.9, 0.5, 0.0, SR);
        let out = impulse(&mut reverb, 10_000);
        assert_eq!(out[0], (1.0, 1.0));
        assert!(out[1..].iter().all(|(l, r)| *l == 0.0 && *r == 0.0));
    }

    #[test]
    fn the_tail_ends_in_zeros_not_denormals() {
        let mut reverb = Reverb::new(2, 0.3, 0.8, 1.0, SR);
        let out = impulse(&mut reverb, (SR * 30.0) as usize);
        let end = &out[out.len() - 1_000..];
        assert!(end.iter().all(|(l, r)| (*l == 0.0 || l.is_normal()) && (*r == 0.0 || r.is_normal())));
    }
}
