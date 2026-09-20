use std::fmt;
use std::fs::File;
use std::path::Path;
use std::time::Duration;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

/// Frames per resampler chunk. Only a starting point: rubato rounds it up to a
/// block size valid for the rate pair, so 1024 becomes 1029 for 44.1k to 48k.
const RESAMPLER_CHUNK: usize = 1024;

/// A whole decoded clip, interleaved, at the rate that was asked for.
///
/// The entire file sits in memory. That is deliberate for now: streaming from
/// disk belongs to the preload thread, and doing it here would spread the
/// transport logic across two crates.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedAudio {
    /// Interleaved samples, `channels` per frame.
    pub samples: Vec<f32>,
    pub channels: u16,
    pub sample_rate: u32,
}

impl DecodedAudio {
    pub fn frames(&self) -> usize {
        if self.channels == 0 { 0 } else { self.samples.len() / usize::from(self.channels) }
    }

    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.frames() as f64 / f64::from(self.sample_rate))
    }
}

#[derive(Debug)]
pub enum FileError {
    Io(std::io::Error),
    /// The container was read but holds no audio track.
    NoAudioTrack,
    /// The track exists but does not say how it should be decoded.
    MissingCodecParameters,
    /// Nothing decoded said what rate it was at, so nothing can be resampled.
    UnknownSampleRate,
    /// Zero channels, or more than can be represented.
    UnsupportedChannelCount(usize),
    Decode(symphonia::core::errors::Error),
    Resample(String),
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot read the file: {e}"),
            Self::NoAudioTrack => write!(f, "the file holds no audio track"),
            Self::MissingCodecParameters => write!(f, "the audio track declares no codec parameters"),
            Self::UnknownSampleRate => write!(f, "the audio track declares no sample rate"),
            Self::UnsupportedChannelCount(n) => write!(f, "unsupported channel count: {n}"),
            Self::Decode(e) => write!(f, "cannot decode the audio: {e}"),
            Self::Resample(e) => write!(f, "cannot resample the audio: {e}"),
        }
    }
}

impl std::error::Error for FileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Decode(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for FileError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<symphonia::core::errors::Error> for FileError {
    fn from(e: symphonia::core::errors::Error) -> Self {
        Self::Decode(e)
    }
}

/// Decodes `path` and converts it to `target_rate`.
///
/// The extension is passed to the prober as a hint; it only speeds up format
/// detection, so a file whose name lies is still identified by its content.
pub fn decode_file(path: &Path, target_rate: u32) -> Result<DecodedAudio, FileError> {
    let file = File::open(path)?;
    let extension = path.extension().and_then(|e| e.to_str()).map(str::to_owned);
    decode(Box::new(file), extension.as_deref(), target_rate)
}

/// Same as [`decode_file`], for a source that is not a file on disk.
///
/// Takes an optional file extension as a format hint.
pub fn decode(
    source: Box<dyn MediaSource>,
    extension: Option<&str>,
    target_rate: u32,
) -> Result<DecodedAudio, FileError> {
    resample(decode_native(source, extension)?, target_rate)
}

/// Decodes without touching the sample rate.
fn decode_native(source: Box<dyn MediaSource>, extension: Option<&str>) -> Result<DecodedAudio, FileError> {
    let stream = MediaSourceStream::new(source, MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(extension) = extension {
        hint.with_extension(extension);
    }

    let mut format =
        symphonia::default::get_probe().probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())?;

    let track = format.default_track(TrackType::Audio).ok_or(FileError::NoAudioTrack)?;
    let track_id = track.id;
    let params = match track.codec_params.as_ref() {
        Some(CodecParameters::Audio(params)) => params.clone(),
        _ => return Err(FileError::MissingCodecParameters),
    };

    let mut decoder = symphonia::default::get_codecs().make_audio_decoder(&params, &AudioDecoderOptions::default())?;

    let mut samples = Vec::new();
    // copy_to_vec_interleaved resizes its destination rather than appending, so
    // packets land in a scratch buffer and are copied on from there.
    let mut scratch = Vec::new();
    // Taken from the decoded buffers rather than the track header, which does
    // not always carry them.
    let (mut channels, mut sample_rate) = (0usize, 0u32);

    while let Some(packet) = format.next_packet()? {
        if packet.track_id != track_id {
            continue;
        }
        let audio = decoder.decode(&packet)?;
        let frames = audio.frames();
        if frames == 0 {
            continue;
        }
        let spec = audio.spec();
        (channels, sample_rate) = (spec.channels().count(), spec.rate());
        audio.copy_to_vec_interleaved(&mut scratch);

        // Drop the encoder delay and padding the packet declares. Without this,
        // a lossy file starts with a sliver of silence and ends with a tail of
        // invented audio — audible on its own, and the reason albums meant to
        // play gapless click between tracks.
        let head = usize::try_from(packet.trim_start.get()).unwrap_or(frames).min(frames);
        let tail = usize::try_from(packet.trim_end.get()).unwrap_or(frames).min(frames - head);
        samples.extend_from_slice(&scratch[head * channels..(frames - tail) * channels]);
    }

    if sample_rate == 0 {
        return Err(FileError::UnknownSampleRate);
    }
    let channels = u16::try_from(channels).map_err(|_| FileError::UnsupportedChannelCount(channels))?;
    if channels == 0 {
        return Err(FileError::UnsupportedChannelCount(0));
    }

    Ok(DecodedAudio { samples, channels, sample_rate })
}

/// Converts `audio` to `target_rate`, returning it untouched when it already is.
fn resample(audio: DecodedAudio, target_rate: u32) -> Result<DecodedAudio, FileError> {
    if audio.sample_rate == target_rate || audio.samples.is_empty() {
        return Ok(DecodedAudio { sample_rate: target_rate, ..audio });
    }

    let channels = usize::from(audio.channels);
    let frames = audio.frames();
    let mut resampler =
        Fft::<f32>::new(audio.sample_rate as usize, target_rate as usize, RESAMPLER_CHUNK, channels, FixedSync::Both)
            .map_err(|e| FileError::Resample(e.to_string()))?;

    let input =
        InterleavedSlice::new(&audio.samples, channels, frames).map_err(|e| FileError::Resample(e.to_string()))?;
    // process_all runs the chunk loop, trims the resampler's startup delay and
    // returns exactly the resampled frames.
    let output = resampler.process_all(&input, frames, None).map_err(|e| FileError::Resample(e.to_string()))?;

    Ok(DecodedAudio { samples: output.take_data(), channels: audio.channels, sample_rate: target_rate })
}
