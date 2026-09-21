use std::fmt;

use crate::{AudioBuffer, AutomationQueue, Node, ParamId};

/// Where a node sits in a graph under construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(usize);

impl NodeId {
    /// Where the node sits in the graph. Automation travels as plain data, so
    /// an event has to carry this rather than a borrow of the node.
    pub fn index(self) -> usize {
        self.0
    }

    /// Rebuilds an id from an index, for callers holding one on its own.
    ///
    /// An index that names no node addresses nothing: the graph ignores events
    /// it cannot place rather than guessing at a neighbour.
    pub fn from_index(index: usize) -> Self {
        Self(index)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum GraphError {
    /// The routing loops back on itself, so no order can satisfy it.
    Cycle,
    /// No node was named as the output.
    NoOutput,
    /// A connection named a node that does not exist.
    UnknownNode(usize),
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cycle => write!(f, "the routing contains a cycle"),
            Self::NoOutput => write!(f, "the graph has no output node"),
            Self::UnknownNode(id) => write!(f, "connection refers to unknown node {id}"),
        }
    }
}

impl std::error::Error for GraphError {}

struct Entry {
    node: Box<dyn Node>,
    inputs: Vec<usize>,
}

/// Assembles a graph. Lives on the control side, where allocating and failing
/// are both fine; what it produces is what the audio thread runs.
#[derive(Default)]
pub struct GraphBuilder {
    entries: Vec<Entry>,
    output: Option<usize>,
}

impl GraphBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, node: Box<dyn Node>) -> NodeId {
        self.entries.push(Entry { node, inputs: Vec::new() });
        NodeId(self.entries.len() - 1)
    }

    /// Feeds `from` into `to`. A node with several inputs sums them.
    pub fn connect(&mut self, from: NodeId, to: NodeId) -> &mut Self {
        if let Some(entry) = self.entries.get_mut(to.0) {
            entry.inputs.push(from.0);
        }
        self
    }

    /// Names the node whose buffer becomes the graph's output.
    pub fn output(&mut self, node: NodeId) -> &mut Self {
        self.output = Some(node.0);
        self
    }

    /// Works out the evaluation order and allocates the buffers.
    ///
    /// All of the expensive and fallible work happens here, off the audio
    /// thread. What comes back only has to be run.
    pub fn build(self, channels: usize, max_frames: usize) -> Result<Graph, GraphError> {
        let output = self.output.ok_or(GraphError::NoOutput)?;
        if output >= self.entries.len() {
            return Err(GraphError::UnknownNode(output));
        }
        for entry in &self.entries {
            for &input in &entry.inputs {
                if input >= self.entries.len() {
                    return Err(GraphError::UnknownNode(input));
                }
            }
        }

        let order = topological_order(&self.entries)?;
        let count = self.entries.len();
        let latency = path_latency(&self.entries, &order, output);
        let (nodes, inputs): (Vec<_>, Vec<_>) = self.entries.into_iter().map(|e| (e.node, e.inputs)).unzip();

        Ok(Graph {
            nodes,
            inputs,
            order,
            latency,
            buffers: vec![vec![0.0; channels * max_frames]; count],
            frame: 0,
            channels,
            max_frames,
            output,
        })
    }
}

/// Kahn's algorithm: repeatedly take a node whose inputs are all already
/// placed. Anything left over is part of a cycle.
fn topological_order(entries: &[Entry]) -> Result<Vec<usize>, GraphError> {
    let mut remaining: Vec<usize> = entries.iter().map(|e| e.inputs.len()).collect();
    let mut ready: Vec<usize> = (0..entries.len()).filter(|&i| remaining[i] == 0).collect();
    let mut order = Vec::with_capacity(entries.len());

    while let Some(node) = ready.pop() {
        order.push(node);
        for (index, entry) in entries.iter().enumerate() {
            for &input in &entry.inputs {
                if input == node {
                    remaining[index] -= 1;
                    if remaining[index] == 0 {
                        ready.push(index);
                    }
                }
            }
        }
    }

    if order.len() == entries.len() { Ok(order) } else { Err(GraphError::Cycle) }
}

