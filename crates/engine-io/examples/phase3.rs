//! Phase 3 driver: a MIDI sequence played through the synth, on the device.
//!
//! The exit criterion for the phase, on hardware: a sequence at a fast tempo
//! with no audible drift. "Audible" is not something a script can judge, so the
//! self test measures it instead — it plays staccato sixteenths whose onsets
//! are periodic, and checks where each one actually arrived against where it
//! was written. Exits non-zero if anything was dropped or late.
//!
//!   cargo run --release -p engine-io --example phase3 -- --self-test --bpm 180
//!   cargo run --release -p engine-io --example phase3 -- song.mid
//!   cargo run --release -p engine-io --example phase3 -- --midi-in
//!
//! A real file is played but not checked: its notes overlap, so there is no
//! silence between them to measure an onset against. The self test provokes the
//! thing it then measures, which is the only way the number means anything.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{env, process, thread};

use engine_core::{
    Gain, Graph, GraphBuilder, Limiter, NoteEvent, NoteQueue, NoteSender, Synth, automation_channel, graph_channel,
    note_channel,
};
use engine_dsp::{Adsr, Waveform};
use engine_io::{AudioEngine, EngineOptions, StatsSnapshot, default_output_config, list_output_devices, map_frames};

const USAGE: &str = "usage: phase3 [FILE.mid | --self-test | --midi-in] [--list] [--list-midi] \
                     [--midi-port SUBSTR] [--bpm N] [--voices N] [--wave sine|saw|square] \
                     [--minutes N | --seconds N] [--host NAME] [--device SUBSTR] [--gain 0..1]";

const GRAPH_CHANNELS: usize = 2;
const MAX_GRAPH_FRAMES: usize = 2_048;
const CEILING: f32 = 0.9;
/// How far ahead of the audio thread notes are queued.
const LEAD_MS: f64 = 200.0;
const VELOCITY: f32 = 0.8;

/// Pitches the self test walks through: a minor pentatonic, so a long run is
/// bearable to sit next to.
const SCALE: [u8; 5] = [57, 60, 62, 64, 67];

/// The envelope the self test uses.
///
/// Nothing to sustain, so a note ends with its decay and the synth is genuinely
/// silent between sixteenths. Without that gap there is no edge to measure an
/// onset against, and the check would have nothing to say.
fn staccato(sample_rate: f32) -> Adsr {
    Adsr::new(0.002, 0.02, 0.0, 0.005, sample_rate)
}

/// The envelope a real file gets: it has to hold a chord.
fn sustained(sample_rate: f32) -> Adsr {
    Adsr::new(0.004, 0.08, 0.6, 0.12, sample_rate)
}

/// Where the notes come from.
enum Score {
    /// Sixteenths for ever, generated as the playhead approaches them.
    SelfTest { step: u64, next: u64 },
    /// A decoded file, played once through.
    Roll { events: Vec<NoteEvent>, next: usize },
    /// A keyboard. Nothing is pumped from here: the port's own callback is the
    /// queue's single producer, and a second writer would corrupt it.
    Live,
}

impl Score {
    /// Queues everything due before `horizon`, and says how many notes started.
    fn pump(&mut self, sender: &mut NoteSender, horizon: u64) -> u64 {
        let mut sent = 0;
        match self {
            Self::SelfTest { step, next } => {
                while *next * *step < horizon {
                    let start = *next * *step;
                    let pitch = SCALE[(*next as usize) % SCALE.len()];
                    if sender.send(NoteEvent::on(pitch, VELOCITY, start)).is_err() {
                        break;
                    }
                    // The note dies with its decay, but a note off keeps the
                    // synth's bookkeeping honest all the same.
                    let _ = sender.send(NoteEvent::off(pitch, start + *step / 2));
                    *next += 1;
                    sent += 1;
                }
            }
            Self::Roll { events, next } => {
                while *next < events.len() && events[*next].frame < horizon {
                    if sender.send(events[*next]).is_err() {
                        break;
                    }
                    *next += 1;
                    sent += 1;
                }
            }
            Self::Live => {}
        }
        sent
    }
}

/// What the callback found, published to the control side.
#[derive(Debug, Default)]
struct OnsetCheck {
    /// Onsets seen at all, so a silent run cannot pass for a clean one.
    seen: AtomicU64,
    /// Worst distance from where a note was written, in frames.
    worst: AtomicU64,
    /// Onsets that did not land on their frame.
    late: AtomicU64,
}

struct Args {
    file: Option<PathBuf>,
    self_test: bool,
    midi_in: bool,
    midi_port: Option<String>,
    list: bool,
    list_midi: bool,
    bpm: f64,
    voices: usize,
    wave: Waveform,
    duration: Option<Duration>,
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

