//! Clips and placements, held to what decision D3 promises.
//!
//! The properties in `undo.rs` prove that no sequence of edits breaks the
//! project. These pin down what the edits *mean*: a linked clip is one clip
//! wherever it is placed, a copy is a separate one, and material leaves the
//! project only when nothing plays it any more.

use project_model::{
    Asset, AssetId, ClipId, Command, CommandError, NewClip, Note, PlacementId, Session, Source, Tick, TrackId,
};

const BAR: Tick = Tick(3_840);

fn with_track() -> (Session, TrackId) {
    let mut session = Session::new();
    session.apply(Command::AddTrack { name: "t".into(), source: Source::Clips, clip: None }).expect("adding works");
    let id = session.project().tracks().next().expect("one track").id;
    (session, id)
}

/// A one-bar MIDI clip on `track` at the start, and its placement.
fn midi_clip(session: &mut Session, track: TrackId) -> (ClipId, PlacementId) {
    session
        .apply(Command::AddMidiClip { track, name: "m".into(), start: Tick(0), length: BAR })
        .expect("the track exists");
    let placement = *session.project().placements_on(track).last().expect("just placed");
    (placement.clip, placement.id)
}

fn notes_of(session: &Session, clip: ClipId) -> usize {
    session.project().clip(clip).and_then(|c| c.notes()).map_or(0, |n| n.len())
}

#[test]
fn a_linked_placement_plays_the_same_clip() {
    let (mut session, track) = with_track();
    let (clip, first) = midi_clip(&mut session, track);
    session.apply(Command::DuplicatePlacement { id: first, track, start: BAR, linked: true }).unwrap();

    let on = session.project().placements_on(track);
    assert_eq!(on.len(), 2);
    assert!(on.iter().all(|p| p.clip == clip), "a linked duplicate got a clip of its own");
    assert_eq!(session.project().clips().len(), 1);

    // An edit to the clip is an edit to both placements, because there is one.
    session.apply(Command::AddNote { clip, note: Note::new(60, 100, Tick(0), Tick(960)) }).unwrap();
    assert_eq!(notes_of(&session, clip), 1);
}

#[test]
fn a_copy_is_a_separate_clip_with_separate_notes() {
    let (mut session, track) = with_track();
    let (clip, first) = midi_clip(&mut session, track);
    session.apply(Command::AddNote { clip, note: Note::new(60, 100, Tick(0), Tick(960)) }).unwrap();
    session.apply(Command::DuplicatePlacement { id: first, track, start: BAR, linked: false }).unwrap();

    let copy = session.project().placements_on(track)[1].clip;
    assert_ne!(copy, clip, "a copy shares the original's clip");
    assert_eq!(notes_of(&session, copy), 1, "the copy lost its notes");
    let keys = |c: ClipId| session.project().clip(c).unwrap().notes().unwrap().keys().copied().collect::<Vec<_>>();
    assert_ne!(keys(clip), keys(copy), "the copy's notes are the same notes under another clip");

    // Editing one leaves the other alone.
    session.apply(Command::AddNote { clip: copy, note: Note::new(67, 100, Tick(960), Tick(960)) }).unwrap();
    assert_eq!((notes_of(&session, clip), notes_of(&session, copy)), (1, 2));
}

#[test]
fn making_a_linked_placement_unique_splits_it_off() {
    let (mut session, track) = with_track();
    let (clip, first) = midi_clip(&mut session, track);
    session.apply(Command::DuplicatePlacement { id: first, track, start: BAR, linked: true }).unwrap();
    let second = session.project().placements_on(track)[1].id;

    session.apply(Command::MakeUnique { id: second }).unwrap();
    let own = session.project().placement(second).unwrap().clip;
    assert_ne!(own, clip);
    assert_eq!(session.project().placement(first).unwrap().clip, clip, "the other placement was moved too");

    // Now it has its own clip, there is nothing left to split.
    assert_eq!(session.apply(Command::MakeUnique { id: second }), Err(CommandError::Unchanged));
}

