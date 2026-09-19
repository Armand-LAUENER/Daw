use std::sync::atomic::{AtomicU64, Ordering};

/// Events emitted by the audio thread, drained by a non-real-time logger.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RtEvent {
    Xrun {
        source: XrunSource,
        callback_index: u64,
        /// Estimated number of frames lost (0 when the backend does not say).
        lost_frames: u64,
    },
    /// The playback timestamp jumped while callbacks stayed on time: the backend's
    /// latency estimate moved, no audio was lost. Informational only.
    LatencyShift { callback_index: u64, delta_frames: i64 },
    BufferSizeChanged { callback_index: u64, from: u32, to: u32 },
    /// Callback processing took more than [`OVERLOAD_THRESHOLD_PERMILLE`] of the buffer duration.
    Overload { callback_index: u64, load_permille: u32 },
}

/// Fraction of the buffer duration (in ‰) above which a callback is reported as overloaded.
pub const OVERLOAD_THRESHOLD_PERMILLE: u32 = 700;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XrunSource {
    /// Reported by cpal through the stream error callback.
    Backend,
    /// Inferred from a playback timestamp gap explained by a late callback.
    TimestampGap,
    /// Inferred from a drop in cumulative lead: callbacks delivered less audio
    /// than real time consumed. Catches progressive stalls that leave the
    /// playback timestamps frozen.
    Starvation,
}

/// Labels of the per-kind stream error counters, indexed like [`Stats::error_kinds`].
pub const STREAM_ERROR_KINDS: [&str; 14] = [
    "DeviceBusy",
    "DeviceChanged",
    "DeviceNotAvailable",
    "HostUnavailable",
    "InvalidInput",
    "PermissionDenied",
    "RealtimeDenied",
    "ResourceExhausted",
    "StreamInvalidated",
    "UnsupportedConfig",
    "UnsupportedOperation",
    "Xrun",
    "BackendError",
    "Other",
];

/// Counters shared between the audio thread and observers.
///
/// Written with `Relaxed` atomics only: they are statistics, not synchronisation.
#[derive(Debug, Default)]
pub struct Stats {
    pub callbacks: AtomicU64,
    pub frames: AtomicU64,
    pub backend_xruns: AtomicU64,
    pub detected_xruns: AtomicU64,
    pub starvations: AtomicU64,
    pub overloads: AtomicU64,
    pub latency_shifts: AtomicU64,
    pub commands_applied: AtomicU64,
    /// Commands ignored because their value was not finite (NaN, infinity).
    pub commands_rejected: AtomicU64,
    /// Events lost because the event ring buffer was full.
    pub dropped_events: AtomicU64,
    /// Stream errors reported by the backend other than xruns.
    pub stream_errors: AtomicU64,
    /// All backend stream errors by kind, see [`STREAM_ERROR_KINDS`].
    pub error_kinds: [AtomicU64; STREAM_ERROR_KINDS.len()],
    pub max_load_permille: AtomicU64,
    pub min_buffer_frames: AtomicU64,
    pub max_buffer_frames: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub callbacks: u64,
    pub frames: u64,
    pub backend_xruns: u64,
    pub detected_xruns: u64,
    pub starvations: u64,
    pub overloads: u64,
    pub latency_shifts: u64,
    pub commands_applied: u64,
    pub commands_rejected: u64,
    pub dropped_events: u64,
    pub stream_errors: u64,
    pub error_kinds: [u64; STREAM_ERROR_KINDS.len()],
    pub max_load_permille: u64,
    pub min_buffer_frames: u64,
    pub max_buffer_frames: u64,
}

impl Stats {
    pub fn new() -> Self {
        Self {
            min_buffer_frames: AtomicU64::new(u64::MAX),
            ..Self::default()
        }
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        StatsSnapshot {
            callbacks: get(&self.callbacks),
            frames: get(&self.frames),
            backend_xruns: get(&self.backend_xruns),
            detected_xruns: get(&self.detected_xruns),
            starvations: get(&self.starvations),
            overloads: get(&self.overloads),
            latency_shifts: get(&self.latency_shifts),
            commands_applied: get(&self.commands_applied),
            commands_rejected: get(&self.commands_rejected),
            dropped_events: get(&self.dropped_events),
            stream_errors: get(&self.stream_errors),
            error_kinds: std::array::from_fn(|i| get(&self.error_kinds[i])),
            max_load_permille: get(&self.max_load_permille),
            min_buffer_frames: get(&self.min_buffer_frames),
            max_buffer_frames: get(&self.max_buffer_frames),
        }
    }
}

