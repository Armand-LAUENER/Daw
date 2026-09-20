//! The phase 1 signal path, rendered offline.
//!
//! Transport, filter and channel mapping, in the order the audio callback runs
//! them, without a sound card. This is the exit criterion for the phase turned
//! into a test: repeated seeks under a moving filter cutoff must not produce a
//! step the ear would hear as a click.

use std::thread;
use std::time::{Duration, Instant};

use engine_dsp::{FilterKind, SmoothedBiquad};
use engine_io::map_frames;
use engine_io_file::{DecodedAudio, MemorySource, TransportCommand, TransportOptions, start};

const RATE: u32 = 48_000;
const FILE_CHANNELS: usize = 2;
/// What a 7.1 headset asks for, the case the mapping exists to handle.
const DEVICE_CHANNELS: usize = 8;
const BUFFER_FRAMES: usize = 480;
const PROBE_HZ: f32 = 100.0;

/// Largest step the chain may produce: the transport's fade on a full-scale
/// sample, plus the probe's own motion, plus slack.
const MAX_STEP: f32 =
    1.0 / (engine_io_file::FADE_MS * 1e-3 * RATE as f32) + std::f32::consts::TAU * PROBE_HZ / RATE as f32 + 1e-3;

fn clip(seconds: f32) -> DecodedAudio {
    let frames = (seconds * RATE as f32) as usize;
    let mut samples = Vec::with_capacity(frames * FILE_CHANNELS);
    for i in 0..frames {
        let value = (std::f32::consts::TAU * PROBE_HZ * i as f32 / RATE as f32).sin();
        samples.push(value);
        samples.push(value);
    }
    DecodedAudio { samples, channels: FILE_CHANNELS as u16, sample_rate: RATE }
}

#[test]
fn seeking_under_a_moving_cutoff_never_clicks() {
    let (mut handle, mut playback) =
        start(Box::new(MemorySource::new(clip(10.0))), TransportOptions { buffer_ms: 200.0, ..Default::default() });
    handle.send(TransportCommand::Play).unwrap();

    // One filter per channel: a biquad carries state, so sharing one would mix
    // the two together.
    let mut filters: Vec<SmoothedBiquad> = (0..FILE_CHANNELS)
        .map(|_| SmoothedBiquad::new(FilterKind::LowPass, 2_000.0, 0.707, RATE as f32, 10.0))
        .collect();

    let mut from_file = vec![0.0f32; BUFFER_FRAMES * FILE_CHANNELS];
    let mut to_device = vec![0.0f32; BUFFER_FRAMES * DEVICE_CHANNELS];
    let mut left: Vec<f32> = Vec::new();

    // Let the preloader get ahead before measuring.
    thread::sleep(Duration::from_millis(100));

    for block in 0..400 {
        match block {
            50 => handle.send(TransportCommand::Seek(200_000)).unwrap(),
            150 => handle.send(TransportCommand::Seek(20_000)).unwrap(),
            250 => handle.send(TransportCommand::Seek(400_000)).unwrap(),
            _ => {}
        }
        // Sweep the cutoff throughout, so the filter is never at rest.
        let t = block as f32 / 400.0;
        let cutoff = 300.0 * (8_000.0f32 / 300.0).powf(t);
        for filter in &mut filters {
            filter.set_cutoff(cutoff);
        }

        playback.fill(&mut from_file);
        for (channel, filter) in filters.iter_mut().enumerate() {
            for frame in 0..BUFFER_FRAMES {
                let index = frame * FILE_CHANNELS + channel;
                from_file[index] = filter.process_sample(from_file[index]);
            }
        }
        map_frames(&from_file, FILE_CHANNELS, &mut to_device, DEVICE_CHANNELS);
        left.extend(to_device.iter().step_by(DEVICE_CHANNELS));

        // The preloader needs a moment to refill after a flush.
        if matches!(block, 50 | 150 | 250) {
            thread::sleep(Duration::from_millis(30));
        }
    }

    for (i, w) in left.windows(2).enumerate() {
        let step = (w[1] - w[0]).abs();
        assert!(step <= MAX_STEP, "step {step} at sample {i}, limit {MAX_STEP}");
    }
    assert!(left.iter().any(|s| *s != 0.0), "the pipeline produced nothing");
}

#[test]
fn the_rear_channels_of_a_surround_device_stay_silent() {
    // The headset presents eight channels; a stereo file must not be smeared
    // across them, and the six it does not fill must be silent.
    let (mut handle, mut playback) =
        start(Box::new(MemorySource::new(clip(1.0))), TransportOptions { buffer_ms: 200.0, ..Default::default() });
    handle.send(TransportCommand::Play).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut from_file = vec![0.0f32; BUFFER_FRAMES * FILE_CHANNELS];
    let mut to_device = vec![0.0f32; BUFFER_FRAMES * DEVICE_CHANNELS];
    let mut heard_audio = false;

    while Instant::now() < deadline && !heard_audio {
        playback.fill(&mut from_file);
        map_frames(&from_file, FILE_CHANNELS, &mut to_device, DEVICE_CHANNELS);
        heard_audio = to_device.iter().step_by(DEVICE_CHANNELS).any(|s| s.abs() > 0.1);

        for frame in to_device.as_chunks::<DEVICE_CHANNELS>().0 {
            assert!(frame[2..].iter().all(|s| *s == 0.0), "audio leaked into the rear channels: {frame:?}");
        }
    }
    assert!(heard_audio, "no audio reached the front channels");
}
