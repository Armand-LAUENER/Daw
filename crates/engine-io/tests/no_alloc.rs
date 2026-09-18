//! Enforces the core rule: the audio callback never allocates.
//!
//! `AllocDisabler` aborts the process if anything allocates inside
//! `assert_no_alloc`, so a regression fails this test loudly (debug builds).

use std::sync::Arc;

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use engine_io::{CallbackTiming, Command, SineProcessor, Stats};
use rtrb::RingBuffer;

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

#[test]
fn processing_path_does_not_allocate() {
    let (mut commands, cmd_rx) = RingBuffer::new(64);
    let (ev_tx, mut events) = RingBuffer::new(8);
    let stats = Arc::new(Stats::new());
    let mut proc = SineProcessor::new(48_000, 440.0, 0.2, cmd_rx, ev_tx, Arc::clone(&stats));
    let mut buf = vec![0.0f32; 2 * 1024];

    for i in 0..500u32 {
        commands.push(Command::SetFrequency(200.0 + i as f32)).unwrap();
        let frames = 64 + (i as usize * 37) % 960;
        let out = &mut buf[..frames * 2];
        // Timestamp gaps + size changes exercise the event path, including a full queue.
        // Alternate late and punctual callbacks so both xrun and latency-shift paths run.
        let t = u128::from(i) * 50_000_000;
        let timing = CallbackTiming { callback_ns: if i % 2 == 0 { t } else { t / 2 }, playback_ns: t };
        assert_no_alloc(|| {
            proc.process(out, 2, Some(timing));
            proc.record_load(1_000_000, frames as u32);
        });
    }

    while events.pop().is_ok() {}
    assert!(stats.snapshot().dropped_events > 0, "event overflow path not exercised");
}
