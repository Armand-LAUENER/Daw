use std::sync::Arc;
use std::sync::atomic::Ordering;

use cpal::{FromSample, Sample};
use engine_dsp::{LinearSmoother, SineOsc};
use rtrb::{Consumer, Producer};

use crate::control::Command;
use crate::diagnostics::{
    CallbackTiming, OVERLOAD_THRESHOLD_PERMILLE, RtEvent, Stats, TimingAnomaly, XrunDetector, XrunSource,
};

const FREQ_GLIDE_MS: f32 = 10.0;
/// Gain ramp, also used as the initial fade-in so the stream never starts on a step.
const GAIN_RAMP_MS: f32 = 50.0;

/// Everything the audio callback owns. Built on the control side, then moved
/// into the callback; after that it is only touched by the audio thread.
///
/// Real-time contract of [`SineProcessor::process`]: no allocation, no lock,
/// no syscall other than what cpal itself does, bounded time per frame.
pub struct SineProcessor {
    osc: SineOsc,
    freq: LinearSmoother,
    gain: LinearSmoother,
    commands: Consumer<Command>,
    events: Producer<RtEvent>,
    stats: Arc<Stats>,
    xruns: XrunDetector,
    sample_rate: u32,
    callback_index: u64,
    last_frames: u32,
}

impl SineProcessor {
    pub fn new(
        sample_rate: u32,
        initial_freq: f32,
        gain: f32,
        commands: Consumer<Command>,
        events: Producer<RtEvent>,
        stats: Arc<Stats>,
    ) -> Self {
        let sr = sample_rate as f32;
        let mut fade_in = LinearSmoother::with_duration(0.0, GAIN_RAMP_MS, sr);
        fade_in.set_target(gain);
        Self {
            osc: SineOsc::new(sr),
            freq: LinearSmoother::with_duration(initial_freq, FREQ_GLIDE_MS, sr),
            gain: fade_in,
            commands,
            events,
            stats,
            xruns: XrunDetector::new(sample_rate),
            sample_rate,
            callback_index: 0,
            last_frames: 0,
        }
    }

    /// Fills an interleaved buffer. `timing` feeds xrun detection when the
    /// backend provides timestamps.
    pub fn process<T>(&mut self, out: &mut [T], channels: usize, timing: Option<CallbackTiming>)
    where
        T: Sample + FromSample<f32>,
    {
        let frames = (out.len() / channels.max(1)) as u32;
        self.track_buffer(frames, timing);
        self.apply_commands();

        for frame in out.chunks_exact_mut(channels) {
            let s = self.osc.next_sample(self.freq.next_value()) * self.gain.next_value();
            frame.fill(T::from_sample(s));
        }

        self.callback_index += 1;
        self.stats.callbacks.fetch_add(1, Ordering::Relaxed);
        self.stats.frames.fetch_add(u64::from(frames), Ordering::Relaxed);
    }

    /// Records how long the last `process` call took relative to its buffer duration.
    pub fn record_load(&mut self, elapsed_ns: u64, frames: u32) {
        if frames == 0 {
            return;
        }
        let budget_ns = u64::from(frames) * 1_000_000_000 / u64::from(self.sample_rate);
        let permille = elapsed_ns * 1000 / budget_ns.max(1);
        self.stats.max_load_permille.fetch_max(permille, Ordering::Relaxed);
        if permille > u64::from(OVERLOAD_THRESHOLD_PERMILLE) {
            self.stats.overloads.fetch_add(1, Ordering::Relaxed);
            self.emit(RtEvent::Overload {
                callback_index: self.callback_index,
                load_permille: permille.min(u64::from(u32::MAX)) as u32,
            });
        }
    }

    fn track_buffer(&mut self, frames: u32, timing: Option<CallbackTiming>) {
        self.stats.min_buffer_frames.fetch_min(u64::from(frames), Ordering::Relaxed);
        self.stats.max_buffer_frames.fetch_max(u64::from(frames), Ordering::Relaxed);
        if self.last_frames != 0 && frames != self.last_frames {
            self.emit(RtEvent::BufferSizeChanged {
                callback_index: self.callback_index,
                from: self.last_frames,
                to: frames,
            });
        }
        self.last_frames = frames;

        let anomaly = timing.and_then(|t| self.xruns.observe(t, frames));
        match anomaly {
            None => {}
            Some(TimingAnomaly::Xrun { lost_frames }) => {
                self.stats.detected_xruns.fetch_add(1, Ordering::Relaxed);
                self.emit(RtEvent::Xrun {
                    source: XrunSource::TimestampGap,
                    callback_index: self.callback_index,
                    lost_frames,
                });
            }
            Some(TimingAnomaly::LatencyShift { delta_frames }) => {
                self.stats.latency_shifts.fetch_add(1, Ordering::Relaxed);
                self.emit(RtEvent::LatencyShift { callback_index: self.callback_index, delta_frames });
            }
        }
    }

