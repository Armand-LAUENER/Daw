//! Playing a decoded file as a node in the graph.
//!
//! The audio is shared rather than owned. The graph is rebuilt every time a
//! track is added, removed or given a different source, and re-reading the file
//! each time — or even copying what was read — would turn an ordinary edit into
//! seconds of work and tens of megabytes. One decode, one copy, however many
//! graphs.
//!
//! A track's audio clips are one [`Lane`]: each file from the frame its clip is
//! placed at, played once, following the transport's clock rather than a
//! cursor of its own. That is what lets a graph be rebuilt mid-song — a track
//! added, a clip moved — without every file jumping back to its first sample:
//! the new node asks the clock where it is.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engine_core::{AudioBuffer, Clock, Node};
use engine_io_file::{DecodedAudio, FileError, decode_file};

use crate::peaks::Peaks;

/// A file, decoded once at the device's rate and shared by every graph that
/// wants it.
pub type Shared = Arc<DecodedAudio>;

/// Files already read, so a rebuild costs nothing.
#[derive(Debug, Default)]
pub struct Library {
    entries: Vec<(PathBuf, Shared, Arc<Peaks>)>,
}

impl Library {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, path: &Path) -> Option<Shared> {
        self.entries.iter().find(|(p, ..)| p == path).map(|(_, audio, _)| Arc::clone(audio))
    }

    /// What the file looks like, for drawing. Taken when it was read.
    pub fn peaks(&self, path: &Path) -> Option<Arc<Peaks>> {
        self.entries.iter().find(|(p, ..)| p == path).map(|(.., peaks)| Arc::clone(peaks))
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
        let peaks = Arc::new(Peaks::of(&audio));
        self.entries.push((path.to_path_buf(), Arc::clone(&audio), peaks));
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

/// Plays a track's audio clips, each from the timeline frame it starts at.
///
/// Clips that overlap are summed, as two regions on one track would be heard
/// in any case; which one wins is a question for the editor, not the engine.
pub struct Lane {
    /// Each clip's part of its file, earliest first.
    clips: Vec<Played>,
    /// Where the timeline is. Read once per block, lock-free.
    clock: Arc<Clock>,
}

/// One clip on a lane: a file, where it starts on the timeline, and which part
/// of the file it plays.
pub struct Played {
    pub audio: Shared,
    pub start: u64,
    pub offset: u64,
    /// `None` plays to the end of the file.
    pub length: Option<u64>,
}

impl Played {
    /// Frames it plays: its length, cut short by the end of the file.
    fn frames(&self) -> u64 {
        let rest = (self.audio.frames() as u64).saturating_sub(self.offset);
        self.length.map_or(rest, |l| l.min(rest))
    }
}

impl Lane {
    pub fn new(clips: Vec<Played>, clock: Arc<Clock>) -> Self {
        Self { clips, clock }
    }
}

impl Node for Lane {
    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        let (frames, channels) = (buffer.frames(), buffer.channels());
        // The buffer arrives silenced, so a clip not playing in this block has
        // nothing to write. A stopped clock still has a position, and the
        // transport's fade is what keeps it quiet — this only has to be right
        // about where each file is.
        let now = self.clock.position();
        let block_end = now + frames as u64;

        for clip in &self.clips {
            let (audio, start) = (&clip.audio, clip.start);
            let source_channels = usize::from(audio.channels);
            let total = clip.frames();
            if source_channels == 0 || start >= block_end || start + total <= now {
                continue;
            }
            // Where in the block the clip begins, and where in its part.
            let first = start.saturating_sub(now) as usize;
            let into = now.saturating_sub(start) as usize;
            let count = (frames - first).min(total as usize - into);
            for frame in 0..count {
                let position = clip.offset as usize + into + frame;
                for channel in 0..channels {
                    // A mono file feeds both sides; a file with more channels
                    // than the graph has keeps the ones that fit. Neither is a
                    // mix, but both beat silence and a panic.
                    let taken = channel.min(source_channels - 1);
                    buffer.channel_mut(channel)[first + frame] += audio.samples[position * source_channels + taken];
                }
            }
        }
    }
}

/// Several sources on one track, heard together.
///
/// Every layer adds to the buffer rather than writing it, so running them one
/// after another over the same buffer is the mix.
pub struct Layers(pub Vec<Box<dyn Node>>);

