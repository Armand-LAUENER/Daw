//! Audio device I/O and the real-time callback.
//!
//! Threading model (phase 0):
//! - the **audio thread** (cpal callback) runs [`StreamProcessor`]: no allocation,
//!   no lock, no I/O, no blocking;
//! - the **control side** sends [`Command`]s through an SPSC ring buffer;
//! - the audio thread reports [`RtEvent`]s through another SPSC ring buffer and
//!   counters in [`Stats`], both drained by a non-real-time thread.

mod channels;
mod control;
mod diagnostics;
mod processor;
mod stream;

pub use channels::map_frames;
pub use control::Command;
pub use diagnostics::{
    CallbackTiming, DRIFT_LEAK, DriftMonitor, LOAD_BUCKETS, OVERLOAD_THRESHOLD_PERMILLE, RtEvent, STREAM_ERROR_KINDS,
    Stats, StatsSnapshot, TimingAnomaly, XrunDetector, XrunSource, load_bucket_edge,
};
pub use processor::{RenderFn, StreamProcessor};
pub use stream::{
    AudioEngine, DeviceSummary, EngineError, EngineOptions, StreamInfo, default_output_config, list_output_devices,
};
