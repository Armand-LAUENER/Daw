use crate::{AudioBuffer, Node};

/// How far ahead the limiter looks, and so how much latency it adds.
///
/// Long enough for the gain to come down smoothly, short enough that the delay
/// stays under what a player feels. Everything downstream has to compensate for
/// it, which is why it is a constant rather than a free parameter.
pub const LIMITER_LOOKAHEAD_MS: f32 = 2.0;

/// How long the gain takes to come most of the way back after a peak.
///
/// Slow on purpose. A fast release re-levels everything sitting under the
/// transient, which is the breathing you hear on a badly set limiter.
pub const LIMITER_RELEASE_MS: f32 = 100.0;

/// Holds the output under a ceiling, without clipping it.
///
/// A ceiling applied where the sample is read can only ever clip: by the time
/// the peak is in hand it is already the output. This one delays the signal and
/// watches the delay, so a peak is seen `LIMITER_LOOKAHEAD_MS` before it is
/// heard and the gain is already down when it lands. That head start is what
/// turns a clip into a dip.
///
/// The guarantee is exact, not approximate. The sliding maximum covers the
/// whole delay, so the gain demanded by a sample is asked for the moment it
/// enters the window; the fall is sized to arrive within the delay; and the
/// gain only ever rises when nothing in the window needs it down. No sample
/// leaves above the threshold.
///
/// One gain serves every channel of a frame. A per-channel gain would pull the
/// stereo image toward the quiet side every time a transient hit one speaker.
pub struct Limiter {
    threshold: f32,
    lookahead: usize,
    channels: usize,
    /// The delayed signal, one channel after another, like everything else here.
    delay: Vec<f32>,
    write: usize,
    window: SlidingMax,
    gain: f32,
    /// How far the gain drops per sample while it is coming down.
    fall: f32,
    release: f32,
}

impl Limiter {
    /// The ceiling, linear. 1.0 is full scale.
    pub const PARAM_THRESHOLD: u32 = 0;

    pub fn new(threshold: f32, channels: usize, sample_rate: f32) -> Self {
        let lookahead = ((LIMITER_LOOKAHEAD_MS * 1e-3 * sample_rate) as usize).max(1);
        Self {
            threshold: threshold.max(f32::MIN_POSITIVE),
            lookahead,
            channels: channels.max(1),
            delay: vec![0.0; channels.max(1) * lookahead],
            write: 0,
            // The window has to reach one sample further back than the delay,
            // or the sample being output would not be in it.
            window: SlidingMax::new(lookahead + 1),
            gain: 1.0,
            fall: 0.0,
            release: 1.0 - (-1.0 / (LIMITER_RELEASE_MS * 1e-3 * sample_rate)).exp(),
        }
    }

    /// How many frames the limiter delays the signal by.
    ///
    /// Anything that has to line up with this output — a metronome, a recorded
    /// take, another path through the graph — has to account for it.
    pub fn latency_frames(&self) -> usize {
        self.lookahead
    }

    pub fn set_threshold(&mut self, threshold: f32) {
        self.threshold = threshold.max(f32::MIN_POSITIVE);
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    /// The gain currently applied, for metering. 1.0 means nothing is held back.
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// The gain to apply to the sample leaving the delay this step.
    fn next_gain(&mut self, target: f32) -> f32 {
        if target < self.gain {
            // The sample that asked for this gain entered the window just now,
            // so it reaches the output `lookahead` steps from here. Fall fast
            // enough to have arrived by then, and no faster.
            let needed = (self.gain - target) / self.lookahead as f32;
            self.fall = self.fall.max(needed);
            self.gain = (self.gain - self.fall).max(target);
        } else {
            self.fall = 0.0;
            self.gain += (target - self.gain) * self.release;
        }
        self.gain
    }
}

impl Node for Limiter {
    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        // The graph gives every node the channel count it was built with, so
        // this only bites if a limiter is moved between graphs of different
        // shapes. Limiting the channels it has a delay line for is safer than
        // letting the rest through unheld.
        debug_assert_eq!(buffer.channels(), self.channels, "limiter built for a different channel count");
        let channels = buffer.channels().min(self.channels);
        let frames = buffer.frames();

        for frame in 0..frames {
            let mut peak = 0.0f32;
            for channel in 0..channels {
                peak = peak.max(buffer.channel(channel)[frame].abs());
            }

            let ahead = self.window.push(peak);
            let target = if ahead > self.threshold { self.threshold / ahead } else { 1.0 };
            let gain = self.next_gain(target);

            let slot = self.write;
            for channel in 0..channels {
                let incoming = buffer.channel(channel)[frame];
                let line = &mut self.delay[channel * self.lookahead + slot];
                let outgoing = *line;
                *line = incoming;
                buffer.channel_mut(channel)[frame] = outgoing * gain;
            }
            self.write = (self.write + 1) % self.lookahead;
        }
    }

