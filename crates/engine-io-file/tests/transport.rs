//! Transport behaviour, driven without an audio device.
//!
//! The preload thread is real, so these tests wait on its progress rather than
//! assuming it. What they assert is what a listener would notice: the position
//! follows the audio, a seek lands where it was asked to, and nothing ever
//! steps faster than the anti-click ramp allows.

use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use engine_io_file::{
    DecodedAudio, MemorySource, Playback, TransportCommand, TransportHandle, TransportOptions, start,
};

const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const BUFFER: usize = 480;
const FADE_FRAMES: usize = (engine_io_file::FADE_MS * 1e-3 * RATE as f32) as usize;
/// Largest step the fade can produce on a full-scale signal, plus the motion of
/// the 100 Hz probe itself, plus slack for the two landing on the same sample.
const MAX_STEP: f32 = 1.0 / FADE_FRAMES as f32 + std::f32::consts::TAU * 100.0 / RATE as f32 + 1e-3;

/// A clip whose left channel is a 100 Hz sine and whose right channel carries
/// the frame index, so a test can tell exactly which frame came out.
fn clip(frames: usize) -> DecodedAudio {
    let mut samples = Vec::with_capacity(frames * usize::from(CHANNELS));
    for i in 0..frames {
        let t = i as f32 / RATE as f32;
        samples.push((std::f32::consts::TAU * 100.0 * t).sin());
        samples.push(i as f32);
    }
    DecodedAudio { samples, channels: CHANNELS, sample_rate: RATE }
}

fn transport(frames: usize) -> (TransportHandle, Playback) {
    let opts = TransportOptions { buffer_ms: 200.0, ..Default::default() };
    start(Box::new(MemorySource::new(clip(frames))), opts)
}

fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(1));
    }
    false
}

/// Plays `buffers` buffers and returns the left channel, which holds the sine.
fn play(playback: &mut Playback, buffers: usize) -> Vec<f32> {
    let mut out = vec![0.0; BUFFER * usize::from(CHANNELS)];
    let mut left = Vec::with_capacity(buffers * BUFFER);
    for _ in 0..buffers {
        playback.fill(&mut out);
        left.extend(out.iter().step_by(usize::from(CHANNELS)));
    }
    left
}

fn assert_no_click(samples: &[f32], what: &str) {
    for (i, w) in samples.windows(2).enumerate() {
        let step = (w[1] - w[0]).abs();
        assert!(step <= MAX_STEP, "{what}: step {step} at sample {i}, limit {MAX_STEP}");
    }
}

#[test]
fn plays_from_the_start_and_tracks_the_position() {
    let (mut handle, mut playback) = transport(RATE as usize);
    handle.send(TransportCommand::Play).unwrap();
    assert!(wait_for(|| handle.shared().is_playing()));
    // Let the preloader get ahead, so the count is not confused by start-up.
    thread::sleep(Duration::from_millis(100));

    play(&mut playback, 10);
    assert_eq!(handle.position(), (10 * BUFFER) as u64);
    assert_eq!(handle.shared().underruns.load(Ordering::Relaxed), 0);
}

#[test]
fn a_pause_fades_out_instead_of_cutting() {
    let (mut handle, mut playback) = transport(RATE as usize);
    handle.send(TransportCommand::Play).unwrap();
    thread::sleep(Duration::from_millis(100));
    let before = play(&mut playback, 5);

    handle.send(TransportCommand::Pause).unwrap();
    assert!(wait_for(|| !handle.shared().is_playing()));
    let after = play(&mut playback, 5);

    let mut whole = before;
    whole.extend_from_slice(&after);
    assert_no_click(&whole, "pause");
    // The tail must actually be silent, not merely quiet.
    assert!(after[after.len() - BUFFER..].iter().all(|s| *s == 0.0), "still playing after the fade");
}

#[test]
fn a_seek_lands_where_it_was_asked_to() {
    let (mut handle, mut playback) = transport(RATE as usize);
    handle.send(TransportCommand::Play).unwrap();
    thread::sleep(Duration::from_millis(100));
    play(&mut playback, 5);

    handle.send(TransportCommand::Seek(20_000)).unwrap();
    // The flush is honoured inside fill, so keep playing until it is answered.
    let shared = handle.shared().clone();
    assert!(wait_for(|| {
        play(&mut playback, 1);
        shared.flush_ack.load(Ordering::Acquire) == shared.flush_request.load(Ordering::Acquire)
            && shared.flush_ack.load(Ordering::Acquire) > 0
    }));
    thread::sleep(Duration::from_millis(50));

    // The frame index rides on the right channel, but so does the fade-in gain,
    // so read it once the ramp has finished rather than at the first sample.
    let mut out = vec![0.0; BUFFER * usize::from(CHANNELS)];
    playback.fill(&mut out);
    playback.fill(&mut out);
    let index = out[(BUFFER - 1) * usize::from(CHANNELS) + 1];
    assert!(index >= 20_000.0, "came back at frame {index}, expected 20000 or later");
    assert!(index < 21_000.0, "came back at frame {index}, too far past the seek");
}

#[test]
fn seeking_does_not_click() {
    let (mut handle, mut playback) = transport(RATE as usize);
    handle.send(TransportCommand::Play).unwrap();
    thread::sleep(Duration::from_millis(100));

    let mut whole = play(&mut playback, 5);
    for target in [30_000u64, 5_000, 40_000] {
        handle.send(TransportCommand::Seek(target)).unwrap();
        thread::sleep(Duration::from_millis(30));
        whole.extend_from_slice(&play(&mut playback, 8));
    }
    assert_no_click(&whole, "repeated seeks");
}

#[test]
fn a_loop_wraps_and_keeps_the_position_inside_it() {
    let (mut handle, mut playback) = transport(RATE as usize);
    handle.send(TransportCommand::SetLoop(Some((0, 2_400)))).unwrap();
    handle.send(TransportCommand::Play).unwrap();
    assert!(wait_for(|| handle.shared().looping() == Some((0, 2_400))));
    thread::sleep(Duration::from_millis(100));

    // Five times round the loop.
    let played = play(&mut playback, 25);
    assert!(handle.position() < 2_400, "position {} ran past the loop", handle.position());
    assert!(!handle.shared().finished.load(Ordering::Relaxed), "a loop should never finish");
    assert!(played.iter().skip(BUFFER).any(|s| *s != 0.0), "the loop stopped producing audio");
}

#[test]
fn the_end_of_the_source_fades_out_and_is_not_an_underrun() {
    // A short clip, played past its end.
    let (mut handle, mut playback) = transport(BUFFER * 3);
    handle.send(TransportCommand::Play).unwrap();
    assert!(wait_for(|| handle.shared().finished.load(Ordering::Relaxed)));

    let played = play(&mut playback, 10);
    assert_no_click(&played, "end of source");
    assert!(played[played.len() - BUFFER..].iter().all(|s| *s == 0.0), "audio past the end of the clip");
    assert_eq!(
        handle.shared().underruns.load(Ordering::Relaxed),
        0,
        "reaching the end is not the preloader falling behind"
    );
}

#[test]
fn a_paused_transport_stays_silent() {
    let (handle, mut playback) = transport(RATE as usize);
    thread::sleep(Duration::from_millis(50));
    let played = play(&mut playback, 5);
    assert!(played.iter().all(|s| *s == 0.0), "audio came out without a Play command");
    assert_eq!(handle.position(), 0);
}
