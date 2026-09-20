//! Audio file decoding and sample-rate conversion.
//!
//! Unlike `engine-dsp`, nothing here is real-time safe: decoding allocates,
//! reads from disk and takes unbounded time. It belongs on the preload thread,
//! which hands finished buffers to the audio thread through a ring buffer —
//! never the other way round.

mod decode;

pub use decode::{DecodedAudio, FileError, decode, decode_file};