    fn reset(&mut self) {
        self.delay.fill(0.0);
        self.write = 0;
        self.window.reset();
        self.gain = 1.0;
        self.fall = 0.0;
    }

    fn set_param(&mut self, param: u32, value: f32) {
        if param == Self::PARAM_THRESHOLD {
            self.set_threshold(value);
        }
    }
}

/// The largest value of the last `window` samples, in constant time per sample.
///
/// Scanning the delay line every sample would cost the lookahead over again on
/// each one. Instead the candidates are kept in a queue that decreases from
/// front to back: a sample smaller than one already waiting can never be the
/// maximum while that one is still in the window, so it is dropped on the way
/// in and the front is always the answer.
struct SlidingMax {
    values: Vec<f32>,
    /// When each candidate arrived, to know when it falls out of the window.
    arrivals: Vec<u64>,
    head: usize,
    len: usize,
    window: usize,
    position: u64,
}

impl SlidingMax {
    fn new(window: usize) -> Self {
        let window = window.max(1);
        Self { values: vec![0.0; window], arrivals: vec![0; window], head: 0, len: 0, window, position: 0 }
    }

    fn push(&mut self, value: f32) -> f32 {
        // Expire first, then insert: the queue never holds more than a window's
        // worth, so the ring it lives in never has to grow.
        while self.len > 0 && self.arrivals[self.head] + self.window as u64 <= self.position {
            self.head = (self.head + 1) % self.window;
            self.len -= 1;
        }
        while self.len > 0 {
            let back = (self.head + self.len - 1) % self.window;
            if self.values[back] > value {
                break;
            }
            self.len -= 1;
        }

        let slot = (self.head + self.len) % self.window;
        self.values[slot] = value;
        self.arrivals[slot] = self.position;
        self.len += 1;
        self.position += 1;

        self.values[self.head]
    }

