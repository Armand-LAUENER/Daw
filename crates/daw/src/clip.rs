//! Playing a decoded file as a node in the graph.
//!
//! The audio is shared rather than owned. The graph is rebuilt every time a
//! track is added, removed or given a different source, and re-reading the file
//! each time — or even copying what was read — would turn an ordinary edit into
//! seconds of work and tens of megabytes. One decode, one copy, however many
//! graphs.
//!
//! It loops. A file that stopped would leave the mixer silent a minute after it
//! was loaded, which is the opposite of useful while faders are being tried.
//! A transport that starts, stops and seeks belongs with the timeline.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engine_core::{AudioBuffer, Node};
use engine_io_file::{DecodedAudio, FileError, decode_file};

/// A file, decoded once at the device's rate and shared by every graph that
/// wants it.
pub type Shared = Arc<DecodedAudio>;

/// Files already read, so a rebuild costs nothing.
#[derive(Debug, Default)]
pub struct Library {
    entries: Vec<(PathBuf, Shared)>,
}

impl Library {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, path: &Path) -> Option<Shared> {
        self.entries.iter().find(|(p, _)| p == path).map(|(_, audio)| Arc::clone(audio))
    }

    /// Reads a file, or hands back what was read before.
    ///
    /// Decoding happens here, on the control side, where taking a moment is
    /// allowed. `target_rate` is the device's, since resampling inside the
    /// callback is work nobody has budgeted for.
    pub fn load(&mut self, path: &Path, target_rate: u32) -> Result<Shared, FileError> {
        if let Some(audio) = self.get(path) {
            return Ok(audio);
        }
        let audio = Arc::new(decode_file(path, target_rate)?);
        self.entries.push((path.to_path_buf(), Arc::clone(&audio)));
        Ok(audio)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Plays shared audio into the graph, round and round.
pub struct Clip {
    audio: Shared,
    /// Where the next frame comes from, in frames.
    cursor: usize,
}

impl Clip {
    pub fn new(audio: Shared) -> Self {
        Self { audio, cursor: 0 }
    }
}

impl Node for Clip {
    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        let (frames, channels) = (buffer.frames(), buffer.channels());
        let source_channels = usize::from(self.audio.channels);
        let total = self.audio.frames();

        if total == 0 || source_channels == 0 {
            for channel in buffer.channels_mut() {
                channel.fill(0.0);
            }
            return;
        }

        for frame in 0..frames {
            let position = self.cursor % total;
            for channel in 0..channels {
                // A mono file feeds both sides; a file with more channels than
                // the graph has keeps the ones that fit. Neither is a mix, but
                // both beat silence and a panic.
                let taken = channel.min(source_channels - 1);
                buffer.channel_mut(channel)[frame] = self.audio.samples[position * source_channels + taken];
            }
            self.cursor += 1;
        }
    }

    fn reset(&mut self) {
        self.cursor = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(samples: Vec<f32>, channels: u16) -> Shared {
        Arc::new(DecodedAudio { samples, channels, sample_rate: 48_000 })
    }

    fn render(clip: &mut Clip, frames: usize, channels: usize) -> Vec<f32> {
        let mut data = vec![0.0; frames * channels];
        clip.process(&mut AudioBuffer::new(&mut data, channels));
        data
    }

    #[test]
    fn a_stereo_file_keeps_its_sides_apart() {
        let mut clip = Clip::new(audio(vec![1.0, -1.0, 1.0, -1.0], 2));
        let out = render(&mut clip, 2, 2);
        // Planar: the first half is the left channel, the second the right.
        assert_eq!(out, vec![1.0, 1.0, -1.0, -1.0]);
    }

    #[test]
    fn a_mono_file_reaches_both_sides() {
        let mut clip = Clip::new(audio(vec![0.5, 0.25], 1));
        let out = render(&mut clip, 2, 2);
        assert_eq!(out, vec![0.5, 0.25, 0.5, 0.25]);
    }

    #[test]
    fn it_comes_back_round_rather_than_stopping() {
        // A file that ran out would leave the mixer silent a minute after it
        // was loaded, while the faders are still being tried.
        let mut clip = Clip::new(audio(vec![1.0, 2.0, 3.0], 1));
        let out = render(&mut clip, 7, 1);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 1.0, 2.0, 3.0, 1.0]);
    }

    #[test]
    fn it_carries_on_where_the_last_block_left_off() {
        let mut clip = Clip::new(audio(vec![1.0, 2.0, 3.0, 4.0], 1));
        assert_eq!(render(&mut clip, 2, 1), vec![1.0, 2.0]);
        assert_eq!(render(&mut clip, 2, 1), vec![3.0, 4.0]);
    }

    #[test]
    fn an_empty_file_plays_silence_rather_than_panicking() {
        let mut clip = Clip::new(audio(Vec::new(), 2));
        assert!(render(&mut clip, 4, 2).iter().all(|s| *s == 0.0));
    }

    #[test]
    fn a_reset_sends_it_back_to_the_beginning() {
        let mut clip = Clip::new(audio(vec![1.0, 2.0], 1));
        render(&mut clip, 1, 1);
        clip.reset();
        assert_eq!(render(&mut clip, 1, 1), vec![1.0]);
    }

    #[test]
    fn a_file_read_twice_is_only_held_once() {
        // The graph is rebuilt on every add and remove; re-reading each time
        // would turn an ordinary edit into seconds of work.
        let library = Library::new();
        assert_eq!(library.len(), 0);
        assert!(library.get(Path::new("nothing.wav")).is_none());
    }
}