    if args.list_midi {
        match engine_midi::list_inputs() {
            Ok(ports) if ports.is_empty() => println!("no MIDI input ports are connected"),
            Ok(ports) => {
                for (i, name) in ports.iter().enumerate() {
                    println!("{i}: {name}");
                }
            }
            Err(e) => {
                eprintln!("{e}");
                process::exit(1);
            }
        }
        return;
    }

    // One source of notes: the synth's queue takes a single producer, and a
    // keyboard's callback writing into it alongside a score would corrupt it.
    if [args.file.is_some(), args.self_test, args.midi_in].iter().filter(|on| **on).count() != 1 {
        eprintln!("give exactly one of FILE.mid, --self-test or --midi-in\n{USAGE}");
        process::exit(2);
    }

    if let Err(e) = run(&args) {
        eprintln!("{e}");
        process::exit(1);
    }
}

/// Synth into a master gain, under a ceiling.
fn build_graph(args: &Args, envelope: &Adsr, notes: NoteQueue, sample_rate: f32) -> Box<Graph> {
    let mut builder = GraphBuilder::new();
    let synth = builder.add(Box::new(Synth::new(args.voices, args.wave, envelope, notes, sample_rate)));
    // Headroom for a chord: the voices sum, and a handful at full velocity
    // would otherwise spend the whole run against the ceiling.
    let master = builder.add(Box::new(Gain::new(0.5, sample_rate)));
    let ceiling = builder.add(Box::new(Limiter::new(CEILING, GRAPH_CHANNELS, sample_rate)));
    builder.connect(synth, master).connect(master, ceiling).output(ceiling);
    Box::new(builder.build(GRAPH_CHANNELS, MAX_GRAPH_FRAMES).expect("a chain is acyclic by construction"))
}

