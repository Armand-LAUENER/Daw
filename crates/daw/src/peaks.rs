//! What a file looks like, drawn at any zoom without reading the audio again.
//!
//! A waveform on screen is one vertical line per pixel, from the lowest sample
//! under that pixel to the highest. Scanning the samples for every pixel on
//! every frame would cost a pass over the whole file sixty times a second. So
//! the minimum and maximum of each block of [`BLOCK`] frames is taken once,
//! when the file is read, and a pixel then folds the handful of blocks it
//! covers.

use engine_io_file::DecodedAudio;

/// Frames per block. At 48 kHz, 5 ms: finer than any zoom the timeline offers
/// shows as a pixel.
pub const BLOCK: usize = 256;

/// The lowest and highest sample of each block, across every channel.
#[derive(Debug, Clone, Default)]
pub struct Peaks {
    blocks: Vec<(f32, f32)>,
    /// Frames in the file, which the last block may not fill.
    frames: u64,
}

impl Peaks {
    pub fn of(audio: &DecodedAudio) -> Self {
        let channels = usize::from(audio.channels.max(1));
        let blocks = audio
            .samples
            .chunks(BLOCK * channels)
            .map(|block| block.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &s| (lo.min(s), hi.max(s))))
            .collect();
        Self { blocks, frames: audio.frames() as u64 }
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// The lowest and highest sample between two frames, to the block. `None`
    /// outside the file.
    pub fn range(&self, from: u64, to: u64) -> Option<(f32, f32)> {
        let first = usize::try_from(from / BLOCK as u64).ok()?;
        let last = usize::try_from(to.saturating_sub(1) / BLOCK as u64).ok()?.min(self.blocks.len().checked_sub(1)?);
        if first > last {
            return None;
        }
        Some(
            self.blocks[first..=last]
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &(l, h)| (lo.min(l), hi.max(h))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(samples: Vec<f32>, channels: u16) -> DecodedAudio {
        DecodedAudio { samples, channels, sample_rate: 48_000 }
    }

    #[test]
    fn a_block_holds_its_lowest_and_highest_sample_across_channels() {
        // Stereo: the left channel peaks at 0.5, the right dips to -0.8.
        let mut samples = vec![0.0; BLOCK * 2];
        samples[10] = 0.5;
        samples[21] = -0.8;
        let peaks = Peaks::of(&audio(samples, 2));
        assert_eq!(peaks.range(0, BLOCK as u64), Some((-0.8, 0.5)));
    }

    #[test]
    fn a_range_folds_every_block_it_touches() {
        let mut samples = vec![0.0; BLOCK * 4];
        samples[BLOCK * 3 + 7] = 0.9;
        let peaks = Peaks::of(&audio(samples, 1));
        assert_eq!(peaks.range(0, BLOCK as u64 * 2).map(|r| r.1), Some(0.0));
        assert_eq!(peaks.range(BLOCK as u64, BLOCK as u64 * 4).map(|r| r.1), Some(0.9));
    }

    #[test]
    fn past_the_end_there_is_nothing_to_draw() {
        let peaks = Peaks::of(&audio(vec![0.1; 100], 1));
        assert_eq!(peaks.frames(), 100);
        assert!(peaks.range(BLOCK as u64 * 5, BLOCK as u64 * 6).is_none());
        assert!(Peaks::of(&audio(Vec::new(), 1)).range(0, 10).is_none());
    }
}
