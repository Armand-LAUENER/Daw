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
use engine_core::{AudioBuffer, Gain, GraphBuilder, Mute, Node, Pan};

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
    let master = builder.add(Box::new(Gain::new(0.5, SR)));

    for track in 0..8 {
        let tone = builder.add(Box::new(Tone { phase: 0.0, step: (110.0 + track as f32 * 55.0) / SR }));
        let gain = builder.add(Box::new(Gain::new(0.8, SR)));
        let pan = builder.add(Box::new(Pan::new(-1.0 + track as f32 * 0.25, SR)));
        let mute = builder.add(Box::new(Mute::new(false, SR)));
        builder.connect(tone, gain).connect(gain, pan).connect(pan, mute).connect(mute, master);
    }
    builder.output(master);
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