    fn reset(&mut self) {
        self.head = 0;
        self.len = 0;
        self.position = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    /// Float error on `threshold / peak * peak`, not a tolerance on the ceiling.
    const SLACK: f32 = 1e-6;

    fn run(limiter: &mut Limiter, data: &mut [f32], channels: usize) {
        let mut buffer = AudioBuffer::new(data, channels);
        limiter.process(&mut buffer);
    }

    /// Feeds one mono block and returns what comes out.
    fn through(limiter: &mut Limiter, input: &[f32]) -> Vec<f32> {
        let mut data = input.to_vec();
        run(limiter, &mut data, 1);
        data
    }

    #[test]
    fn a_signal_under_the_ceiling_is_not_touched() {
        // A limiter that colours what it is not limiting is a limiter nobody
        // leaves switched on. Below the threshold this has to be a delay line
        // and nothing else, to the bit.
        let mut limiter = Limiter::new(0.9, 1, SR);
        let input: Vec<f32> = (0..2_000).map(|i| 0.5 * (i as f32 * 0.01).sin()).collect();
        let out = through(&mut limiter, &input);

        let delay = limiter.latency_frames();
        assert_eq!(&out[delay..], &input[..input.len() - delay], "the signal was altered under the ceiling");
        assert_eq!(limiter.gain(), 1.0);
    }

    #[test]
    fn nothing_escapes_the_ceiling() {
        let threshold = 0.5;
        let mut limiter = Limiter::new(threshold, 1, SR);

        // Everything a limiter hates at once: a cold start straight into full
        // scale, lone spikes with no warning, and a sustained overload.
        let mut input = vec![4.0f32; 64];
        input.extend(std::iter::repeat_n(0.0, 500));
        for i in 0..5_000 {
            let spike = if i % 611 == 0 { 8.0 } else { 0.1 };
            input.push(spike * if i % 2 == 0 { 1.0 } else { -1.0 });
        }
        input.extend(std::iter::repeat_n(2.0, 3_000));

        let out = through(&mut limiter, &input);
        for (i, sample) in out.iter().enumerate() {
            assert!(sample.abs() <= threshold + SLACK, "sample {i} left at {sample}");
        }
    }

    #[test]
    fn a_lone_transient_does_not_get_through_before_the_gain_does() {
        // The whole reason for the lookahead. Without it the first sample of a
        // spike is already out of the door when its level is known, and the
        // best a limiter can do is clip it.
        let threshold = 0.25;
        let mut limiter = Limiter::new(threshold, 1, SR);
        let mut input = vec![0.0f32; 1_000];
        input[500] = 4.0;
        let out = through(&mut limiter, &input);

        let peak = out.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
        assert!(peak <= threshold + SLACK, "peak {peak} over the {threshold} ceiling");
        assert!(peak > threshold * 0.99, "peak {peak}: the transient was crushed, not limited");
    }

    #[test]
    fn the_ceiling_holds_across_block_boundaries() {
        // State lives in the node, and a spike that enters near the end of one
        // callback is output in the next. Getting this wrong only shows up at
        // one block size in a thousand.
        let threshold = 0.4;
        let mut limiter = Limiter::new(threshold, 1, SR);
        let mut worst = 0.0f32;
        for block in 0..80 {
            let mut data: Vec<f32> = (0..37).map(|i| if (block * 37 + i) % 97 == 0 { 3.0 } else { 0.2 }).collect();
            run(&mut limiter, &mut data, 1);
            worst = worst.max(data.iter().fold(0.0f32, |acc, s| acc.max(s.abs())));
        }
        assert!(worst <= threshold + SLACK, "worst {worst}");
    }

    #[test]
    fn both_channels_of_a_frame_share_one_gain() {
        // A gain computed per channel would drag the image toward whichever
        // side was quiet whenever a transient hit the other.
        let mut limiter = Limiter::new(0.3, 2, SR);
        let left: Vec<f32> = (0..600).map(|i| if i == 200 { 3.0 } else { 0.4 }).collect();
        let mut data: Vec<f32> = left.iter().copied().chain(left.iter().map(|s| s * 0.25)).collect();
        run(&mut limiter, &mut data, 2);

        let (out_left, out_right) = data.split_at(600);
        for (i, (l, r)) in out_left.iter().zip(out_right).enumerate() {
            assert!((r - l * 0.25).abs() <= SLACK, "frame {i}: {l} and {r} were scaled differently");
        }
    }

    #[test]
    fn the_gain_moves_without_stepping() {
        // A gain that jumped to where it was needed would trade a clip for a
        // click, which is not a trade.
        let threshold = 0.25;
        let mut limiter = Limiter::new(threshold, 1, SR);
        let steepest = 1.0 / limiter.latency_frames() as f32 + SLACK;

        // Long enough to cover the release several times over: the pole takes
        // LIMITER_RELEASE_MS to come 63% of the way back, so a run that ends
        // after one of them would be reading the curve, not the recovery.
        let settle = (LIMITER_RELEASE_MS * 1e-3 * SR * 6.0) as usize;
        let mut previous = limiter.gain();
        for i in 0..1_000 + settle {
            let mut frame = [if i == 1_000 { 10.0 } else { 0.1 }];
            run(&mut limiter, &mut frame, 1);
            let gain = limiter.gain();
            assert!((gain - previous).abs() <= steepest, "gain jumped at {i}: {previous} -> {gain}");
            previous = gain;
        }
        assert!(previous > 0.99, "the gain never came back: {previous}");
    }

    #[test]
    fn the_latency_is_the_lookahead_it_reports() {
        let mut limiter = Limiter::new(1.0, 1, SR);
        let mut input = vec![0.0f32; 500];
        input[0] = 0.5;
        let out = through(&mut limiter, &input);

        let landed = out.iter().position(|s| *s != 0.0).expect("the impulse never came out");
        assert_eq!(landed, limiter.latency_frames());
    }

    #[test]
    fn resetting_empties_the_delay() {
        let mut limiter = Limiter::new(0.5, 1, SR);
        through(&mut limiter, &vec![2.0; 400]);
        assert!(limiter.gain() < 1.0, "nothing was being held back, so the reset proves nothing");

        limiter.reset();
        assert_eq!(limiter.gain(), 1.0);
        let out = through(&mut limiter, &vec![0.0; 400]);
        assert!(out.iter().all(|s| *s == 0.0), "the old signal survived the reset");
    }

    #[test]
    fn the_threshold_is_automatable() {
        let mut limiter = Limiter::new(1.0, 1, SR);
        limiter.set_param(Limiter::PARAM_THRESHOLD, 0.1);
        assert_eq!(limiter.threshold(), 0.1);
        limiter.set_param(999, 0.7);
        assert_eq!(limiter.threshold(), 0.1, "an unknown parameter moved the threshold");
    }

    #[test]
    fn a_zero_threshold_does_not_divide_by_zero() {
        let mut limiter = Limiter::new(0.0, 1, SR);
        let out = through(&mut limiter, &vec![1.0; 500]);
        assert!(out.iter().all(|s| s.is_finite()), "the limiter went non-finite");
    }

    #[test]
    fn the_sliding_maximum_agrees_with_scanning_the_window() {
        // The queue is the one piece of cleverness here; everything else trusts
        // its answer, so it is checked against the obvious version.
        const WINDOW: usize = 9;
        let mut sliding = SlidingMax::new(WINDOW);
        let input: Vec<f32> = (0..500).map(|i| ((i * 37 % 23) as f32 - 11.0) / 7.0).collect();

        for (i, value) in input.iter().enumerate() {
            let naive = input[i.saturating_sub(WINDOW - 1)..=i].iter().fold(f32::MIN, |a, b| a.max(*b));
            assert_eq!(sliding.push(*value), naive, "at {i}");
        }
    }
}