impl StatsSnapshot {
    /// Distinct xruns: a stall seen by both the timestamp and the starvation
    /// detector is only counted once, as a timestamp gap.
    pub fn total_xruns(&self) -> u64 {
        self.backend_xruns + self.detected_xruns + self.starvations
    }

    /// Non-zero error counters as `(kind, count)`.
    pub fn error_breakdown(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        STREAM_ERROR_KINDS.iter().zip(self.error_kinds).filter(|(_, n)| *n > 0).map(|(k, n)| (*k, n))
    }
}

/// Timestamps of one output callback, in nanoseconds on the stream clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackTiming {
    /// When the callback was invoked.
    pub callback_ns: u128,
    /// Predicted playback instant of the first frame of the buffer.
    pub playback_ns: u128,
}

/// Result of feeding one callback to the [`XrunDetector`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingAnomaly {
    Xrun { lost_frames: u64 },
    LatencyShift { delta_frames: i64 },
}

/// Detects lost output from callback timestamps.
///
/// Successive playback timestamps should advance by exactly the duration of
/// the previous buffer. A deviation has two possible causes:
/// - **underrun**: the callback came late, the device ran dry, and playback
///   resumes later than planned. The callback lateness explains the gap.
/// - **latency re-estimation**: callbacks are on time but the backend revised
///   its output delay (typical of the ALSA `pulse` plugin while its buffer
///   fills). Nothing was lost.
///
/// Independent of cpal so it can be unit-tested with synthetic timestamps.
#[derive(Debug, Clone)]
pub struct XrunDetector {
    sample_rate: f64,
    last: Option<(CallbackTiming, u32)>,
}

impl XrunDetector {
    pub fn new(sample_rate: u32) -> Self {
        Self { sample_rate: f64::from(sample_rate), last: None }
    }

    /// Feeds the timestamps and frame count of a callback.
    pub fn observe(&mut self, timing: CallbackTiming, frames: u32) -> Option<TimingAnomaly> {
        let (prev, prev_frames) = self.last.replace((timing, frames))?;
        let prev_duration = self.frames_to_ns(f64::from(prev_frames));
        // Timestamps jitter; deviations under half a buffer are ignored.
        let tolerance = self.frames_to_ns(f64::from(prev_frames.max(frames)) * 0.5);

        let playback_gap = signed_diff(timing.playback_ns, prev.playback_ns) - prev_duration;
        if playback_gap.abs() <= tolerance {
            return None;
        }
        let callback_lateness = signed_diff(timing.callback_ns, prev.callback_ns) - prev_duration;
        if playback_gap > 0.0 && callback_lateness >= playback_gap * 0.5 {
            Some(TimingAnomaly::Xrun { lost_frames: self.ns_to_frames(playback_gap).round() as u64 })
        } else {
            Some(TimingAnomaly::LatencyShift { delta_frames: self.ns_to_frames(playback_gap).round() as i64 })
        }
    }

    pub fn reset(&mut self) {
        self.last = None;
    }

    fn frames_to_ns(&self, frames: f64) -> f64 {
        frames * 1e9 / self.sample_rate
    }

    fn ns_to_frames(&self, ns: f64) -> f64 {
        ns * self.sample_rate / 1e9
    }
}

fn signed_diff(a: u128, b: u128) -> f64 {
    a as f64 - b as f64
}

