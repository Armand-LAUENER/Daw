//! Notes arriving from a keyboard or controller, in real time.
//!
//! The port's callback runs on a thread the MIDI backend owns, not the audio
//! thread and not the control thread. It pushes straight into the synth's
//! queue, which is why it must be the **only** thing doing so: that queue is
//! single-producer, and a score being pumped at the same time would be a second
//! writer. A caller plays live or plays a score, never both into one synth.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use engine_core::{NoteEvent, NoteSender};
use midir::{MidiInput as Backend, MidiInputConnection};
use midly::MidiMessage;
use midly::live::LiveEvent;

use crate::MidiError;

/// What this client calls itself to the system.
const CLIENT: &str = "Daw";

/// The names of the MIDI input ports, in the order the system reports them.
pub fn list_inputs() -> Result<Vec<String>, MidiError> {
    let backend = Backend::new(CLIENT).map_err(|e| MidiError::Backend(e.to_string()))?;
    backend.ports().iter().map(|port| backend.port_name(port).map_err(|e| MidiError::Backend(e.to_string()))).collect()
}

/// An open MIDI input. Dropping it closes the port and stops the notes.
pub struct MidiInput {
    name: String,
    dropped: Arc<AtomicU64>,
    /// Held only to keep the port open; the work happens in its callback.
    _connection: MidiInputConnection<()>,
}

impl MidiInput {
    /// The port that was opened, as the system named it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Notes refused because the synth's queue was full.
    ///
    /// Worth reporting rather than swallowing: it means the audio thread is not
    /// draining, and on a keyboard that shows up as a stuck or missing note.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Opens an input and feeds `sender` from it.
///
/// `port` selects by substring, case-insensitively; without one, the first port
/// is used. `sender` must be the only producer for that queue.
pub fn connect(port: Option<&str>, mut sender: NoteSender) -> Result<MidiInput, MidiError> {
    let backend = Backend::new(CLIENT).map_err(|e| MidiError::Backend(e.to_string()))?;
    let ports = backend.ports();
    if ports.is_empty() {
        return Err(MidiError::NoInput("no MIDI input ports are connected".into()));
    }

    let chosen = match port {
        None => ports[0].clone(),
        Some(wanted) => {
            let needle = wanted.to_lowercase();
            ports
                .iter()
                .find(|p| backend.port_name(p).is_ok_and(|n| n.to_lowercase().contains(&needle)))
                .cloned()
                .ok_or_else(|| MidiError::NoInput(format!("no MIDI input matches {wanted:?}")))?
        }
    };
    let name = backend.port_name(&chosen).map_err(|e| MidiError::Backend(e.to_string()))?;

    let dropped = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&dropped);
    let connection = backend
        .connect(
            &chosen,
            "daw-in",
            move |_timestamp, bytes, ()| {
                if let Some(event) = to_note(bytes)
                    && sender.send(event).is_err()
                {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            },
            (),
        )
        .map_err(|e| MidiError::Backend(e.to_string()))?;

    Ok(MidiInput { name, dropped, _connection: connection })
}

/// A wire message as a note, or nothing if it is not one.
///
/// Scheduled for frame zero, which is always in the past, so the queue applies
/// it at the start of the next block. There is nothing better to aim at: the
/// key has already been pressed, so the only question is how soon it can be
/// heard, and that is the device's buffer either way.
fn to_note(bytes: &[u8]) -> Option<NoteEvent> {
    let LiveEvent::Midi { message, .. } = LiveEvent::parse(bytes).ok()? else {
        return None;
    };
    match message {
        // A note on with no velocity is a note off; plenty of keyboards never
        // send a real one.
        MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => {
            Some(NoteEvent::on(key.as_int(), f32::from(vel.as_int()) / 127.0, 0))
        }
        MidiMessage::NoteOff { key, .. } | MidiMessage::NoteOn { key, .. } => Some(NoteEvent::off(key.as_int(), 0)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::NoteAction;

    #[test]
    fn a_note_on_becomes_a_note_on() {
        // 0x90 is note on, channel 1.
        let event = to_note(&[0x90, 60, 100]).expect("a note on");
        assert_eq!(event.pitch, 60);
        assert!(event.is_on());
        let NoteAction::On { velocity } = event.action else { panic!("expected a note on") };
        assert!((velocity - 100.0 / 127.0).abs() < 1e-6);
    }

    #[test]
    fn a_note_off_becomes_a_note_off() {
        let event = to_note(&[0x80, 60, 0]).expect("a note off");
        assert_eq!(event.pitch, 60);
        assert!(!event.is_on());
    }

    #[test]
    fn a_note_on_with_no_velocity_is_a_note_off() {
        // What most keyboards actually send when a key comes back up.
        let event = to_note(&[0x90, 60, 0]).expect("a note off in disguise");
        assert!(!event.is_on());
    }

    #[test]
    fn a_live_note_is_due_immediately() {
        // Frame zero is always overdue, so the queue releases it at the start
        // of the next block rather than holding it.
        assert_eq!(to_note(&[0x90, 60, 100]).expect("a note on").frame, 0);
    }

    #[test]
    fn anything_that_is_not_a_note_is_ignored() {
        // Clock, control change, aftertouch, and plain rubbish.
        for bytes in [&[0xF8][..], &[0xB0, 7, 100], &[0xD0, 64], &[0x00]] {
            assert!(to_note(bytes).is_none(), "{bytes:?} was taken for a note");
        }
    }
}