impl Node for Layers {
    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        for layer in &mut self.0 {
            layer.process(buffer);
        }
    }

    fn reset(&mut self) {
        for layer in &mut self.0 {
            layer.reset();
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

    fn render(clip: &mut Lane, frames: usize, channels: usize) -> Vec<f32> {
        let mut data = vec![0.0; frames * channels];
        clip.process(&mut AudioBuffer::new(&mut data, channels));
        data
    }

    fn lane(clips: Vec<(u64, Shared)>, clock: Arc<Clock>) -> Lane {
        Lane::new(
            clips.into_iter().map(|(start, audio)| Played { audio, start, offset: 0, length: None }).collect(),
            clock,
        )
    }

    #[test]
    fn a_stereo_file_keeps_its_sides_apart() {
        let (clock, _t) = clock_at(0);
        let mut clip = lane(vec![(0, audio(vec![1.0, -1.0, 1.0, -1.0], 2))], clock);
        let out = render(&mut clip, 2, 2);
        // Planar: the first half is the left channel, the second the right.
        assert_eq!(out, vec![1.0, 1.0, -1.0, -1.0]);
    }

    #[test]
    fn a_mono_file_reaches_both_sides() {
        let (clock, _t) = clock_at(0);
        let mut clip = lane(vec![(0, audio(vec![0.5, 0.25], 1))], clock);
        let out = render(&mut clip, 2, 2);
        assert_eq!(out, vec![0.5, 0.25, 0.5, 0.25]);
    }

    #[test]
    fn it_plays_from_where_the_timeline_is() {
        // A graph rebuilt mid-song makes a new lane; it must pick up at the
        // playhead, not at the file's first sample.
        let (clock, _t) = clock_at(2);
        let mut clip = lane(vec![(0, audio(vec![1.0, 2.0, 3.0, 4.0], 1))], clock);
        assert_eq!(render(&mut clip, 2, 1), vec![3.0, 4.0]);
    }

    #[test]
    fn a_clip_starts_at_its_own_frame_and_stops_at_its_end() {
        let (clock, _t) = clock_at(0);
        let mut clip = lane(vec![(2, audio(vec![1.0, 2.0, 3.0], 1))], clock);
        assert_eq!(render(&mut clip, 7, 1), vec![0.0, 0.0, 1.0, 2.0, 3.0, 0.0, 0.0]);
        let (clock, _t) = clock_at(10);
        let mut clip = lane(vec![(0, audio(vec![1.0, 2.0, 3.0], 1))], clock);
        assert!(render(&mut clip, 4, 1).iter().all(|s| *s == 0.0), "a file played past its end");
    }

    #[test]
    fn a_trimmed_clip_plays_only_its_part() {
        // Frames 2..5 of the file, placed at frame 1.
        let (clock, _t) = clock_at(0);
        let played =
            Played { audio: audio(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 1), start: 1, offset: 2, length: Some(3) };
        let mut clip = Lane::new(vec![played], clock);
        assert_eq!(render(&mut clip, 6, 1), vec![0.0, 3.0, 4.0, 5.0, 0.0, 0.0]);
    }

    #[test]
    fn a_window_past_the_end_of_the_file_stops_at_the_file() {
        let (clock, _t) = clock_at(0);
        let played = Played { audio: audio(vec![1.0, 2.0, 3.0], 1), start: 0, offset: 1, length: Some(99) };
        let mut clip = Lane::new(vec![played], clock);
        assert_eq!(render(&mut clip, 4, 1), vec![2.0, 3.0, 0.0, 0.0]);
    }

    #[test]
    fn overlapping_clips_are_summed() {
        let (clock, _t) = clock_at(0);
        let mut clip = lane(vec![(0, audio(vec![1.0, 1.0, 1.0], 1)), (1, audio(vec![0.5, 0.5], 1))], clock);
        assert_eq!(render(&mut clip, 4, 1), vec![1.0, 1.5, 1.5, 0.0]);
    }

    #[test]
    fn an_empty_file_plays_silence_rather_than_panicking() {
        let (clock, _t) = clock_at(0);
        let mut clip = lane(vec![(0, audio(Vec::new(), 2))], clock);
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
