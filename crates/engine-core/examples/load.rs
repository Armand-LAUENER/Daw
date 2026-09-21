//! How many tracks fit in a callback, and what runs out first.
//!
//! Run with `cargo run --release -p engine-core --example load`; a debug build
//! measures the optimiser rather than the code. `--minutes N` extends the
//! sustained run at the end, which is the one that answers whether the cost
//! holds over a session rather than over a second.
//!
//! The column that decides everything is `% of buffer`: the share of one 10 ms
//! callback that rendering it costs. Past 100% the device is already late. In
//! practice the ceiling is far lower, because the callback shares the machine
//! with a plugin host, a UI and whatever else is running.
//!
//! The cases build on each other, so the difference between two lines is the
//! cost of what was added. That is the point: a total says a graph is slow, a
//! difference says which part of it is.

use std::env;
use std::hint::black_box;
use std::time::{Duration, Instant};

use engine_core::{
    AudioBuffer, AutomationSender, Gain, Graph, GraphBuilder, Limiter, Mute, Node, Pan, ParamEvent, ParamId,
    automation_channel,
};
use engine_dsp::SineOsc;

const SR: f32 = 48_000.0;
const CHANNELS: usize = 2;
/// One 10 ms callback at 48 kHz, the size WASAPI imposes in shared mode.
const BLOCK_FRAMES: usize = 480;
const BLOCK: Duration = Duration::from_micros(10_000);

/// A track's source: one sine, written to every channel of the frame.
struct Source {
    osc: SineOsc,
    frequency: f32,
    amplitude: f32,
}

impl Node for Source {
    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        // One sample per frame, not per channel: advancing the oscillator once
        // per channel would double its pitch on a stereo bus.
        let (frames, channels) = (buffer.frames(), buffer.channels());
        for frame in 0..frames {
            let sample = self.osc.next_sample(self.frequency) * self.amplitude;
            for channel in 0..channels {
                buffer.channel_mut(channel)[frame] = sample;
            }
        }
    }
}

/// What a case puts on each track, in the order a mixer would.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chain {
    /// Sources summed into a bus, and nothing else: the plumbing on its own.
    Bare,
    /// The strip a real track carries.
    Strip,
    /// The strip, plus the ceiling on the master.
    Limited,
}

/// The faders automation aims at, one set per track.
struct Faders {
    gain: ParamId,
    pan: ParamId,
    mute: ParamId,
}

fn build(tracks: usize, chain: Chain) -> (Graph, Vec<Faders>) {
    let mut builder = GraphBuilder::new();
    // Headroom for the sum: sixteen tracks at full scale would leave the
    // limiter doing all the work and measure that instead of the mix.
    let master = builder.add(Box::new(Gain::new(1.0 / tracks as f32, SR)));
    let mut faders = Vec::with_capacity(tracks);

    for track in 0..tracks {
        let source = builder.add(Box::new(Source {
            osc: SineOsc::new(SR),
            frequency: 110.0 + track as f32 * 30.0,
            amplitude: 0.8,
        }));

        if chain == Chain::Bare {
            builder.connect(source, master);
            continue;
        }

        let gain = builder.add(Box::new(Gain::new(0.8, SR)));
        let pan = builder.add(Box::new(Pan::new(-1.0 + track as f32 * 2.0 / tracks as f32, SR)));
        let mute = builder.add(Box::new(Mute::new(false, SR)));
        builder.connect(source, gain).connect(gain, pan).connect(pan, mute).connect(mute, master);
        faders.push(Faders {
            gain: ParamId::new(gain, Gain::PARAM_GAIN),
            pan: ParamId::new(pan, Pan::PARAM_POSITION),
            mute: ParamId::new(mute, Mute::PARAM_MUTED),
        });
    }

    let output = if chain == Chain::Limited {
        let ceiling = builder.add(Box::new(Limiter::new(0.9, CHANNELS, SR)));
        builder.connect(master, ceiling);
        ceiling
    } else {
        master
    };
    builder.output(output);

    let graph = builder.build(CHANNELS, BLOCK_FRAMES).expect("the mixer shape is acyclic by construction");
    (graph, faders)
}

/// Schedules the kind of traffic a hand on a fader produces: a few moves per
/// block, each on its own frame, spread across the tracks.
fn schedule(sender: &mut AutomationSender, faders: &[Faders], block: usize, frame: u64) {
    let track = &faders[block % faders.len()];
    let sweep = (block as f32 * 0.03).sin();
    let moves =
        [(track.gain, 0.5 + 0.4 * sweep), (track.pan, sweep), (track.mute, if block % 400 < 200 { 0.0 } else { 1.0 })];
    for (step, (param, value)) in moves.into_iter().enumerate() {
        // Spread across the block so it is actually split, rather than all
        // landing on the first frame and costing one pass like no automation.
        let _ = sender.send(ParamEvent::new(param, value, frame + step as u64 * (BLOCK_FRAMES as u64 / 4)));
    }
}

#[derive(Default)]
struct Stats {
    blocks: Vec<Duration>,
}

impl Stats {
    fn percentile(&self, p: f64) -> Duration {
        let mut sorted = self.blocks.clone();
        sorted.sort_unstable();
        sorted[((sorted.len() - 1) as f64 * p) as usize]
    }

    fn mean(&self) -> Duration {
        self.blocks.iter().sum::<Duration>() / self.blocks.len() as u32
    }

    fn max(&self) -> Duration {
        self.blocks.iter().copied().max().unwrap_or_default()
    }

