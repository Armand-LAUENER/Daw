use rtrb::{Consumer, Producer, RingBuffer};

use crate::{AutomationQueue, Graph};

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

    /// Fills `out`, breaking the block at each scheduled change so it lands on
    /// its own frame.
    ///
    /// Changes go to the graph that is playing. During a crossfade the outgoing
    /// graph keeps whatever settings it had: the two graphs were built
    /// separately, so a node id means different things in each, and applying an
    /// event to both would move something unrelated. The outgoing graph has
    /// `SWAP_FADE_MS` left to live, so what it misses is inaudible.
    pub fn process_automated(&mut self, out: &mut [f32], events: &mut AutomationQueue, start_frame: u64) {
        // Pick up a waiting graph before any event is applied, not after. A
        // change scheduled for the same block as a swap is meant for the graph
        // taking over; leaving this to `process` would land it on the one on
        // its way out, which is then faded away with the change on it.
        self.return_retired();
        self.take_incoming();

        let frames = out.len() / self.channels;
        let mut done = 0;

        while done < frames {
            let current = &mut self.current;
            let end = events.span(start_frame, done, frames, |event| {
                if let Some(graph) = current {
                    graph.set_param(event.param, event.value);
                }
            });
            self.process(&mut out[done * self.channels..end * self.channels]);
            done = end;
        }
    }

    /// How many frames the playing graph delays its output by, or zero before
    /// one has arrived. Changes when a graph of a different shape is swapped in.
    pub fn latency_frames(&self) -> usize {
        self.current.as_ref().map_or(0, |graph| graph.latency_frames())
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
    use crate::{AudioBuffer, GraphBuilder, Limiter, Node, NodeId, ParamEvent, ParamId, automation_channel};

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

    /// A one-node graph whose level answers automation without a ramp, so a
    /// test can read the exact sample a change landed on.
    fn levelled_graph(level: f32) -> (Box<Graph>, ParamId) {
        struct Level(f32);
        impl Node for Level {
            fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
                for channel in buffer.channels_mut() {
                    channel.fill(self.0);
                }
            }
            fn set_param(&mut self, _param: u32, value: f32) {
                self.0 = value;
            }
        }
        let mut builder = GraphBuilder::new();
        let node = builder.add(Box::new(Level(level)));
        builder.output(node);
        (Box::new(builder.build(CHANNELS, MAX_FRAMES).unwrap()), ParamId::new(node, 0))
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

    #[test]
    fn the_player_places_a_change_on_its_own_frame() {
        let (graph, level) = levelled_graph(1.0);
        let (_swapper, mut player) = graph_channel(Some(graph), CHANNELS, MAX_FRAMES, SR);
        let (mut sender, mut queue) = automation_channel(8);
        sender.send(ParamEvent::new(level, 0.25, 100)).unwrap();

        let mut out = vec![0.0; 256 * CHANNELS];
        player.process_automated(&mut out, &mut queue, 0);

        assert!(out[..100 * CHANNELS].iter().all(|s| *s == 1.0), "the change arrived early");
        assert!(out[100 * CHANNELS..].iter().all(|s| *s == 0.25), "the change arrived late");
    }

    #[test]
    fn a_change_during_a_fade_goes_to_the_graph_taking_over() {
        // The two graphs were built separately, so a node id means something
        // different in each. Applying an event to both would move whatever
        // happened to sit at that index in the outgoing one.
        let (first, _) = levelled_graph(1.0);
        let (mut swapper, mut player) = graph_channel(Some(first), CHANNELS, MAX_FRAMES, SR);
        let (mut sender, mut queue) = automation_channel(8);

        let mut out = vec![0.0; 64 * CHANNELS];
        player.process_automated(&mut out, &mut queue, 0);

        let (second, level) = levelled_graph(0.0);
        swapper.swap(second).map_err(|_| "the queue was empty").unwrap();
        sender.send(ParamEvent::new(level, 0.5, 64)).unwrap();
        player.process_automated(&mut out, &mut queue, 64);

        assert!(player.is_swapping(), "the fade should still be running");
        // The incoming graph moved to 0.5 and the outgoing one is still at 1.0,
        // so the mix sits between them rather than at either.
        let last = out[out.len() - 1];
        assert!(last > 0.5 && last < 1.0, "expected a blend of 0.5 and 1.0, got {last}");
    }

    #[test]
    fn silence_takes_automation_without_complaint() {
        // Events can be in flight when no graph has arrived yet. Dropping them
        // is right; panicking on them is not.
        let (_swapper, mut player) = graph_channel(None, CHANNELS, MAX_FRAMES, SR);
        let (mut sender, mut queue) = automation_channel(8);
        sender.send(ParamEvent::new(ParamId::new(NodeId::from_index(0), 0), 0.5, 10)).unwrap();

        let mut out = vec![1.0; 64 * CHANNELS];
        player.process_automated(&mut out, &mut queue, 0);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn the_player_reports_the_latency_of_what_is_playing() {
        let mut builder = GraphBuilder::new();
        let node = builder.add(Box::new(Constant(0.5)));
        let ceiling = builder.add(Box::new(Limiter::new(0.9, CHANNELS, SR)));
        builder.connect(node, ceiling).output(ceiling);
        let latent = Box::new(builder.build(CHANNELS, MAX_FRAMES).unwrap());
        let expected = latent.latency_frames();
        assert!(expected > 0, "the limiter reported no lookahead");

        let (_swapper, silent) = graph_channel(None, CHANNELS, MAX_FRAMES, SR);
        assert_eq!(silent.latency_frames(), 0, "silence has no latency to report");

        let (_swapper, player) = graph_channel(Some(latent), CHANNELS, MAX_FRAMES, SR);
        assert_eq!(player.latency_frames(), expected);
    }
}
