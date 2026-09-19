//! Phase 0 driver: sine output, live frequency changes from a control
//! thread, xrun monitoring. Also serves as the load test.
//!
//! ```text
//! cargo run --release -p engine-io --example phase0 -- --list
//! cargo run --release -p engine-io --example phase0 -- --minutes 30 --buffer 256
//! cargo run --release -p engine-io --features pulseaudio --example phase0 -- --host pulseaudio
//! ```
//!
//! Exits with status 1 if any xrun was observed.

use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use engine_io::{AudioEngine, Command, EngineOptions, RtEvent, StatsSnapshot, list_output_devices};

// Makes `assert_no_alloc` in the audio callback abort on allocation.
// Debug builds only: in release the check compiles to nothing.
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

/// A minor pentatonic over two octaves, cycled by the control thread.
const NOTES_HZ: [f32; 10] = [220.0, 261.63, 293.66, 329.63, 392.0, 440.0, 523.25, 587.33, 659.26, 783.99];

struct Args {
    list: bool,
    duration: Duration,
    change_every: Duration,
    report_every: Duration,
    engine: EngineOptions,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        list: false,
        duration: Duration::from_secs(60),
        change_every: Duration::from_secs(1),
        report_every: Duration::from_secs(10),
        engine: EngineOptions::default(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        let number = |v: String| v.parse::<f64>().map_err(|e| format!("{flag}: {e}"));
        match flag.as_str() {
            "--list" => args.list = true,
            "--minutes" => args.duration = Duration::from_secs_f64(number(value()?)? * 60.0),
            "--seconds" => args.duration = Duration::from_secs_f64(number(value()?)?),
            "--change-ms" => args.change_every = Duration::from_millis(number(value()?)? as u64),
            "--report-s" => args.report_every = Duration::from_secs_f64(number(value()?)?),
            "--buffer" => args.engine.buffer_frames = Some(number(value()?)? as u32),
            "--host" => args.engine.host = Some(value()?),
            "--device" => args.engine.device = Some(value()?),
            "--gain" => args.engine.gain = number(value()?)? as f32,
            "-h" | "--help" => {
                return Err("usage: phase0 [--list] [--minutes N | --seconds N] [--change-ms MS] \
                            [--report-s S] [--buffer FRAMES] [--host NAME] [--device SUBSTR] [--gain 0..1]"
                    .into());
            }
            other => return Err(format!("unknown argument {other:?} (try --help)")),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    if args.list {
        return match list_output_devices(args.engine.host.as_deref()) {
            Ok(devices) => {
                for d in devices {
                    let mark = if d.is_default { "*" } else { " " };
                    println!("{mark} {} [{}]\n    {}", d.name, d.id, d.default_config.unwrap_or_default());
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        };
    }

    let (mut engine, mut commands) = match AudioEngine::start_sine(&args.engine) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to start audio: {e}");
            return ExitCode::FAILURE;
        }
    };
    let info = &engine.info;
    println!(
        "host {} | device {}\n{} Hz, {} ch, {} | buffer: {}\nrunning {:?}, frequency change every {:?}",
        info.host, info.device, info.sample_rate, info.channels, info.sample_format, info.buffer_size,
        args.duration, args.change_every
    );

    let started = Instant::now();
    let deadline = started + args.duration;

    // Control thread: owns the command producer, never touches the stream.
    let change_every = args.change_every;
    let control = thread::spawn(move || {
        let mut sent = 0u64;
        let mut next = Instant::now() + change_every;
        while next < deadline {
            thread::sleep(next.saturating_duration_since(Instant::now()));
            let hz = NOTES_HZ[(sent as usize + 1) % NOTES_HZ.len()];
            if commands.push(Command::SetFrequency(hz)).is_ok() {
                sent += 1;
            }
            next += change_every;
        }
        // Fade out before the stream is dropped.
        if commands.push(Command::SetGain(0.0)).is_ok() {
            sent += 1;
        }
        sent
    });

    // Logger loop: the only place that prints.
    let mut next_report = started + args.report_every;
    loop {
        let now = Instant::now();
        while let Ok(event) = engine.events.pop() {
            print_event(started.elapsed(), &event);
        }
        if now >= next_report {
            print_status(started.elapsed(), &engine.stats.snapshot());
            next_report += args.report_every;
        }
        if now >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(50).min(deadline - now));
    }

    let sent = control.join().unwrap_or(0);
    thread::sleep(Duration::from_millis(100)); // let the fade-out play
    let s = engine.stats.snapshot();
    drop(engine);

    println!("\n=== summary ===");
    print_status(started.elapsed(), &s);
    println!(
        "commands sent {sent}, applied {}, rejected {} | buffer frames min {} max {} | events dropped {} | other stream errors {}",
        s.commands_applied,
        s.commands_rejected,
        s.min_buffer_frames,
        s.max_buffer_frames,
        s.dropped_events,
        s.stream_errors
    );
    for (kind, n) in s.error_breakdown() {
        println!("  stream error {kind}: {n}");
    }
    if s.total_xruns() == 0 && s.stream_errors == 0 {
        println!("PASS: no xrun");
        ExitCode::SUCCESS
    } else {
        println!(
            "FAIL: {} xrun(s) ({} backend, {} detected, {} starvation)",
            s.total_xruns(),
            s.backend_xruns,
            s.detected_xruns,
            s.starvations
        );
        ExitCode::FAILURE
    }
}

fn print_event(t: Duration, event: &RtEvent) {
    println!("[{:>8.2}s] {event:?}", t.as_secs_f64());
}

fn print_status(t: Duration, s: &StatsSnapshot) {
    println!(
        "[{:>8.2}s] callbacks {} | frames {} | xruns {} (backend {}, detected {}, starvation {}) | latency shifts {} | overloads {} | max load {:.1}%",
        t.as_secs_f64(),
        s.callbacks,
        s.frames,
        s.total_xruns(),
        s.backend_xruns,
        s.detected_xruns,
        s.starvations,
        s.latency_shifts,
        s.overloads,
        s.max_load_permille as f64 / 10.0,
    );
}
