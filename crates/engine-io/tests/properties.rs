//! Property tests for the audio-thread logic, driven without a sound card.

use std::sync::Arc;

use engine_io::{
    CallbackTiming, Command, DRIFT_LEAK, DriftMonitor, Stats, StreamProcessor, TimingAnomaly, XrunDetector,
};
use proptest::collection::vec;
use proptest::prelude::*;
use rtrb::RingBuffer;

const SR: u32 = 48_000;

fn value() -> impl Strategy<Value = f32> {
    prop_oneof![-1e5f32..1e5, 0.0f32..1.0, any::<f32>(), Just(f32::NAN), Just(f32::INFINITY), Just(f32::NEG_INFINITY),]
}

fn command() -> impl Strategy<Value = Command> {
    prop_oneof![value().prop_map(Command::SetFrequency), value().prop_map(Command::SetGain)]
}

fn seconds_to_ns(s: f64) -> u128 {
    (s * 1e9) as u128
}

/// Deterministic uniform noise in [-1, 1], so failing cases shrink and replay.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }
}

proptest! {
    /// Whatever the control side sends, the output stays a valid audio signal.
    #[test]
    fn processor_output_is_always_finite_and_bounded(
        callbacks in vec((vec(command(), 0..4), 0usize..1024, 1usize..=8), 1..40),
    ) {
        let (mut tx, cmd_rx) = RingBuffer::new(256);
        let (ev_tx, _events) = RingBuffer::new(64);
        let stats = Arc::new(Stats::new());
        let mut proc = StreamProcessor::new(SR, 440.0, 0.5, cmd_rx, ev_tx, Arc::clone(&stats));
        let (mut pushed, mut non_finite) = (0u64, 0u64);

        for (commands, frames, channels) in callbacks {
            for cmd in commands {
                let (Command::SetFrequency(v) | Command::SetGain(v)) = cmd;
                non_finite += u64::from(!v.is_finite());
                pushed += 1;
                tx.push(cmd).unwrap();
            }
            let mut buf = vec![0.0f32; frames * channels];
            proc.process(&mut buf, channels, None);
            for frame in buf.chunks_exact(channels) {
                prop_assert!(frame[0].is_finite() && frame[0].abs() <= 1.0, "sample {}", frame[0]);
                prop_assert!(frame.iter().all(|&s| s == frame[0]), "channels differ");
            }
        }
        // Commands pending behind empty buffers are applied by the next real one.
        proc.process(&mut [0.0f32; 1], 1, None);

        let s = stats.snapshot();
        prop_assert_eq!(s.commands_applied + s.commands_rejected, pushed);
        prop_assert_eq!(s.commands_rejected, non_finite);
    }

    /// Contiguous buffers never raise an anomaly, whatever their sizes and the
    /// timestamp jitter, as long as the jitter stays under the tolerance.
    #[test]
    fn contiguous_stream_is_never_flagged(
        buffers in vec(64u32..4096, 2..200),
        latency in 0u32..48_000,
        seed in 1u64..,
    ) {
        let mut noise = Noise(seed);
        let min_buf = f64::from(*buffers.iter().min().unwrap());
        let ns = |frames: f64| (frames * 1e9 / f64::from(SR)) as u128;
        let mut d = XrunDetector::new(SR);
        let mut t = 48_000.0;
        for frames in buffers {
            let jitter = noise.next() * 0.2 * min_buf;
            // Callback timestamps may wander freely: only playback gaps matter.
            let callback = t - noise.next().abs() * min_buf;
            let timing = CallbackTiming { callback_ns: ns(callback), playback_ns: ns(t + f64::from(latency) + jitter) };
            prop_assert_eq!(d.observe(timing, frames), None);
            t += f64::from(frames);
        }
    }

    /// A late callback followed by an equal playback gap is an xrun of that size.
    #[test]
    fn late_callback_with_playback_gap_is_an_xrun_of_its_size(buf in 64u32..4096, gap in 0u32..96_000) {
        let gap = gap.max(buf); // above the half-buffer tolerance
        let ns = |frames: u32| u128::from(frames) * 1_000_000_000 / u128::from(SR);
        let at = |f: u32| CallbackTiming { callback_ns: ns(f), playback_ns: ns(f + 4_800) };
        let mut d = XrunDetector::new(SR);
        d.observe(at(0), buf);
        match d.observe(at(buf + gap), buf) {
            Some(TimingAnomaly::Xrun { lost_frames }) => prop_assert!(lost_frames.abs_diff(u64::from(gap)) <= 1),
            other => prop_assert!(false, "expected an xrun, got {other:?}"),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Jitter up to half a buffer and crystal drift up to 200 ppm are normal
    /// operation: 30 s of it must not report any starvation.
    #[test]
    fn healthy_stream_never_reports_starvation(
        buf in 64u32..2048,
        jitter in 0.0f64..0.5,
        ppm in -200.0f64..200.0,
        seed in 1u64..,
    ) {
        let mut noise = Noise(seed);
        let period = f64::from(buf) / f64::from(SR) / (1.0 + ppm * 1e-6);
        let jitter_s = jitter * f64::from(buf) / f64::from(SR);
        let mut m = DriftMonitor::new(SR);
        for k in 0..(30.0 / period) as u64 {
            let t = 1.0 + k as f64 * period + noise.next() * jitter_s;
            prop_assert_eq!(m.observe(seconds_to_ns(t), buf), None, "report at {:.3} s", t - 1.0);
        }
    }

    /// A stall is reported once, with its size.
    #[test]
    fn stall_is_reported_once_with_its_size(buf in 64u32..2048, stall in 0.1f64..2.0) {
        let period = f64::from(buf) / f64::from(SR);
        let mut m = DriftMonitor::new(SR);
        let mut t = 0.0;
        while t < 2.0 {
            prop_assert_eq!(m.observe(seconds_to_ns(t), buf), None);
            t += period;
        }
        t += stall;
        let deficit = m.observe(seconds_to_ns(t), buf);
        // The reference leaks during the stall too.
        let expected = stall * f64::from(SR) * (1.0 - DRIFT_LEAK);
        prop_assert!(
            deficit.is_some_and(|d| (d as f64 - expected).abs() <= f64::from(buf)),
            "deficit {deficit:?}, expected ~{expected:.0}"
        );
        for _ in 0..(5.0 / period) as u64 {
            t += period;
            prop_assert_eq!(m.observe(seconds_to_ns(t), buf), None, "stall reported twice");
        }
    }
}
