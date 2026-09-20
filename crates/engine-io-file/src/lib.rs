//! Audio file decoding and sample-rate conversion.
//!
//! Unlike `engine-dsp`, nothing here is real-time safe: decoding allocates,
//! reads from disk and takes unbounded time. It belongs on the preload thread,
//! which hands finished buffers to the audio thread through a ring buffer —
//! never the other way round.
//!
//! Scaffolding at this point: the crate, its dependencies and the format
//! selection are in place, the decoder is not. `docs/REPRISE.md` holds the
//! state of increment 3 and the API notes gathered for `symphonia` 0.6 and
//! `rubato` 5.0.