    /// Share of one callback spent rendering it.
    fn load(&self) -> f64 {
        self.mean().as_secs_f64() / BLOCK.as_secs_f64() * 100.0
    }
}

fn measure(tracks: usize, chain: Chain, automate: bool, blocks: usize) -> Stats {
    let (mut graph, faders) = build(tracks, chain);
    let (mut sender, mut queue) = automation_channel(64);
    let mut out = vec![0.0f32; BLOCK_FRAMES * CHANNELS];
    let mut stats = Stats { blocks: Vec::with_capacity(blocks) };
    let mut frame = 0u64;

    // Let the ramps settle and the caches warm before anything is recorded.
    for _ in 0..200 {
        graph.process(&mut out);
    }

    for block in 0..blocks {
        if automate {
            schedule(&mut sender, &faders, block, frame);
        }
        let start = Instant::now();
        if automate {
            graph.process_automated(&mut out, &mut queue, frame);
        } else {
            graph.process(&mut out);
        }
        stats.blocks.push(start.elapsed());
        black_box(&out);
        frame += BLOCK_FRAMES as u64;
    }

    stats
}

fn row(label: &str, tracks: usize, stats: &Stats) {
    let per_track = stats.mean().as_secs_f64() * 1e6 / tracks as f64;
    println!(
        "{label:<26} {tracks:>7} {:>11.1} {:>13.2} {:>12.1} {:>10.1} {:>10.1}",
        stats.mean().as_secs_f64() * 1e9 / BLOCK_FRAMES as f64,
        stats.load(),
        per_track,
        stats.percentile(0.99).as_secs_f64() * 1e6,
        stats.max().as_secs_f64() * 1e6,
    );
}

/// Runs for `minutes` of audio and reports each ten seconds separately.
///
/// A mean over the whole run hides the thing worth knowing: whether the cost is
/// the same at the end as at the start. Something that grows — a queue nobody
/// drains, a buffer that creeps — shows up as a rising column and as nothing
/// at all in a single average.
fn sustained(tracks: usize, minutes: f64) {
    let bucket_blocks = (10.0 * SR as f64 / BLOCK_FRAMES as f64) as usize;
    let buckets = ((minutes * 60.0 / 10.0) as usize).max(1);

    let (mut graph, faders) = build(tracks, Chain::Limited);
    let (mut sender, mut queue) = automation_channel(64);
    let mut out = vec![0.0f32; BLOCK_FRAMES * CHANNELS];
    let mut frame = 0u64;
    let mut block = 0usize;
    let mut first = None;
    let mut stats_mean_of_last = Duration::ZERO;

    println!("\nSustained: {tracks} tracks, full chain, automation, {minutes:.1} min of audio\n");
    println!("{:>8} {:>12} {:>12} {:>12} {:>14}", "elapsed", "mean µs", "p99 µs", "max µs", "load %");

    for bucket in 0..buckets {
        let mut stats = Stats { blocks: Vec::with_capacity(bucket_blocks) };
        for _ in 0..bucket_blocks {
            schedule(&mut sender, &faders, block, frame);
            let start = Instant::now();
            graph.process_automated(&mut out, &mut queue, frame);
            stats.blocks.push(start.elapsed());
            black_box(&out);
            frame += BLOCK_FRAMES as u64;
            block += 1;
        }
        let mean = stats.mean();
        first.get_or_insert(mean);
        stats_mean_of_last = mean;
        println!(
            "{:>7}s {:>12.1} {:>12.1} {:>12.1} {:>14.2}",
            (bucket + 1) * 10,
            mean.as_secs_f64() * 1e6,
            stats.percentile(0.99).as_secs_f64() * 1e6,
            stats.max().as_secs_f64() * 1e6,
            stats.load(),
        );
    }

    let (first, last) = (first.unwrap_or_default(), stats_mean_of_last);
    let change = (last.as_secs_f64() / first.as_secs_f64() - 1.0) * 100.0;
    println!(
        "\nFirst bucket {:.1} µs, last {:.1} µs: {change:+.1} %. A cost that climbs over a run is \n\
         something accumulating — a queue nobody drains, a buffer that creeps — and a single \n\
         average over the whole run would hide it.",
        first.as_secs_f64() * 1e6,
        last.as_secs_f64() * 1e6,
    );
}

fn main() {
    let minutes: f64 = env::args().skip_while(|a| a != "--minutes").nth(1).and_then(|v| v.parse().ok()).unwrap_or(1.0);

    if cfg!(debug_assertions) {
        println!("WARNING: debug build. These numbers measure the optimiser being off, not the graph.\n");
    }

    println!(
        "{CHANNELS} channels, {BLOCK_FRAMES} frames per block ({:.0} ms at {:.0} kHz)\n",
        BLOCK.as_secs_f64() * 1e3,
        SR / 1e3
    );
    println!(
        "{:<26} {:>7} {:>11} {:>13} {:>12} {:>10} {:>10}",
        "case", "tracks", "ns/frame", "% of buffer", "µs/track", "p99 µs", "max µs"
    );

    let blocks = 4_000;
    for tracks in [1, 8, 16, 32] {
        row("sources into a bus", tracks, &measure(tracks, Chain::Bare, false, blocks));
        row("+ gain, pan, mute", tracks, &measure(tracks, Chain::Strip, false, blocks));
        row("+ master limiter", tracks, &measure(tracks, Chain::Limited, false, blocks));
        row("+ automation", tracks, &measure(tracks, Chain::Limited, true, blocks));
        println!();
    }

    sustained(16, minutes);
}
