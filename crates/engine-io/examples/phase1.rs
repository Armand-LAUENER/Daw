//! Phase 1 driver: play a file through the engine, with a filter and seeks.
//!
//! The exit criterion for the phase, run on the real device: a long file with
//! repeated seeks under a filter, without an xrun and without an audible click.
//! Exits non-zero if anything was dropped, so it can be scripted.
//!
//!   cargo run --release -p engine-io --example phase1 -- track.flac --minutes 10

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use std::{env, process, thread};

use engine_dsp::{FilterKind, SmoothedBiquad};
use engine_io::{AudioEngine, EngineOptions, StatsSnapshot, default_output_config, list_output_devices, map_frames};
use engine_io_file::{MemorySource, Shared, TransportCommand, TransportOptions, decode_file, start};

const USAGE: &str = "usage: phase1 FILE [--list] [--minutes N | --seconds N] [--seek-every S] \
                     [--cutoff HZ] [--loop] [--host NAME] [--device SUBSTR] [--gain 0..1]";

struct Args {
    file: Option<PathBuf>,
    list: bool,
    duration: Duration,
    seek_every: Option<Duration>,
    cutoff: f32,
    looping: bool,
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

    let Some(file) = args.file.clone() else {
        eprintln!("{USAGE}");
        process::exit(2);
    };

    if let Err(e) = run(&file, &args) {
        eprintln!("{e}");
        process::exit(1);
    }
}

fn run(file: &Path, args: &Args) -> Result<(), String> {
    // The device decides the rate: decoding to anything else would detune the
    // file or need a second resampler inside the callback.
    let (rate, device_channels) = default_output_config(args.engine.host.as_deref(), args.engine.device.as_deref())
        .map_err(|e| format!("cannot query the output device: {e}"))?;
    println!("device: {rate} Hz, {device_channels} channels");

    let started_decode = Instant::now();
    let audio = decode_file(file, rate).map_err(|e| format!("{}: {e}", file.display()))?;
    let file_channels = usize::from(audio.channels);
    println!(
        "decoded {} in {:.2}s: {file_channels} channels, {:.1}s",
        file.display(),
        started_decode.elapsed().as_secs_f64(),
        audio.duration().as_secs_f64()
    );
    let total_frames = audio.frames() as u64;
    if total_frames == 0 {
        return Err("the file holds no audio".into());
    }

    let (mut transport, mut playback) =
        start(Box::new(MemorySource::new(audio)), TransportOptions { buffer_ms: 500.0, ..Default::default() });
    let shared = transport.shared().clone();

    // Everything the callback needs is built here and moved in: allocating
    // inside it would trip the no-allocation check the engine wraps it in.
    let mut filters: Vec<SmoothedBiquad> = (0..file_channels)
        .map(|_| SmoothedBiquad::new(FilterKind::LowPass, args.cutoff, 0.707, rate as f32, 10.0))
        .collect();
    let mut from_file = vec![0.0f32; 8_192 * file_channels];

    let (mut engine, _commands) = AudioEngine::start_with(&args.engine, move |out, channels| {
        let frames = out.len() / channels.max(1);
        let src = &mut from_file[..frames * file_channels];
        playback.fill(src);
        for (channel, filter) in filters.iter_mut().enumerate() {
            for frame in 0..frames {
                let i = frame * file_channels + channel;
                src[i] = filter.process_sample(src[i]);
            }
        }
        map_frames(src, file_channels, out, channels);
    })
    .map_err(|e| format!("failed to start audio: {e}"))?;

    let info = &engine.info;
    println!(
        "host {} | device {}\n{} Hz, {} ch, {} | buffer: {}",
        info.host, info.device, info.sample_rate, info.channels, info.sample_format, info.buffer_size
    );
    transport.send(TransportCommand::Play).map_err(|_| "the transport refused to start")?;
    if args.looping {
        let end = total_frames.min(u64::from(rate) * 10);
        transport.send(TransportCommand::SetLoop(Some((0, end)))).map_err(|_| "loop rejected")?;
        println!("looping over the first {:.1}s", end as f64 / f64::from(rate));
    }

    let started = Instant::now();
    let mut next_seek = args.seek_every.map(|d| started + d);
    let mut next_report = started + Duration::from_secs(10);
    let mut seeks = 0u32;

    while started.elapsed() < args.duration {
        while let Ok(event) = engine.events.pop() {
            println!("[{:>8.2}s] {event:?}", started.elapsed().as_secs_f64());
        }

        if let (Some(at), Some(every)) = (next_seek, args.seek_every)
            && Instant::now() >= at
        {
            // Stepping by a prime keeps the targets spread out and repeatable,
            // so a failure can be reproduced.
            seeks += 1;
            let target = (u64::from(seeks) * 7_919 * u64::from(rate) / 1_000) % total_frames;
            transport.send(TransportCommand::Seek(target)).map_err(|_| "seek rejected")?;
            println!("[{:>8.2}s] seek to {:.2}s", started.elapsed().as_secs_f64(), target as f64 / f64::from(rate));
            next_seek = Some(Instant::now() + every);
        }

        if Instant::now() >= next_report {
            print_status(started.elapsed(), &engine.stats.snapshot(), &shared, rate);
            next_report = Instant::now() + Duration::from_secs(10);
        }
        thread::sleep(Duration::from_millis(20));
    }

    let stats = engine.stats.snapshot();
    let underruns = shared.underruns.load(Ordering::Relaxed);
    println!("\n=== summary ===");
    print_status(started.elapsed(), &stats, &shared, rate);
    println!("seeks {seeks} | underruns {underruns}");
    for (kind, n) in stats.error_breakdown() {
        println!("  stream error {kind}: {n}");
    }

    if stats.total_xruns() == 0 && underruns == 0 {
        println!("PASS: no xrun, no underrun");
        Ok(())
    } else {
        Err(format!("FAIL: {} xruns, {underruns} underruns", stats.total_xruns()))
    }
}