/// Detects audio starvation from the cumulative lead of delivered audio.
///
/// `lead = frames delivered - elapsed callback time * sample rate`. In steady
/// state it only jitters by about a buffer. If callbacks stop coming or come
/// too slowly, the device keeps consuming and the lead falls: that is lost
/// audio, whatever the backend's playback timestamps claim.
///
/// The reference is a *leaky maximum*: it rises instantly with the lead and
/// decays at [`DRIFT_LEAK`] of real time, which absorbs the slow drift between
/// the device crystal and the system clock (typically < 100 ppm) while a stall
/// (lead falling at several % of real time or more) still crosses the threshold.
#[derive(Debug, Clone)]
pub struct DriftMonitor {
    sample_rate: f64,
    origin_ns: Option<u128>,
    delivered: u64,
    reference: f64,
    last_elapsed: f64,
    max_buffer: u32,
}

/// Decay rate of the reference, as a fraction of real time (1000 ppm).
pub const DRIFT_LEAK: f64 = 1e-3;
/// Minimum lead drop reported as starvation, in seconds.
const DRIFT_MIN_THRESHOLD_S: f64 = 0.05;
/// The device clock does not start exactly at the first callback; ignore
/// the lead during this settling period.
const DRIFT_ARM_DELAY_S: f64 = 0.5;