/// Delay on the longest path from a source to `output`.
///
/// Walking in topological order means a node's inputs are always settled by the
/// time it is reached, so one pass is enough.
fn path_latency(entries: &[Entry], order: &[usize], output: usize) -> usize {
    let mut latency = vec![0usize; entries.len()];
    for &index in order {
        let upstream = entries[index].inputs.iter().map(|&input| latency[input]).max().unwrap_or(0);
        latency[index] = upstream + entries[index].node.latency_frames();
    }
    latency[output]
}

/// A routed set of nodes, ready to run.
///
/// Built on the control side and handed over whole. The audio thread never
/// edits one: a change of routing produces a new graph that replaces this one,
/// so what is running is never modified underneath itself.
pub struct Graph {
    nodes: Vec<Box<dyn Node>>,
    inputs: Vec<Vec<usize>>,
    order: Vec<usize>,
    /// One scratch buffer per node, sized for the largest callback.
    buffers: Vec<Vec<f32>>,
    channels: usize,
    max_frames: usize,
    output: usize,
    /// Frames of delay on the longest path to the output.
    latency: usize,
    /// Where the next block sits on the engine's timeline.
    frame: u64,
}

impl fmt::Debug for Graph {
    /// Nodes are opaque, so this reports the shape instead: how many there are,
    /// what order they run in, and what the buffers were sized for.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Graph")
            .field("nodes", &self.nodes.len())
            .field("order", &self.order)
            .field("output", &self.output)
            .field("channels", &self.channels)
            .field("max_frames", &self.max_frames)
            .field("latency", &self.latency)
            .finish()
    }
}

impl Graph {
    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn max_frames(&self) -> usize {
        self.max_frames
    }

    /// Runs every node in order and writes the output node's buffer to `out`,
    /// interleaved.
    ///
    /// # Panics
    ///
    /// If `out` holds more frames than the graph was built for. The caller
    /// chose that size; truncating quietly would drop audio.
    pub fn process(&mut self, out: &mut [f32]) {
        let frames = out.len() / self.channels;
        assert!(frames <= self.max_frames, "callback of {frames} frames exceeds the graph's {}", self.max_frames);
        let used = frames * self.channels;

        for node in &mut self.nodes {
            node.set_frame(self.frame);
        }

        for step in 0..self.order.len() {
            let index = self.order[step];
            // Lifting the buffer out lets this node's inputs be read while it
            // is written. Swapping a Vec moves three words and allocates nothing.
            let mut owned = std::mem::take(&mut self.buffers[index]);
            {
                let mut buffer = AudioBuffer::new(&mut owned[..used], self.channels);
                buffer.silence();
                for &input in &self.inputs[index] {
                    let source = AudioBuffer::new(&mut self.buffers[input][..used], self.channels);
                    buffer.add(&source);
                }
                self.nodes[index].process(&mut buffer);
            }
            self.buffers[index] = owned;
        }

        AudioBuffer::new(&mut self.buffers[self.output][..used], self.channels).write_interleaved(out);
        self.frame += u64::from(frames as u32);
    }

    /// Places the next block on the engine's timeline.
    ///
    /// Without it the graph keeps its own count from zero, which is right for
    /// something rendered on its own and wrong for a graph swapped in halfway
    /// through a session.
    pub fn set_frame(&mut self, frame: u64) {
        self.frame = frame;
    }

    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Runs the block, stopping at each scheduled change so it lands on its
    /// own frame rather than at the edge of the callback.
    ///
    /// `start_frame` is where `out` begins on the engine's timeline. Events
    /// already overdue apply before the first sample; events past the end of
    /// the block stay queued for the next one.
    ///
    /// Splitting costs one extra pass over the graph per event. Automation is
    /// sparse next to the sample rate, so that is cheaper than the alternative
    /// of handing every node its own event list.
    pub fn process_automated(&mut self, out: &mut [f32], events: &mut AutomationQueue, start_frame: u64) {
        let frames = out.len() / self.channels;
        let mut done = 0;

        while done < frames {
            let nodes = &mut self.nodes;
            let end = events.span(start_frame, done, frames, |event| {
                if let Some(node) = nodes.get_mut(event.param.node.index()) {
                    node.set_param(event.param.param, event.value);
                }
            });
            self.frame = start_frame + done as u64;
            self.process(&mut out[done * self.channels..end * self.channels]);
            done = end;
        }
    }

