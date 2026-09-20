//! Playback transport: a preload thread feeding the audio thread through a
//! ring buffer, with play, pause, seek and looping.
//!
//! The two halves have opposite constraints. The preloader runs on its own
//! thread and may do anything — decode, read from disk, allocate. [`Playback`]
//! is called from the audio callback and does none of those things: it copies
//! out of a ring buffer and applies a gain ramp, nothing else.
//!
//! ## Why a seek needs a handshake
//!
//! When a seek arrives, the ring already holds audio for the old position. It
//! has to go, and only the consumer can drop it. So the preloader raises
//! [`Shared::flush_request`] and then **stops filling** until the consumer has
//! discarded the ring and answered on [`Shared::flush_ack`]. Nothing from the
//! new position is written while stale audio is still queued, so the two can
//! never be mixed up, and a consumer that never runs simply stalls the
//! preloader instead of corrupting the stream.
//!
//! Looping is not a seek in that sense: the preloader jumps back by itself
//! while it fills, so the audio stays continuous and no flush is needed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use engine_dsp::LinearSmoother;
use rtrb::{Consumer, Producer, RingBuffer};

use crate::DecodedAudio;

/// Length of the anti-click ramp applied on pause, seek and underrun.
///
/// Long enough to bring a full-scale sample to zero inaudibly, short enough
/// that a pause still feels instant.
pub const FADE_MS: f32 = 5.0;

/// How long the preload thread sleeps when it has nothing to do.
const IDLE_SLEEP: Duration = Duration::from_millis(2);

/// Frames the preloader moves per batch.
const BATCH_FRAMES: usize = 4_096;

/// What the transport is asked to do. Sent from the control thread, never from
/// the audio thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportCommand {
    Play,
    Pause,
    /// Jump to a frame index, counted from the start of the source.
    Seek(u64),
    /// Repeat `start..end` forever, or play straight through with `None`.
    SetLoop(Option<(u64, u64)>),
    /// Stop the preload thread. Playback goes silent once the ring drains.
    Stop,
}

/// Audio the transport can play.
///
/// Implemented here for a clip already in memory. A streaming decoder fits the
/// same shape: the transport never learns where the samples come from, which is
/// the point of having a preload thread at all.
pub trait Source: Send {
    fn channels(&self) -> u16;
    fn sample_rate(&self) -> u32;
    /// Total length, when it is known.
    fn frames(&self) -> Option<u64>;
    /// Fills `out` with interleaved frames from the current position and
    /// advances it. Returns the number of **frames** written, which falls short
    /// only at the end of the source.
    fn read(&mut self, out: &mut [f32]) -> usize;
    /// Moves the read position. Seeking past the end leaves it at the end.
    fn seek(&mut self, frame: u64);
}

/// A [`Source`] over a clip held in memory.
#[derive(Debug, Clone)]
pub struct MemorySource {
    audio: DecodedAudio,
    position: u64,
}

impl MemorySource {
    pub fn new(audio: DecodedAudio) -> Self {
        Self { audio, position: 0 }
    }
}

impl Source for MemorySource {
    fn channels(&self) -> u16 {
        self.audio.channels
    }

    fn sample_rate(&self) -> u32 {
        self.audio.sample_rate
    }

    fn frames(&self) -> Option<u64> {
        Some(self.audio.frames() as u64)
    }

    fn read(&mut self, out: &mut [f32]) -> usize {
        let channels = usize::from(self.audio.channels);
        let start = self.position as usize * channels;
        let available = self.audio.samples.len().saturating_sub(start);
        let take = out.len().min(available) / channels * channels;
        out[..take].copy_from_slice(&self.audio.samples[start..start + take]);
        self.position += (take / channels) as u64;
        take / channels
    }

    fn seek(&mut self, frame: u64) {
        self.position = frame.min(self.audio.frames() as u64);
    }
}

