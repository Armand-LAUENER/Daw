//! Phase 2 driver: a mixer graph on the real device, with faders moving.
//!
//! The exit criterion for the phase, on hardware rather than offline: tracks
//! mixed under a master limiter, parameters moved live from the control thread,
//! and the routing itself replaced while it plays — without an xrun and without
//! a discontinuity the signal cannot account for. Exits non-zero if anything
//! was dropped, so it can be scripted.
//!
//!   cargo run --release -p engine-io --example phase2 -- --tracks 16 --minutes 30
//!
//! Unlike the phase 1 driver, the click check is always on. Everything here is
//! synthesised, so the output's slope is known in advance; there is no snare
//! hit that a threshold could mistake for a click.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{env, process, thread};

use engine_core::{
    AudioBuffer, AutomationSender, Gain, Graph, GraphBuilder, LIMITER_LOOKAHEAD_MS, Limiter, Mute, Node, PARAM_RAMP_MS,
    Pan, ParamEvent, ParamId, SWAP_FADE_MS, Solo, SoloBus, automation_channel, graph_channel,
};
use engine_dsp::SineOsc;
use engine_io::{AudioEngine, EngineOptions, StatsSnapshot, default_output_config, list_output_devices, map_frames};

const USAGE: &str = "usage: phase2 [--tracks N] [--list] [--minutes N | --seconds N] \
                     [--swap-every S] [--host NAME] [--device SUBSTR] [--gain 0..1]";

/// The graph is stereo whatever the device is: panning needs a pair, and the
/// device's own layout is reached by mapping afterwards, exactly as phase 1
/// does for a file.
const GRAPH_CHANNELS: usize = 2;
/// Largest block the graph is built for. A bigger callback is rendered in
/// several passes rather than sizing every node's buffer for the worst case.
const MAX_GRAPH_FRAMES: usize = 2_048;

const CEILING: f32 = 0.9;
const TRACK_AMPLITUDE: f32 = 0.8;
const TRACK_GAIN: f32 = 0.9;
/// Twice what would fit, so the limiter is doing real work rather than watching.
const MASTER_HEADROOM: f32 = 2.0;
/// How far ahead of the audio thread the control side schedules a change.
///
/// It has to clear a callback and the control thread's own jitter. An event
/// that misses anyway is not lost — it lands at the start of the next block,
/// which is the accuracy there would be without scheduling at all.
const AUTOMATION_LEAD_MS: f64 = 30.0;

fn frequency(track: usize) -> f32 {
    110.0 + track as f32 * 30.0
}

/// One track's source: a sine, written to both channels of the frame.
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

/// Where a track's controls live, for the automation to aim at.
struct Faders {
    gain: ParamId,
    pan: ParamId,
    mute: ParamId,
    solo: ParamId,
}

/// Tracks, each through gain, pan and mute, into a master under a ceiling.
fn mixer(tracks: usize, sample_rate: f32) -> (Box<Graph>, Vec<Faders>) {
    let mut builder = GraphBuilder::new();
    // One bus per graph: a graph built to replace this one gets its own, and
    // the control side carries the solo state over by rebuilding with it.
    let bus = SoloBus::new();
    let master = builder.add(Box::new(Gain::new(MASTER_HEADROOM / tracks as f32, sample_rate)));
    let mut faders = Vec::with_capacity(tracks);

    for track in 0..tracks {
        let source = builder.add(Box::new(Source { osc: SineOsc::new(sample_rate), frequency: frequency(track) }));
        let gain = builder.add(Box::new(Gain::new(TRACK_GAIN, sample_rate)));
        let pan = builder.add(Box::new(Pan::new(-1.0 + track as f32 * 2.0 / tracks as f32, sample_rate)));
        let mute = builder.add(Box::new(Mute::new(false, sample_rate)));
        let solo = builder.add(Box::new(Solo::new(&bus, false, sample_rate)));
        builder.connect(source, gain).connect(gain, pan).connect(pan, mute).connect(mute, solo).connect(solo, master);
        faders.push(Faders {
            gain: ParamId::new(gain, Gain::PARAM_GAIN),
            pan: ParamId::new(pan, Pan::PARAM_POSITION),
            mute: ParamId::new(mute, Mute::PARAM_MUTED),
            solo: ParamId::new(solo, Solo::PARAM_SOLOED),
        });
    }

    let ceiling = builder.add(Box::new(Limiter::new(CEILING, GRAPH_CHANNELS, sample_rate)));
    builder.connect(master, ceiling).output(ceiling);

    let graph = builder.build(GRAPH_CHANNELS, MAX_GRAPH_FRAMES).expect("a mixer is acyclic by construction");
    (Box::new(graph), faders)
}

