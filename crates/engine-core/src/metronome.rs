//! The click that marks each beat.
//!
//! Follows the timeline the way a [`Sequencer`](crate::Sequencer) follows its
//! notes: a plan of clicks in timeline frames, built once on the control side,
//! walked here by asking the [`Clock`] where it is. Unlike a note there is
//! nothing to chase after a jump — a click either falls in a block or it does
//! not, and there is no tail held across the gap.

use std::sync::Arc;

use engine_dsp::{Adsr, SineOsc};

use crate::{AudioBuffer, Clock, Node};

/// One click, in timeline frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Click {
    pub frame: u64,
    /// A bar's first beat, sounded louder and higher than the others.
    pub accent: bool,
}

/// How long a click's blip takes to die away. Linear, like every envelope in
/// this engine (see [`Adsr`]): a pole never actually reaches zero, and a click
/// left ringing at 1e-30 between beats is a denormal for nothing anyone hears.
const DECAY_S: f32 = 0.03;
const NORMAL_HZ: f32 = 1_000.0;
const ACCENT_HZ: f32 = 1_600.0;
const NORMAL_LEVEL: f32 = 0.35;
const ACCENT_LEVEL: f32 = 0.55;

/// Plays a sorted list of timeline clicks through one voice.
///
/// One voice is enough: a click's blip is shorter than any beat a person
/// would set, so two are never sounding at once.
pub struct Metronome {
    clicks: Vec<Click>,
    clock: Arc<Clock>,
    /// The next click to play.
    next: usize,
    /// Where this block should start if playback simply carried on. `None`
    /// until the first block, and after a stop or a jump.
    expected: Option<u64>,
    envelope: Adsr,
    osc: SineOsc,
    accented: bool,
}

impl Metronome {
    /// `clicks` is a plan in timeline frames; sorted here, on the control side.
    pub fn new(mut clicks: Vec<Click>, clock: Arc<Clock>, sample_rate: f32) -> Self {
        clicks.sort_by_key(|c| c.frame);
        Self {
            clicks,
            clock,
            next: 0,
            expected: None,
            envelope: Adsr::new(0.0, DECAY_S, 0.0, 0.0, sample_rate),
            osc: SineOsc::new(sample_rate),
            accented: false,
        }
    }
}

impl Node for Metronome {
    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        let (frames, channels) = (buffer.frames(), buffer.channels());
        let now = self.clock.position();

        if self.clock.is_running() {
            if self.expected != Some(now) {
                // A jump: pick up from here, playing nothing that was skipped.
                self.next = self.clicks.partition_point(|c| c.frame < now);
            }
            self.expected = Some(now + frames as u64);
        } else {
            self.expected = None;
        }
        let running = self.expected.is_some();

        for frame in 0..frames {
            let at = now + frame as u64;
            while running && let Some(click) = self.clicks.get(self.next).filter(|c| c.frame <= at) {
                self.accented = click.accent;
                self.envelope.note_on();
                // A fresh phase every time, so the attack is the same blip
                // whether the last click's tail has finished or not.
                self.osc.reset();
                self.next += 1;
            }
            let freq = if self.accented { ACCENT_HZ } else { NORMAL_HZ };
            let level = if self.accented { ACCENT_LEVEL } else { NORMAL_LEVEL };
            let sample = self.osc.next_sample(freq) * self.envelope.next_value() * level;
            for channel in 0..channels {
                buffer.channel_mut(channel)[frame] += sample;
            }
        }
    }

    fn reset(&mut self) {
        self.envelope.reset();
        self.osc.reset();
        self.next = 0;
        self.expected = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Transport, TransportCommand, transport_channel};

    const SR: f32 = 48_000.0;

    fn buffer(len: usize) -> Vec<f32> {
        vec![0.0; len]
    }

    fn process(metronome: &mut Metronome, frames: usize) -> Vec<f32> {
        let mut data = buffer(frames);
        metronome.process(&mut AudioBuffer::new(&mut data, 1));
        data
    }

    fn playing(clicks: Vec<Click>) -> (Metronome, Transport) {
        let (mut control, transport) = transport_channel(SR, 8);
        control.send(TransportCommand::Play);
        (Metronome::new(clicks, Arc::clone(transport.clock()), SR), transport)
    }

    #[test]
    fn a_click_sounds_at_its_frame_and_not_before() {
        let (mut metronome, mut transport) = playing(vec![Click { frame: 100, accent: false }]);
        transport.begin();
        let out = process(&mut metronome, 200);
        transport.finish(&mut vec![1.0; 200], 1);
        assert!(out[..100].iter().all(|s| *s == 0.0), "the click sounded early");
        assert!(out[100..].iter().any(|s| *s != 0.0), "no click at its frame");
    }

    #[test]
    fn nothing_sounds_while_the_transport_is_stopped() {
        let (_control, mut transport) = transport_channel(SR, 8);
        let mut metronome = Metronome::new(vec![Click { frame: 0, accent: false }], Arc::clone(transport.clock()), SR);
        transport.begin();
        let out = process(&mut metronome, 64);
        assert!(out.iter().all(|s| *s == 0.0), "the click sounded on a stopped transport");
    }

    #[test]
    fn an_accent_is_louder_and_higher_than_a_plain_click() {
        let peak = |accent: bool| {
            let (mut metronome, mut transport) = playing(vec![Click { frame: 0, accent }]);
            transport.begin();
            let out = process(&mut metronome, 64);
            transport.finish(&mut vec![1.0; 64], 1);
            out.iter().fold(0.0f32, |m, s| m.max(s.abs()))
        };
        assert!(peak(true) > peak(false), "the accent was not louder");
    }

    #[test]
    fn a_jump_plays_only_what_lies_ahead_of_it() {
        let (mut control, mut transport) = transport_channel(SR, 8);
        let mut metronome = Metronome::new(
            vec![Click { frame: 100, accent: false }, Click { frame: 9_500, accent: false }],
            Arc::clone(transport.clock()),
            SR,
        );
        // A seek before play lands at once: starting at 9000 is well past the
        // first click and short of the second. Without jump detection the
        // sequencer's cursor would still be at the first click and fire it
        // late, since nothing else stops a frame in the past being "due".
        control.send(TransportCommand::Seek(9_000));
        control.send(TransportCommand::Play);
        transport.begin();
        let out = process(&mut metronome, 1_000);
        transport.finish(&mut vec![1.0; 1_000], 1);
        assert!(out[..500].iter().all(|s| *s == 0.0), "a click skipped past by the seek still sounded");
        assert!(out[500..].iter().any(|s| *s != 0.0), "the click ahead of the seek never sounded");
    }

    #[test]
    fn the_click_dies_away_and_does_not_ring_into_the_next_beat() {
        let (mut metronome, mut transport) = playing(vec![Click { frame: 0, accent: false }]);
        transport.begin();
        let out = process(&mut metronome, (SR * 0.5) as usize);
        transport.finish(&mut vec![1.0; out.len()], 1);
        assert!(out[out.len() - 1] == 0.0, "the click was still ringing half a second on");
    }
}