/// State both halves of the transport read.
///
/// Relaxed atomics carry the reporting counters: they are statistics, not
/// synchronisation. The flush handshake is the exception and uses acquire and
/// release, because it decides whether queued audio is stale.
#[derive(Debug, Default)]
pub struct Shared {
    /// Raised by the preloader to ask the consumer to drop the queued audio.
    pub flush_request: AtomicU64,
    /// Echoed by the consumer once it has.
    pub flush_ack: AtomicU64,
    /// Where playback actually is, in frames, as opposed to where decoding is.
    /// Written by the consumer alone: a second writer would race it.
    pub position: AtomicU64,
    /// Where the preloader has moved the source to. Read by the consumer when
    /// it honours the matching flush, so it knows what the new audio is.
    seek_position: AtomicU64,
    /// Buffers that ran out of audio. Each one is a fade rather than a click,
    /// but it still means the preloader did not keep up.
    pub underruns: AtomicU64,
    /// Set once the source has been played to its end.
    pub finished: AtomicBool,
    /// Whether the consumer should be letting audio through.
    playing: AtomicBool,
    /// Active loop, as frame indices. `loop_end` of zero means no loop; the
    /// consumer needs it to report a position that wraps with the audio.
    loop_start: AtomicU64,
    loop_end: AtomicU64,
}

impl Shared {
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    pub fn looping(&self) -> Option<(u64, u64)> {
        let end = self.loop_end.load(Ordering::Relaxed);
        (end > 0).then(|| (self.loop_start.load(Ordering::Relaxed), end))
    }

    fn set_loop(&self, range: Option<(u64, u64)>) {
        let (start, end) = range.filter(|(s, e)| e > s).unwrap_or((0, 0));
        self.loop_start.store(start, Ordering::Relaxed);
        self.loop_end.store(end, Ordering::Relaxed);
    }
}

