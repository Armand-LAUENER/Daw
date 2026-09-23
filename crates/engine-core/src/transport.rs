//! Play, pause, stop and seek: where the timeline is, and whether it moves.
//!
//! Two clocks, kept apart on purpose. The engine's frame counter never stops —
//! automation and graph swaps are scheduled against it, and a pause must not
//! stall them. The timeline position is what a person starts and stops, and it
//! is what a file on a track plays from. [`Clock`] is the second one.
//!
//! Not to be confused with `engine-io-file`'s transport, which streams one
//! file from disk through a preload thread. This one reads nothing: it is the
//! project-wide position every node playing material follows.
//!
//! The audio thread owns the position and publishes it once per block through
//! atomics: nodes that play material read it, and the interface reads it to
//! show where playback is. Nobody else writes it — requests arrive over a
//! lock-free queue and are carried out between blocks.
//!
//! Nothing starts or stops dead. Cutting a waveform mid-cycle is a click, so
//! every transition goes through a short fade: stopping fades out and then
//! stops the clock, a seek while playing fades out, jumps, and fades back in.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rtrb::{Consumer, Producer, RingBuffer};

/// How long starting, stopping and jumping take to fade, in milliseconds.
///
/// Long enough that no transition clicks, short enough that nobody hears a
/// press of play as late.
pub const TRANSPORT_FADE_MS: f32 = 5.0;

/// Where the transport repeats, in frames. `start` is always below `end`.
///
/// The wrap itself is not faded like a seek: a region is a deliberate,
/// exact edit, and fading every single pass would be a stutter on anything
/// played in time, worse than the click a loop point not chosen on a zero
/// crossing can already make — the same trade-off a tape loop's splice makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loop {
    pub start: u64,
    pub end: u64,
}

/// The timeline position, as the audio thread last published it.
///
/// Shared between the audio thread, which writes it at the start of each block,
/// and anything that reads it: a node playing a file, the interface drawing a
/// playhead. Reading is a relaxed atomic load — no lock, no wait.
#[derive(Debug, Default)]
pub struct Clock {
    position: AtomicU64,
    running: AtomicBool,
}

impl Clock {
    pub fn new() -> Self {
        Self::default()
    }

    /// The timeline frame the current block starts at.
    pub fn position(&self) -> u64 {
        self.position.load(Ordering::Relaxed)
    }

    /// Whether the timeline is moving, fades included.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    fn publish(&self, position: u64, running: bool) {
        self.position.store(position, Ordering::Relaxed);
        self.running.store(running, Ordering::Relaxed);
    }
}

/// What the control side can ask of the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportCommand {
    Play,
    /// Stops where it is.
    Pause,
    /// Stops and goes back to the start.
    Stop,
    /// Moves to a timeline frame, playing or not.
    Seek(u64),
    /// Where to repeat, or `None` to stop repeating. A region shorter than
    /// [`MIN_LOOP_FRAMES`] is refused rather than spinning the audio thread
    /// on a wrap every handful of samples.
    SetLoop(Option<Loop>),
}

/// The shortest a loop region may be: below this a wrap would land inside
/// almost every block, which is real-time work for a region nobody could
/// hear as a loop rather than a buzz.
pub const MIN_LOOP_FRAMES: u64 = 480;

/// The control side: asks for changes, and reads where playback is.
pub struct TransportControl {
    tx: Producer<TransportCommand>,
    clock: Arc<Clock>,
    /// What was last asked for, so a play/pause button shows the state the
    /// person chose rather than lagging a block behind it.
    playing: bool,
}

