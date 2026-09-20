//! The transport's audio-thread half must not allocate.
//!
//! The rest of this crate allocates freely — decoding and preloading are meant
//! to. `Playback::fill` is the one path the audio callback runs, and it is held
//! to the same rule as `engine-io`: `AllocDisabler` aborts the process if
//! anything allocates inside `assert_no_alloc`. Debug builds only, since
//! `assert_no_alloc` compiles to nothing in release.
#![cfg(debug_assertions)]

use std::thread;
use std::time::Duration;

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use engine_io_file::{DecodedAudio, MemorySource, TransportCommand, TransportOptions, start};

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

#[test]
fn filling_the_output_does_not_allocate() {
    let channels = 2u16;
    let audio = DecodedAudio {
        samples: (0..48_000 * usize::from(channels)).map(|i| (i % 100) as f32 / 100.0).collect(),
        channels,
        sample_rate: 48_000,
    };
    let (mut handle, mut playback) =
        start(Box::new(MemorySource::new(audio)), TransportOptions { buffer_ms: 200.0, fade_ms: 5.0 });

    handle.send(TransportCommand::Play).unwrap();
    thread::sleep(Duration::from_millis(100));

    let mut buf = vec![0.0f32; 1_024 * usize::from(channels)];
    for i in 0..400u32 {
        // Vary the buffer size, and cross a seek and a pause, so the flush and
        // fade paths are covered and not just the steady one.
        let frames = 64 + (i as usize * 37) % 960;
        match i {
            100 => handle.send(TransportCommand::Seek(20_000)).unwrap(),
            200 => handle.send(TransportCommand::Pause).unwrap(),
            300 => handle.send(TransportCommand::Play).unwrap(),
            _ => {}
        }
        let out = &mut buf[..frames * usize::from(channels)];
        assert_no_alloc(|| playback.fill(out));
    }
}