fn print_status(t: Duration, s: &StatsSnapshot, shared: &Shared, rate: u32) {
    println!(
        "[{:>8.2}s] callbacks {} | xruns {} | overloads {} | load p50 {:.1}% p99 {:.1}% max {:.1}% | position {:.2}s",
        t.as_secs_f64(),
        s.callbacks,
        s.total_xruns(),
        s.overloads,
        s.load_percentile(0.5) as f64 / 10.0,
        s.load_percentile(0.99) as f64 / 10.0,
        s.max_load_permille as f64 / 10.0,
        shared.position.load(Ordering::Relaxed) as f64 / f64::from(rate),
    );
}

fn parse_args(argv: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut args = Args {
        file: None,
        list: false,
        duration: Duration::from_secs(60),
        seek_every: None,
        cutoff: 20_000.0,
        looping: false,
        engine: EngineOptions::default(),
    };
    let mut argv = argv.into_iter();
    while let Some(arg) = argv.next() {
        let mut value = || argv.next().ok_or_else(|| format!("{arg}: missing value"));
        match arg.as_str() {
            "--list" => args.list = true,
            "--loop" => args.looping = true,
            "--minutes" => args.duration = seconds(value()?, 60.0)?,
            "--seconds" => args.duration = seconds(value()?, 1.0)?,
            "--seek-every" => args.seek_every = Some(seconds(value()?, 1.0)?),
            "--cutoff" => {
                let v = value()?;
                args.cutoff = v
                    .parse::<f32>()
                    .ok()
                    .filter(|c| c.is_finite() && *c > 0.0)
                    .ok_or_else(|| format!("--cutoff: expected a positive frequency, got {v}"))?;
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
    fn a_bare_path_is_the_file() {
        let args = parse(&["track.flac"]).unwrap();
        assert_eq!(args.file, Some(PathBuf::from("track.flac")));
        assert!(!args.list);
    }

    #[test]
    fn parses_every_flag() {
        let args = parse(&[
            "song.mp3",
            "--minutes",
            "2",
            "--seek-every",
            "3",
            "--cutoff",
            "800",
            "--loop",
            "--gain",
            "0.4",
            "--device",
            "PRO X",
        ])
        .unwrap();
        assert_eq!(args.duration, Duration::from_secs(120));
        assert_eq!(args.seek_every, Some(Duration::from_secs(3)));
        assert_eq!(args.cutoff, 800.0);
        assert!(args.looping);
        assert_eq!(args.engine.gain, 0.4);
        assert_eq!(args.engine.device.as_deref(), Some("PRO X"));
    }

    #[test]
    fn rejects_values_that_would_misbehave() {
        for bad in [
            &["f.wav", "--minutes", "-1"][..],
            &["f.wav", "--seconds", "NaN"],
            &["f.wav", "--cutoff", "0"],
            &["f.wav", "--cutoff", "-100"],
            &["f.wav", "--gain", "2"],
            &["f.wav", "--seek-every", "0"],
            &["f.wav", "--nope"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should have been refused");
        }
    }

    #[test]
    fn parsed_options_pass_engine_validation() {
        let args = parse(&["f.wav", "--gain", "0.3"]).unwrap();
        assert!(args.engine.validate().is_ok());
    }
}