impl TransportControl {
    /// False when the queue is full, which means the audio thread has not run
    /// for a while; the request is dropped rather than blocking the interface.
    pub fn send(&mut self, command: TransportCommand) -> bool {
        let sent = self.tx.push(command).is_ok();
        if sent {
            match command {
                TransportCommand::Play => self.playing = true,
                TransportCommand::Pause | TransportCommand::Stop => self.playing = false,
                TransportCommand::Seek(_) | TransportCommand::SetLoop(_) => {}
            }
        }
        sent
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn clock(&self) -> &Arc<Clock> {
        &self.clock
    }
}

/// The audio thread's side: owns the position and the fade.
pub struct Transport {
    rx: Consumer<TransportCommand>,
    clock: Arc<Clock>,
    position: u64,
    /// Whether playback has been asked for.
    wanted: bool,
    /// A jump waiting for the fade-out to finish.
    seek: Option<u64>,
    /// The fade, 0 silent to 1 full.
    gain: f32,
    /// How far the fade moves per frame.
    step: f32,
    /// Where playback repeats, if it does.
    loop_region: Option<Loop>,
}

/// Opens a transport: the control side, and the side the callback keeps.
pub fn transport_channel(sample_rate: f32, capacity: usize) -> (TransportControl, Transport) {
    let (tx, rx) = RingBuffer::new(capacity);
    let clock = Arc::new(Clock::new());
    let frames = (TRANSPORT_FADE_MS / 1000.0 * sample_rate).max(1.0);
    let transport = Transport {
        rx,
        clock: Arc::clone(&clock),
        position: 0,
        wanted: false,
        seek: None,
        gain: 0.0,
        step: 1.0 / frames,
        loop_region: None,
    };
    (TransportControl { tx, clock, playing: false }, transport)
}

/// A transport for rendering offline: playing from the first frame at full
/// level, with nobody to ask it anything.
///
/// No fade in: a live start fades so a press of play does not click, but a
/// render starts before any sound, and fading would clip the attack of
/// whatever begins at zero.
pub fn offline_transport(sample_rate: f32) -> Transport {
    let (_control, mut transport) = transport_channel(sample_rate, 1);
    transport.wanted = true;
    transport.gain = 1.0;
    transport
}

impl Transport {
    /// The clock this transport publishes to, for the nodes that follow it.
    pub fn clock(&self) -> &Arc<Clock> {
        &self.clock
    }

    /// Takes in what was asked for and publishes where this block starts.
    ///
    /// Called before the graph renders the block, so every node reading the
    /// clock during it sees the same position.
    pub fn begin(&mut self) -> u64 {
        while let Ok(command) = self.rx.pop() {
            match command {
                TransportCommand::Play => self.wanted = true,
                TransportCommand::Pause => self.wanted = false,
                TransportCommand::Stop => {
                    self.wanted = false;
                    self.seek = Some(0);
                }
                TransportCommand::Seek(frame) => self.seek = Some(frame),
                TransportCommand::SetLoop(region) => {
                    self.loop_region = region.filter(|r| r.end > r.start && r.end - r.start >= MIN_LOOP_FRAMES);
                }
            }
        }
        // Silent already: a jump has nothing to fade through.
        if self.gain == 0.0
            && let Some(frame) = self.seek.take()
        {
            self.position = frame;
        }
        self.clock.publish(self.position, self.running());
        self.position
    }

    /// How many frames may still be rendered before the loop's end, if a
    /// block starting here would otherwise cross it.
    ///
    /// Called after [`Self::begin`] latches this block's position. Clipping
    /// the render to this keeps the wrap sample-accurate: the alternative
    /// would be rendering material from beyond the loop and throwing it away,
    /// which is what every other jump in this transport avoids by fading
    /// instead — a loop wrap has nothing to fade, so it is cut at the sample
    /// instead of the millisecond.
    pub fn frames_until_loop(&self) -> Option<u64> {
        let region = self.loop_region?;
        (self.running() && self.position < region.end).then(|| region.end - self.position)
    }

    /// Fades the rendered block and moves the timeline past it.
    ///
    /// `data` is interleaved, `channels` wide. Whatever the graph wrote is
    /// scaled by the fade, so a stopped transport is silent however the graph
    /// is built.
    pub fn finish(&mut self, data: &mut [f32], channels: usize) {
        let channels = channels.max(1);
        let frames = data.len() / channels;
        let running = self.running();
        // A pending jump fades out first, then comes back in at its target.
        let target = if self.wanted && self.seek.is_none() { 1.0 } else { 0.0 };

        if self.gain == target {
            if target == 0.0 {
                data.fill(0.0);
            }
        } else {
            for frame in data.chunks_exact_mut(channels) {
                // Lands exactly on the target: a fade that stopped a rounding
                // error short of zero would never let a pending jump happen.
                let remaining = target - self.gain;
                self.gain =
                    if remaining.abs() <= self.step * 1.5 { target } else { self.gain + self.step.copysign(remaining) };
                for sample in frame {
                    *sample *= self.gain;
                }
            }
        }

        if running {
            self.position += frames as u64;
            // The wrap, not a jump: `frames_until_loop` is what keeps this
            // landing exactly on `end` in ordinary use, so the modulo below
            // is a safety net for a loop switched on after the fact, not the
            // usual path.
            if let Some(region) = self.loop_region
                && self.position >= region.end
            {
                let length = region.end - region.start;
                self.position = region.start + (self.position - region.end) % length;
            }
        }
        if self.gain == 0.0
            && let Some(frame) = self.seek.take()
        {
            self.position = frame;
        }
    }

