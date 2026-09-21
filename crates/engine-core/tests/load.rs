//! The phase 2 exit criterion, as something CI can fail on.
//!
//! "Tracks mixed with parameters moved live and no glitch" is an ear's verdict,
//! and an ear is not available in CI. What a glitch actually is, though, is a
//! jump in the output larger than the signal itself can make — so the test is
//! to work out how fast the mix is allowed to move and check that it never
//! moves faster.
//!
//! The bound is derived rather than tuned: the slope of the sines being mixed,
//! plus the steepest a parameter ramp can move, plus the steepest the limiter's
//! gain can fall. A stepped parameter would land an order of magnitude above
//! it, which is the margin that makes this worth running.

use engine_core::{
    AudioBuffer, AutomationSender, Gain, Graph, GraphBuilder, LIMITER_LOOKAHEAD_MS, Limiter, Mute, Node, PARAM_RAMP_MS,
    Pan, ParamEvent, ParamId, automation_channel,
};
use engine_dsp::SineOsc;

const SR: f32 = 48_000.0;
const CHANNELS: usize = 2;
const BLOCK_FRAMES: usize = 480;
/// Long enough for every ramp, every mute and the limiter's release to have
/// happened many times over.
const SECONDS: usize = 20;

const TRACK_AMPLITUDE: f32 = 0.8;
const TRACK_GAIN: f32 = 0.9;
const CEILING: f32 = 0.9;
/// Twice what would fit, so the limiter is doing real work rather than watching.
const MASTER_HEADROOM: f32 = 2.0;
const SLACK: f32 = 1e-6;

fn frequency(track: usize) -> f32 {
    110.0 + track as f32 * 30.0
}

struct Source {
    osc: SineOsc,
    frequency: f32,
}

impl Node for Source {
    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        let (frames, channels) = (buffer.frames(), buffer.channels());
        for frame in 0..frames {
            let sample = self.osc.next_sample(self.frequency) * TRACK_AMPLITUDE;
            for channel in 0..channels {
                buffer.channel_mut(channel)[frame] = sample;
            }
        }
    }
}

struct Faders {
    gain: ParamId,
    pan: ParamId,
    mute: ParamId,
}

/// Tracks, each through gain, pan and mute, into a master under a ceiling.
fn mixer(tracks: usize) -> (Graph, Vec<Faders>) {
    let mut builder = GraphBuilder::new();
    let master = builder.add(Box::new(Gain::new(MASTER_HEADROOM / tracks as f32, SR)));
    let mut faders = Vec::with_capacity(tracks);

    for track in 0..tracks {
        let source = builder.add(Box::new(Source { osc: SineOsc::new(SR), frequency: frequency(track) }));
        let gain = builder.add(Box::new(Gain::new(TRACK_GAIN, SR)));
        let pan = builder.add(Box::new(Pan::new(-1.0 + track as f32 * 2.0 / tracks as f32, SR)));
        let mute = builder.add(Box::new(Mute::new(false, SR)));
        builder.connect(source, gain).connect(gain, pan).connect(pan, mute).connect(mute, master);
        faders.push(Faders {
            gain: ParamId::new(gain, Gain::PARAM_GAIN),
            pan: ParamId::new(pan, Pan::PARAM_POSITION),
            mute: ParamId::new(mute, Mute::PARAM_MUTED),
        });
    }

    let ceiling = builder.add(Box::new(Limiter::new(CEILING, CHANNELS, SR)));
    builder.connect(master, ceiling).output(ceiling);

    let graph = builder.build(CHANNELS, BLOCK_FRAMES).expect("a mixer is acyclic by construction");
    (graph, faders)
}

/// The steepest the mix is allowed to move between two consecutive samples.
///
/// Three terms, each an upper bound on its own: the sines, whose slope is
/// `amplitude · 2πf / rate`; the parameter ramps, each of which can cross its
/// whole range in `PARAM_RAMP_MS`; and the limiter, whose gain can fall its
/// whole range across the lookahead.
fn slope_bound(tracks: usize) -> f32 {
    let share = MASTER_HEADROOM / tracks as f32;
    let signal: f32 = (0..tracks)
        .map(|track| TRACK_AMPLITUDE * TRACK_GAIN * share * std::f32::consts::TAU * frequency(track) / SR)
        .sum();
    // Gain, pan and mute can all be moving at once on every track.
    let ramps = 3.0 * tracks as f32 * TRACK_AMPLITUDE * share / (PARAM_RAMP_MS * 1e-3 * SR);
    let limiting = CEILING / (LIMITER_LOOKAHEAD_MS * 1e-3 * SR);
    signal + ramps + limiting
}