/// The steepest the mix is allowed to move between two consecutive samples.
///
/// Each term is an upper bound on its own: the slope of the sines being summed,
/// the steepest a parameter ramp moves, the steepest the limiter's gain falls,
/// and the steepest a crossfade between two graphs can step. Anything above the
/// total is a step the signal path invented, which is what a click is.
fn slope_bound(tracks: usize, sample_rate: f32) -> f32 {
    let share = MASTER_HEADROOM / tracks as f32;
    let signal: f32 = (0..tracks)
        .map(|t| TRACK_AMPLITUDE * TRACK_GAIN * share * std::f32::consts::TAU * frequency(t) / sample_rate)
        .sum();
    // Gain, pan, mute and solo can all be moving at once on every track.
    let ramps = 4.0 * tracks as f32 * TRACK_AMPLITUDE * share / (PARAM_RAMP_MS * 1e-3 * sample_rate);
    let limiting = CEILING / (LIMITER_LOOKAHEAD_MS * 1e-3 * sample_rate);
    // Two graphs at the ceiling, blended the wrong way round for one sample.
    let swapping = 2.0 * CEILING / (SWAP_FADE_MS * 1e-3 * sample_rate);
    signal + ramps + limiting + swapping
}

/// Discontinuities seen in the rendered mix, published to the control side.
#[derive(Debug)]
struct ClickCheck {
    violations: AtomicU64,
    /// Worst step seen at all, in billionths, so a plain `fetch_max` tracks it.
    /// On a healthy run it settles at the signal's own slope, which is what
    /// shows the check was looking at audio rather than at silence.
    worst_step_nano: AtomicU64,
    first_frame: AtomicU64,
}

impl Default for ClickCheck {
    fn default() -> Self {
        Self {
            violations: AtomicU64::new(0),
            worst_step_nano: AtomicU64::new(0),
            first_frame: AtomicU64::new(u64::MAX),
        }
    }
}

struct Args {
    tracks: usize,
    list: bool,
    duration: Duration,
    swap_every: Option<Duration>,
    engine: EngineOptions,
}

fn main() {
    let args = match parse_args(env::args().skip(1)) {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("{msg}");
            process::exit(2);
        }
    };

    if args.list {
        match list_output_devices(args.engine.host.as_deref()) {
            Ok(devices) => {
                for d in devices {
                    let mark = if d.is_default { '*' } else { ' ' };
                    println!("{mark} {} [{}]\n    {}", d.name, d.id, d.default_config.unwrap_or_default());
                }
            }
            Err(e) => {
                eprintln!("{e}");
                process::exit(1);
            }
        }
        return;
    }

    if let Err(e) = run(&args) {
        eprintln!("{e}");
        process::exit(1);
    }
}

