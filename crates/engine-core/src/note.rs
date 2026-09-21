use crate::{Scheduled, event_channel};

/// Concert A, the reference every other pitch is derived from.
const A4_HZ: f32 = 440.0;
/// MIDI note number of concert A.
const A4_PITCH: f32 = 69.0;

/// The frequency of a MIDI note number, in equal temperament.
pub fn pitch_to_hz(pitch: u8) -> f32 {
    A4_HZ * ((f32::from(pitch) - A4_PITCH) / 12.0).exp2()
}

/// What a note event does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NoteAction {
    /// Velocity in `0..=1`, scaling how loud the note is.
    On {
        velocity: f32,
    },
    Off,
}

/// A note starting or stopping, on the engine's timeline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteEvent {
    pub pitch: u8,
    pub action: NoteAction,
    pub frame: u64,
}

impl NoteEvent {
    pub fn on(pitch: u8, velocity: f32, frame: u64) -> Self {
        Self { pitch, action: NoteAction::On { velocity: velocity.clamp(0.0, 1.0) }, frame }
    }

    pub fn off(pitch: u8, frame: u64) -> Self {
        Self { pitch, action: NoteAction::Off, frame }
    }

    pub fn is_on(&self) -> bool {
        matches!(self.action, NoteAction::On { .. })
    }
}

impl Scheduled for NoteEvent {
    fn frame(&self) -> u64 {
        self.frame
    }
}

/// The control side: schedules notes.
pub type NoteSender = crate::EventSender<NoteEvent>;
/// The audio thread's side: hands over notes as their frame comes up.
pub type NoteQueue = crate::EventQueue<NoteEvent>;

/// Opens a one-way lane for notes.
pub fn note_channel(capacity: usize) -> (NoteSender, NoteQueue) {
    event_channel(capacity)
}

/// A note as written down: where it starts and how long it lasts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Note {
    pub pitch: u8,
    pub velocity: f32,
    pub start: u64,
    pub length: u64,
}

impl Note {
    pub fn new(pitch: u8, velocity: f32, start: u64, length: u64) -> Self {
        Self { pitch, velocity, start, length }
    }

    pub fn end(&self) -> u64 {
        self.start + self.length
    }
}

/// Notes held in memory, as a sequencer would.
///
/// Here so that scheduling can be tested without a MIDI file or a keyboard
/// plugged in: what the synth has to get right is that a note starts on the
/// sample it was written for, and that is the same problem whatever wrote it.
///
/// Lives on the control side, so it may allocate and sort freely; what crosses
/// to the audio thread is the flat list of events it produces.
#[derive(Debug, Clone, Default)]
pub struct PianoRoll {
    notes: Vec<Note>,
}

impl PianoRoll {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, note: Note) -> &mut Self {
        self.notes.push(note);
        self
    }

    pub fn notes(&self) -> &[Note] {
        &self.notes
    }

    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    /// The frame the last note finishes on.
    pub fn duration(&self) -> u64 {
        self.notes.iter().map(Note::end).max().unwrap_or(0)
    }

    /// Every start and stop, in the order the audio thread should receive them.
    ///
    /// A stop sorts before a start on the same frame. Otherwise a note repeated
    /// back to back would take a second voice while the first was still held,
    /// and on a full synth would steal one from something else.
    pub fn events(&self) -> Vec<NoteEvent> {
        let mut events = Vec::with_capacity(self.notes.len() * 2);
        for note in &self.notes {
            events.push(NoteEvent::on(note.pitch, note.velocity, note.start));
            events.push(NoteEvent::off(note.pitch, note.end()));
        }
        events.sort_by_key(|e| (e.frame, e.is_on()));
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concert_a_is_where_it_should_be() {
        assert!((pitch_to_hz(69) - 440.0).abs() < 1e-3);
        assert!((pitch_to_hz(57) - 220.0).abs() < 1e-3, "an octave down should halve");
        assert!((pitch_to_hz(81) - 880.0).abs() < 1e-3, "an octave up should double");
    }

    #[test]
    fn middle_c_is_where_a_tuner_says_it_is() {
        assert!((pitch_to_hz(60) - 261.6256).abs() < 1e-2, "got {}", pitch_to_hz(60));
    }

    #[test]
    fn a_velocity_outside_the_range_is_brought_back_into_it() {
        let NoteAction::On { velocity } = NoteEvent::on(60, 9.0, 0).action else {
            panic!("expected a note on");
        };
        assert_eq!(velocity, 1.0);
    }

    #[test]
    fn a_note_becomes_a_start_and_a_stop() {
        let mut roll = PianoRoll::new();
        roll.add(Note::new(60, 0.8, 100, 50));
        let events = roll.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], NoteEvent::on(60, 0.8, 100));
        assert_eq!(events[1], NoteEvent::off(60, 150));
    }

    #[test]
    fn events_come_out_in_frame_order_whatever_order_they_went_in() {
        let mut roll = PianoRoll::new();
        roll.add(Note::new(64, 1.0, 500, 100)).add(Note::new(60, 1.0, 0, 100)).add(Note::new(67, 1.0, 200, 100));
        let frames: Vec<u64> = roll.events().iter().map(|e| e.frame).collect();
        assert!(frames.windows(2).all(|w| w[0] <= w[1]), "{frames:?}");
    }

    #[test]
    fn a_stop_sorts_before_a_start_on_the_same_frame() {
        // A note repeated back to back would otherwise hold two voices at the
        // seam, and on a full synth would steal one from something else.
        let mut roll = PianoRoll::new();
        roll.add(Note::new(60, 1.0, 0, 100)).add(Note::new(60, 1.0, 100, 100));
        let at_seam: Vec<bool> = roll.events().iter().filter(|e| e.frame == 100).map(NoteEvent::is_on).collect();
        assert_eq!(at_seam, vec![false, true]);
    }

    #[test]
    fn an_empty_roll_has_nothing_to_play() {
        let roll = PianoRoll::new();
        assert!(roll.is_empty());
        assert_eq!(roll.duration(), 0);
        assert!(roll.events().is_empty());
    }

    #[test]
    fn the_duration_reaches_the_end_of_the_last_note() {
        let mut roll = PianoRoll::new();
        roll.add(Note::new(60, 1.0, 0, 480)).add(Note::new(64, 1.0, 100, 90));
        assert_eq!(roll.duration(), 480);
    }
}
