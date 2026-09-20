use std::sync::Arc;
use std::sync::atomic::Ordering;

use cpal::{FromSample, Sample};
use engine_dsp::{LinearSmoother, SineOsc};
use rtrb::{Consumer, Producer};

use crate::control::Command;
use crate::diagnostics::{
    CallbackTiming, DriftMonitor, OVERLOAD_THRESHOLD_PERMILLE, RtEvent, Stats, TimingAnomaly, XrunDetector, XrunSource,
};

const FREQ_GLIDE_MS: f32 = 10.0;
/// Gain ramp, also used as the initial fade-in so the stream never starts on a step.
const GAIN_RAMP_MS: f32 = 50.0;
/// Size of the conversion buffer an external renderer writes into, in samples.
///
/// Covers eight channels of 8192 frames, far above the 480 WASAPI asks for in
/// shared mode and the ~1056 of its catch-up bursts. A larger callback is
/// rendered in several passes rather than allocating.
const MAX_RENDER_SAMPLES: usize = 8_192 * 8;

/// An external source of audio, called once per callback with an interleaved
/// buffer and the device's channel count.
pub type RenderFn = Box<dyn FnMut(&mut [f32], usize) + Send>;

/// What fills the buffer.
///
/// The oscillator is the phase-0 test tone; an external renderer is anything
/// else — a file transport today, a mixer later. It is called from the audio
/// thread, so it carries the same no-allocation contract as this module.
enum Renderer {
    Sine { osc: SineOsc, freq: LinearSmoother },
    External(RenderFn),
}

/// Everything the audio callback owns. Built on the control side, then moved
/// into the callback; after that it is only touched by the audio thread.
///
/// Real-time contract of [`StreamProcessor::process`]: no allocation, no lock,
/// no syscall other than what cpal itself does, bounded time per frame.
pub struct StreamProcessor {
    renderer: Renderer,
    /// Where an external renderer writes before conversion to the device format.
    /// Allocated once, on the control side.
    scratch: Vec<f32>,
    gain: LinearSmoother,
    commands: Consumer<Command>,
    events: Producer<RtEvent>,
    stats: Arc<Stats>,
    xruns: XrunDetector,
    drift: DriftMonitor,
    sample_rate: u32,
    callback_index: u64,
    last_frames: u32,
}

impl StreamProcessor {
    pub fn new(
        sample_rate: u32,
        initial_freq: f32,
        gain: f32,
        commands: Consumer<Command>,
        events: Producer<RtEvent>,
        stats: Arc<Stats>,
    ) -> Self {
        // Runs on the control side, so failing loudly is fine: a NaN here would
        // silence the stream for good (see `apply_commands`).
        assert!(initial_freq.is_finite() && gain.is_finite(), "initial frequency and gain must be finite");
        let sr = sample_rate as f32;
        let mut fade_in = LinearSmoother::with_duration(0.0, GAIN_RAMP_MS, sr);
        fade_in.set_target(gain.clamp(0.0, 1.0));
        let renderer = Renderer::Sine {
            osc: SineOsc::new(sr),
            freq: LinearSmoother::with_duration(initial_freq.clamp(0.0, sr * 0.5), FREQ_GLIDE_MS, sr),
        };
        Self::build(renderer, Vec::new(), fade_in, sample_rate, commands, events, stats)
    }

    /// Builds a processor whose audio comes from `render` instead of the test
    /// oscillator.
    ///
    /// `render` is handed an interleaved buffer and the channel count, and must
    /// fill all of it — silence included. It runs on the audio thread, so it may
    /// not allocate, lock or do I/O.
    pub fn with_renderer(
        sample_rate: u32,
        gain: f32,
        render: RenderFn,
        commands: Consumer<Command>,
        events: Producer<RtEvent>,
        stats: Arc<Stats>,
    ) -> Self {
        assert!(gain.is_finite(), "gain must be finite");
        let mut fade_in = LinearSmoother::with_duration(0.0, GAIN_RAMP_MS, sample_rate as f32);
        fade_in.set_target(gain.clamp(0.0, 1.0));
        let scratch = vec![0.0; MAX_RENDER_SAMPLES];
        Self::build(Renderer::External(render), scratch, fade_in, sample_rate, commands, events, stats)
    }

