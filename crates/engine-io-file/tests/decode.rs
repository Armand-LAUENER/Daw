//! Decoding and rate conversion, driven by WAV files built in memory.
//!
//! Building the input here rather than committing a binary fixture keeps the
//! expected content under the test's control: the frequency, the rate and the
//! per-channel content are all known exactly, so the assertions can be about
//! the audio rather than about byte counts.

use std::io::Cursor;

use engine_io_file::{DecodedAudio, FileError, decode};

const TARGET_RATE: u32 = 48_000;

/// A PCM WAV file holding one sine tone per channel.
fn wav(freqs: &[f32], rate: u32, frames: usize) -> Vec<u8> {
    let channels = freqs.len() as u16;
    let mut samples = Vec::with_capacity(frames * freqs.len());
    for i in 0..frames {
        for freq in freqs {
            let t = i as f32 / rate as f32;
            let value = (std::f32::consts::TAU * freq * t).sin() * 0.8;
            samples.push((value * f32::from(i16::MAX)) as i16);
        }
    }

    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let block_align = channels * 2;
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM header length
    out.extend_from_slice(&1u16.to_le_bytes()); // format: integer PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(block_align)).to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

fn decode_wav(bytes: Vec<u8>, target_rate: u32) -> Result<DecodedAudio, FileError> {
    decode(Box::new(Cursor::new(bytes)), Some("wav"), target_rate)
}

/// Frequency of one channel, from its rising zero crossings. Immune to the
/// amplitude and phase changes a resampler is allowed to introduce.
///
/// Counting crossings quantises to whole cycles, and a tone that starts at
/// phase zero has no negative sample before its first crossing, so the count
/// runs one low. Over a one-second window that is 1 Hz: tolerances below that
/// cannot be met, whatever the audio does.
fn measured_hz(audio: &DecodedAudio, channel: usize) -> f32 {
    let channels = usize::from(audio.channels);
    let chan: Vec<f32> = audio.samples.iter().skip(channel).step_by(channels).copied().collect();
    let crossings = chan.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    crossings as f32 / (chan.len() as f32 / audio.sample_rate as f32)
}

#[test]
fn decodes_a_wav_at_the_rate_it_was_asked_for() {
    let audio = decode_wav(wav(&[1_000.0], 48_000, 48_000), TARGET_RATE).unwrap();
    assert_eq!(audio.channels, 1);
    assert_eq!(audio.sample_rate, TARGET_RATE);
    assert_eq!(audio.frames(), 48_000);
    assert_eq!(audio.duration().as_millis(), 1_000);
}

#[test]
fn leaves_the_samples_alone_when_the_rate_already_matches() {
    // No resampler runs, so a 1 kHz tone comes back at exactly 1 kHz.
    let audio = decode_wav(wav(&[1_000.0], 48_000, 48_000), TARGET_RATE).unwrap();
    let hz = measured_hz(&audio, 0);
    assert!((hz - 1_000.0).abs() <= 1.0, "{hz} Hz, expected 1000");
    assert!(audio.samples.iter().all(|s| s.abs() <= 1.0), "samples outside [-1, 1]");
}

#[test]
fn resamples_44_1_to_48_khz_without_moving_the_pitch() {
    // The case that made rubato worth taking: most music is 44.1 kHz and the
    // device runs at 48. The tone must survive the conversion unchanged.
    let audio = decode_wav(wav(&[1_000.0], 44_100, 44_100), TARGET_RATE).unwrap();
    assert_eq!(audio.sample_rate, TARGET_RATE);

    let expected = 48_000.0;
    let drift = (audio.frames() as f64 - expected).abs() / expected;
    assert!(drift < 0.01, "{} frames, expected about {expected}", audio.frames());

    let hz = measured_hz(&audio, 0);
    assert!((hz - 1_000.0).abs() < 2.0, "{hz} Hz after resampling, expected 1000");
}

#[test]
fn keeps_the_channels_apart_through_a_resample() {
    // Interleaving is easy to get subtly wrong, and a swap or a smear only
    // shows up as a wrong frequency on one side.
    let audio = decode_wav(wav(&[500.0, 2_000.0], 44_100, 44_100), TARGET_RATE).unwrap();
    assert_eq!(audio.channels, 2);
    let (left, right) = (measured_hz(&audio, 0), measured_hz(&audio, 1));
    assert!((left - 500.0).abs() < 2.0, "left channel is {left} Hz, expected 500");
    assert!((right - 2_000.0).abs() < 4.0, "right channel is {right} Hz, expected 2000");
}

#[test]
fn a_resampled_clip_has_no_discontinuity() {
    // A clip stitched from chunks can carry a step at every block boundary.
    // A 200 Hz sine cannot move faster than 2*pi*200/rate per sample.
    let audio = decode_wav(wav(&[200.0], 44_100, 44_100), TARGET_RATE).unwrap();
    let max_step = std::f32::consts::TAU * 200.0 / TARGET_RATE as f32 * 1.1;
    // The ends hold the resampler's own edge transient.
    let body = &audio.samples[64..audio.samples.len() - 64];
    for (i, w) in body.windows(2).enumerate() {
        let step = (w[1] - w[0]).abs();
        assert!(step <= max_step, "step {step} at frame {i}, limit {max_step}");
    }
}

#[test]
fn rejects_bytes_that_are_not_audio() {
    let err = decode_wav(b"this is not a sound file, not even close".to_vec(), TARGET_RATE).unwrap_err();
    // It must come back as an error rather than a panic; the message is shown
    // to whoever dropped the file in.
    assert!(!err.to_string().is_empty());
}

#[test]
fn rejects_a_truncated_header() {
    let mut bytes = wav(&[1_000.0], 44_100, 1_000);
    bytes.truncate(20);
    assert!(decode_wav(bytes, TARGET_RATE).is_err());
}