/// Controls a running transport. Dropping it stops the preload thread.
pub struct TransportHandle {
    commands: Producer<TransportCommand>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl TransportHandle {
    /// Queues a command, giving it back if the queue is full or the preload
    /// thread has gone away.
    pub fn send(&mut self, command: TransportCommand) -> Result<(), TransportCommand> {
        self.commands.push(command).map_err(|rtrb::PushError::Full(command)| command)
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Playback position in frames.
    pub fn position(&self) -> u64 {
        self.shared.position.load(Ordering::Relaxed)
    }
}

impl Drop for TransportHandle {
    fn drop(&mut self) {
        let _ = self.commands.push(TransportCommand::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The audio thread's end of the transport.
///
/// Every method here is real-time safe: no allocation, no locking, no I/O.
pub struct Playback {
    audio: Consumer<f32>,
    shared: Arc<Shared>,
    channels: u16,
    gain: LinearSmoother,
    gain_target: f32,
    /// The flush generation this side has already honoured.
    flushed: u64,
    /// Frames played since that flush, used to report the position.
    since_flush: u64,
    flush_position: u64,
}

impl Playback {
    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Fills `out` with interleaved audio, in place.
    ///
    /// Always writes the whole buffer: silence when paused, when the source has
    /// ended, or when the preloader has fallen behind. The gain ramp makes each
    /// of those a short fade rather than a step.
    pub fn fill(&mut self, out: &mut [f32]) {
        let channels = usize::from(self.channels);
        debug_assert_eq!(out.len() % channels, 0, "buffer is not a whole number of frames");

        self.honour_flush();

        // Once the fade has run out, a paused transport stops consuming. Going
        // on would eat the queued audio and walk the position forward through
        // silence. The fade itself does consume, so resuming picks up a few
        // milliseconds later than it stopped.
        if !self.shared.is_playing() && self.gain_target == 0.0 && !self.gain.is_ramping() {
            out.fill(0.0);
            return;
        }

        // Take what the ring holds, rounded down to whole frames.
        let available = self.audio.slots() / channels * channels;
        let take = out.len().min(available);
        if take > 0
            && let Ok(chunk) = self.audio.read_chunk(take)
        {
            let (head, tail) = chunk.as_slices();
            out[..head.len()].copy_from_slice(head);
            out[head.len()..take].copy_from_slice(&tail[..take - head.len()]);
            chunk.commit_all();
        }
        out[take..].fill(0.0);

        // Running dry at the end of the source is not the preloader falling
        // behind, and counting it would blunt the one number that says so.
        let starved = take < out.len();
        if starved && self.gain_target > 0.0 && !self.shared.finished.load(Ordering::Relaxed) {
            self.shared.underruns.fetch_add(1, Ordering::Relaxed);
        }

        // Fade out whenever there is nothing to play, rather than cutting.
        let wanted = if self.shared.is_playing() && !starved { 1.0 } else { 0.0 };
        if wanted != self.gain_target {
            self.gain_target = wanted;
            self.gain.set_target(wanted);
        }
        for frame in out.chunks_exact_mut(channels) {
            let gain = self.gain.next_value();
            for sample in frame {
                *sample *= gain;
            }
        }

        self.since_flush += (take / channels) as u64;
        self.shared.position.store(self.reported_position(), Ordering::Relaxed);
    }

    /// Frames played since the last flush, folded back into the loop range so
    /// the reported position follows the audio instead of running past it.
    fn reported_position(&self) -> u64 {
        let played = self.flush_position + self.since_flush;
        match self.shared.looping() {
            Some((start, end)) if played >= end => start + (played - start) % (end - start),
            _ => played,
        }
    }

    /// Drops audio queued for a position that has been seeked away from, and
    /// tells the preloader it may start filling again.
    fn honour_flush(&mut self) {
        let requested = self.shared.flush_request.load(Ordering::Acquire);
        if requested == self.flushed {
            return;
        }
        // Discarding a chunk is O(1): the read index simply moves.
        if let Ok(chunk) = self.audio.read_chunk(self.audio.slots()) {
            chunk.commit_all();
        }
        self.flushed = requested;
        self.flush_position = self.shared.seek_position.load(Ordering::Relaxed);
        self.since_flush = 0;
        self.shared.position.store(self.flush_position, Ordering::Relaxed);
        // Silence now, so the audio from the new position fades in.
        self.gain_target = 0.0;
        self.gain.set_target(0.0);
        self.shared.flush_ack.store(requested, Ordering::Release);
    }
}

/// How much audio to keep queued ahead of the audio thread.
///
/// The margin buys tolerance to a slow disk or a busy machine; it is also the
/// audio a seek throws away, so it trades robustness against seek latency.
#[derive(Debug, Clone, Copy)]
pub struct TransportOptions {
    pub buffer_ms: f32,
    pub fade_ms: f32,
}

impl Default for TransportOptions {
    fn default() -> Self {
        Self { buffer_ms: 500.0, fade_ms: FADE_MS }
    }
}

/// Starts the preload thread for `source`.
pub fn start(source: Box<dyn Source>, opts: TransportOptions) -> (TransportHandle, Playback) {
    let channels = source.channels().max(1);
    let sample_rate = source.sample_rate().max(1);
    let queued_frames = (opts.buffer_ms * 1e-3 * sample_rate as f32).max(1.0) as usize;
    let (producer, consumer) = RingBuffer::<f32>::new(queued_frames * usize::from(channels));
    let (commands_tx, commands_rx) = RingBuffer::<TransportCommand>::new(64);

    let shared = Arc::new(Shared::default());
    let preloader = Preloader {
        source,
        commands: commands_rx,
        audio: producer,
        shared: Arc::clone(&shared),
        channels: usize::from(channels),
        looping: None,
        scratch: vec![0.0; BATCH_FRAMES * usize::from(channels)],
        decode_position: 0,
        ended: false,
    };
    let thread = thread::Builder::new()
        .name("daw-preload".into())
        .spawn(move || preloader.run())
        .expect("the preload thread can be spawned");

    let playback = Playback {
        audio: consumer,
        shared: Arc::clone(&shared),
        channels,
        gain: LinearSmoother::with_duration(0.0, opts.fade_ms, sample_rate as f32),
        gain_target: 0.0,
        flushed: 0,
        since_flush: 0,
        flush_position: 0,
    };

    (TransportHandle { commands: commands_tx, shared, thread: Some(thread) }, playback)
}

/// The non-real-time half: owns the source and keeps the ring buffer fed.
struct Preloader {
    source: Box<dyn Source>,
    commands: Consumer<TransportCommand>,
    audio: Producer<f32>,
    shared: Arc<Shared>,
    channels: usize,
    looping: Option<(u64, u64)>,
    scratch: Vec<f32>,
    decode_position: u64,
    ended: bool,
}

impl Preloader {
    fn run(mut self) {
        loop {
            if self.handle_commands().is_break() {
                return;
            }
            // While a flush is outstanding, writing would mix audio from the new
            // position into audio the consumer has not dropped yet.
            if self.flush_pending() || !self.fill() {
                thread::sleep(IDLE_SLEEP);
            }
        }
    }

    fn handle_commands(&mut self) -> std::ops::ControlFlow<()> {
        while let Ok(command) = self.commands.pop() {
            match command {
                TransportCommand::Play => {
                    self.shared.finished.store(false, Ordering::Relaxed);
                    self.shared.playing.store(true, Ordering::Relaxed);
                }
                TransportCommand::Pause => self.shared.playing.store(false, Ordering::Relaxed),
                TransportCommand::Seek(frame) => self.start_seek(frame),
                TransportCommand::SetLoop(range) => {
                    self.looping = range.filter(|(start, end)| end > start);
                    self.shared.set_loop(self.looping);
                }
                TransportCommand::Stop => {
                    self.shared.playing.store(false, Ordering::Relaxed);
                    return std::ops::ControlFlow::Break(());
                }
            }
        }
        std::ops::ControlFlow::Continue(())
    }

    fn start_seek(&mut self, frame: u64) {
        self.source.seek(frame);
        self.decode_position = frame;
        self.ended = false;
        self.shared.finished.store(false, Ordering::Relaxed);
        // Published before the request, so a consumer that sees the request
        // already sees the position it belongs to.
        self.shared.seek_position.store(frame, Ordering::Relaxed);
        self.shared.flush_request.fetch_add(1, Ordering::Release);
    }

    fn flush_pending(&self) -> bool {
        self.shared.flush_request.load(Ordering::Acquire) != self.shared.flush_ack.load(Ordering::Acquire)
    }

    /// Writes one batch. Returns false when there was nothing to do.
    fn fill(&mut self) -> bool {
        if self.ended {
            return false;
        }
        let room = self.audio.slots() / self.channels * self.channels;
        let take = room.min(self.scratch.len());
        if take == 0 {
            return false;
        }

        let mut wanted = take / self.channels;
        // Stop short at the loop end, so the jump lands exactly on it.
        if let Some((_, end)) = self.looping
            && self.decode_position < end
        {
            wanted = wanted.min((end - self.decode_position) as usize);
        }
        if wanted == 0 {
            return false;
        }

        let frames = self.source.read(&mut self.scratch[..wanted * self.channels]);
        if frames > 0 {
            let samples = frames * self.channels;
            if let Ok(chunk) = self.audio.write_chunk_uninit(samples) {
                chunk.fill_from_iter(self.scratch[..samples].iter().copied());
            }
            self.decode_position += frames as u64;
        }

        match self.looping {
            Some((start, _)) if self.at_loop_end() => {
                self.source.seek(start);
                self.decode_position = start;
                true
            }
            _ if frames == 0 => {
                self.ended = true;
                self.shared.finished.store(true, Ordering::Relaxed);
                false
            }
            _ => true,
        }
    }

    fn at_loop_end(&self) -> bool {
        self.looping.is_some_and(|(_, end)| self.decode_position >= end)
    }
}
