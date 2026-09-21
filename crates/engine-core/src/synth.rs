use engine_dsp::{Adsr, Osc, Waveform};

use crate::{AudioBuffer, Node, NoteAction, NoteQueue, pitch_to_hz};

/// One note being played: an oscillator and the envelope that shapes it.
struct Voice {
    osc: Osc,
    env: Adsr,
    pitch: u8,
    velocity: f32,
    /// When this voice was last started, so the oldest can be identified.
    started: u64,
}

/// A polyphonic synth, played by scheduled notes.
///
/// Notes land on the sample they were written for. The synth produces audio one
/// sample at a time anyway, so it checks the queue as it reaches each frame
/// rather than splitting the block the way a block-at-a-time node has to. That
/// is the trap this phase exists to avoid: applying a note at the edge of the
/// callback is up to a buffer of slop, which at a fast tempo is audible as the
/// rhythm drifting.
///
/// The voices are allocated once. A note played when they are all busy steals
/// one — a released voice first, then the oldest — rather than asking the
/// allocator, which on the audio thread is not an option.
///
/// It adds to whatever reaches it instead of overwriting, so it can sit on a
/// bus with something else.
pub struct Synth {
    voices: Vec<Voice>,
    shape: Waveform,
    notes: NoteQueue,
    /// Where the next block starts on the engine's timeline.
    frame: u64,
    /// Counts note starts, which is all "oldest" has to mean here.
    age: u64,
}

impl Synth {
    /// `envelope` is the shape every voice takes; `voices` is the polyphony.
    pub fn new(voices: usize, shape: Waveform, envelope: &Adsr, notes: NoteQueue, sample_rate: f32) -> Self {
        let voices = (0..voices.max(1))
            .map(|_| Voice { osc: Osc::new(sample_rate), env: envelope.clone(), pitch: 0, velocity: 0.0, started: 0 })
            .collect();
        Self { voices, shape, notes, frame: 0, age: 0 }
    }

    /// How many voices are sounding, for metering and for tests.
    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|v| v.env.is_active()).count()
    }

    pub fn polyphony(&self) -> usize {
        self.voices.len()
    }

    fn start(&mut self, pitch: u8, velocity: f32) {
        self.age += 1;
        let index = self.pick(pitch);
        let voice = &mut self.voices[index];
        if voice.pitch != pitch {
            // A voice taken from another note starts that note's waveform from
            // the beginning; retriggering the same pitch keeps its phase, so a
            // repeated note does not restart with a click.
            voice.osc.reset();
            voice.env.reset();
            voice.pitch = pitch;
        }
        voice.velocity = velocity;
        voice.started = self.age;
        voice.env.note_on();
    }

    /// Which voice should play `pitch`, in order of what costs least to take.
    fn pick(&self, pitch: u8) -> usize {
        let oldest = |a: &(usize, &Voice), b: &(usize, &Voice)| a.1.started.cmp(&b.1.started);

        // The same note again: reuse its voice rather than stacking a second
        // one, which would double its level and hold two voices for one key.
        if let Some((i, _)) = self.voices.iter().enumerate().find(|(_, v)| v.env.is_active() && v.pitch == pitch) {
            return i;
        }
        if let Some((i, _)) = self.voices.iter().enumerate().find(|(_, v)| !v.env.is_active()) {
            return i;
        }
        // Everything is busy. A voice already fading out is the least missed.
        if let Some((i, _)) = self.voices.iter().enumerate().filter(|(_, v)| v.env.is_releasing()).min_by(oldest) {
            return i;
        }
        self.voices.iter().enumerate().min_by(oldest).map_or(0, |(i, _)| i)
    }

    fn stop(&mut self, pitch: u8) {
        // The oldest voice still holding this pitch: releasing the newest would
        // leave an older one sounding with nothing left to release it.
        let found = self
            .voices
            .iter_mut()
            .filter(|v| v.pitch == pitch && v.env.is_active() && !v.env.is_releasing())
            .min_by_key(|v| v.started);
        if let Some(voice) = found {
            voice.env.note_off();
        }
    }
}

impl Node for Synth {
    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        let (frames, channels) = (buffer.frames(), buffer.channels());

