use std::fmt;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, ErrorKind, FromSample, OutputCallbackInfo, SampleFormat, SizedSample, StreamConfig};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::control::Command;
use crate::diagnostics::{CallbackTiming, RtEvent, Stats};
use crate::processor::SineProcessor;

const COMMAND_QUEUE_CAPACITY: usize = 256;
const EVENT_QUEUE_CAPACITY: usize = 1024;

#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// Audio host name (e.g. "alsa", "pulseaudio"); `None` selects cpal's default host.
    pub host: Option<String>,
    /// Substring of the device id or name; `None` selects the default output device.
    pub device: Option<String>,
    /// Requested buffer size in frames; `None` lets the backend choose.
    pub buffer_frames: Option<u32>,
    pub initial_freq: f32,
    pub gain: f32,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self { host: None, device: None, buffer_frames: None, initial_freq: 440.0, gain: 0.2 }
    }
}

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub host: String,
    pub device: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: String,
    pub buffer_size: String,
}

#[derive(Debug, Clone)]
pub struct DeviceSummary {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub default_config: Option<String>,
}

#[derive(Debug)]
pub enum EngineError {
    NoHost(String),
    NoDevice(Option<String>),
    UnsupportedFormat(SampleFormat),
    Backend(cpal::Error),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHost(name) => {
                let available: Vec<&str> = cpal::available_hosts().iter().map(|h| h.name()).collect();
                write!(f, "no audio host named {name:?} (available: {})", available.join(", "))
            }
            Self::NoDevice(None) => write!(f, "no default output device"),
            Self::NoDevice(Some(q)) => write!(f, "no output device matching {q:?}"),
            Self::UnsupportedFormat(fmt_) => write!(f, "unsupported sample format {fmt_}"),
            Self::Backend(e) => write!(f, "audio backend error: {e}"),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<cpal::Error> for EngineError {
    fn from(e: cpal::Error) -> Self {
        Self::Backend(e)
    }
}

/// A running output stream plus the non-real-time ends of its channels.
///
/// Dropping it stops the stream. `cpal::Stream` is not `Send` on every
/// platform, so the engine stays on the thread that created it.
pub struct AudioEngine {
    pub events: Consumer<RtEvent>,
    pub stats: Arc<Stats>,
    pub info: StreamInfo,
    _stream: cpal::Stream,
}

impl AudioEngine {
    /// Opens the output device and starts playing a sine wave.
    ///
    /// Returns the command producer separately: it belongs to the control
    /// side and can be moved to any thread.
    pub fn start_sine(opts: &EngineOptions) -> Result<(Self, Producer<Command>), EngineError> {
        let host = select_host(opts.host.as_deref())?;
        let device = find_output_device(&host, opts.device.as_deref())?;
        let supported = device.default_output_config()?;
        let sample_format = supported.sample_format();
        let mut config: StreamConfig = supported.config();
        if let Some(frames) = opts.buffer_frames {
            config.buffer_size = BufferSize::Fixed(frames);
        }

        let (commands, cmd_rx) = RingBuffer::new(COMMAND_QUEUE_CAPACITY);
        let (ev_tx, events) = RingBuffer::new(EVENT_QUEUE_CAPACITY);
        let stats = Arc::new(Stats::new());
        let processor =
            SineProcessor::new(config.sample_rate, opts.initial_freq, opts.gain, cmd_rx, ev_tx, Arc::clone(&stats));

        let info = StreamInfo {
            host: host.id().name().to_string(),
            device: device_label(&device),
            sample_rate: config.sample_rate,
            channels: config.channels,
            sample_format: sample_format.to_string(),
            buffer_size: match config.buffer_size {
                BufferSize::Default => format!("backend default {:?}", supported.buffer_size()),
                BufferSize::Fixed(n) => format!("{n} frames (requested)"),
            },
        };

        let stream = match sample_format {
            SampleFormat::F32 => build::<f32>(&device, &config, processor, &stats)?,
            SampleFormat::F64 => build::<f64>(&device, &config, processor, &stats)?,
            SampleFormat::I16 => build::<i16>(&device, &config, processor, &stats)?,
            SampleFormat::I32 => build::<i32>(&device, &config, processor, &stats)?,
            SampleFormat::U16 => build::<u16>(&device, &config, processor, &stats)?,
            other => return Err(EngineError::UnsupportedFormat(other)),
        };
        stream.play()?;

        Ok((Self { events, stats, info, _stream: stream }, commands))
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mut processor: SineProcessor,
    stats: &Arc<Stats>,
) -> Result<cpal::Stream, EngineError>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels);
    let err_stats = Arc::clone(stats);

