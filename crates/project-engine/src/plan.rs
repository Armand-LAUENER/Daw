//! What a track plays, turned into frames on the timeline.
//!
//! The model says *which clip, where, in ticks*; the engine needs *what, when,
//! in frames*. This is that translation, done once on the control side when a
//! graph is built, so the audio thread only ever walks a sorted list.
//!
//! Frames here are timeline frames — the transport's clock — not the engine's
//! running counter: a plan stays right however long the program has been open
//! and wherever playback was started from.

use engine_core::NoteEvent;
use project_model::{AssetId, Content, Project, Tick, TrackId};

use crate::Timing;

/// One audio clip on the timeline, in frames at the engine's rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSpan {
    pub asset: AssetId,
    /// Where on the timeline it starts.
    pub start: u64,
    /// How far into the file it starts playing.
    pub offset: u64,
    /// How much of the file it plays; `None` to the end.
    pub length: Option<u64>,
}

/// Each audio clip on a track, earliest first: where it starts, and which part
/// of its file it plays, converted to the engine's rate.
pub fn audio_spans(project: &Project, track: TrackId, timing: Timing) -> Vec<AudioSpan> {
    project
        .placements_on(track)
        .into_iter()
        .filter_map(|p| {
            let asset = project.clip(p.clip)?.asset()?;
            let (offset, length) = match p.audio {
                Some(window) => {
                    let (offset, length) = window.at(timing.sample_rate);
                    (offset, Some(length))
                }
                None => (0, None),
            };
            Some(AudioSpan { asset, start: timing.frame_of(p.start), offset, length })
        })
        .collect()
}

