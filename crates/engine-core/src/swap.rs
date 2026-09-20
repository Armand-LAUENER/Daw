use rtrb::{Consumer, Producer, RingBuffer};

use crate::Graph;

/// How long the crossfade between two graphs lasts.
///
/// A new graph starts with cold nodes — filters empty, ramps at rest — so its
/// first samples rarely line up with what the old one was producing. Swapping
/// outright would step by the difference. Both run for this long instead, one
/// fading down as the other comes up.
pub const SWAP_FADE_MS: f32 = 10.0;

/// How many graphs may be in flight in either direction at once.
const QUEUE_CAPACITY: usize = 8;

/// The control side of a swap: hands new graphs over, takes retired ones back.
///
/// Taking them back matters. Dropping a graph frees every buffer in it, and
/// that is allocator work: doing it on the audio thread would be exactly the
/// pause the whole design exists to avoid.
pub struct GraphSwapper {
    outgoing: Producer<Box<Graph>>,
    returned: Consumer<Box<Graph>>,
}

impl GraphSwapper {
    /// Queues `graph` to replace whatever is playing.
    ///
    /// Gives it back if the queue is full, which means the audio thread has not
    /// picked up the previous ones yet.
    pub fn swap(&mut self, graph: Box<Graph>) -> Result<(), Box<Graph>> {
        self.outgoing.push(graph).map_err(|rtrb::PushError::Full(graph)| graph)
    }

    /// Takes back one retired graph, to be dropped here rather than there.
    ///
    /// Call it periodically; anything still queued is freed when this is
    /// dropped, which is also off the audio thread.
    pub fn collect(&mut self) -> Option<Box<Graph>> {
        self.returned.pop().ok()
    }

    /// Whether a graph handed over earlier has finished being replaced.
    pub fn has_retired(&self) -> bool {
        !self.returned.is_empty()
    }
}

/// The audio thread's side: runs the current graph and crossfades to new ones.
pub struct GraphPlayer {
    current: Option<Box<Graph>>,
    /// The graph being faded out, still running until the fade ends.
    previous: Option<Box<Graph>>,
    /// Retired graph waiting for room in the return queue.
    pending_return: Option<Box<Graph>>,
    fade_remaining: u32,
    fade_length: u32,
    /// Where the outgoing graph renders during a fade. Allocated up front.
    scratch: Vec<f32>,
    incoming: Consumer<Box<Graph>>,
    retiring: Producer<Box<Graph>>,
    channels: usize,
}

impl GraphPlayer {
    /// Fills `out` from the current graph, picking up a new one if one arrived.
    ///
    /// Writes silence when no graph has been handed over yet. Real-time safe:
    /// no allocation, no locking, no I/O.
    pub fn process(&mut self, out: &mut [f32]) {
        self.return_retired();
        self.take_incoming();

        match (&mut self.current, &mut self.previous) {
            (None, _) => out.fill(0.0),
            (Some(current), None) => current.process(out),
            (Some(current), Some(previous)) => {
                current.process(out);
                let scratch = &mut self.scratch[..out.len()];
                previous.process(scratch);

                // Equal-gain rather than equal-power: the two graphs usually
                // render the same material, so they add coherently and a
                // constant-power pair would bulge by 3 dB through the middle.
                let channels = self.channels;
                let pairs = out.chunks_exact_mut(channels).zip(scratch.chunks_exact(channels));
                for (frame, (new, old)) in pairs.enumerate() {
                    let remaining = self.fade_remaining.saturating_sub(frame as u32);
                    let old_gain = remaining as f32 / self.fade_length as f32;
                    for (n, o) in new.iter_mut().zip(old) {
                        *n = *n * (1.0 - old_gain) + o * old_gain;
                    }
                }

                let frames = (out.len() / channels) as u32;
                self.fade_remaining = self.fade_remaining.saturating_sub(frames);
                if self.fade_remaining == 0 {
                    self.pending_return = self.previous.take();
                }
            }
        }
    }

    /// Whether a crossfade is under way, so two graphs are running at once.
    pub fn is_swapping(&self) -> bool {
        self.previous.is_some()
    }

    fn take_incoming(&mut self) {
        // One at a time: starting a second fade before the first finished would
        // leave a graph with nowhere to go but this thread's allocator.
        if self.previous.is_some() || self.pending_return.is_some() {
            return;
        }
        let Ok(graph) = self.incoming.pop() else {
            return;
        };
        if let Some(old) = self.current.replace(graph) {
            self.previous = Some(old);
            self.fade_remaining = self.fade_length;
        }
    }

    fn return_retired(&mut self) {
        if let Some(graph) = self.pending_return.take()
            && let Err(rtrb::PushError::Full(graph)) = self.retiring.push(graph)
        {
            // No room yet; hold on to it rather than freeing it here.
            self.pending_return = Some(graph);
        }
    }
}

