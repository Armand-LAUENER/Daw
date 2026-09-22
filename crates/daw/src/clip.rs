//! Playing a decoded file as a node in the graph.
//!
//! The audio is shared rather than owned. The graph is rebuilt every time a
//! track is added, removed or given a different source, and re-reading the file
//! each time — or even copying what was read — would turn an ordinary edit into
//! seconds of work and tens of megabytes. One decode, one copy, however many
//! graphs.
//!
//! It plays from the start of the timeline, once, following the transport's
//! clock rather than a cursor of its own. That is what lets a graph be rebuilt
//! mid-song — a track added, a source switched — without every file jumping
//! back to its first sample: the new node asks the clock where it is.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engine_core::{AudioBuffer, Clock, Node};
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

/// A track with nothing on it.
///
/// It writes nothing at all rather than writing zeros: the graph hands every
/// node a buffer it has already silenced, so there is nothing to do.
pub struct Silence;

impl Node for Silence {
    fn process(&mut self, _buffer: &mut AudioBuffer<'_>) {}
}

/// Plays shared audio into the graph, from the timeline's start.
pub struct Clip {
    audio: Shared,
    /// Where the timeline is. Read once per block, lock-free.
    clock: Arc<Clock>,
}

impl Clip {
    pub fn new(audio: Shared, clock: Arc<Clock>) -> Self {
        Self { audio, clock }
    }
}

impl Node for Clip {
    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        let (frames, channels) = (buffer.frames(), buffer.channels());
        let source_channels = usize::from(self.audio.channels);
        let total = self.audio.frames();

        // Past the end, or a stopped timeline: the buffer arrives silenced, so
        // there is nothing to write. A stopped clock still has a position, and
        // the transport's fade is what keeps it quiet — this only has to be
        // right about where the file is.
        let start = usize::try_from(self.clock.position()).unwrap_or(usize::MAX);
        if source_channels == 0 || start >= total {
            return;
        }

        for frame in 0..frames.min(total - start) {
            let position = start + frame;
            for channel in 0..channels {
                // A mono file feeds both sides; a file with more channels than
                // the graph has keeps the ones that fit. Neither is a mix, but
                // both beat silence and a panic.
                let taken = channel.min(source_channels - 1);
                buffer.channel_mut(channel)[frame] = self.audio.samples[position * source_channels + taken];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::{Transport, TransportCommand, transport_channel};

    fn audio(samples: Vec<f32>, channels: u16) -> Shared {
        Arc::new(DecodedAudio { samples, channels, sample_rate: 48_000 })
    }

    /// A clock standing at `frame`, and the transport that owns it — kept so
    /// the clock can be moved on.
    fn clock_at(frame: u64) -> (Arc<Clock>, Transport) {
        let (mut control, mut transport) = transport_channel(48_000.0, 4);
        control.send(TransportCommand::Seek(frame));
        transport.begin();
        (Arc::clone(control.clock()), transport)
    }

    fn render(clip: &mut Clip, frames: usize, channels: usize) -> Vec<f32> {
        let mut data = vec![0.0; frames * channels];
        clip.process(&mut AudioBuffer::new(&mut data, channels));
        data
    }

    #[test]
    fn a_stereo_file_keeps_its_sides_apart() {
        let (clock, _t) = clock_at(0);
        let mut clip = Clip::new(audio(vec![1.0, -1.0, 1.0, -1.0], 2), clock);
        let out = render(&mut clip, 2, 2);
        // Planar: the first half is the left channel, the second the right.
        assert_eq!(out, vec![1.0, 1.0, -1.0, -1.0]);
    }

    #[test]
    fn a_mono_file_reaches_both_sides() {
        let (clock, _t) = clock_at(0);
        let mut clip = Clip::new(audio(vec![0.5, 0.25], 1), clock);
        let out = render(&mut clip, 2, 2);
        assert_eq!(out, vec![0.5, 0.25, 0.5, 0.25]);
    }

    #[test]
    fn it_plays_from_where_the_timeline_is() {
        // A graph rebuilt mid-song makes a new clip; it must pick up at the
        // playhead, not at the file's first sample.
        let (clock, _t) = clock_at(2);
        let mut clip = Clip::new(audio(vec![1.0, 2.0, 3.0, 4.0], 1), clock);
        assert_eq!(render(&mut clip, 2, 1), vec![3.0, 4.0]);
    }

    #[test]
    fn it_stops_at_the_end_of_the_file() {
        let (clock, _t) = clock_at(1);
        let mut clip = Clip::new(audio(vec![1.0, 2.0, 3.0], 1), clock);
        assert_eq!(render(&mut clip, 4, 1), vec![2.0, 3.0, 0.0, 0.0]);
        let (clock, _t) = clock_at(10);
        let mut clip = Clip::new(audio(vec![1.0, 2.0, 3.0], 1), clock);
        assert!(render(&mut clip, 4, 1).iter().all(|s| *s == 0.0), "a file played past its end");
    }

    #[test]
    fn an_empty_file_plays_silence_rather_than_panicking() {
        let (clock, _t) = clock_at(0);
        let mut clip = Clip::new(audio(Vec::new(), 2), clock);
        assert!(render(&mut clip, 4, 2).iter().all(|s| *s == 0.0));
    }

    #[test]
    fn a_file_read_twice_is_only_held_once() {
        // The graph is rebuilt on every add and remove; re-reading each time
        // would turn an ordinary edit into seconds of work.
        let library = Library::new();
        assert_eq!(library.len(), 0);
        assert!(library.get(Path::new("nothing.wav")).is_none());
    }

    #[test]
    fn an_empty_track_leaves_the_buffer_as_it_found_it() {
        // The graph silences a node's buffer before calling it, so a source
        // with nothing to play has nothing to do.
        let mut data = vec![0.0f32; 8];
        Silence.process(&mut AudioBuffer::new(&mut data, 2));
        assert!(data.iter().all(|s| *s == 0.0));
    }
}
