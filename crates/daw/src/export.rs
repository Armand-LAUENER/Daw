//! Rendering the project to a file.
//!
//! The same graph the live engine plays, compiled afresh and driven offline by
//! a transport of its own: as fast as the machine allows, and the same every
//! time, since nothing depends on when a callback happens to come. Three
//! things differ from playing it live:
//!
//! - the graph's latency (the master limiter looks ahead) is rendered past
//!   and dropped from the front, so the file starts on the first sample of the
//!   music rather than a few milliseconds before it;
//! - the render runs on past the last clip until the tails of the delays and
//!   reverbs have died away, rather than cutting them off;
//! - it runs on a thread of its own, so the window keeps drawing and can say
//!   how far along it is.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;

use engine_core::{Graph, Transport};
use project_engine::{Timing, audio_spans, note_plan};
use project_model::Project;

use crate::clip::Library;

/// Below this peak a block is silence, for deciding a tail has ended: -80 dB.
const QUIET: f32 = 1e-4;
/// A tail is over after this long below [`QUIET`].
const QUIET_SECONDS: f32 = 1.0;
/// However a tail behaves, the render stops this long after the music.
const LONGEST_TAIL_SECONDS: f32 = 30.0;

/// How the samples are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// For CDs and most players. Dithered, so quiet passages fade into noise
    /// rather than into the distortion of truncation.
    Pcm16,
    /// For further work in another program.
    Pcm24,
    /// Exactly what the engine made.
    Float32,
}

impl Format {
    pub const ALL: [Format; 3] = [Self::Pcm24, Self::Pcm16, Self::Float32];

    pub fn name(self) -> &'static str {
        match self {
            Self::Pcm16 => "16-bit",
            Self::Pcm24 => "24-bit",
            Self::Float32 => "32-bit float",
        }
    }

    fn bytes(self) -> u16 {
        match self {
            Self::Pcm16 => 2,
            Self::Pcm24 => 3,
            Self::Float32 => 4,
        }
    }
}

/// Where the music ends, in frames: the end of the last audio clip or the
/// last note. Zero for a project with nothing placed.
pub fn content_end(project: &Project, timing: &Timing, library: &Library) -> u64 {
    let mut end = 0;
    for track in project.tracks() {
        for span in audio_spans(project, track.id, timing) {
            let file = project
                .asset(span.asset)
                .and_then(|asset| library.get(&asset.path))
                .map_or(0, |audio| audio.frames() as u64);
            let played =
                span.length.map_or(file.saturating_sub(span.offset), |l| l.min(file.saturating_sub(span.offset)));
            end = end.max(span.start + played);
        }
        if let Some(last) = note_plan(project, track.id, timing).last() {
            end = end.max(last.frame);
        }
    }
    end
}

/// Renders `graph` from the start to `end`, then on until its tail is
/// silent, and returns the interleaved samples.
///
/// `progress` counts the frames rendered, for a progress bar; `end` is what
/// a bar can measure against, since a tail's length is not known in advance.
pub fn render(
    graph: &mut Graph,
    mut transport: Transport,
    end: u64,
    block: usize,
    sample_rate: u32,
    progress: &AtomicU64,
) -> Vec<f32> {
    let channels = graph.channels();
    let latency = graph.latency_frames();
    let quiet_after = (QUIET_SECONDS * sample_rate as f32) as u64;
    let longest = end + (LONGEST_TAIL_SECONDS * sample_rate as f32) as u64;

    let mut out = Vec::with_capacity((end as usize + sample_rate as usize) * channels);
    let mut buffer = vec![0.0f32; block * channels];
    let mut rendered = 0u64;
    let mut quiet_for = 0u64;
    loop {
        transport.begin();
        graph.process(&mut buffer);
        transport.finish(&mut buffer, channels);
        out.extend_from_slice(&buffer);
        rendered += block as u64;
        progress.store(rendered, Ordering::Relaxed);

        // Past the music, count how long it has been quiet; the latency is
        // still owed at the end, since the output runs that far behind.
        if rendered >= end + latency as u64 {
            let peak = buffer.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            quiet_for = if peak < QUIET { quiet_for + block as u64 } else { 0 };
            if quiet_for >= quiet_after || rendered >= longest {
                break;
            }
        }
    }

    // The front: the latency, which the graph spent filling its lookahead.
    out.drain(..(latency * channels).min(out.len()));
    // The back: the silence counted to be sure the tail was over, less a
    // short margin so a file does not end on a cut.
    let keep = (rendered as usize).saturating_sub(latency).saturating_sub(quiet_for as usize) + block;
    out.truncate(keep.min(out.len() / channels) * channels);
    out
}

