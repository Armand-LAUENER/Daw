//! Audio file decoding, sample-rate conversion and playback transport.
//!
//! Decoding allocates, reads from disk and takes unbounded time, so it belongs
//! on the preload thread. The transport is what connects that thread to the
//! audio one: [`start`] launches it, and the [`Playback`] half it hands back is
//! real-time safe — the only thing here the audio callback may touch.

mod decode;
mod transport;

pub use decode::{DecodedAudio, FileError, decode, decode_file};
pub use transport::{
    FADE_MS, MemorySource, Playback, Shared, Source, TransportCommand, TransportHandle, TransportOptions, start,
};
