//! Laying a file's channels onto a device's.
//!
//! The two rarely match. A headset that presents itself as 7.1 wants eight
//! channels for a stereo file, and the six it does not get must be silent
//! rather than left holding whatever was in the buffer.

/// Copies interleaved frames from `src` to `dst`, matching channels by index.
///
/// Channels present on the device but not in the source are filled with
/// silence; channels present in the source but not on the device are dropped.
/// Returns the number of frames written, which is the shorter of the two.
///
/// Index order is the convention every common layout starts with — front left,
/// front right — so a stereo file lands on the front pair of a surround device
/// and nothing is duplicated into the rears.
#[inline]
pub fn map_frames(src: &[f32], src_channels: usize, dst: &mut [f32], dst_channels: usize) -> usize {
    if src_channels == 0 || dst_channels == 0 {
        dst.fill(0.0);
        return 0;
    }
    let frames = (src.len() / src_channels).min(dst.len() / dst_channels);
    let shared = src_channels.min(dst_channels);

    for (out, input) in dst.chunks_exact_mut(dst_channels).zip(src.chunks_exact(src_channels)).take(frames) {
        out[..shared].copy_from_slice(&input[..shared]);
        out[shared..].fill(0.0);
    }
    // Whatever the device asked for beyond what the source could fill.
    dst[frames * dst_channels..].fill(0.0);
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_lands_on_the_front_pair_of_a_surround_device() {
        let src = [1.0, 2.0, 3.0, 4.0];
        let mut dst = [9.0; 16]; // two frames of eight channels, pre-dirtied
        assert_eq!(map_frames(&src, 2, &mut dst, 8), 2);
        assert_eq!(&dst[..8], &[1.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(&dst[8..], &[3.0, 4.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn extra_source_channels_are_dropped() {
        let src = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]; // two frames of three channels
        let mut dst = [0.0; 4];
        assert_eq!(map_frames(&src, 3, &mut dst, 2), 2);
        assert_eq!(dst, [1.0, 2.0, 4.0, 5.0]);
    }

    #[test]
    fn a_short_source_leaves_silence_rather_than_stale_audio() {
        let src = [1.0, 2.0]; // one frame
        let mut dst = [7.0; 8]; // room for four
        assert_eq!(map_frames(&src, 2, &mut dst, 2), 1);
        assert_eq!(dst, [1.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn zero_channels_are_refused_without_panicking() {
        let mut dst = [5.0; 4];
        assert_eq!(map_frames(&[1.0], 0, &mut dst, 2), 0);
        assert_eq!(dst, [0.0; 4]);
    }
}