#[test]
fn a_clip_leaves_with_its_last_placement_and_a_file_with_its_last_clip() {
    let (mut session, track) = with_track();
    let kick = Asset::new(AssetId::of_bytes(b"kick"), "/a/kick.wav");
    session.apply(Command::AddAudioClip { track, asset: kick.clone(), start: Tick(0) }).unwrap();
    session.apply(Command::AddAudioClip { track, asset: kick, start: BAR }).unwrap();
    let on: Vec<_> = session.project().placements_on(track).iter().map(|p| p.id).collect();
    assert_eq!(session.project().assets().len(), 1, "one file placed twice is one asset");
    assert_eq!(session.project().clips().len(), 2);

    session.apply(Command::RemovePlacement { id: on[0] }).unwrap();
    assert_eq!(session.project().clips().len(), 1);
    assert_eq!(session.project().assets().len(), 1, "the file left while a clip still played it");

    session.apply(Command::RemovePlacement { id: on[1] }).unwrap();
    assert_eq!(session.project().clips().len(), 0);
    assert_eq!(session.project().assets().len(), 0, "a file nothing plays was kept");

    // And one undo brings each back whole.
    session.undo();
    assert_eq!((session.project().clips().len(), session.project().assets().len()), (1, 1));
}

#[test]
fn removing_a_track_takes_its_placements_and_undo_brings_them_back() {
    let (mut session, track) = with_track();
    let (clip, first) = midi_clip(&mut session, track);
    session.apply(Command::DuplicatePlacement { id: first, track, start: BAR, linked: true }).unwrap();
    session.apply(Command::AddNote { clip, note: Note::new(60, 100, Tick(0), Tick(960)) }).unwrap();
    let before = session.project().clone();

    session.apply(Command::RemoveTrack { id: track }).unwrap();
    assert_eq!(session.project().placements().len(), 0);
    assert_eq!(session.project().clips().len(), 0, "a clip outlived every placement of it");
    session.check().unwrap();

    session.undo();
    assert_eq!(session.project(), &before, "the track came back without all of its material");
}

#[test]
fn a_note_cannot_leave_its_clip_and_a_clip_cannot_shrink_under_its_notes() {
    let (mut session, track) = with_track();
    let (clip, _) = midi_clip(&mut session, track);
    let past_the_end = Note::new(60, 100, Tick(3_000), Tick(960));
    assert_eq!(session.apply(Command::AddNote { clip, note: past_the_end }), Err(CommandError::OutOfRange("a note")));
    let silent = Note::new(60, 0, Tick(0), Tick(960));
    assert!(session.apply(Command::AddNote { clip, note: silent }).is_err(), "velocity 0 is a note-off, not a note");

    session.apply(Command::AddNote { clip, note: Note::new(60, 100, Tick(2_880), Tick(960)) }).unwrap();
    let shorter = Command::SetClipLength { id: clip, length: Tick(1_920) };
    assert!(session.apply(shorter).is_err(), "the clip shrank and left a note outside it");
}

#[test]
fn an_audio_clip_has_no_notes_and_no_window() {
    let (mut session, track) = with_track();
    let asset = Asset::new(AssetId::of_bytes(b"x"), "/a/x.wav");
    session.apply(Command::AddAudioClip { track, asset, start: Tick(0) }).unwrap();
    let placement = *session.project().placements_on(track)[0];

    let note = Note::new(60, 100, Tick(0), Tick(1));
    assert_eq!(
        session.apply(Command::AddNote { clip: placement.clip, note }),
        Err(CommandError::NotMidi(placement.clip))
    );
    let window = Some(project_model::Window { offset: Tick(0), length: Tick(960) });
    assert!(session.apply(Command::SetWindow { id: placement.id, window }).is_err());
}

#[test]
fn a_dragged_placement_is_one_step() {
    let (mut session, track) = with_track();
    let (_, placement) = midi_clip(&mut session, track);
    let depth = session.depth();
    for beat in 1..=16u64 {
        session.apply(Command::MovePlacement { id: placement, track, start: Tick(beat * 960) }).unwrap();
    }
    assert_eq!(session.depth(), depth + 1, "a drag left a step per frame");
    session.undo();
    assert_eq!(session.project().placement(placement).unwrap().start, Tick(0));
}