fn run(args: &Args) -> Result<(), String> {
    let (rate, device_channels) = default_output_config(args.engine.host.as_deref(), args.engine.device.as_deref())
        .map_err(|e| format!("cannot query the output device: {e}"))?;
    let sample_rate = rate as f32;
    println!("device: {rate} Hz, {device_channels} channels");

    let (graph, mut faders) = mixer(args.tracks, sample_rate);
    let latency = graph.latency_frames();
    println!(
        "graph: {} tracks, stereo, limiter at {CEILING} | latency {latency} frames ({:.2} ms)",
        args.tracks,
        latency as f64 * 1e3 / f64::from(rate),
    );

    let (mut swapper, mut player) = graph_channel(Some(graph), GRAPH_CHANNELS, MAX_GRAPH_FRAMES, sample_rate);
    let (mut sender, mut queue) = automation_channel(256);

    let bound = slope_bound(args.tracks, sample_rate);
    let check = Arc::new(ClickCheck::default());
    let callback_check = Arc::clone(&check);
    // The control thread cannot schedule against the audio thread's clock
    // without being told what it reads, so the callback publishes it.
    let position = Arc::new(AtomicU64::new(0));
    let callback_position = Arc::clone(&position);

    // Everything the callback needs is built here and moved in: allocating
    // inside it would trip the no-allocation check the engine wraps it in.
    let mut mix = vec![0.0f32; MAX_GRAPH_FRAMES * GRAPH_CHANNELS];
    let (mut previous, mut frame_index) = (0.0f32, 0u64);

    let (mut engine, _commands) = AudioEngine::start_with(&args.engine, move |out, channels| {
        let frames = out.len() / channels.max(1);
        let mut done = 0;
        while done < frames {
            let span = (frames - done).min(MAX_GRAPH_FRAMES);
            let stereo = &mut mix[..span * GRAPH_CHANNELS];
            player.process_automated(stereo, &mut queue, frame_index);

            // Checked here rather than after the mapping: this is the graph's
            // own output, and the mapping only copies it around.
            for frame in stereo.as_chunks::<GRAPH_CHANNELS>().0 {
                let step = (frame[0] - previous).abs();
                callback_check.worst_step_nano.fetch_max((step * 1e9) as u64, Ordering::Relaxed);
                if step > bound {
                    callback_check.violations.fetch_add(1, Ordering::Relaxed);
                    callback_check.first_frame.fetch_min(frame_index, Ordering::Relaxed);
                }
                previous = frame[0];
            }

            map_frames(stereo, GRAPH_CHANNELS, &mut out[done * channels..(done + span) * channels], channels);
            frame_index += span as u64;
            done += span;
        }
        callback_position.store(frame_index, Ordering::Relaxed);
    })
    .map_err(|e| format!("failed to start audio: {e}"))?;

    let info = &engine.info;
    println!(
        "host {} | device {}\n{} Hz, {} ch, {} | buffer: {}",
        info.host, info.device, info.sample_rate, info.channels, info.sample_format, info.buffer_size
    );

    let started = Instant::now();
    let lead = (AUTOMATION_LEAD_MS * 1e-3 * f64::from(rate)) as u64;
    let mut next_swap = args.swap_every.map(|d| started + d);
    let mut next_report = started + Duration::from_secs(10);
    let (mut moves, mut swaps, mut refused, mut tick) = (0u64, 0u32, 0u64, 0usize);

    while started.elapsed() < args.duration {
        while let Ok(event) = engine.events.pop() {
            println!("[{:>8.2}s] {event:?}", started.elapsed().as_secs_f64());
        }

        let at = position.load(Ordering::Relaxed) + lead;
        moves += automate(&mut sender, &faders, tick, at, &mut refused);
        tick += 1;

        if let (Some(due), Some(every)) = (next_swap, args.swap_every)
            && Instant::now() >= due
        {
            // Alternate the track count, so the routing really changes rather
            // than being rebuilt identically.
            let tracks = if swaps.is_multiple_of(2) { args.tracks.div_ceil(2).max(1) } else { args.tracks };
            let (graph, new_faders) = mixer(tracks, sample_rate);
            match swapper.swap(graph) {
                Ok(()) => {
                    // The old ids name nodes that are on their way out; aiming
                    // at them after the swap would move nothing at best.
                    faders = new_faders;
                    swaps += 1;
                    println!("[{:>8.2}s] swapped to {tracks} tracks", started.elapsed().as_secs_f64());
                }
                Err(_) => println!("[{:>8.2}s] swap refused, queue full", started.elapsed().as_secs_f64()),
            }
            next_swap = Some(Instant::now() + every);
        }

        // Retired graphs are freed here. Doing it on the audio thread is what
        // handing them back exists to avoid.
        while swapper.collect().is_some() {}

        if Instant::now() >= next_report {
            print_status(started.elapsed(), &engine.stats.snapshot(), &player_position(&position, rate));
            next_report = Instant::now() + Duration::from_secs(10);
        }
        thread::sleep(Duration::from_millis(20));
    }

    let stats = engine.stats.snapshot();
    let violations = check.violations.load(Ordering::Relaxed);
    let worst = check.worst_step_nano.load(Ordering::Relaxed) as f32 / 1e9;
    println!("\n=== summary ===");
    print_status(started.elapsed(), &stats, &player_position(&position, rate));
    println!("fader moves {moves} | refused {refused} | graph swaps {swaps}");
    for (kind, n) in stats.error_breakdown() {
        println!("  stream error {kind}: {n}");
    }
    println!("discontinuities {violations} | worst step {worst:.6} | limit {bound:.6}");
    if violations > 0 {
        let at = check.first_frame.load(Ordering::Relaxed);
        println!("  first at frame {at} ({:.2}s into the run)", at as f64 / f64::from(rate));
    }

    if stats.total_xruns() == 0 && violations == 0 {
        println!("PASS: no xrun, no discontinuity");
        Ok(())
    } else {
        Err(format!("FAIL: {} xruns, {violations} discontinuities", stats.total_xruns()))
    }
}

