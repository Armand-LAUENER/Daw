use std::fmt;

use crate::{AudioBuffer, Node};

/// Where a node sits in a graph under construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(usize);

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
        let (nodes, inputs): (Vec<_>, Vec<_>) = self.entries.into_iter().map(|e| (e.node, e.inputs)).unzip();

        Ok(Graph {
            nodes,
            inputs,
            order,
            buffers: vec![vec![0.0; channels * max_frames]; count],
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
    use crate::Gain;

    const SR: f32 = 48_000.0;

    /// Writes a constant into every channel, standing in for a track.
    struct Tone(f32);

    impl Node for Tone {
        fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
            for channel in buffer.channels_mut() {
                channel.fill(self.0);
            }
        }
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
}