    let data_callback = move |out: &mut [T], info: &OutputCallbackInfo| {
        assert_no_alloc::assert_no_alloc(|| {
            let start = Instant::now();
            let ts = info.timestamp();
            let timing = CallbackTiming { callback_ns: ts.callback.as_nanos(), playback_ns: ts.playback.as_nanos() };
            processor.process(out, channels, Some(timing));
            let elapsed = start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            processor.record_load(elapsed, (out.len() / channels) as u32);
        });
    };

    // May run on the audio thread: only touch atomics, never format or print.
    let error_callback = move |err: cpal::Error| {
        let kind = err.kind();
        let total = if kind == ErrorKind::Xrun { &err_stats.backend_xruns } else { &err_stats.stream_errors };
        total.fetch_add(1, Ordering::Relaxed);
        err_stats.error_kinds[error_kind_index(kind)].fetch_add(1, Ordering::Relaxed);
    };

    Ok(device.build_output_stream(*config, data_callback, error_callback, None)?)
}

/// Index into [`crate::STREAM_ERROR_KINDS`]; kinds added by future cpal versions map to "Other".
fn error_kind_index(kind: ErrorKind) -> usize {
    match kind {
        ErrorKind::DeviceBusy => 0,
        ErrorKind::DeviceChanged => 1,
        ErrorKind::DeviceNotAvailable => 2,
        ErrorKind::HostUnavailable => 3,
        ErrorKind::InvalidInput => 4,
        ErrorKind::PermissionDenied => 5,
        ErrorKind::RealtimeDenied => 6,
        ErrorKind::ResourceExhausted => 7,
        ErrorKind::StreamInvalidated => 8,
        ErrorKind::UnsupportedConfig => 9,
        ErrorKind::UnsupportedOperation => 10,
        ErrorKind::Xrun => 11,
        ErrorKind::BackendError => 12,
        _ => 13,
    }
}

fn select_host(name: Option<&str>) -> Result<cpal::Host, EngineError> {
    let Some(name) = name else {
        return Ok(cpal::default_host());
    };
    let id = cpal::available_hosts()
        .into_iter()
        .find(|id| id.name().eq_ignore_ascii_case(name))
        .ok_or_else(|| EngineError::NoHost(name.to_string()))?;
    Ok(cpal::host_from_id(id)?)
}

fn find_output_device(host: &cpal::Host, query: Option<&str>) -> Result<cpal::Device, EngineError> {
    let Some(query) = query else {
        return host.default_output_device().ok_or(EngineError::NoDevice(None));
    };
    let q = query.to_lowercase();
    host.output_devices()?
        .find(|d| device_label(d).to_lowercase().contains(&q))
        .ok_or_else(|| EngineError::NoDevice(Some(query.to_string())))
}

fn device_label(device: &cpal::Device) -> String {
    let id = device.id().map(|id| id.to_string()).unwrap_or_else(|_| "?".into());
    match device.description() {
        Ok(desc) => format!("{} [{id}]", desc.name()),
        Err(_) => id,
    }
}

/// Enumerates output devices of the given host (default host if `None`).
pub fn list_output_devices(host: Option<&str>) -> Result<Vec<DeviceSummary>, EngineError> {
    let host = select_host(host)?;
    let default_id = host.default_output_device().and_then(|d| d.id().ok());
    Ok(host
        .output_devices()?
        .map(|d| {
            let id = d.id().ok();
            DeviceSummary {
                is_default: id.is_some() && id == default_id,
                id: id.map(|i| i.to_string()).unwrap_or_else(|| "?".into()),
                name: d.description().map(|x| x.name().to_string()).unwrap_or_default(),
                default_config: d.default_output_config().ok().map(|c| format!("{c:?}")),
            }
        })
        .collect())
}