        for frame in 0..frames {
            let at = self.frame + frame as u64;
            while let Some(event) = self.notes.take_due(at) {
                match event.action {
                    NoteAction::On { velocity } => self.start(event.pitch, velocity),
                    NoteAction::Off => self.stop(event.pitch),
                }
            }

            let mut mixed = 0.0;
            for voice in &mut self.voices {
                if !voice.env.is_active() {
                    continue;
                }
                let level = voice.env.next_value() * voice.velocity;
                mixed += voice.osc.next_sample(pitch_to_hz(voice.pitch), self.shape) * level;
            }
            for channel in 0..channels {
                buffer.channel_mut(channel)[frame] += mixed;
            }
        }

        self.frame += frames as u64;
    }

    /// Where the block about to be rendered starts. The graph supplies this so
    /// that every synth in it agrees with the timeline the control thread
    /// schedules against.
    fn set_frame(&mut self, frame: u64) {
        self.frame = frame;
    }

    fn reset(&mut self) {
        for voice in &mut self.voices {
            voice.env.reset();
            voice.osc.reset();
        }
        // Notes queued for wherever playback was no longer mean anything.
        self.notes.clear();
        self.frame = 0;
        self.age = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Note, NoteEvent, NoteSender, PianoRoll, note_channel};

    const SR: f32 = 48_000.0;

    /// An envelope that is fully open a sample after the note and shuts a
    /// sample after it is released, so a test reads onsets rather than slopes.
    fn instant() -> Adsr {
        Adsr::new(0.0, 0.0, 1.0, 0.0, SR)
    }

    fn synth(voices: usize, envelope: &Adsr) -> (NoteSender, Synth) {
        let (sender, queue) = note_channel(64);
        // Saw rather than sine: a sine's first sample is zero whatever the
        // envelope does, which would hide the very thing being measured.
        (sender, Synth::new(voices, Waveform::Saw, envelope, queue, SR))
    }

    fn render(synth: &mut Synth, frames: usize, at: u64) -> Vec<f32> {
        let mut data = vec![0.0; frames];
        synth.set_frame(at);
        synth.process(&mut AudioBuffer::new(&mut data, 1));
        data
    }

    fn first_sounding(samples: &[f32]) -> Option<usize> {
        samples.iter().position(|s| *s != 0.0)
    }

    #[test]
    fn a_synth_with_nothing_to_play_is_silent() {
        let (_sender, mut synth) = synth(4, &instant());
        assert!(render(&mut synth, 256, 0).iter().all(|s| *s == 0.0));
        assert_eq!(synth.active_voices(), 0);
    }

    #[test]
    fn a_note_starts_on_the_frame_it_was_written_for() {
        // The whole point of the phase. Applying it at the edge of the block
        // instead would put it up to 256 frames out — 5 ms, which at any real
        // tempo is the rhythm drifting.
        let (mut sender, mut synth) = synth(4, &instant());
        sender.send(NoteEvent::on(60, 1.0, 100)).unwrap();

        let out = render(&mut synth, 256, 0);
        assert!(out[..100].iter().all(|s| *s == 0.0), "it sounded before its frame");
        assert_eq!(first_sounding(&out), Some(101), "the envelope opens the sample after the note");
    }

    #[test]
    fn a_note_is_placed_against_the_timeline_not_the_block() {
        let (mut sender, mut synth) = synth(4, &instant());
        sender.send(NoteEvent::on(60, 1.0, 1_000)).unwrap();

        let first = render(&mut synth, 256, 512);
        assert!(first.iter().all(|s| *s == 0.0), "a note at frame 1000 sounded during 512..768");

        let second = render(&mut synth, 256, 768);
        assert_eq!(first_sounding(&second), Some(233), "frame 1000 is 232 into this block");
    }

    #[test]
    fn several_notes_sound_at_once() {
        let (mut sender, mut synth) = synth(8, &instant());
        for pitch in [60, 64, 67] {
            sender.send(NoteEvent::on(pitch, 1.0, 0)).unwrap();
        }
        render(&mut synth, 64, 0);
        assert_eq!(synth.active_voices(), 3);
    }

    #[test]
    fn the_same_pitch_twice_keeps_one_voice() {
        // Stacking a second voice on one key would double its level and hold
        // two voices for one note.
        let (mut sender, mut synth) = synth(8, &instant());
        sender.send(NoteEvent::on(60, 1.0, 0)).unwrap();
        sender.send(NoteEvent::on(60, 1.0, 32)).unwrap();
        render(&mut synth, 64, 0);
        assert_eq!(synth.active_voices(), 1);
    }

    #[test]
    fn releasing_a_note_frees_its_voice() {
        let (mut sender, mut synth) = synth(4, &instant());
        sender.send(NoteEvent::on(60, 1.0, 0)).unwrap();
        sender.send(NoteEvent::off(60, 32)).unwrap();
        let out = render(&mut synth, 128, 0);
        assert_eq!(synth.active_voices(), 0);
        assert!(out[40..].iter().all(|s| *s == 0.0), "it kept sounding after its release");
    }

    #[test]
    fn more_notes_than_voices_steals_rather_than_dropping_one() {
        // The audio thread cannot ask for another voice, so the choice is
        // between stealing and silence. A note that simply vanished would be
        // the worse of the two.
        let (mut sender, mut synth) = synth(2, &instant());
        for (i, pitch) in [60u8, 64, 67, 72].into_iter().enumerate() {
            sender.send(NoteEvent::on(pitch, 1.0, i as u64 * 8)).unwrap();
        }
        let out = render(&mut synth, 128, 0);
        assert_eq!(synth.active_voices(), 2, "polyphony was exceeded");
        assert!(out[100..].iter().any(|s| *s != 0.0), "the newest notes were dropped");
    }

    #[test]
    fn a_released_voice_is_stolen_before_a_held_one() {
        let (mut sender, mut synth) = synth(2, &instant());
        sender.send(NoteEvent::on(60, 1.0, 0)).unwrap();
        sender.send(NoteEvent::on(64, 1.0, 0)).unwrap();
        render(&mut synth, 16, 0);
        // 60 is let go, so it is the one worth taking.
        sender.send(NoteEvent::off(60, 16)).unwrap();
        sender.send(NoteEvent::on(67, 1.0, 24)).unwrap();
        render(&mut synth, 64, 16);

        // 64 was never released, so it must still be the voice that is held.
        sender.send(NoteEvent::off(64, 80)).unwrap();
        render(&mut synth, 64, 80);
        assert_eq!(synth.active_voices(), 1, "releasing 64 should have left only 67");
    }

    #[test]
    fn a_synth_adds_to_what_reaches_it() {
        let (_sender, mut synth) = synth(4, &instant());
        let mut data = vec![0.5; 32];
        synth.process(&mut AudioBuffer::new(&mut data, 1));
        assert!(data.iter().all(|s| *s == 0.5), "the synth overwrote its input");
    }

    #[test]
    fn resetting_silences_everything_and_forgets_what_was_queued() {
        let (mut sender, mut synth) = synth(4, &instant());
        sender.send(NoteEvent::on(60, 1.0, 0)).unwrap();
        render(&mut synth, 32, 0);
        assert_eq!(synth.active_voices(), 1);

        sender.send(NoteEvent::on(64, 1.0, 64)).unwrap();
        synth.reset();
        assert_eq!(synth.active_voices(), 0);
        assert!(render(&mut synth, 256, 0).iter().all(|s| *s == 0.0), "a queued note survived the reset");
    }

    #[test]
    fn sixteenths_at_a_fast_tempo_land_on_their_own_samples() {
        // The phase's exit criterion, offline: 180 BPM in sixteenths is a note
        // every 41.7 ms. Rounding each one to a 256-frame block would move it
        // by up to 5.3 ms, which is an eighth of the gap between them.
        const BPM: f64 = 180.0;
        let frames_per_sixteenth = (60.0 / BPM / 4.0 * f64::from(SR)) as u64;

        let mut roll = PianoRoll::new();
        for step in 0..16u64 {
            let start = step * frames_per_sixteenth;
            roll.add(Note::new(60 + (step % 5) as u8 * 2, 1.0, start, frames_per_sixteenth / 2));
        }

        let (mut sender, mut synth) = synth(8, &instant());
        for event in roll.events() {
            sender.send(event).unwrap();
        }

        // Rendered in blocks that do not divide the note spacing, so a
        // block-aligned synth could not pass by luck.
        const BLOCK: usize = 256;
        let total = (roll.duration() + BLOCK as u64) as usize;
        let mut out = Vec::with_capacity(total);
        let mut at = 0u64;
        while out.len() < total {
            out.extend(render(&mut synth, BLOCK, at));
            at += BLOCK as u64;
        }

        for step in 0..16u64 {
            let start = (step * frames_per_sixteenth) as usize;
            assert_eq!(out[start], 0.0, "note {step} sounded before its frame");
            assert!(out[start + 1] != 0.0, "note {step} did not start on its frame");
        }
    }
}