/// Moves one track's controls, the way a hand on a surface would.
///
/// Returns how many landed; a refusal means the audio thread has not drained
/// the queue, which is worth counting rather than ignoring.
fn automate(sender: &mut AutomationSender, faders: &[Faders], tick: usize, at: u64, refused: &mut u64) -> u64 {
    if faders.is_empty() {
        return 0;
    }
    let track = &faders[tick % faders.len()];
    let sweep = (tick as f32 * 0.07).sin();
    // Solo one track at a time, and only now and then — a solo held forever
    // would leave the click check watching a single sine. The state is sent on
    // every visit rather than on the change, so no track can be left soloed by
    // a rotation that happened to skip it.
    let soloing = tick % 400 < 80 && tick % faders.len() == (tick / 400) % faders.len();
    let moves = [
        (track.gain, 0.5 + 0.45 * sweep),
        (track.pan, sweep),
        (track.mute, if tick % 211 < 105 { 0.0 } else { 1.0 }),
        (track.solo, if soloing { 1.0 } else { 0.0 }),
    ];

    let mut sent = 0;
    for (step, (param, value)) in moves.into_iter().enumerate() {
        // Spread across a few milliseconds so they land on different frames,
        // which is the part a block-aligned scheduler would get wrong.
        match sender.send(ParamEvent::new(param, value, at + step as u64 * 64)) {
            Ok(()) => sent += 1,
            Err(_) => *refused += 1,
        }
    }
    sent
}

fn player_position(position: &AtomicU64, rate: u32) -> f64 {
    position.load(Ordering::Relaxed) as f64 / f64::from(rate)
}

fn print_status(t: Duration, s: &StatsSnapshot, played: &f64) {
    println!(
        "[{:>8.2}s] callbacks {} | xruns {} | overloads {} | load p50 {:.1}% p99 {:.1}% max {:.1}% | played {played:.2}s",
        t.as_secs_f64(),
        s.callbacks,
        s.total_xruns(),
        s.overloads,
        s.load_percentile(0.5) as f64 / 10.0,
        s.load_percentile(0.99) as f64 / 10.0,
        s.max_load_permille as f64 / 10.0,
    );
}