#[test]
fn a_track_made_with_its_notes_is_one_step_and_refused_whole() {
    // What dropping a MIDI file does: the track, its clip and every note in
    // one command, so one undo takes it all back.
    let mut session = Session::new();
    let notes = vec![Note::new(60, 100, Tick(0), Tick(960)), Note::new(64, 100, Tick(960), Tick(960))];
    let clip = Some(NewClip::Midi { length: BAR, notes });
    session.apply(Command::AddTrack { name: "song".into(), source: Source::Clips, clip }).unwrap();
    assert_eq!(session.depth(), 1);
    let track = session.project().tracks().next().unwrap().id;
    let placed = session.project().placements_on(track)[0].clip;
    assert_eq!(notes_of(&session, placed), 2);

    session.undo();
    assert_eq!(session.project(), &project_model::Project::new());

    // One note too long and none of it happens.
    let overlong = vec![Note::new(60, 100, Tick(0), Tick(9_999))];
    let clip = Some(NewClip::Midi { length: BAR, notes: overlong });
    assert!(session.apply(Command::AddTrack { name: "bad".into(), source: Source::Clips, clip }).is_err());
    assert_eq!(session.project().track_count(), 0);
}

#[test]
fn trimming_the_front_keeps_the_notes_where_they_were_in_time() {
    let (mut session, track) = with_track();
    let (clip, placement) = midi_clip(&mut session, track);
    session.apply(Command::AddNote { clip, note: Note::new(60, 100, Tick(1_920), Tick(960)) }).unwrap();
    let window = Some(project_model::Window { offset: Tick(960), length: Tick(2_880) });
    session.apply(Command::TrimPlacement { id: placement, start: Tick(960), window, audio: None }).unwrap();
    let p = *session.project().placement(placement).unwrap();
    // The note is 1 920 into the clip; the window starts 960 in, placed at 960:
    // it still sounds at 960 + (1 920 - 960) = 1 920 on the timeline.
    assert_eq!(p.start.0 + 1_920 - p.window.unwrap().offset.0, 1_920);

    // A trim and then a trim again is one gesture.
    let depth = session.depth();
    let window = Some(project_model::Window { offset: Tick(0), length: Tick(1_920) });
    session.apply(Command::TrimPlacement { id: placement, start: Tick(0), window, audio: None }).unwrap();
    assert_eq!(session.depth(), depth, "a trim continuing the same gesture took a new step");

    let outside = Some(project_model::Window { offset: Tick(3_000), length: Tick(960) });
    assert!(
        session.apply(Command::TrimPlacement { id: placement, start: Tick(0), window: outside, audio: None }).is_err()
    );
}

#[test]
fn an_audio_window_goes_on_audio_clips_only() {
    let (mut session, track) = with_track();
    let asset = Asset::new(AssetId::of_bytes(b"x"), "/a/x.wav");
    session.apply(Command::AddAudioClip { track, asset, start: Tick(0) }).unwrap();
    let audio_placement = session.project().placements_on(track)[0].id;
    let (_, midi_placement) = midi_clip(&mut session, track);

    let audio = Some(project_model::AudioWindow { offset: 4_800, length: 48_000, rate: 48_000 });
    session.apply(Command::TrimPlacement { id: audio_placement, start: Tick(960), window: None, audio }).unwrap();
    assert_eq!(session.project().placement(audio_placement).unwrap().audio, audio);
    session.check().unwrap();

    assert!(
        session.apply(Command::TrimPlacement { id: midi_placement, start: Tick(0), window: None, audio }).is_err(),
        "a MIDI clip was given an audio window"
    );
    let empty = Some(project_model::AudioWindow { offset: 0, length: 0, rate: 48_000 });
    let refused = Command::TrimPlacement { id: audio_placement, start: Tick(0), window: None, audio: empty };
    assert!(session.apply(refused).is_err());
}

#[test]
fn an_audio_window_means_the_same_moment_at_any_rate() {
    let window = project_model::AudioWindow { offset: 44_100, length: 22_050, rate: 44_100 };
    assert_eq!(window.at(48_000), (48_000, 24_000));
    assert_eq!(window.at(44_100), (44_100, 22_050));
}