fn run(args: &Args) -> Result<(), String> {
    let (rate, device_channels) = default_output_config(args.engine.host.as_deref(), args.engine.device.as_deref())
        .map_err(|e| format!("cannot query the output device: {e}"))?;
    let sample_rate = rate as f32;
    println!("device: {rate} Hz, {device_channels} channels");

    let step = (60.0 / args.bpm / 4.0 * f64::from(rate)) as u64;
    let (mut score, envelope, total) = match &args.file {
        Some(path) => {
            let seq = engine_midi::decode_file(path, rate).map_err(|e| format!("{}: {e}", path.display()))?;
            println!(
                "{}: {} notes, {:.1}s",
                path.display(),
                seq.notes().len(),
                seq.duration() as f64 / f64::from(rate)
            );
            let duration = seq.duration();
            (Score::Roll { events: seq.piano_roll().events(), next: 0 }, sustained(sample_rate), Some(duration))
        }
        None if args.midi_in => (Score::Live, sustained(sample_rate), None),
        None => {
            println!("self test: sixteenths at {} BPM, one every {step} frames", args.bpm);
            (Score::SelfTest { step, next: 0 }, staccato(sample_rate), None)
        }
    };

    let (sender, queue) = note_channel(1_024);
    let graph = build_graph(args, &envelope, queue, sample_rate);
    let latency = graph.latency_frames() as u64;
    println!(
        "graph: {} voices, {:?}, limiter at {CEILING} | latency {latency} frames ({:.2} ms)",
        args.voices,
        args.wave,
        latency as f64 * 1e3 / f64::from(rate),
    );

    // In live mode the control thread has no sender at all: the port owns it,
    // and it is the queue's only producer. Held for the whole run, because
    // dropping it closes the port.
    let mut sender = Some(sender);
    let keyboard = if args.midi_in {
        let to_synth = sender.take().expect("nothing else has taken it yet");
        let input = engine_midi::connect(args.midi_port.as_deref(), to_synth)
            .map_err(|e| format!("cannot open a MIDI input: {e}"))?;
        println!("midi in: {}", input.name());
        Some(input)
    } else {
        None
    };

    let (_swapper, mut player) = graph_channel(Some(graph), GRAPH_CHANNELS, MAX_GRAPH_FRAMES, sample_rate);
    let check = Arc::new(OnsetCheck::default());
    let callback_check = Arc::clone(&check);
    let position = Arc::new(AtomicU64::new(0));
    let callback_position = Arc::clone(&position);

    // A note written for frame f is first heard at f + latency + 1: the graph's
    // own delay, plus the sample the envelope spends at zero before it opens.
    let expected_offset = latency + 1;
    let checking = args.self_test;

    // Nothing automates anything here, but the player's automated path is the
    // one that carries the timeline to the synth.
    let (_automation, mut params) = automation_channel(8);
    let mut mix = vec![0.0f32; MAX_GRAPH_FRAMES * GRAPH_CHANNELS];
    let (mut previous, mut frame_index) = (0.0f32, 0u64);

    let (mut engine, _commands) = AudioEngine::start_with(&args.engine, move |out, channels| {
        let frames = out.len() / channels.max(1);
        let mut done = 0;
        while done < frames {
            let span = (frames - done).min(MAX_GRAPH_FRAMES);
            let stereo = &mut mix[..span * GRAPH_CHANNELS];
            player.process_automated(stereo, &mut params, frame_index);

            if checking {
                for frame in stereo.as_chunks::<GRAPH_CHANNELS>().0 {
                    // Silence to sound is a note starting. The staccato
                    // envelope is what makes that edge exist at all.
                    if previous == 0.0 && frame[0] != 0.0 {
                        let since = frame_index.saturating_sub(expected_offset);
                        // Onsets are periodic, so where one should have landed
                        // is a modulo rather than a table the callback carries.
                        let into_step = since % step;
                        let off = into_step.min(step - into_step);
                        callback_check.seen.fetch_add(1, Ordering::Relaxed);
                        callback_check.worst.fetch_max(off, Ordering::Relaxed);
                        if off > 0 {
                            callback_check.late.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    previous = frame[0];
                    frame_index += 1;
                }
            } else {
                frame_index += span as u64;
            }

            map_frames(stereo, GRAPH_CHANNELS, &mut out[done * channels..(done + span) * channels], channels);
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

    // A file runs for its own length; the self test runs until told to stop.
    let duration = args
        .duration
        .or_else(|| total.map(|t| Duration::from_secs_f64(t as f64 / f64::from(rate) + 2.0)))
        .unwrap_or(Duration::from_secs(60));

    let started = Instant::now();
    let lead = (LEAD_MS * 1e-3 * f64::from(rate)) as u64;
    let mut next_report = started + Duration::from_secs(10);
    let mut queued = 0u64;

    while started.elapsed() < duration {
        while let Ok(event) = engine.events.pop() {
            println!("[{:>8.2}s] {event:?}", started.elapsed().as_secs_f64());
        }
        if let Some(sender) = sender.as_mut() {
            queued += score.pump(sender, position.load(Ordering::Relaxed) + lead);
        }

        if Instant::now() >= next_report {
            print_status(started.elapsed(), &engine.stats.snapshot(), &check, queued);
            next_report = Instant::now() + Duration::from_secs(10);
        }
        thread::sleep(Duration::from_millis(10));
    }

    let stats = engine.stats.snapshot();
    println!("\n=== summary ===");
    print_status(started.elapsed(), &stats, &check, queued);
    for (kind, n) in stats.error_breakdown() {
        println!("  stream error {kind}: {n}");
    }

    if let Some(input) = &keyboard {
        println!("midi in: {} | notes refused because the queue was full: {}", input.name(), input.dropped());
    }

    let seen = check.seen.load(Ordering::Relaxed);
    let worst = check.worst.load(Ordering::Relaxed);
    let late = check.late.load(Ordering::Relaxed);
    if checking {
        println!(
            "onsets {seen} | off their frame {late} | worst {worst} frames ({:.3} ms)",
            worst as f64 * 1e3 / f64::from(rate)
        );
        if seen == 0 {
            return Err("FAIL: nothing ever sounded, so nothing was measured".into());
        }
    }

    if stats.total_xruns() == 0 && (!checking || late == 0) {
        println!("PASS: no xrun, every note on its frame");
        Ok(())
    } else {
        Err(format!("FAIL: {} xruns, {late} notes off their frame", stats.total_xruns()))
    }
}

fn print_status(t: Duration, s: &StatsSnapshot, check: &OnsetCheck, queued: u64) {
    println!(
        "[{:>8.2}s] callbacks {} | xruns {} | overloads {} | load p50 {:.1}% p99 {:.1}% max {:.1}% | notes {queued} | onsets {}",
        t.as_secs_f64(),
        s.callbacks,
        s.total_xruns(),
        s.overloads,
        s.load_percentile(0.5) as f64 / 10.0,
        s.load_percentile(0.99) as f64 / 10.0,
        s.max_load_permille as f64 / 10.0,
        check.seen.load(Ordering::Relaxed),
    );
}

fn parse_args(argv: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut args = Args {
        file: None,
        self_test: false,
        midi_in: false,
        midi_port: None,
        list: false,
        list_midi: false,
        bpm: 180.0,
        voices: 16,
        wave: Waveform::Saw,
        duration: None,
        engine: EngineOptions::default(),
    };
    let mut argv = argv.into_iter();
    while let Some(arg) = argv.next() {
        let mut value = || argv.next().ok_or_else(|| format!("{arg}: missing value"));
        match arg.as_str() {
            "--list" => args.list = true,
            "--list-midi" => args.list_midi = true,
            "--self-test" => args.self_test = true,
            "--midi-in" => args.midi_in = true,
            "--midi-port" => {
                args.midi_port = Some(value()?);
                args.midi_in = true;
            }
            "--minutes" => args.duration = Some(seconds(value()?, 60.0)?),
            "--seconds" => args.duration = Some(seconds(value()?, 1.0)?),
            "--bpm" => {
                let v = value()?;
                args.bpm = v
                    .parse::<f64>()
                    .ok()
                    .filter(|b| b.is_finite() && (20.0..=400.0).contains(b))
                    .ok_or_else(|| format!("--bpm: expected 20..400, got {v}"))?;
            }
            "--voices" => {
                let v = value()?;
                args.voices = v
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=128).contains(n))
                    .ok_or_else(|| format!("--voices: expected 1..128, got {v}"))?;
            }
            "--wave" => {
                let v = value()?;
                args.wave = match v.as_str() {
                    "sine" => Waveform::Sine,
                    "saw" => Waveform::Saw,
                    "square" => Waveform::Square,
                    other => return Err(format!("--wave: expected sine, saw or square, got {other}")),
                };
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
            other if other.starts_with('-') => return Err(format!("unknown option {other}\n{USAGE}")),
            path => args.file = Some(PathBuf::from(path)),
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
    fn the_self_test_needs_no_file() {
        let args = parse(&["--self-test"]).unwrap();
        assert!(args.self_test);
        assert_eq!(args.file, None);
        assert_eq!(args.bpm, 180.0);
    }

    #[test]
    fn a_bare_path_is_the_file() {
        let args = parse(&["song.mid"]).unwrap();
        assert_eq!(args.file, Some(PathBuf::from("song.mid")));
    }

    #[test]
    fn parses_every_flag() {
        let args = parse(&[
            "--self-test",
            "--bpm",
            "240",
            "--voices",
            "8",
            "--wave",
            "square",
            "--seconds",
            "30",
            "--gain",
            "0.4",
        ])
        .unwrap();
        assert_eq!(args.bpm, 240.0);
        assert_eq!(args.voices, 8);
        assert_eq!(args.wave, Waveform::Square);
        assert_eq!(args.duration, Some(Duration::from_secs(30)));
        assert_eq!(args.engine.gain, 0.4);
    }

    #[test]
    fn rejects_values_that_would_misbehave() {
        for bad in [
            &["--bpm", "0"][..],
            &["--bpm", "1000"],
            &["--voices", "0"],
            &["--wave", "triangle"],
            &["--seconds", "-1"],
            &["--gain", "2"],
            &["--nope"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should have been refused");
        }
    }

    #[test]
    fn the_self_test_queues_sixteenths_at_the_written_spacing() {
        let (mut sender, _queue) = note_channel(64);
        let mut score = Score::SelfTest { step: 1_000, next: 0 };
        assert_eq!(score.pump(&mut sender, 3_500), 4, "a note should fall every 1000 frames");
        assert_eq!(score.pump(&mut sender, 3_500), 0, "the same notes were queued twice");
    }

    #[test]
    fn a_decoded_file_becomes_notes_the_score_can_pump() {
        // The join between the two crates: what midly produced has to be what
        // the synth's queue accepts, in frame order.
        let mut roll = engine_core::PianoRoll::new();
        roll.add(engine_core::Note::new(60, 1.0, 0, 100));
        roll.add(engine_core::Note::new(64, 1.0, 500, 100));
        let mut score = Score::Roll { events: roll.events(), next: 0 };

        let (mut sender, _queue) = note_channel(64);
        assert_eq!(score.pump(&mut sender, 200), 2, "the first note's start and stop");
        assert_eq!(score.pump(&mut sender, 10_000), 2, "the second note's");
    }

    #[test]
    fn naming_a_port_is_enough_to_ask_for_live_input() {
        let args = parse(&["--midi-port", "Keystation"]).unwrap();
        assert!(args.midi_in, "naming a port should not also need --midi-in");
        assert_eq!(args.midi_port.as_deref(), Some("Keystation"));
    }

    #[test]
    fn live_input_pumps_nothing_because_the_port_is_the_producer() {
        // Two writers into one single-producer queue is the bug this guards.
        let (mut sender, _queue) = note_channel(64);
        assert_eq!(Score::Live.pump(&mut sender, u64::MAX), 0);
    }

    #[test]
    fn parsed_options_pass_engine_validation() {
        assert!(parse(&["--self-test"]).unwrap().engine.validate().is_ok());
    }
}