    fn build(
        renderer: Renderer,
        scratch: Vec<f32>,
        gain: LinearSmoother,
        sample_rate: u32,
        commands: Consumer<Command>,
        events: Producer<RtEvent>,
        stats: Arc<Stats>,
    ) -> Self {
        Self {
            renderer,
            scratch,
            gain,
            commands,
            events,
            stats,
            xruns: XrunDetector::new(sample_rate),
            drift: DriftMonitor::new(sample_rate),
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
        if channels == 0 || frames == 0 {
            // Nothing to play; ignoring it keeps the buffer-size and timing history intact.
            return;
        }
        self.track_buffer(frames, timing);
        self.apply_commands();

        match &mut self.renderer {
            Renderer::Sine { osc, freq } => {
                for frame in out.chunks_exact_mut(channels) {
                    let s = osc.next_sample(freq.next_value()) * self.gain.next_value();
                    frame.fill(T::from_sample(s));
                }
            }
            Renderer::External(render) => {
                // Rendered in f32 then converted, in passes no larger than the
                // scratch buffer, so an unusually big callback costs an extra
                // round rather than an allocation.
                let per_pass = (self.scratch.len() / channels).max(1);
                for chunk in out.chunks_mut(per_pass * channels) {
                    let buf = &mut self.scratch[..chunk.len()];
                    render(buf, channels);
                    for (out_frame, in_frame) in chunk.chunks_exact_mut(channels).zip(buf.chunks_exact(channels)) {
                        let g = self.gain.next_value();
                        for (dst, src) in out_frame.iter_mut().zip(in_frame) {
                            *dst = T::from_sample(*src * g);
                        }
                    }
                }
            }
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
        self.stats.record_load_bucket(permille);
        if permille > u64::from(OVERLOAD_THRESHOLD_PERMILLE) {
            self.stats.overloads.fetch_add(1, Ordering::Relaxed);
            self.emit(RtEvent::Overload {
                // `process` already advanced the index past the measured callback.
                callback_index: self.callback_index.saturating_sub(1),
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
        let starved = timing.and_then(|t| self.drift.observe(t.callback_ns, frames));
        // A late callback that left a playback gap is also a lead drop: report it once.
        if let Some(lost_frames) = starved
            && !matches!(anomaly, Some(TimingAnomaly::Xrun { .. }))
        {
            self.stats.starvations.fetch_add(1, Ordering::Relaxed);
            self.emit(RtEvent::Xrun {
                source: XrunSource::Starvation,
                callback_index: self.callback_index,
                lost_frames,
            });
        }

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
            // `clamp` passes NaN through, and a NaN phase never recovers.
            let (Command::SetFrequency(value) | Command::SetGain(value)) = cmd;
            if !value.is_finite() {
                self.stats.commands_rejected.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            match cmd {
                Command::SetFrequency(hz) => {
                    // Meaningless for an external renderer, which owns its own
                    // pitch; still counted as applied so the caller is not told
                    // the command was rejected as invalid.
                    if let Renderer::Sine { freq, .. } = &mut self.renderer {
                        let nyquist = self.sample_rate as f32 * 0.5;
                        freq.set_target(hz.clamp(0.0, nyquist));
                    }
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
        proc: StreamProcessor,
        commands: Producer<Command>,
        events: Consumer<RtEvent>,
        stats: Arc<Stats>,
    }

    fn harness(freq: f32) -> Harness {
        let (commands, cmd_rx) = RingBuffer::new(64);
        let (ev_tx, events) = RingBuffer::new(64);
        let stats = Arc::new(Stats::new());
        let proc = StreamProcessor::new(SR, freq, 0.5, cmd_rx, ev_tx, Arc::clone(&stats));
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
    fn non_finite_commands_are_rejected() {
        let mut h = harness(440.0);
        for cmd in [Command::SetFrequency(f32::NAN), Command::SetGain(f32::NAN), Command::SetGain(f32::INFINITY)] {
            h.commands.push(cmd).unwrap();
        }
        let mut buf = vec![0.0f32; 4800];
        h.proc.process(&mut buf, 1, None);
        h.commands.push(Command::SetFrequency(880.0)).unwrap();
        h.proc.process(&mut buf, 1, None);

        assert!(buf.iter().all(|s| s.is_finite()));
        let s = h.stats.snapshot();
        assert_eq!((s.commands_rejected, s.commands_applied), (3, 1));
    }

    #[test]
    fn late_callback_is_one_xrun_not_also_a_starvation() {
        let mut h = harness(440.0);
        let ns = |frames: f64| (frames * 1e9 / SR as f64) as u128;
        let at = |frames: f64| CallbackTiming { callback_ns: ns(frames), playback_ns: ns(frames + 4800.0) };
        let mut buf = vec![0.0f32; 512];
        // One second of punctual callbacks arms the starvation detector.
        let mut t = 0.0;
        for _ in 0..100 {
            h.proc.process(&mut buf, 1, Some(at(t)));
            t += 512.0;
        }
        // 200 ms stall: both detectors see it.
        t += 9600.0;
        h.proc.process(&mut buf, 1, Some(at(t)));

        let s = h.stats.snapshot();
        assert_eq!((s.detected_xruns, s.starvations, s.total_xruns()), (1, 0, 1));
    }

    #[test]
    fn overload_is_tagged_with_the_measured_callback() {
        let mut h = harness(440.0);
        let mut buf = vec![0.0f32; 480];
        h.proc.process(&mut buf, 1, None);
        h.proc.record_load(10_000_000, 480); // 10 ms for a 10 ms buffer
        assert_eq!(h.events.pop(), Ok(RtEvent::Overload { callback_index: 0, load_permille: 1000 }));
    }

    #[test]
    fn empty_buffer_is_ignored() {
        let mut h = harness(440.0);
        let mut buf = vec![0.0f32; 512];
        h.proc.process(&mut buf, 1, None);
        h.proc.process(&mut [0.0f32; 0], 1, None);
        h.proc.process(&mut [0.0f32; 1], 2, None); // less than one frame
        h.proc.process(&mut [0.0f32; 4], 0, None);
        h.proc.process(&mut buf, 1, None);

        assert!(h.events.pop().is_err(), "an empty buffer was reported as a size change");
        let s = h.stats.snapshot();
        assert_eq!((s.callbacks, s.min_buffer_frames), (2, 512));
    }

    #[test]
    #[should_panic(expected = "must be finite")]
    fn non_finite_initial_gain_is_refused() {
        let (_, cmd_rx) = RingBuffer::new(1);
        let (ev_tx, _) = RingBuffer::new(1);
        StreamProcessor::new(SR, 440.0, f32::NAN, cmd_rx, ev_tx, Arc::new(Stats::new()));
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