    /// Moving: playing, or fading in or out of it.
    fn running(&self) -> bool {
        self.wanted || self.gain > 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    /// Frames in one fade at `SR`.
    const FADE: usize = 240;

    /// Runs one block of ones through the transport and returns what came out.
    fn block(transport: &mut Transport, frames: usize) -> (u64, Vec<f32>) {
        let start = transport.begin();
        let mut data = vec![1.0; frames * 2];
        transport.finish(&mut data, 2);
        (start, data)
    }

    #[test]
    fn an_offline_transport_plays_from_the_first_frame_at_full_level() {
        let mut transport = offline_transport(SR);
        let (start, data) = block(&mut transport, 64);
        assert_eq!(start, 0);
        assert!(data.iter().all(|&s| s == 1.0), "an offline render faded in");
        assert_eq!(block(&mut transport, 64).0, 64);
        assert!(transport.clock().is_running());
    }

    #[test]
    fn a_new_transport_is_stopped_and_silent() {
        let (_control, mut transport) = transport_channel(SR, 8);
        let (start, data) = block(&mut transport, 64);
        assert_eq!(start, 0);
        assert!(data.iter().all(|&s| s == 0.0));
        assert!(!transport.clock.is_running());
        assert_eq!(block(&mut transport, 64).0, 0, "a stopped timeline moved");
    }

    #[test]
    fn play_fades_in_and_moves_the_timeline() {
        let (mut control, mut transport) = transport_channel(SR, 8);
        control.send(TransportCommand::Play);
        let (_, data) = block(&mut transport, FADE * 2);
        let left: Vec<f32> = data.iter().step_by(2).copied().collect();
        assert!(left[0] > 0.0 && left[0] < 0.01, "play started at full level: {}", left[0]);
        assert!(left.windows(2).all(|w| w[1] >= w[0]), "the fade-in went down");
        assert_eq!(left[FADE], 1.0, "the fade-in did not arrive in {FADE} frames");
        assert_eq!(block(&mut transport, 64).0, (FADE * 2) as u64);
        assert!(control.clock().is_running());
    }

    #[test]
    fn pause_fades_out_and_holds_the_position() {
        let (mut control, mut transport) = transport_channel(SR, 8);
        control.send(TransportCommand::Play);
        block(&mut transport, 1_000);
        control.send(TransportCommand::Pause);
        let (_, data) = block(&mut transport, FADE * 2);
        assert!(data[0] > 0.99, "pause cut the sound instead of fading it");
        assert!(data[FADE * 2..].iter().all(|&s| s == 0.0), "still sounding after the fade");

        let held = block(&mut transport, 64).0;
        assert_eq!(block(&mut transport, 64).0, held, "a paused timeline kept moving");
        assert!(!control.clock().is_running());

        control.send(TransportCommand::Play);
        assert_eq!(block(&mut transport, 64).0, held, "play did not resume where pause left off");
    }

    #[test]
    fn stop_goes_back_to_the_start_after_fading() {
        let (mut control, mut transport) = transport_channel(SR, 8);
        control.send(TransportCommand::Play);
        block(&mut transport, 5_000);
        control.send(TransportCommand::Stop);
        let (_, data) = block(&mut transport, FADE * 2);
        assert!(data[0] > 0.99, "stop cut the sound instead of fading it");
        assert_eq!(block(&mut transport, 64).0, 0);
    }

    #[test]
    fn a_seek_while_playing_fades_out_jumps_and_fades_back_in() {
        let (mut control, mut transport) = transport_channel(SR, 8);
        control.send(TransportCommand::Play);
        block(&mut transport, 1_000);
        control.send(TransportCommand::Seek(96_000));

        let (_, out) = block(&mut transport, FADE);
        assert!(out[out.len() - 1] < 0.01, "the jump was not faded out");
        let (start, back) = block(&mut transport, FADE * 2);
        assert_eq!(start, 96_000, "the jump did not land where it was aimed");
        assert!(back[0] < 0.01 && back[back.len() - 1] == 1.0, "playback did not fade back in");
    }

    #[test]
    fn a_seek_while_stopped_lands_at_once() {
        let (mut control, mut transport) = transport_channel(SR, 8);
        control.send(TransportCommand::Seek(12_345));
        assert_eq!(block(&mut transport, 64).0, 12_345);
        assert_eq!(block(&mut transport, 64).0, 12_345, "a seek started playback");
    }

    #[test]
    fn the_clock_shows_what_the_block_starts_at() {
        let (mut control, mut transport) = transport_channel(SR, 8);
        control.send(TransportCommand::Play);
        for n in 0..10u64 {
            let start = transport.begin();
            assert_eq!(control.clock().position(), start);
            assert_eq!(start, n * 128);
            transport.finish(&mut [0.0; 256], 2);
        }
    }

    /// One begin/finish pair, clipped to the loop's boundary the way a real
    /// callback would — the sample-accurate path, not the defensive modulo.
    fn looped_block(transport: &mut Transport, want: usize) -> u64 {
        let start = transport.begin();
        let span = transport.frames_until_loop().map_or(want, |left| want.min(left as usize).max(1));
        transport.finish(&mut vec![1.0; span * 2], 2);
        start
    }

    #[test]
    fn a_loop_region_repeats_once_the_playhead_reaches_it() {
        let (mut control, mut transport) = transport_channel(SR, 8);
        control.send(TransportCommand::Play);
        control.send(TransportCommand::SetLoop(Some(Loop { start: 1_000, end: 2_000 })));

        // Starting before the region, the first pass plays through it once —
        // 0 is as legitimate a place to start as any other — and only then
        // does it start repeating. `want` is bigger than the region so every
        // pass is clipped by the loop rather than by the caller's own size.
        let starts: Vec<u64> = (0..5).map(|_| looped_block(&mut transport, 5_000)).collect();
        assert_eq!(starts[0], 0, "the region skipped what came before it");
        assert!(starts[1..].iter().all(|&s| s == 1_000), "{starts:?} did not settle on the region's start");
    }

    #[test]
    fn the_wrap_lands_on_the_start_even_without_the_caller_clipping_to_it() {
        // The modulo in `finish` is a safety net: a block that overruns the
        // region — the loop switched on after the block was already sized,
        // or simply not clipped by the caller — must not leave the position
        // stuck past `end`.
        let (mut control, mut transport) = transport_channel(SR, 8);
        control.send(TransportCommand::SetLoop(Some(Loop { start: 1_000, end: 1_500 })));
        control.send(TransportCommand::Seek(1_400));
        control.send(TransportCommand::Play);

        assert_eq!(transport.begin(), 1_400);
        transport.finish(&mut vec![1.0; 200 * 2], 2);
        // 1400 + 200 = 1600, overrunning the 1000..1500 region by 100.
        assert_eq!(transport.begin(), 1_100);
    }

    #[test]
    fn a_region_shorter_than_the_minimum_is_refused() {
        let (mut control, mut transport) = transport_channel(SR, 8);
        control.send(TransportCommand::Play);
        control.send(TransportCommand::SetLoop(Some(Loop { start: 100, end: 100 + MIN_LOOP_FRAMES - 1 })));
        transport.begin();
        assert!(transport.frames_until_loop().is_none(), "a too-short region was accepted");
        // Playback runs straight through where the refused region would have
        // wrapped it.
        assert_eq!(block(&mut transport, 1_000).0, 0);
        assert_eq!(block(&mut transport, 64).0, 1_000);
    }

    #[test]
    fn clearing_the_loop_lets_playback_run_on() {
        let (mut control, mut transport) = transport_channel(SR, 8);
        control.send(TransportCommand::Play);
        control.send(TransportCommand::SetLoop(Some(Loop { start: 100, end: 200 })));
        control.send(TransportCommand::SetLoop(None));
        transport.begin();
        assert!(transport.frames_until_loop().is_none(), "clearing the loop left it active");
        block(&mut transport, 1_000);
        assert_eq!(block(&mut transport, 64).0, 1_000, "a cleared loop still wrapped the timeline");
    }
}
