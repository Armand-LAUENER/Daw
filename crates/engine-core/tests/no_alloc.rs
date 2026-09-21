//! The graph runs on the audio thread, so running it must not allocate.
//!
//! This is what the buffer design is for. Each node owns a scratch buffer
//! allocated when the graph was built, and processing lifts it out with
//! `mem::take` so the node's inputs can be read while it is written — three
//! words moved, nothing asked of the allocator. A `Vec::clone` slipped in
//! anywhere along that path aborts this test rather than showing up months
//! later as an xrun under load.
#![cfg(debug_assertions)]

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use engine_core::{
    AudioBuffer, Gain, GraphBuilder, Limiter, Mute, Node, NodeId, Pan, ParamEvent, ParamId, Solo, SoloBus,
    automation_channel, graph_channel,
};

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

const SR: f32 = 48_000.0;
const CHANNELS: usize = 2;
const MAX_FRAMES: usize = 1_024;

/// Stands in for a track: writes a tone without touching the outside world.
struct Tone {
    phase: f32,
    step: f32,
}

impl Node for Tone {
    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        let frames = buffer.frames();
        for channel in 0..buffer.channels() {
            let mut phase = self.phase;
            for sample in buffer.channel_mut(channel).iter_mut().take(frames) {
                *sample = (phase * std::f32::consts::TAU).sin();
                phase = (phase + self.step).fract();
            }
        }
        self.phase = (self.phase + self.step * frames as f32).fract();
    }
}

#[test]
fn processing_a_graph_does_not_allocate() {
    // Eight tracks, each with gain, pan and mute, summed into a master — the
    // shape phase 2 exists to reach.
    let mut builder = GraphBuilder::new();
    let bus = SoloBus::new();
    let master = builder.add(Box::new(Gain::new(0.5, SR)));

    for track in 0..8 {
        let tone = builder.add(Box::new(Tone { phase: 0.0, step: (110.0 + track as f32 * 55.0) / SR }));
        let gain = builder.add(Box::new(Gain::new(0.8, SR)));
        let pan = builder.add(Box::new(Pan::new(-1.0 + track as f32 * 0.25, SR)));
        let mute = builder.add(Box::new(Mute::new(false, SR)));
        let solo = builder.add(Box::new(Solo::new(&bus, false, SR)));
        builder.connect(tone, gain).connect(gain, pan).connect(pan, mute).connect(mute, solo).connect(solo, master);
    }
    let ceiling = builder.add(Box::new(Limiter::new(0.9, CHANNELS, SR)));
    builder.connect(master, ceiling).output(ceiling);
    let mut graph = builder.build(CHANNELS, MAX_FRAMES).unwrap();

    let mut out = vec![0.0f32; MAX_FRAMES * CHANNELS];
    for i in 0..200usize {
        // Vary the callback size, the way a backend does.
        let frames = 64 + (i * 37) % 960;
        let block = &mut out[..frames * CHANNELS];
        assert_no_alloc(|| graph.process(block));
    }

    assert!(out.iter().any(|s| *s != 0.0), "the graph produced silence, so it proved nothing");
    assert!(out.iter().all(|s| s.is_finite()), "the mix went non-finite");
}

/// Builds a one-node graph at `level`, for swapping in.
fn constant_graph(level: f32) -> Box<engine_core::Graph> {
    struct Constant(f32);
    impl Node for Constant {
        fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
            for channel in buffer.channels_mut() {
                channel.fill(self.0);
            }
        }
    }
    let mut builder = GraphBuilder::new();
    let node = builder.add(Box::new(Constant(level)));
    builder.output(node);
    Box::new(builder.build(CHANNELS, MAX_FRAMES).expect("a one-node graph is always routable"))
}

#[test]
fn swapping_a_graph_does_not_allocate() {
    // The reason retired graphs travel back to the control side at all: freeing
    // one means freeing every buffer in it, and that is allocator work. If the
    // audio thread ever drops a graph itself, this aborts.
    let (mut swapper, mut player) = graph_channel(Some(constant_graph(0.1)), CHANNELS, MAX_FRAMES, SR);
    let mut out = vec![0.0f32; MAX_FRAMES * CHANNELS];

    for i in 0..300usize {
        if i.is_multiple_of(40) {
            // Ignored when the queue is full, which is the swapper's business.
            let _ = swapper.swap(constant_graph(0.1 + i as f32 / 1_000.0));
        }
        let frames = 64 + (i * 53) % 900;
        let block = &mut out[..frames * CHANNELS];
        assert_no_alloc(|| player.process(block));
        // Dropping the retired graphs here is the point; doing it above is not.
        while swapper.collect().is_some() {}
    }

    assert!(out.iter().any(|s| *s != 0.0), "nothing played, so nothing was proved");
}

