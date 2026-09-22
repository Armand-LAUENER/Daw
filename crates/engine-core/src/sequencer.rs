//! Notes from the timeline, played in time with the transport.
//!
//! The live [`Synth`](crate::Synth) is fed by a queue the control thread fills
//! against the engine's running frame counter. Clips on a timeline are the
//! other case: every note is already known, positioned in timeline frames, and
//! what decides when it plays is the transport's [`Clock`] — which stops,
//! resumes, and jumps. This node walks a sorted plan built on the control side
//! and asks the clock where it is at the start of every block.
//!
//! Three things a plan needs that a queue does not:
//!
//! - **Stopping** lets every note go, as a hardware sequencer's "all notes
//!   off" does, rather than leaving voices held until playback resumes.
//! - **A jump** — a seek, a stop back to the start, playback resumed — moves
//!   the plan to the new position without playing what was skipped.
//! - **Chasing**: after a jump, a note that started before the new position
//!   and has not ended yet is started at once. Without it, every edit made
//!   while playing — which rebuilds the graph, and so starts a new sequencer
//!   mid-song — would silence whatever notes were being held.

use std::sync::Arc;

use engine_dsp::{Adsr, Waveform};

use crate::synth::Voices;
use crate::{AudioBuffer, Clock, Node, NoteAction, NoteEvent};

/// A note's whole life on the timeline, kept for chasing.
#[derive(Debug, Clone, Copy)]
struct Span {
    on: u64,
    off: u64,
    pitch: u8,
    velocity: f32,
}

/// Plays a sorted list of timeline notes through a pool of voices.
pub struct Sequencer {
    voices: Voices,
    /// Note-ons and offs in timeline frames, in the order they happen.
    events: Vec<NoteEvent>,
    /// Every note as one piece, earliest start first.
    spans: Vec<Span>,
    clock: Arc<Clock>,
    /// The next event to play.
    next: usize,
    /// Where this block should start if playback simply carried on. `None`
    /// until the first block, and after a stop, so both count as a jump.
    expected: Option<u64>,
}

impl Sequencer {
    /// `events` is a plan in timeline frames; it is sorted here, and paired
    /// into whole notes for chasing — both on the control side, where the
    /// allocations are allowed.
    pub fn new(
        voices: usize,
        shape: Waveform,
        envelope: &Adsr,
        mut events: Vec<NoteEvent>,
        clock: Arc<Clock>,
        sample_rate: f32,
    ) -> Self {
        // Offs before ons at the same frame, so a repeated note ends and
        // restarts instead of being cut off by its predecessor.
        events.sort_by_key(|e| (e.frame, e.is_on()));
        let spans = spans_of(&events);
        Self {
            voices: Voices::new(voices, shape, envelope, sample_rate),
            events,
            spans,
            clock,
            next: 0,
            expected: None,
        }
    }

    /// How many voices are sounding, for metering and for tests.
    pub fn active_voices(&self) -> usize {
        self.voices.active()
    }

    /// Moves the plan to `now` without playing what lies before it, then
    /// starts every note that should already be sounding there.
    fn jump(&mut self, now: u64) {
        self.voices.release_all();
        self.next = self.events.partition_point(|e| e.frame < now);
        for span in &self.spans {
            if span.on >= now {
                // Sorted by start: nothing after this began before `now`.
                break;
            }
            if span.off > now {
                self.voices.play(span.pitch, NoteAction::On { velocity: span.velocity });
            }
        }
    }
}

impl Node for Sequencer {
    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        let (frames, channels) = (buffer.frames(), buffer.channels());
        let now = self.clock.position();

        if self.clock.is_running() {
            if self.expected != Some(now) {
                self.jump(now);
            }
            self.expected = Some(now + frames as u64);
        } else if self.expected.take().is_some() {
            // Just stopped: let everything go. The release still plays out,
            // under the transport's own fade.
            self.voices.release_all();
        }
        let running = self.expected.is_some();

        for frame in 0..frames {
            let at = now + frame as u64;
            while running && let Some(event) = self.events.get(self.next).filter(|e| e.frame <= at) {
                let (pitch, action) = (event.pitch, event.action);
                self.voices.play(pitch, action);
                self.next += 1;
            }
            let mixed = self.voices.next_sample();
            for channel in 0..channels {
                buffer.channel_mut(channel)[frame] += mixed;
            }
        }
    }

    fn reset(&mut self) {
        self.voices.reset();
        self.next = 0;
        self.expected = None;
    }
}