    /// How many frames the graph delays its output by.
    ///
    /// The longest path to the output, since a node cannot produce its first
    /// sample before everything feeding it has. Worked out when the graph is
    /// built, where walking it is free.
    ///
    /// This reports the delay; it does not correct it. Nothing here aligns a
    /// path that bypasses a latent node against one that goes through it, so a
    /// graph with parallel routes of different depth will have them drift apart
    /// by the difference.
    pub fn latency_frames(&self) -> usize {
        self.latency
    }

    /// Applies one change now. Events for a node that is not in this graph are
    /// dropped: a graph swap can retire the node an event was aimed at.
    pub fn set_param(&mut self, param: ParamId, value: f32) {
        if let Some(node) = self.nodes.get_mut(param.node.index()) {
            node.set_param(param.param, value);
        }
    }

    pub fn reset(&mut self) {
        for node in &mut self.nodes {
            node.reset();
        }
        for buffer in &mut self.buffers {
            buffer.fill(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gain, PARAM_RAMP_MS, ParamEvent, automation_channel};

    const SR: f32 = 48_000.0;

    /// Writes a constant into every channel, standing in for a track.
    ///
    /// Its level answers automation without a ramp, so a test can read the
    /// exact sample a change landed on instead of the middle of a slope.
    struct Tone(f32);

    /// The only parameter `Tone` has.
    const TONE_LEVEL: u32 = 0;

    impl Node for Tone {
        fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
            for channel in buffer.channels_mut() {
                channel.fill(self.0);
            }
        }

        fn set_param(&mut self, param: u32, value: f32) {
            if param == TONE_LEVEL {
                self.0 = value;
            }
        }
    }

    /// A one-node graph whose level is automatable, plus that level's address.
    fn tone_graph() -> (Graph, ParamId) {
        let mut builder = GraphBuilder::new();
        let tone = builder.add(Box::new(Tone(1.0)));
        builder.output(tone);
        (builder.build(1, 512).unwrap(), ParamId::new(tone, TONE_LEVEL))
    }

    #[test]
    fn a_single_node_reaches_the_output() {
        let mut builder = GraphBuilder::new();
        let tone = builder.add(Box::new(Tone(0.25)));
        builder.output(tone);
        let mut graph = builder.build(2, 64).unwrap();

        let mut out = [0.0; 8];
        graph.process(&mut out);
        assert_eq!(out, [0.25; 8]);
    }

    #[test]
    fn two_tracks_sum_into_a_bus() {
        let mut builder = GraphBuilder::new();
        let a = builder.add(Box::new(Tone(0.25)));
        let b = builder.add(Box::new(Tone(0.5)));
        // A unity gain standing in for a bus: it only sums what feeds it.
        let bus = builder.add(Box::new(Gain::new(1.0, SR)));
        builder.connect(a, bus).connect(b, bus).output(bus);
        let mut graph = builder.build(2, 64).unwrap();

        let mut out = [0.0; 8];
        graph.process(&mut out);
        assert!(out.iter().all(|s| (*s - 0.75).abs() < 1e-6), "{out:?}");
    }

    #[test]
    fn a_node_runs_after_everything_that_feeds_it() {
        // The gain must see the tone rather than the silence it started as,
        // which only holds if the order puts the tone first.
        let mut builder = GraphBuilder::new();
        let tone = builder.add(Box::new(Tone(1.0)));
        let gain = builder.add(Box::new(Gain::new(0.5, SR)));
        builder.connect(tone, gain).output(gain);
        let mut graph = builder.build(1, 64).unwrap();

        let mut out = [0.0; 16];
        graph.process(&mut out);
        assert!(out.iter().all(|s| (*s - 0.5).abs() < 1e-6), "{out:?}");
    }

    #[test]
    fn a_branch_that_leads_nowhere_does_not_reach_the_output() {
        let mut builder = GraphBuilder::new();
        let heard = builder.add(Box::new(Tone(0.25)));
        let _orphan = builder.add(Box::new(Tone(1.0)));
        builder.output(heard);
        let mut graph = builder.build(1, 64).unwrap();

        let mut out = [0.0; 8];
        graph.process(&mut out);
        assert_eq!(out, [0.25; 8]);
    }

    #[test]
    fn a_cycle_is_refused_rather_than_hanging() {
        let mut builder = GraphBuilder::new();
        let a = builder.add(Box::new(Gain::new(1.0, SR)));
        let b = builder.add(Box::new(Gain::new(1.0, SR)));
        builder.connect(a, b).connect(b, a).output(b);
        assert_eq!(builder.build(2, 64).unwrap_err(), GraphError::Cycle);
    }

    #[test]
    fn a_graph_with_no_output_is_refused() {
        let mut builder = GraphBuilder::new();
        builder.add(Box::new(Tone(1.0)));
        assert_eq!(builder.build(2, 64).unwrap_err(), GraphError::NoOutput);
    }

    #[test]
    fn a_short_callback_is_laid_out_for_its_own_length() {
        // The planar stride is the frame count, so a buffer built for 64 frames
        // and run with 4 has to be read with a stride of 4, not 64.
        let mut builder = GraphBuilder::new();
        let tone = builder.add(Box::new(Tone(0.5)));
        let gain = builder.add(Box::new(Gain::new(1.0, SR)));
        builder.connect(tone, gain).output(gain);
        let mut graph = builder.build(2, 64).unwrap();

        let mut long = [0.0; 128];
        graph.process(&mut long);
        let mut short = [0.0; 8];
        graph.process(&mut short);
        assert!(short.iter().all(|s| (*s - 0.5).abs() < 1e-6), "{short:?}");
    }

    #[test]
    #[should_panic(expected = "exceeds the graph")]
    fn a_callback_larger_than_the_graph_is_refused() {
        let mut builder = GraphBuilder::new();
        let tone = builder.add(Box::new(Tone(1.0)));
        builder.output(tone);
        let mut graph = builder.build(2, 8).unwrap();
        let mut out = [0.0; 64];
        graph.process(&mut out);
    }

    #[test]
    fn a_change_lands_on_its_own_frame_not_the_block_edge() {
        // The whole point of splitting the block: at 48 kHz a callback is
        // several milliseconds, and rounding every change to that boundary is
        // audible as soon as anything is played in time.
        let (mut graph, level) = tone_graph();
        let (mut sender, mut queue) = automation_channel(8);
        sender.send(ParamEvent::new(level, 0.25, 100)).unwrap();

        let mut out = [0.0; 256];
        graph.process_automated(&mut out, &mut queue, 0);

        assert!(out[..100].iter().all(|s| *s == 1.0), "the change arrived early");
        assert!(out[100..].iter().all(|s| *s == 0.25), "the change arrived late");
    }

    #[test]
    fn a_change_is_placed_against_the_timeline_not_the_block() {
        let (mut graph, level) = tone_graph();
        let (mut sender, mut queue) = automation_channel(8);
        sender.send(ParamEvent::new(level, 0.5, 1_000)).unwrap();

        let mut out = [0.0; 256];
        graph.process_automated(&mut out, &mut queue, 512);
        assert!(out.iter().all(|s| *s == 1.0), "an event 1000 frames in fired during 512..768");

        graph.process_automated(&mut out, &mut queue, 768);
        assert!(out[..232].iter().all(|s| *s == 1.0));
        assert!(out[232..].iter().all(|s| *s == 0.5), "frame 1000 is 232 into this block");
    }

    #[test]
    fn several_changes_in_one_block_keep_their_order() {
        let (mut graph, level) = tone_graph();
        let (mut sender, mut queue) = automation_channel(8);
        for (frame, value) in [(10u64, 0.8), (50, 0.6), (200, 0.4)] {
            sender.send(ParamEvent::new(level, value, frame)).unwrap();
        }

        let mut out = [0.0; 256];
        graph.process_automated(&mut out, &mut queue, 0);

        assert_eq!((out[9], out[10]), (1.0, 0.8));
        assert_eq!((out[49], out[50]), (0.8, 0.6));
        assert_eq!((out[199], out[200]), (0.6, 0.4));
    }

    #[test]
    fn an_overdue_change_applies_before_the_first_sample() {
        // Anything the control thread sent without a schedule, and anything a
        // slow block left behind, has to take effect rather than be skipped.
        let (mut graph, level) = tone_graph();
        let (mut sender, mut queue) = automation_channel(8);
        sender.send(ParamEvent::new(level, 0.1, 5)).unwrap();

        let mut out = [0.0; 64];
        graph.process_automated(&mut out, &mut queue, 1_000);
        assert!(out.iter().all(|s| *s == 0.1), "{:?}", &out[..4]);
    }

    #[test]
    fn a_change_aimed_at_a_missing_node_is_dropped() {
        // Ids come from the builder that made the graph, and a swap can retire
        // the node one was aimed at. Applying it to whatever now holds that
        // index would move the wrong fader.
        let (mut graph, _) = tone_graph();
        let (mut sender, mut queue) = automation_channel(8);
        sender.send(ParamEvent::new(ParamId::new(NodeId::from_index(99), 0), 0.0, 0)).unwrap();

        let mut out = [0.0; 64];
        graph.process_automated(&mut out, &mut queue, 0);
        assert!(out.iter().all(|s| *s == 1.0));
    }

    #[test]
    fn an_empty_queue_renders_the_block_whole() {
        let (mut graph, _) = tone_graph();
        let (_sender, mut queue) = automation_channel(8);
        let mut out = [0.0; 256];
        graph.process_automated(&mut out, &mut queue, 0);
        assert!(out.iter().all(|s| *s == 1.0));
    }

    #[test]
    fn automating_a_gain_still_ramps() {
        // Splitting the block must not restart or skip the node's own ramp: the
        // two halves have to join without a step.
        let mut builder = GraphBuilder::new();
        let tone = builder.add(Box::new(Tone(1.0)));
        let gain = builder.add(Box::new(Gain::new(1.0, SR)));
        builder.connect(tone, gain).output(gain);
        let mut graph = builder.build(1, 1_024).unwrap();

        let (mut sender, mut queue) = automation_channel(8);
        sender.send(ParamEvent::new(ParamId::new(gain, Gain::PARAM_GAIN), 0.0, 37)).unwrap();

        let mut out = [0.0; 960];
        graph.process_automated(&mut out, &mut queue, 0);

        let max_step = 1.0 / (PARAM_RAMP_MS * 1e-3 * SR) * 1.001;
        for (i, w) in out.windows(2).enumerate() {
            assert!((w[1] - w[0]).abs() <= max_step, "step at {i}: {} -> {}", w[0], w[1]);
        }
        assert_eq!(out[959], 0.0, "the ramp never arrived");
    }

    #[test]
    fn a_graph_of_plain_nodes_has_no_latency() {
        let (graph, _) = tone_graph();
        assert_eq!(graph.latency_frames(), 0);
    }

    #[test]
    fn latency_adds_up_along_a_chain() {
        use crate::Limiter;
        let mut builder = GraphBuilder::new();
        let tone = builder.add(Box::new(Tone(1.0)));
        let first = builder.add(Box::new(Limiter::new(0.9, 1, SR)));
        let second = builder.add(Box::new(Limiter::new(0.9, 1, SR)));
        builder.connect(tone, first).connect(first, second).output(second);
        let graph = builder.build(1, 512).unwrap();

        let one = Limiter::new(0.9, 1, SR).latency_frames();
        assert_eq!(graph.latency_frames(), one * 2, "two in series delay by both");
    }

    #[test]
    fn parallel_paths_report_the_longest_not_the_sum() {
        // Two routes into one bus: the bus cannot produce a sample before the
        // slowest of them has, and no sooner than that either.
        use crate::Limiter;
        let mut builder = GraphBuilder::new();
        let tone = builder.add(Box::new(Tone(1.0)));
        let direct = builder.add(Box::new(Gain::new(1.0, SR)));
        let latent = builder.add(Box::new(Limiter::new(0.9, 1, SR)));
        let bus = builder.add(Box::new(Gain::new(0.5, SR)));
        builder.connect(tone, direct).connect(tone, latent);
        builder.connect(direct, bus).connect(latent, bus).output(bus);
        let graph = builder.build(1, 512).unwrap();

        assert_eq!(graph.latency_frames(), Limiter::new(0.9, 1, SR).latency_frames());
    }
}