#[test]
fn automating_a_graph_does_not_allocate() {
    // Splitting a block at each scheduled change is the one place in the graph
    // where the audio thread does work whose size it did not know in advance.
    // If that path ever reaches for a Vec, it does so under a moving fader —
    // exactly when an xrun is least forgivable.
    let mut builder = GraphBuilder::new();
    let bus = SoloBus::new();
    let master = builder.add(Box::new(Gain::new(0.5, SR)));
    let mut faders = Vec::new();

    for track in 0..8 {
        let tone = builder.add(Box::new(Tone { phase: 0.0, step: (110.0 + track as f32 * 55.0) / SR }));
        let gain = builder.add(Box::new(Gain::new(0.8, SR)));
        let pan = builder.add(Box::new(Pan::new(-1.0 + track as f32 * 0.25, SR)));
        let mute = builder.add(Box::new(Mute::new(false, SR)));
        let solo = builder.add(Box::new(Solo::new(&bus, false, SR)));
        builder.connect(tone, gain).connect(gain, pan).connect(pan, mute).connect(mute, solo).connect(solo, master);
        faders.push((ParamId::new(gain, Gain::PARAM_GAIN), ParamId::new(pan, Pan::PARAM_POSITION)));
    }
    let ceiling = builder.add(Box::new(Limiter::new(0.9, CHANNELS, SR)));
    builder.connect(master, ceiling).output(ceiling);
    let mut graph = builder.build(CHANNELS, MAX_FRAMES).unwrap();

    let (mut sender, mut queue) = automation_channel(64);
    let mut out = vec![0.0f32; MAX_FRAMES * CHANNELS];
    let mut frame = 0u64;

    for i in 0..200usize {
        let frames = 64 + (i * 37) % 960;

        // Several changes inside the block, so it is split rather than run
        // whole, and their frames rise the way a control thread would send them.
        let (gain, pan) = faders[i % faders.len()];
        for step in 0..4u64 {
            let at = frame + step * (frames as u64 / 4);
            let target = if step.is_multiple_of(2) { gain } else { pan };
            let value = if step.is_multiple_of(2) { 0.2 + step as f32 * 0.1 } else { -0.5 + step as f32 * 0.2 };
            let _ = sender.send(ParamEvent::new(target, value, at));
        }

        let block = &mut out[..frames * CHANNELS];
        assert_no_alloc(|| graph.process_automated(block, &mut queue, frame));
        frame += frames as u64;
    }

    assert!(out.iter().any(|s| *s != 0.0), "the graph produced silence, so it proved nothing");
    assert!(out.iter().all(|s| s.is_finite()), "the mix went non-finite");
}

#[test]
fn automating_through_a_swap_does_not_allocate() {
    // The two halves of the audio thread's job at once: a fade running while
    // changes are landing, each on its own frame. Splitting the block here
    // means the fade is stepped through in pieces, which is where a scratch
    // buffer sized on the fly would hide.
    let (mut swapper, mut player) = graph_channel(Some(constant_graph(0.1)), CHANNELS, MAX_FRAMES, SR);
    let (mut sender, mut queue) = automation_channel(64);
    let mut out = vec![0.0f32; MAX_FRAMES * CHANNELS];
    let mut frame = 0u64;

    for i in 0..300usize {
        if i.is_multiple_of(40) {
            let _ = swapper.swap(constant_graph(0.1 + i as f32 / 1_000.0));
        }
        let frames = 64 + (i * 53) % 900;
        for step in 0..4u64 {
            // Aimed at the one node a constant graph has.
            let param = ParamId::new(NodeId::from_index(0), 0);
            let _ = sender.send(ParamEvent::new(param, 0.2, frame + step * (frames as u64 / 4)));
        }

        let block = &mut out[..frames * CHANNELS];
        assert_no_alloc(|| player.process_automated(block, &mut queue, frame));
        while swapper.collect().is_some() {}
        frame += frames as u64;
    }

    assert!(out.iter().all(|s| s.is_finite()), "the mix went non-finite");
}