/// Pairs each note-on with the next note-off of the same pitch.
///
/// A note-on with no off after it is held to the end of time, which is what
/// its events would do if played through.
fn spans_of(events: &[NoteEvent]) -> Vec<Span> {
    let mut open: [Vec<(u64, f32)>; 128] = std::array::from_fn(|_| Vec::new());
    let mut spans = Vec::new();
    for event in events {
        let pitch = usize::from(event.pitch.min(127));
        match event.action {
            NoteAction::On { velocity } => open[pitch].push((event.frame, velocity)),
            NoteAction::Off => {
                if !open[pitch].is_empty() {
                    let (on, velocity) = open[pitch].remove(0);
                    spans.push(Span { on, off: event.frame, pitch: event.pitch, velocity });
                }
            }
        }
    }
    for (pitch, held) in open.iter().enumerate() {
        for &(on, velocity) in held {
            spans.push(Span { on, off: u64::MAX, pitch: pitch as u8, velocity });
        }
    }
    spans.sort_by_key(|s| s.on);
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Transport, TransportCommand, TransportControl, transport_channel};

    const SR: f32 = 48_000.0;

    /// Opens a sample after the note, shuts a sample after its release.
    fn instant() -> Adsr {
        Adsr::new(0.0, 0.0, 1.0, 0.0, SR)
    }

    struct Rig {
        control: TransportControl,
        transport: Transport,
        sequencer: Sequencer,
    }

    impl Rig {
        fn new(events: Vec<NoteEvent>) -> Self {
            let (control, transport) = transport_channel(SR, 16);
            let clock = Arc::clone(control.clock());
            let sequencer = Sequencer::new(8, Waveform::Saw, &instant(), events, clock, SR);
            Self { control, transport, sequencer }
        }

        /// One block as the callback runs it: the transport publishes where
        /// it is, the sequencer renders, and the timeline moves on. The fade
        /// is left out so what is measured is the sequencer.
        fn block(&mut self, frames: usize) -> Vec<f32> {
            self.transport.begin();
            let mut data = vec![0.0; frames];
            self.sequencer.process(&mut AudioBuffer::new(&mut data, 1));
            self.transport.finish(&mut vec![0.0; frames], 1);
            data
        }

        fn send(&mut self, command: TransportCommand) {
            self.control.send(command);
        }
    }

    fn first_sounding(samples: &[f32]) -> Option<usize> {
        samples.iter().position(|s| *s != 0.0)
    }

    #[test]
    fn nothing_plays_while_the_transport_is_stopped() {
        let mut rig = Rig::new(vec![NoteEvent::on(60, 1.0, 0), NoteEvent::off(60, 10_000)]);
        assert!(rig.block(512).iter().all(|s| *s == 0.0));
        assert_eq!(rig.sequencer.active_voices(), 0);
    }

    #[test]
    fn a_note_starts_on_its_timeline_frame() {
        let mut rig = Rig::new(vec![NoteEvent::on(60, 1.0, 300), NoteEvent::off(60, 10_000)]);
        rig.send(TransportCommand::Play);
        assert!(rig.block(256).iter().all(|s| *s == 0.0), "it sounded before its frame");
        assert_eq!(first_sounding(&rig.block(256)), Some(300 - 256 + 1), "the envelope opens the sample after");
    }

    #[test]
    fn stopping_lets_every_note_go() {
        let mut rig = Rig::new(vec![NoteEvent::on(60, 1.0, 0), NoteEvent::off(60, 100_000)]);
        rig.send(TransportCommand::Play);
        rig.block(256);
        assert_eq!(rig.sequencer.active_voices(), 1);
        rig.send(TransportCommand::Pause);
        // The transport fades for a few blocks before the clock stops.
        for _ in 0..4 {
            rig.block(256);
        }
        assert_eq!(rig.sequencer.active_voices(), 0, "a note was left held while stopped");
    }

    #[test]
    fn a_seek_into_a_held_note_starts_it_there() {
        // A note from 0 to 48 000. Jumping to 24 000 lands in its middle: it
        // must sound at once rather than wait for the next note-on.
        let mut rig = Rig::new(vec![NoteEvent::on(60, 1.0, 0), NoteEvent::off(60, 48_000)]);
        rig.send(TransportCommand::Seek(24_000));
        rig.send(TransportCommand::Play);
        let out = rig.block(256);
        assert_eq!(first_sounding(&out), Some(1), "a held note was not chased after the seek");
    }

    #[test]
    fn a_seek_past_a_note_does_not_play_it() {
        let mut rig = Rig::new(vec![NoteEvent::on(60, 1.0, 1_000), NoteEvent::off(60, 2_000)]);
        rig.send(TransportCommand::Seek(5_000));
        rig.send(TransportCommand::Play);
        assert!(rig.block(512).iter().all(|s| *s == 0.0), "a note behind the playhead was played");
    }

    #[test]
    fn a_sequencer_built_mid_song_picks_up_the_held_note() {
        // What a graph rebuilt during playback looks like: the clock is
        // already running when the node sees its first block.
        let mut rig = Rig::new(vec![NoteEvent::on(60, 1.0, 0), NoteEvent::off(60, 96_000)]);
        rig.send(TransportCommand::Play);
        for _ in 0..40 {
            rig.block(256);
        }
        let clock = Arc::clone(rig.control.clock());
        let events = vec![NoteEvent::on(60, 1.0, 0), NoteEvent::off(60, 96_000)];
        rig.sequencer = Sequencer::new(8, Waveform::Saw, &instant(), events, clock, SR);
        assert!(rig.block(256).iter().any(|s| *s != 0.0), "a rebuild silenced the held note");
    }

    #[test]
    fn a_repeated_note_is_released_and_struck_again() {
        let mut rig = Rig::new(vec![
            NoteEvent::on(60, 1.0, 0),
            NoteEvent::off(60, 100),
            NoteEvent::on(60, 1.0, 100),
            NoteEvent::off(60, 200),
        ]);
        rig.send(TransportCommand::Play);
        let out = rig.block(300);
        assert!(out[150] != 0.0, "the second note was cut off by the first one's end");
        assert!(out[210..].iter().all(|s| *s == 0.0));
    }
}