/// Every note a track's MIDI clips play, as note-ons and note-offs on the
/// timeline, in the order they happen.
///
/// A placement plays its window of the clip: notes wholly outside it are left
/// out, and a note crossing an edge is cut there, so a trimmed clip sounds
/// exactly like what is shown. A linked clip placed twice is simply read twice.
///
/// At the same frame, offs come before ons. A note repeated back to back on
/// one pitch then ends and restarts, instead of the new note-on being cut off
/// by the old note's off.
pub fn note_plan(project: &Project, track: TrackId, timing: Timing) -> Vec<NoteEvent> {
    let mut events = Vec::new();
    for placement in project.placements_on(track) {
        let Some(clip) = project.clip(placement.clip) else { continue };
        let Content::Midi { length, notes } = &clip.content else { continue };
        let (offset, span) = placement.window.map_or((0, length.0), |w| (w.offset.0, w.length.0));
        let (from, to) = (offset, offset + span);

        for note in notes.values() {
            let start = note.start.0.max(from);
            let end = note.end().0.min(to);
            if start >= end {
                continue;
            }
            // From clip time to timeline time: where the placement starts,
            // plus how far into the window the note is.
            let at = |tick: u64| timing.frame_of(Tick(placement.start.0 + tick - from));
            let velocity = f32::from(note.velocity) / 127.0;
            events.push(NoteEvent::on(note.pitch, velocity, at(start)));
            events.push(NoteEvent::off(note.pitch, at(end)));
        }
    }
    // Stable, so two notes starting together keep the clip's order.
    events.sort_by_key(|e| (e.frame, e.is_on()));
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use project_model::{Command, Note, Session, Source, Tempo, Window};

    const BAR: Tick = Tick(3_840);
    /// 120 BPM at 48 kHz: a beat is 24 000 frames, a bar 96 000.
    fn timing() -> Timing {
        Timing::new(Tempo::default(), 48_000)
    }

    fn session_with_clip(notes: &[Note]) -> (Session, TrackId) {
        let mut session = Session::new();
        session.apply(Command::AddTrack { name: "t".into(), source: Source::Clips, clip: None }).unwrap();
        let track = session.project().tracks().next().unwrap().id;
        session.apply(Command::AddMidiClip { track, name: "m".into(), start: BAR, length: BAR }).unwrap();
        let clip = session.project().placements_on(track)[0].clip;
        for note in notes {
            session.apply(Command::AddNote { clip, note: *note }).unwrap();
        }
        (session, track)
    }

    fn frames(events: &[NoteEvent]) -> Vec<(u64, bool)> {
        events.iter().map(|e| (e.frame, e.is_on())).collect()
    }

    #[test]
    fn a_note_lands_where_its_clip_is_placed() {
        // The clip starts at bar 2; its note starts a beat in.
        let (session, track) = session_with_clip(&[Note::new(60, 127, Tick(960), Tick(960))]);
        let plan = note_plan(session.project(), track, timing());
        assert_eq!(frames(&plan), vec![(96_000 + 24_000, true), (96_000 + 48_000, false)]);
        assert!(matches!(plan[0].action, engine_core::NoteAction::On { velocity } if velocity == 1.0));
    }

    #[test]
    fn a_linked_clip_placed_twice_plays_twice() {
        let (mut session, track) = session_with_clip(&[Note::new(60, 100, Tick(0), Tick(960))]);
        let first = session.project().placements_on(track)[0].id;
        session.apply(Command::DuplicatePlacement { id: first, track, start: Tick(BAR.0 * 3), linked: true }).unwrap();
        let plan = note_plan(session.project(), track, timing());
        let ons: Vec<u64> = plan.iter().filter(|e| e.is_on()).map(|e| e.frame).collect();
        assert_eq!(ons, vec![96_000, 288_000]);
    }

    #[test]
    fn a_window_cuts_notes_at_its_edges() {
        // Notes on beats 1, 2 and 3; the window keeps from the middle of beat
        // 1 to the middle of beat 3.
        let notes: Vec<Note> = (0..3u8).map(|b| Note::new(60 + b, 100, Tick(u64::from(b) * 960), Tick(960))).collect();
        let (mut session, track) = session_with_clip(&notes);
        let id = session.project().placements_on(track)[0].id;
        let window = Some(Window { offset: Tick(480), length: Tick(1_920) });
        session.apply(Command::SetWindow { id, window }).unwrap();

        let plan = note_plan(session.project(), track, timing());
        // Placed at bar 2 (96 000); the window's start is what lands there.
        assert_eq!(
            frames(&plan),
            vec![
                (96_000, true),
                (108_000, false),
                (108_000, true),
                (132_000, false),
                (132_000, true),
                (144_000, false)
            ]
        );
    }

    #[test]
    fn a_repeated_note_ends_before_it_starts_again() {
        let notes = [Note::new(60, 100, Tick(0), Tick(960)), Note::new(60, 100, Tick(960), Tick(960))];
        let (session, track) = session_with_clip(&notes);
        let plan = note_plan(session.project(), track, timing());
        let at_beat = &plan[1..3];
        assert_eq!(at_beat[0].frame, at_beat[1].frame);
        assert!(!at_beat[0].is_on() && at_beat[1].is_on(), "the second note was cut off by the first one's end");
    }

    #[test]
    fn audio_starts_follow_the_tempo() {
        let mut session = Session::new();
        session.apply(Command::AddTrack { name: "t".into(), source: Source::Clips, clip: None }).unwrap();
        let track = session.project().tracks().next().unwrap().id;
        let asset = project_model::Asset::new(AssetId::of_bytes(b"a"), "/a.wav");
        session.apply(Command::AddAudioClip { track, asset, start: BAR }).unwrap();

        assert_eq!(audio_spans(session.project(), track, timing())[0].start, 96_000);
        let faster = Timing::new(Tempo::from_bpm(240.0).unwrap(), 48_000);
        assert_eq!(
            audio_spans(session.project(), track, faster)[0].start,
            48_000,
            "the clip did not move with the tempo"
        );
    }

    #[test]
    fn a_trimmed_audio_clip_plays_its_part_at_the_engines_rate() {
        let mut session = Session::new();
        session.apply(Command::AddTrack { name: "t".into(), source: Source::Clips, clip: None }).unwrap();
        let track = session.project().tracks().next().unwrap().id;
        let asset = project_model::Asset::new(AssetId::of_bytes(b"a"), "/a.wav");
        session.apply(Command::AddAudioClip { track, asset, start: Tick(0) }).unwrap();
        let id = session.project().placements_on(track)[0].id;
        // Trimmed at 44.1 kHz: from half a second in, for a second.
        let audio = Some(project_model::AudioWindow { offset: 22_050, length: 44_100, rate: 44_100 });
        session.apply(Command::TrimPlacement { id, start: Tick(960), window: None, audio }).unwrap();

        // Played at 48 kHz: the same moments of the file.
        let span = audio_spans(session.project(), track, timing())[0];
        assert_eq!((span.start, span.offset, span.length), (24_000, 24_000, Some(48_000)));
    }
}