    fn apply_commands(&mut self) {
        // Bounded by the ring buffer capacity, so this loop is bounded too.
        while let Ok(cmd) = self.commands.pop() {
            match cmd {
                Command::SetFrequency(hz) => {
                    let nyquist = self.sample_rate as f32 * 0.5;
                    self.freq.set_target(hz.clamp(0.0, nyquist));
                }
                Command::SetGain(g) => {
                    self.gain.set_target(g.clamp(0.0, 1.0));
                }
            }
            self.stats.commands_applied.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn emit(&mut self, event: RtEvent) {
        if self.events.push(event).is_err() {
            self.stats.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    const SR: u32 = 48_000;

    struct Harness {
        proc: SineProcessor,
        commands: Producer<Command>,
        events: Consumer<RtEvent>,
        stats: Arc<Stats>,
    }

    fn harness(freq: f32) -> Harness {
        let (commands, cmd_rx) = RingBuffer::new(64);
        let (ev_tx, events) = RingBuffer::new(64);
        let stats = Arc::new(Stats::new());
        let proc = SineProcessor::new(SR, freq, 0.5, cmd_rx, ev_tx, Arc::clone(&stats));
        Harness { proc, commands, events, stats }
    }

    #[test]
    fn stereo_channels_carry_the_same_signal() {
        let mut h = harness(440.0);
        let mut buf = vec![0.0f32; 512 * 2];
        h.proc.process(&mut buf, 2, None);
        assert!(buf.as_chunks::<2>().0.iter().all(|[l, r]| l == r));
    }

    #[test]
    fn starts_with_fade_in() {
        let mut h = harness(440.0);
        let mut buf = vec![0.0f32; 4800];
        h.proc.process(&mut buf, 1, None);
        let peak = |s: &[f32]| s.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak(&buf[..240]) < 0.05, "no fade-in at stream start");
        assert!(peak(&buf[2400..]) > 0.45);
    }

    #[test]
    fn frequency_change_has_no_discontinuity() {
        let mut h = harness(220.0);
        let mut prev: Option<f32> = None;
        let max_step = std::f32::consts::TAU * 1760.0 / SR as f32 * 0.5 * 1.01;
        for i in 0..200 {
            if i % 10 == 5 {
                let f = if (i / 10) % 2 == 0 { 1760.0 } else { 220.0 };
                h.commands.push(Command::SetFrequency(f)).unwrap();
            }
            // Variable buffer sizes, as some backends deliver.
            let mut buf = vec![0.0f32; 128 + (i % 7) * 61];
            h.proc.process(&mut buf, 1, None);
            for &s in &buf {
                if let Some(p) = prev {
                    assert!((s - p).abs() <= max_step, "step {} at buffer {i}", (s - p).abs());
                }
                prev = Some(s);
            }
        }
        assert_eq!(h.stats.snapshot().commands_applied, 20);
    }

    #[test]
    fn reports_buffer_size_change_and_timestamp_gap() {
        let mut h = harness(440.0);
        let ns = |frames: f64| (frames * 1e9 / SR as f64) as u128;
        let at = |frames: f64| CallbackTiming { callback_ns: ns(frames), playback_ns: ns(frames + 4800.0) };
        let mut buf = vec![0.0f32; 512];
        h.proc.process(&mut buf, 1, Some(at(0.0)));
        // Next callback comes 1536 frames late and playback resumes that much later.
        let mut small = vec![0.0f32; 256];
        h.proc.process(&mut small, 1, Some(at(2048.0)));

        let events: Vec<RtEvent> = std::iter::from_fn(|| h.events.pop().ok()).collect();
        assert!(events.contains(&RtEvent::BufferSizeChanged { callback_index: 1, from: 512, to: 256 }));
        assert!(events.contains(&RtEvent::Xrun {
            source: XrunSource::TimestampGap,
            callback_index: 1,
            lost_frames: 1536,
        }));
        assert_eq!(h.stats.snapshot().detected_xruns, 1);
    }

    #[test]
    fn full_event_queue_counts_drops_instead_of_blocking() {
        let mut h = harness(440.0);
        for i in 0..100u32 {
            let mut buf = vec![0.0f32; 100 + (i % 2) as usize];
            h.proc.process(&mut buf, 1, None);
        }
        let s = h.stats.snapshot();
        assert_eq!(s.dropped_events, 99 - 64);
    }
}