impl DriftMonitor {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate: f64::from(sample_rate),
            origin_ns: None,
            delivered: 0,
            reference: f64::NEG_INFINITY,
            last_elapsed: 0.0,
            max_buffer: 0,
        }
    }

    /// Feeds one callback. Returns the number of frames starved since the
    /// reference, if above threshold. Each stall is reported once.
    pub fn observe(&mut self, callback_ns: u128, frames: u32) -> Option<u64> {
        let origin = *self.origin_ns.get_or_insert(callback_ns);
        let elapsed = signed_diff(callback_ns, origin) / 1e9;
        let lead = self.delivered as f64 - elapsed * self.sample_rate;
        self.delivered += u64::from(frames);
        self.max_buffer = self.max_buffer.max(frames);

        let dt = (elapsed - self.last_elapsed).max(0.0);
        self.last_elapsed = elapsed;
        if elapsed < DRIFT_ARM_DELAY_S {
            self.reference = lead;
            return None;
        }

        let leaked = self.reference - DRIFT_LEAK * dt * self.sample_rate;
        let threshold = (2.0 * f64::from(self.max_buffer)).max(DRIFT_MIN_THRESHOLD_S * self.sample_rate);
        let deficit = leaked - lead;
        if deficit > threshold {
            // Lost audio does not come back: rebase so the stall is reported once.
            self.reference = lead;
            Some(deficit.round() as u64)
        } else {
            self.reference = leaked.max(lead);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn ns(frames: i64) -> u128 {
        (frames as f64 * 1e9 / f64::from(SR)) as u128
    }

    /// Callback invoked at frame `cb`, playback predicted at frame `pb`.
    fn at(cb: i64, pb: i64) -> CallbackTiming {
        CallbackTiming { callback_ns: ns(cb), playback_ns: ns(pb) }
    }

    const LATENCY: i64 = 4_800;

    #[test]
    fn contiguous_buffers_are_clean() {
        let mut d = XrunDetector::new(SR);
        let mut t = 0;
        for frames in [512u32, 512, 480, 512, 1024, 256] {
            assert_eq!(d.observe(at(t, t + LATENCY), frames), None);
            t += i64::from(frames);
        }
    }

    #[test]
    fn small_jitter_is_tolerated() {
        let mut d = XrunDetector::new(SR);
        d.observe(at(0, LATENCY), 512);
        assert_eq!(d.observe(at(700, 512 + LATENCY + 200), 512), None);
    }

    #[test]
    fn late_callback_with_playback_gap_is_an_xrun() {
        let mut d = XrunDetector::new(SR);
        d.observe(at(0, LATENCY), 512);
        // Callback 1024 frames late, playback resumes 1024 frames later than planned.
        let anomaly = d.observe(at(512 + 1024, 512 + 1024 + LATENCY), 512);
        assert_eq!(anomaly, Some(TimingAnomaly::Xrun { lost_frames: 1024 }));
    }

    #[test]
    fn playback_jump_with_punctual_callback_is_a_latency_shift() {
        let mut d = XrunDetector::new(SR);
        d.observe(at(0, LATENCY), 512);
        let anomaly = d.observe(at(512, 512 + LATENCY + 1000), 512);
        assert_eq!(anomaly, Some(TimingAnomaly::LatencyShift { delta_frames: 1000 }));
    }

    #[test]
    fn latency_decrease_is_a_latency_shift() {
        let mut d = XrunDetector::new(SR);
        d.observe(at(0, LATENCY), 512);
        let anomaly = d.observe(at(512, 512 + LATENCY - 2000), 512);
        assert_eq!(anomaly, Some(TimingAnomaly::LatencyShift { delta_frames: -2000 }));
    }

    #[test]
    fn late_callback_without_playback_gap_is_clean() {
        // A deep backend buffer absorbed the late callback: nothing was lost.
        let mut d = XrunDetector::new(SR);
        d.observe(at(0, LATENCY), 512);
        assert_eq!(d.observe(at(2048, 512 + LATENCY), 512), None);
    }

    mod drift {
        use super::super::*;

        const SR: u32 = 48_000;
        const BUF: u32 = 512;

        fn ns(seconds: f64) -> u128 {
            (seconds * 1e9) as u128
        }

        fn buffer_s(frames: u32) -> f64 {
            f64::from(frames) / f64::from(SR)
        }

        /// Runs `seconds` of steady callbacks starting at `t`, with a clock
        /// ratio `speed` (device/system) and a deterministic +-jitter.
        fn steady(m: &mut DriftMonitor, t: &mut f64, seconds: f64, speed: f64, jitter_s: f64) -> u64 {
            let mut reports = 0;
            let end = *t + seconds;
            let mut i = 0u32;
            while *t < end {
                let j = if i.is_multiple_of(2) { jitter_s } else { -jitter_s };
                if m.observe(ns((*t + j).max(0.0)), BUF).is_some() {
                    reports += 1;
                }
                *t += buffer_s(BUF) / speed;
                i += 1;
            }
            reports
        }

        #[test]
        fn steady_stream_with_jitter_is_clean_for_30_minutes() {
            let mut m = DriftMonitor::new(SR);
            let mut t = 0.0;
            assert_eq!(steady(&mut m, &mut t, 1800.0, 1.0, 0.004), 0);
        }

        #[test]
        fn crystal_drift_of_200_ppm_is_absorbed() {
            // Device consumes 200 ppm faster than the system clock: the lead
            // falls by ~17 k frames over 30 min, slower than the leak.
            let mut m = DriftMonitor::new(SR);
            let mut t = 0.0;
            assert_eq!(steady(&mut m, &mut t, 1800.0, 1.0 + 200e-6, 0.001), 0);
        }

        #[test]
        fn startup_prefill_burst_is_clean() {
            let mut m = DriftMonitor::new(SR);
            for _ in 0..16 {
                assert_eq!(m.observe(0, BUF), None);
            }
            let mut t = buffer_s(BUF);
            assert_eq!(steady(&mut m, &mut t, 10.0, 1.0, 0.0), 0);
        }

        #[test]
        fn stall_is_reported_once_with_its_size() {
            let mut m = DriftMonitor::new(SR);
            let mut t = 0.0;
            steady(&mut m, &mut t, 5.0, 1.0, 0.0);
            t += 0.2; // callbacks stop for 200 ms
            let deficit = m.observe(ns(t), BUF).expect("stall not reported");
            let expected = 0.2 * f64::from(SR);
            assert!((deficit as f64 - expected).abs() < f64::from(BUF), "deficit {deficit}");
            t += buffer_s(BUF);
            assert_eq!(steady(&mut m, &mut t, 5.0, 1.0, 0.0), 0, "stall reported more than once");
        }

        #[test]
        fn progressive_slowdown_is_reported() {
            // The WSLg pattern: callbacks keep coming, each a bit later than the last.
            let mut m = DriftMonitor::new(SR);
            let mut t = 0.0;
            steady(&mut m, &mut t, 5.0, 1.0, 0.0);
            let mut reports = 0;
            let mut interval = buffer_s(BUF);
            for _ in 0..200 {
                interval *= 1.02;
                t += interval;
                reports += u32::from(m.observe(ns(t), BUF).is_some());
            }
            assert!(reports >= 3, "only {reports} starvation reports");
        }
    }
}