/// Builds the two ends of a swap.
///
/// `initial` is what plays until the first handover; `None` means silence.
/// Every buffer either side needs is allocated here, on the control thread.
pub fn graph_channel(
    initial: Option<Box<Graph>>,
    channels: usize,
    max_frames: usize,
    sample_rate: f32,
) -> (GraphSwapper, GraphPlayer) {
    let (outgoing, incoming) = RingBuffer::new(QUEUE_CAPACITY);
    let (retiring, returned) = RingBuffer::new(QUEUE_CAPACITY);
    let fade_length = (SWAP_FADE_MS * 1e-3 * sample_rate).max(1.0) as u32;

    let player = GraphPlayer {
        current: initial,
        previous: None,
        pending_return: None,
        fade_remaining: 0,
        fade_length,
        scratch: vec![0.0; channels * max_frames],
        incoming,
        retiring,
        channels,
    };
    (GraphSwapper { outgoing, returned }, player)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioBuffer, GraphBuilder, Node};

    const SR: f32 = 48_000.0;
    const CHANNELS: usize = 2;
    const MAX_FRAMES: usize = 512;
    /// The fade covers full scale in `SWAP_FADE_MS`, so this is its steepest step.
    const MAX_FADE_STEP: f32 = 1.0 / (SWAP_FADE_MS * 1e-3 * SR) * 1.001 + 1e-6;

    struct Constant(f32);

    impl Node for Constant {
        fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
            for channel in buffer.channels_mut() {
                channel.fill(self.0);
            }
        }
    }

    fn graph(level: f32) -> Box<Graph> {
        let mut builder = GraphBuilder::new();
        let node = builder.add(Box::new(Constant(level)));
        builder.output(node);
        Box::new(builder.build(CHANNELS, MAX_FRAMES).unwrap())
    }

    fn play(player: &mut GraphPlayer, blocks: usize, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * CHANNELS];
        let mut all = Vec::new();
        for _ in 0..blocks {
            player.process(&mut out);
            all.extend_from_slice(&out);
        }
        all
    }

    #[test]
    fn silence_until_a_graph_arrives() {
        let (_swapper, mut player) = graph_channel(None, CHANNELS, MAX_FRAMES, SR);
        assert!(play(&mut player, 2, 64).iter().all(|s| *s == 0.0));
    }

    #[test]
    fn the_first_graph_starts_without_a_fade() {
        // There is nothing to fade from, so it should be at full level at once.
        let (mut swapper, mut player) = graph_channel(None, CHANNELS, MAX_FRAMES, SR);
        swapper.swap(graph(0.5)).unwrap();
        let played = play(&mut player, 1, 64);
        assert!(played.iter().all(|s| (*s - 0.5).abs() < 1e-6), "{:?}", &played[..8]);
    }

    #[test]
    fn swapping_crossfades_instead_of_stepping() {
        // Two graphs as far apart as they get: silence against full scale.
        let (mut swapper, mut player) = graph_channel(Some(graph(0.0)), CHANNELS, MAX_FRAMES, SR);
        let before = play(&mut player, 1, 256);
        swapper.swap(graph(1.0)).unwrap();
        let after = play(&mut player, 8, 256);

        let mut whole = before;
        whole.extend_from_slice(&after);
        let left: Vec<f32> = whole.iter().step_by(CHANNELS).copied().collect();
        for (i, w) in left.windows(2).enumerate() {
            let step = (w[1] - w[0]).abs();
            assert!(step <= MAX_FADE_STEP, "step {step} at frame {i}, limit {MAX_FADE_STEP}");
        }
        assert!(left[left.len() - 1] > 0.99, "the new graph never arrived: {}", left[left.len() - 1]);
    }

    #[test]
    fn the_retired_graph_comes_back_to_be_dropped() {
        let (mut swapper, mut player) = graph_channel(Some(graph(0.25)), CHANNELS, MAX_FRAMES, SR);
        swapper.swap(graph(0.75)).unwrap();
        // Long enough for the fade to finish and the return to go through.
        play(&mut player, 8, 256);
        assert!(swapper.has_retired(), "the old graph was not handed back");
        assert!(swapper.collect().is_some());
        assert!(!player.is_swapping());
    }

    #[test]
    fn a_second_swap_waits_for_the_first_to_finish() {
        // Taking a second graph mid-fade would leave the outgoing one with
        // nowhere to go but this thread's allocator.
        let (mut swapper, mut player) = graph_channel(Some(graph(0.0)), CHANNELS, MAX_FRAMES, SR);
        swapper.swap(graph(0.5)).unwrap();
        swapper.swap(graph(1.0)).unwrap();

        play(&mut player, 1, 64);
        assert!(player.is_swapping());
        play(&mut player, 40, 64);
        // Both handovers land in the end, and both old graphs come back.
        assert!(swapper.collect().is_some());
        assert!(swapper.collect().is_some());
    }

    #[test]
    fn a_full_queue_hands_the_graph_back() {
        let (mut swapper, _player) = graph_channel(None, CHANNELS, MAX_FRAMES, SR);
        for _ in 0..QUEUE_CAPACITY {
            swapper.swap(graph(0.1)).unwrap();
        }
        assert!(swapper.swap(graph(0.1)).is_err(), "an overfull queue should refuse rather than block");
    }
}