/// Writes interleaved samples as a WAV file.
pub fn write_wav(path: &Path, samples: &[f32], channels: u16, sample_rate: u32, format: Format) -> io::Result<()> {
    let bytes = format.bytes();
    let data_len = samples.len() as u64 * u64::from(bytes);
    let data_len = u32::try_from(data_len).map_err(|_| io::Error::other("too long for a WAV file (4 GB)"))?;
    let float = format == Format::Float32;
    // Float WAVs carry the extended format chunk and a fact chunk, as the
    // format asks; some readers refuse one without.
    let fmt_len: u32 = if float { 18 } else { 16 };
    let fact_len: u32 = if float { 12 } else { 0 };
    let riff_len = 4 + (8 + fmt_len) + fact_len + (8 + data_len);

    let mut file = io::BufWriter::new(std::fs::File::create(path)?);
    file.write_all(b"RIFF")?;
    file.write_all(&riff_len.to_le_bytes())?;
    file.write_all(b"WAVE")?;
    file.write_all(b"fmt ")?;
    file.write_all(&fmt_len.to_le_bytes())?;
    file.write_all(&(if float { 3u16 } else { 1u16 }).to_le_bytes())?;
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&sample_rate.to_le_bytes())?;
    file.write_all(&(sample_rate * u32::from(channels) * u32::from(bytes)).to_le_bytes())?;
    file.write_all(&(channels * bytes).to_le_bytes())?;
    file.write_all(&(bytes * 8).to_le_bytes())?;
    if float {
        file.write_all(&0u16.to_le_bytes())?;
        file.write_all(b"fact")?;
        file.write_all(&4u32.to_le_bytes())?;
        file.write_all(&((samples.len() / usize::from(channels.max(1))) as u32).to_le_bytes())?;
    }
    file.write_all(b"data")?;
    file.write_all(&data_len.to_le_bytes())?;

    let mut dither = Dither::new();
    for &sample in samples {
        let x = sample.clamp(-1.0, 1.0);
        match format {
            Format::Float32 => file.write_all(&x.to_le_bytes())?,
            Format::Pcm24 => {
                let v = (x * 8_388_607.0).round() as i32;
                file.write_all(&v.to_le_bytes()[..3])?;
            }
            Format::Pcm16 => {
                let v = (x * 32_767.0 + dither.next()).round().clamp(-32_768.0, 32_767.0) as i16;
                file.write_all(&v.to_le_bytes())?;
            }
        }
    }
    file.flush()
}

/// Triangular dither, one bit wide: the sum of two uniform draws. Added before
/// rounding to 16 bits, it turns the rounding error into a steady hiss far
/// below the music instead of distortion that follows it.
struct Dither(u64);

impl Dither {
    fn new() -> Self {
        // Fixed, so the same project exports to the same bytes.
        Self(0x9E37_79B9_7F4A_7C15)
    }

    fn uniform(&mut self) -> f32 {
        // xorshift64*: fast, and plenty for noise.
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        ((self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32) - 0.5
    }

    fn next(&mut self) -> f32 {
        self.uniform() + self.uniform()
    }
}

/// An export running on its own thread.
pub struct Job {
    pub path: PathBuf,
    /// Frames rendered so far.
    pub progress: Arc<AtomicU64>,
    /// Where the music ends, for a progress bar; the tail runs past it.
    pub end: u64,
    handle: Option<JoinHandle<io::Result<()>>>,
}

impl Job {
    /// Renders and writes on a new thread. The graph is compiled by the
    /// caller, on the control side, like any other.
    pub fn start(
        graph: Box<Graph>,
        transport: Transport,
        end: u64,
        block: usize,
        sample_rate: u32,
        path: PathBuf,
        format: Format,
    ) -> Self {
        let progress = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&progress);
        let target = path.clone();
        let handle = std::thread::spawn(move || {
            let mut graph = graph;
            let channels = graph.channels() as u16;
            let samples = render(&mut graph, transport, end, block, sample_rate, &counter);
            write_wav(&target, &samples, channels, sample_rate, format)
        });
        Self { path, progress, end, handle: Some(handle) }
    }

    /// How far along, 0 to 1. Holds at 1 through the tail.
    pub fn fraction(&self) -> f32 {
        (self.progress.load(Ordering::Relaxed) as f32 / self.end.max(1) as f32).min(1.0)
    }

