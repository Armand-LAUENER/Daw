//! Files dropped onto the window, turned into material for the project.

use engine_midi::MidiScore;
use project_model::{Meter, NewClip, Note, PPQ, Tick};

/// A MIDI file's notes as a clip, in the project's resolution.
///
/// The file's ticks are rescaled to 960 per quarter — never through seconds,
/// so the file's own tempo does not come along: the notes are beats, and the
/// project's tempo decides how fast they go. Start and end are each rounded,
/// and the length taken between them, so notes that touched in the file still
/// touch. The clip runs to the end of the bar the last note ends in.
///
/// Every channel is merged into the one clip. `None` for a file with no notes.
pub fn midi_clip(score: &MidiScore, meter: Meter) -> Option<NewClip> {
    let tpq = u64::from(score.ticks_per_quarter.max(1));
    // Rounded to the nearest, in integers.
    let scale = |ticks: u64| (u128::from(ticks) * u128::from(PPQ) + u128::from(tpq) / 2) / u128::from(tpq);
    let scale = |ticks: u64| u64::try_from(scale(ticks)).unwrap_or(u64::MAX);

    let notes: Vec<Note> = score
        .notes
        .iter()
        .map(|n| {
            let (start, end) = (scale(n.start), scale(n.start + n.length));
            let velocity = (n.velocity * 127.0).round().clamp(1.0, 127.0) as u8;
            Note::new(n.pitch.min(127), velocity, Tick(start), Tick(end.saturating_sub(start).max(1)))
        })
        .collect();
    let last = notes.iter().map(|n| n.end().0).max()?;
    let bar = meter.bar_length().0;
    Some(NewClip::Midi { length: Tick(last.div_ceil(bar).max(1) * bar), notes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_midi::MidiNote;

    fn score(tpq: u16, notes: &[(u8, u64, u64)]) -> MidiScore {
        let notes: Vec<MidiNote> = notes
            .iter()
            .map(|&(pitch, start, length)| MidiNote { channel: 0, pitch, velocity: 100.0 / 127.0, start, length })
            .collect();
        let length = notes.iter().map(|n| n.start + n.length).max().unwrap_or(0);
        MidiScore { ticks_per_quarter: tpq, notes, length }
    }

    fn notes(clip: &NewClip) -> Vec<(u8, u64, u64)> {
        let NewClip::Midi { notes, .. } = clip else { panic!("not a MIDI clip") };
        notes.iter().map(|n| (n.pitch, n.start.0, n.length.0)).collect()
    }

    #[test]
    fn a_files_quarter_is_the_projects_quarter() {
        let clip = midi_clip(&score(480, &[(60, 0, 480), (64, 480, 240)]), Meter::default()).unwrap();
        assert_eq!(notes(&clip), vec![(60, 0, 960), (64, 960, 480)]);
        let NewClip::Midi { length, .. } = clip else { unreachable!() };
        assert_eq!(length, Tick(3_840), "the clip should run to the end of the bar");
    }

    #[test]
    fn notes_that_touched_still_touch_after_rescaling() {
        // 7 PPQ does not divide 960 evenly, so every edge is rounded.
        let clip = midi_clip(&score(7, &[(60, 0, 3), (62, 3, 4)]), Meter::default()).unwrap();
        let n = notes(&clip);
        assert_eq!(n[0].1 + n[0].2, n[1].1, "a gap or an overlap appeared between touching notes");
        assert!(n.iter().all(|&(_, _, length)| length > 0));
    }

    #[test]
    fn velocity_comes_back_as_the_files_value() {
        let clip = midi_clip(&score(480, &[(60, 0, 480)]), Meter::default()).unwrap();
        let NewClip::Midi { notes, .. } = clip else { unreachable!() };
        assert_eq!(notes[0].velocity, 100);
    }

    #[test]
    fn a_file_with_no_notes_makes_no_clip() {
        assert!(midi_clip(&score(480, &[]), Meter::default()).is_none());
    }

    #[test]
    fn the_clip_is_long_enough_for_every_note_it_holds() {
        let clip = midi_clip(&score(480, &[(60, 0, 480 * 9)]), Meter::default()).unwrap();
        let NewClip::Midi { length, notes } = clip else { unreachable!() };
        assert_eq!(length, Tick(3_840 * 3));
        assert!(notes.iter().all(|n| n.end() <= length));
    }
}