/// Moves a fader on every block, the way a hand on a surface would, each move
/// landing on its own frame inside the block.
fn schedule(sender: &mut AutomationSender, faders: &[Faders], block: usize, frame: u64) {
    let track = &faders[block % faders.len()];
    let sweep = (block as f32 * 0.037).sin();
    let moves =
        [(track.gain, 0.5 + 0.45 * sweep), (track.pan, sweep), (track.mute, if block % 97 < 48 { 0.0 } else { 1.0 })];
    for (step, (param, value)) in moves.into_iter().enumerate() {
        let at = frame + step as u64 * (BLOCK_FRAMES as u64 / 4);
        sender.send(ParamEvent::new(param, value, at)).expect("the queue is drained every block");
    }
}

struct Run {
    worst_step: f32,
    peak: f32,
}

fn play(tracks: usize) -> Run {
    let (mut graph, faders) = mixer(tracks);
    let (mut sender, mut queue) = automation_channel(64);
    let mut out = vec![0.0f32; BLOCK_FRAMES * CHANNELS];
    let mut frame = 0u64;
    let mut run = Run { worst_step: 0.0, peak: 0.0 };
    // Carried across blocks: a discontinuity at a callback boundary is exactly
    // the kind this engine could produce, and looking only inside blocks would
    // step straight over it.
    let mut previous = [0.0f32; CHANNELS];

    for block in 0..SECONDS * SR as usize / BLOCK_FRAMES {
        schedule(&mut sender, &faders, block, frame);
        graph.process_automated(&mut out, &mut queue, frame);

        for chunk in out.as_chunks::<CHANNELS>().0 {
            for (channel, sample) in chunk.iter().enumerate() {
                assert!(sample.is_finite(), "the mix went non-finite at frame {frame}");
                assert!(sample.abs() <= CEILING + SLACK, "{sample} broke the ceiling at frame {frame}");
                run.worst_step = run.worst_step.max((sample - previous[channel]).abs());
                run.peak = run.peak.max(sample.abs());
                previous[channel] = *sample;
            }
        }
        frame += BLOCK_FRAMES as u64;
    }

    run
}

#[test]
fn eight_tracks_mix_without_a_glitch() {
    let run = play(8);
    let bound = slope_bound(8);
    assert!(run.worst_step <= bound, "worst step {} over the {bound} the signal allows", run.worst_step);
    assert!(run.peak > 0.5, "peak {}: the mix was too quiet to prove anything", run.peak);
    println!("8 tracks: worst step {} against a bound of {bound}, peak {}", run.worst_step, run.peak);
}

#[test]
fn sixteen_tracks_mix_without_a_glitch() {
    let run = play(16);
    let bound = slope_bound(16);
    assert!(run.worst_step <= bound, "worst step {} over the {bound} the signal allows", run.worst_step);
    assert!(run.peak > 0.5, "peak {}: the mix was too quiet to prove anything", run.peak);
    println!("16 tracks: worst step {} against a bound of {bound}, peak {}", run.worst_step, run.peak);
}

/// A gain applied the moment it is asked for, with no ramp: the bug every
/// ramped node in the crate exists to avoid.
struct HardGain(f32);

impl Node for HardGain {
    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        for channel in buffer.channels_mut() {
            for sample in channel.iter_mut() {
                *sample *= self.0;
            }
        }
    }

    fn set_param(&mut self, _param: u32, value: f32) {
        self.0 = value;
    }
}

#[test]
fn the_bound_would_catch_a_stepped_parameter() {
    // A bound nothing can exceed proves nothing. This is the same measurement
    // the two tests above make, run against a node that steps rather than
    // ramps, to show how far clear of the bound the bug they guard against
    // actually lands.
    let bound = slope_bound(8);
    let mut worst = 0.0f32;

    // Where in the cycle the step falls decides how big it is, so try several
    // and keep the worst rather than trusting one arbitrary moment.
    for offset in 0..8u64 {
        let mut builder = GraphBuilder::new();
        let source = builder.add(Box::new(Source { osc: SineOsc::new(SR), frequency: frequency(0) }));
        let fader = builder.add(Box::new(HardGain(1.0)));
        builder.connect(source, fader).output(fader);
        let mut graph = builder.build(CHANNELS, BLOCK_FRAMES).expect("two nodes in a line are acyclic");

        let (mut sender, mut queue) = automation_channel(8);
        let mut out = vec![0.0f32; BLOCK_FRAMES * CHANNELS];

        // Invert rather than silence: the step is then twice the signal, which
        // is what a phase-flipped fader really does.
        let at = offset * 13 + BLOCK_FRAMES as u64 / 2;
        sender.send(ParamEvent::new(ParamId::new(fader, 0), -1.0, at)).expect("the queue is empty");
        graph.process_automated(&mut out, &mut queue, 0);

        for pair in out.as_chunks::<CHANNELS>().0.windows(2) {
            worst = worst.max((pair[1][0] - pair[0][0]).abs());
        }
    }

    println!("a stepped parameter moved {worst} against a bound of {bound}");
    assert!(worst > bound * 10.0, "a stepped parameter only moved {worst}, against a bound of {bound}");
}