fn parse_args(argv: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut args = Args {
        tracks: 8,
        list: false,
        duration: Duration::from_secs(60),
        swap_every: None,
        engine: EngineOptions::default(),
    };
    let mut argv = argv.into_iter();
    while let Some(arg) = argv.next() {
        let mut value = || argv.next().ok_or_else(|| format!("{arg}: missing value"));
        match arg.as_str() {
            "--list" => args.list = true,
            "--minutes" => args.duration = seconds(value()?, 60.0)?,
            "--seconds" => args.duration = seconds(value()?, 1.0)?,
            "--swap-every" => args.swap_every = Some(seconds(value()?, 1.0)?),
            "--tracks" => {
                let v = value()?;
                args.tracks = v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=128).contains(n))
                    .ok_or_else(|| format!("--tracks: expected 1..128, got {v}"))?;
            }
            "--host" => args.engine.host = Some(value()?),
            "--device" => args.engine.device = Some(value()?),
            "--gain" => {
                let v = value()?;
                args.engine.gain = v
                    .parse::<f32>()
                    .ok()
                    .filter(|g| (0.0..=1.0).contains(g))
                    .ok_or_else(|| format!("--gain: expected 0..1, got {v}"))?;
            }
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        }
    }
    Ok(args)
}

fn seconds(value: String, scale: f64) -> Result<Duration, String> {
    let n: f64 = value.parse().map_err(|_| format!("expected a number, got {value}"))?;
    if !n.is_finite() || n <= 0.0 {
        return Err(format!("expected a positive duration, got {value}"));
    }
    Ok(Duration::from_secs_f64(n * scale))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_to_eight_tracks_for_a_minute() {
        let args = parse(&[]).unwrap();
        assert_eq!(args.tracks, 8);
        assert_eq!(args.duration, Duration::from_secs(60));
        assert_eq!(args.swap_every, None);
    }

    #[test]
    fn parses_every_flag() {
        let args =
            parse(&["--tracks", "16", "--minutes", "2", "--swap-every", "5", "--gain", "0.4", "--device", "PRO X"])
                .unwrap();
        assert_eq!(args.tracks, 16);
        assert_eq!(args.duration, Duration::from_secs(120));
        assert_eq!(args.swap_every, Some(Duration::from_secs(5)));
        assert_eq!(args.engine.gain, 0.4);
        assert_eq!(args.engine.device.as_deref(), Some("PRO X"));
    }

    #[test]
    fn rejects_values_that_would_misbehave() {
        for bad in [
            &["--tracks", "0"][..],
            &["--tracks", "1000"],
            &["--minutes", "-1"],
            &["--seconds", "NaN"],
            &["--swap-every", "0"],
            &["--gain", "2"],
            &["--nope"],
            &["track.flac"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should have been refused");
        }
    }

    #[test]
    fn parsed_options_pass_engine_validation() {
        let args = parse(&["--gain", "0.3"]).unwrap();
        assert!(args.engine.validate().is_ok());
    }

    #[test]
    fn the_mixer_reports_the_limiters_latency() {
        let (graph, faders) = mixer(4, 48_000.0);
        assert_eq!(faders.len(), 4);
        assert!(faders.iter().all(|f| f.solo != f.mute), "solo and mute must name different nodes");
        // Only the limiter looks ahead, so the master path is the only delay.
        assert_eq!(graph.latency_frames(), (LIMITER_LOOKAHEAD_MS * 1e-3 * 48_000.0) as usize);
    }

    #[test]
    fn the_bound_leaves_room_for_the_signal_but_not_for_a_click() {
        // The sines alone should use a real share of it, so the check is not
        // passing on slack, and a full-scale step should be far outside.
        let bound = slope_bound(8, 48_000.0);
        assert!(bound > 0.0 && bound < 0.5, "bound {bound} is not a useful limit");
    }
}