    /// The result, once the thread has finished; `None` while it runs.
    pub fn finished(&mut self) -> Option<io::Result<()>> {
        if !self.handle.as_ref()?.is_finished() {
            return None;
        }
        let handle = self.handle.take()?;
        Some(handle.join().unwrap_or_else(|_| Err(io::Error::other("the export stopped unexpectedly"))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::{AudioBuffer, GraphBuilder, Limiter, Node, offline_transport};

    const SR: u32 = 48_000;

    /// A burst of 0.5 for `until` timeline frames, then silence: something to
    /// render whose start and end are known exactly.
    struct Burst {
        clock: Arc<engine_core::Clock>,
        until: u64,
    }

    impl Node for Burst {
        fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
            let now = self.clock.position();
            for frame in 0..buffer.frames() {
                if now + (frame as u64) < self.until {
                    for channel in 0..buffer.channels() {
                        buffer.channel_mut(channel)[frame] = 0.5;
                    }
                }
            }
        }
    }

    /// The burst through a lookahead limiter, as the master has one.
    fn graph(until: u64) -> (Box<Graph>, Transport) {
        let transport = offline_transport(SR as f32);
        let mut builder = GraphBuilder::new();
        let burst = builder.add(Box::new(Burst { clock: Arc::clone(transport.clock()), until }));
        let limiter = builder.add(Box::new(Limiter::new(0.95, 2, SR as f32)));
        builder.connect(burst, limiter).output(limiter);
        (Box::new(builder.build(2, 512).unwrap()), transport)
    }

    #[test]
    fn a_render_starts_on_the_first_sample_despite_the_lookahead() {
        let (mut graph, transport) = graph(4_800);
        assert!(graph.latency_frames() > 0, "the test needs a graph with latency");
        let out = render(&mut graph, transport, 4_800, 512, SR, &AtomicU64::new(0));
        assert_eq!(out[0], 0.5, "the file began with the limiter's lookahead instead of the music");
        // The burst ends where it should, not a lookahead later.
        assert_eq!(out[4_799 * 2], 0.5);
        assert_eq!(out[4_800 * 2], 0.0);
    }

    #[test]
    fn a_render_stops_once_the_tail_is_quiet() {
        let (mut graph, transport) = graph(4_800);
        let out = render(&mut graph, transport, 4_800, 512, SR, &AtomicU64::new(0));
        let seconds = out.len() as f32 / 2.0 / SR as f32;
        // The burst, then silence: it should end shortly after, not run on
        // for the longest tail allowed.
        assert!(seconds < 0.2, "rendered {seconds} s of a 0.1 s burst");
    }

    fn read_back(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }

    #[test]
    fn a_wav_file_says_what_it_holds() {
        let path = std::env::temp_dir().join(format!("daw-export-{}.wav", std::process::id()));
        let samples = [0.0f32, 0.5, -0.5, 1.0];
        for (format, tag, bits) in [(Format::Pcm16, 1u16, 16u16), (Format::Pcm24, 1, 24), (Format::Float32, 3, 32)] {
            write_wav(&path, &samples, 2, SR, format).unwrap();
            let bytes = read_back(&path);
            assert_eq!(&bytes[0..4], b"RIFF");
            assert_eq!(&bytes[8..12], b"WAVE");
            let riff = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
            assert_eq!(riff + 8, bytes.len(), "{format:?}: the RIFF size is wrong");
            assert_eq!(u16::from_le_bytes([bytes[20], bytes[21]]), tag);
            assert_eq!(u16::from_le_bytes([bytes[22], bytes[23]]), 2, "channels");
            assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), SR);
            assert_eq!(u16::from_le_bytes([bytes[34], bytes[35]]), bits);
            let data = bytes.windows(4).position(|w| w == b"data").unwrap();
            let len = u32::from_le_bytes(bytes[data + 4..data + 8].try_into().unwrap()) as usize;
            assert_eq!(len, samples.len() * usize::from(bits / 8));
        }
        // 24-bit full scale is the largest 24-bit value.
        write_wav(&path, &[1.0, -1.0], 2, SR, Format::Pcm24).unwrap();
        let bytes = read_back(&path);
        let data = bytes.windows(4).position(|w| w == b"data").unwrap() + 8;
        assert_eq!(&bytes[data..data + 3], &[0xFF, 0xFF, 0x7F]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn dither_is_small_centred_and_the_same_every_time() {
        let (mut a, mut b) = (Dither::new(), Dither::new());
        let draws: Vec<f32> = (0..10_000).map(|_| a.next()).collect();
        assert!(draws.iter().all(|d| d.abs() <= 1.0), "dither beyond one bit");
        let mean = draws.iter().sum::<f32>() / draws.len() as f32;
        assert!(mean.abs() < 0.02, "dither leans to one side: {mean}");
        assert!(draws.iter().all(|d| *d == b.next()), "the same export would differ from run to run");
    }
}
