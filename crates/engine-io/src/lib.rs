//! Audio device I/O and the real-time callback.
//!
//! Threading model (phase 0):
//! - the **audio thread** (cpal callback) runs [`SineProcessor`]: no allocation,
//!   no lock, no I/O, no blocking;
//! - the **control side** sends [`Command`]s through an SPSC ring buffer;
//! - the audio thread reports [`RtEvent`]s through another SPSC ring buffer and
//!   counters in [`Stats`], both drained by a non-real-time thread.

mod control;
mod diagnostics;
mod processor;
mod stream;

pub use control::Command;
pub use diagnostics::{
    CallbackTiming, DriftMonitor, RtEvent, STREAM_ERROR_KINDS, Stats, StatsSnapshot, TimingAnomaly, XrunDetector,
    XrunSource,
};
pub use processor::SineProcessor;
pub use stream::{AudioEngine, DeviceSummary, EngineError, EngineOptions, StreamInfo, list_output_devices};
