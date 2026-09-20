/// A block of audio a node works on, laid out one channel after another.
///
/// Planar, not interleaved. A graph moves whole channels around — a filter runs
/// down one, a pan scales two — and interleaving would make every access
/// strided. The device wants interleaved, so the conversion happens once at
/// each end of the graph rather than everywhere inside it.
///
/// Borrows its storage: nodes never allocate, and the buffer a node is handed
/// is the one the caller already owns.
#[derive(Debug)]
pub struct AudioBuffer<'a> {
    data: &'a mut [f32],
    channels: usize,
    frames: usize,
}

impl<'a> AudioBuffer<'a> {
    /// Wraps `data` as `channels` channels of equal length.
    ///
    /// # Panics
    ///
    /// If `data` does not divide evenly. That is a wiring mistake on the
    /// control side, not something the audio thread should paper over.
    pub fn new(data: &'a mut [f32], channels: usize) -> Self {
        assert!(channels > 0, "a buffer needs at least one channel");
        assert_eq!(data.len() % channels, 0, "buffer length {} is not {channels} whole channels", data.len());
        let frames = data.len() / channels;
        Self { data, channels, frames }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn channel(&self, index: usize) -> &[f32] {
        &self.data[index * self.frames..(index + 1) * self.frames]
    }

    pub fn channel_mut(&mut self, index: usize) -> &mut [f32] {
        let frames = self.frames;
        &mut self.data[index * frames..(index + 1) * frames]
    }

    /// Every channel in turn, for nodes that treat them independently.
    pub fn channels_mut(&mut self) -> impl Iterator<Item = &mut [f32]> {
        self.data.chunks_exact_mut(self.frames)
    }

    /// The first two channels together, for nodes that pair them — pan, width,
    /// anything that reads one while writing the other.
    ///
    /// Returns `None` on a buffer with fewer than two channels.
    pub fn stereo_mut(&mut self) -> Option<(&mut [f32], &mut [f32])> {
        if self.channels < 2 {
            return None;
        }
        let (left, rest) = self.data.split_at_mut(self.frames);
        Some((left, &mut rest[..self.frames]))
    }

    pub fn silence(&mut self) {
        self.data.fill(0.0);
    }

    /// Adds `other` into this buffer, channel by channel — the mix itself.
    ///
    /// Stops at the shorter of the two in both channels and frames, so a mono
    /// source summed into a stereo bus lands on the left rather than panicking:
    /// in a graph a shape mismatch should cost a slightly wrong mix, not a
    /// dropped track.
    pub fn add(&mut self, other: &AudioBuffer<'_>) {
        let channels = self.channels.min(other.channels);
        let frames = self.frames.min(other.frames);
        for channel in 0..channels {
            let source = &other.channel(channel)[..frames];
            for (d, s) in self.channel_mut(channel)[..frames].iter_mut().zip(source) {
                *d += s;
            }
        }
    }

    /// Copies interleaved samples in, as they arrive from a file or a device.
    pub fn fill_from_interleaved(&mut self, src: &[f32]) {
        let channels = self.channels;
        let frames = self.frames.min(src.len() / channels);
        for channel in 0..channels {
            for (frame, d) in self.channel_mut(channel)[..frames].iter_mut().enumerate() {
                *d = src[frame * channels + channel];
            }
        }
    }

    /// Writes interleaved samples out, the shape the device expects.
    pub fn write_interleaved(&self, dst: &mut [f32]) {
        let frames = self.frames.min(dst.len() / self.channels);
        for channel in 0..self.channels {
            let src = self.channel(channel);
            for frame in 0..frames {
                dst[frame * self.channels + channel] = src[frame];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_are_laid_out_one_after_another() {
        let mut data = [1.0, 2.0, 3.0, 10.0, 20.0, 30.0];
        let buffer = AudioBuffer::new(&mut data, 2);
        assert_eq!(buffer.frames(), 3);
        assert_eq!(buffer.channel(0), [1.0, 2.0, 3.0]);
        assert_eq!(buffer.channel(1), [10.0, 20.0, 30.0]);
    }

    #[test]
    fn interleaving_round_trips() {
        let interleaved = [1.0, 10.0, 2.0, 20.0, 3.0, 30.0];
        let mut data = [0.0; 6];
        let mut buffer = AudioBuffer::new(&mut data, 2);
        buffer.fill_from_interleaved(&interleaved);
        assert_eq!(buffer.channel(0), [1.0, 2.0, 3.0]);

        let mut out = [0.0; 6];
        buffer.write_interleaved(&mut out);
        assert_eq!(out, interleaved);
    }

    #[test]
    fn stereo_hands_out_both_channels_at_once() {
        let mut data = [1.0, 2.0, 10.0, 20.0];
        let mut buffer = AudioBuffer::new(&mut data, 2);
        let (left, right) = buffer.stereo_mut().unwrap();
        assert_eq!(left, [1.0, 2.0]);
        assert_eq!(right, [10.0, 20.0]);
        // Writing through one must not disturb the other.
        left[0] = 5.0;
        assert_eq!(right, [10.0, 20.0]);
    }

    #[test]
    fn a_mono_buffer_has_no_stereo_pair() {
        let mut data = [1.0, 2.0];
        assert!(AudioBuffer::new(&mut data, 1).stereo_mut().is_none());
    }

    #[test]
    fn adding_sums_channel_by_channel() {
        let mut bus_data = [1.0, 1.0, 2.0, 2.0];
        let mut track_data = [10.0, 10.0, 20.0, 20.0];
        let mut bus = AudioBuffer::new(&mut bus_data, 2);
        bus.add(&AudioBuffer::new(&mut track_data, 2));
        assert_eq!(bus.channel(0), [11.0, 11.0]);
        assert_eq!(bus.channel(1), [22.0, 22.0]);
    }

    #[test]
    fn a_mono_source_sums_into_the_first_channel_of_a_stereo_bus() {
        let mut bus_data = [0.0; 4];
        let mut mono_data = [1.0, 2.0];
        let mut bus = AudioBuffer::new(&mut bus_data, 2);
        bus.add(&AudioBuffer::new(&mut mono_data, 1));
        assert_eq!(bus.channel(0), [1.0, 2.0]);
        assert_eq!(bus.channel(1), [0.0, 0.0]);
    }

    #[test]
    #[should_panic(expected = "whole channels")]
    fn a_ragged_buffer_is_refused() {
        let mut data = [0.0; 5];
        AudioBuffer::new(&mut data, 2);
    }
}
