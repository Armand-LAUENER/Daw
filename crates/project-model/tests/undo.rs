//! Undo, on sequences nobody thought to write by hand.
//!
//! The phase's criterion is that every action has an undo restoring the exact
//! previous state. Written as examples that is a promise; written as a property
//! it is a fact, checked against sequences a person would not think of.
//!
//! This is also what makes reversible edits safe to prefer over snapshots.
//! Storing a copy of the project before every change would be trivially
//! correct and grow without bound; inverses are cheap but every new command is
//! a new chance to write the reverse wrongly. That risk is paid for here, once,
//! rather than discovered later as quietly corrupted work.

use project_model::{Asset, AssetId, Command, Meter, Note, Project, Session, Source, Tempo, Tick, Window};
use proptest::prelude::*;

/// An edit aimed at a track by position, since the ids are not known until the
/// sequence runs.
#[derive(Debug, Clone)]
enum Action {
    Add,
    Remove(u8),
    Rename(u8, String),
    Gain(u8, f32),
    Pan(u8, f32),
    Mute(u8, bool),
    Solo(u8, bool),
    Master(f32),
    AddWithFile(u8),
    /// On a track: one of a handful of files, at a start in beats.
    AddAudio(u8, u8, u8),
    AddMidi(u8, u8, u8),
    Clear(u8),
    /// On a placement: to a track, at a start.
    Move(u8, u8, u8),
    Window(u8, u8, u8),
    Duplicate(u8, u8, u8, bool),
    Unique(u8),
    RemovePlacement(u8),
    /// On a clip.
    ClipLength(u8, u8),
    AddNote(u8, u8, u8, u8),
    SetNote(u8, u8, u8, u8),
    RemoveNote(u8, u8),
    /// Thousandths of a BPM, over a range wider than the valid one so refused
    /// tempos are exercised too.
    Tempo(u32),
    Meter(u8, u8),
}

/// One of four files, by content. Few enough that tracks keep landing on the
/// same one, which is the case the pool has to get right.
fn asset(n: u8) -> Asset {
    let n = n % 4;
    Asset::new(AssetId::of_bytes(&[n]), format!("/audio/{n}.wav"))
}

fn action() -> impl Strategy<Value = Action> {
    prop_oneof![
        // Weighted towards adding, or most sequences would be edits aimed at
        // an empty project and prove nothing.
        3 => Just(Action::Add),
        1 => any::<u8>().prop_map(Action::Remove),
        1 => (any::<u8>(), "[a-z ]{0,12}").prop_map(|(i, n)| Action::Rename(i, n)),
        2 => (any::<u8>(), -4.0f32..4.0).prop_map(|(i, g)| Action::Gain(i, g)),
        2 => (any::<u8>(), -4.0f32..4.0).prop_map(|(i, p)| Action::Pan(i, p)),
        1 => (any::<u8>(), any::<bool>()).prop_map(|(i, m)| Action::Mute(i, m)),
        1 => (any::<u8>(), any::<bool>()).prop_map(|(i, s)| Action::Solo(i, s)),
        1 => (-4.0f32..4.0).prop_map(Action::Master),
        2 => any::<u8>().prop_map(Action::AddWithFile),
        2 => any::<(u8, u8, u8)>().prop_map(|(t, f, s)| Action::AddAudio(t, f, s)),
        2 => any::<(u8, u8, u8)>().prop_map(|(t, s, l)| Action::AddMidi(t, s, l)),
        1 => any::<u8>().prop_map(Action::Clear),
        2 => any::<(u8, u8, u8)>().prop_map(|(p, t, s)| Action::Move(p, t, s)),
        1 => any::<(u8, u8, u8)>().prop_map(|(p, o, l)| Action::Window(p, o, l)),
        2 => any::<(u8, u8, u8, bool)>().prop_map(|(p, t, s, l)| Action::Duplicate(p, t, s, l)),
        1 => any::<u8>().prop_map(Action::Unique),
        1 => any::<u8>().prop_map(Action::RemovePlacement),
        1 => any::<(u8, u8)>().prop_map(|(c, l)| Action::ClipLength(c, l)),
        3 => any::<(u8, u8, u8, u8)>().prop_map(|(c, p, s, l)| Action::AddNote(c, p, s, l)),
        2 => any::<(u8, u8, u8, u8)>().prop_map(|(c, n, s, l)| Action::SetNote(c, n, s, l)),
        1 => any::<(u8, u8)>().prop_map(|(c, n)| Action::RemoveNote(c, n)),
        1 => (0u32..1_200_000).prop_map(Action::Tempo),
        1 => (0u8..40, 0u8..40).prop_map(|(b, u)| Action::Meter(b, u)),
    ]
}

/// Turns an action into a command against whatever the project holds now.
///
/// None when there is no track to aim at, which is not a failure: an empty
/// project is a state the sequence is allowed to reach.
fn to_command(project: &Project, action: &Action) -> Option<Command> {
    let pick = |i: u8| {
        let count = project.track_count();
        (count > 0).then(|| project.tracks().nth(usize::from(i) % count).expect("in range").id)
    };
    let placement = |i: u8| {
        let count = project.placements().len();
        (count > 0).then(|| project.placements().nth(usize::from(i) % count).expect("in range").id)
    };
    let clip = |i: u8| {
        let count = project.clips().len();
        (count > 0).then(|| project.clips().nth(usize::from(i) % count).expect("in range").id)
    };
    // Sizes in quarters of a beat and positions in beats, small enough that
    // edits collide with each other and with the ends of clips.
    let beats = |n: u8| Tick(u64::from(n % 16) * 960);
    let quarters = |n: u8| Tick(u64::from(n % 32) * 240);
    // A note on some existing note of the clip, when it has one.
    let note_of = |c, i: u8| {
        let notes = project.clip(c)?.notes()?;
        (!notes.is_empty()).then(|| *notes.keys().nth(usize::from(i) % notes.len()).expect("in range"))
    };
    Some(match action {
        Action::Add => Command::AddTrack { name: "track".into(), source: Default::default(), file: None },
        Action::Remove(i) => Command::RemoveTrack { id: pick(*i)? },
        Action::Rename(i, name) => Command::RenameTrack { id: pick(*i)?, name: name.clone() },
        Action::Gain(i, gain) => Command::SetGain { id: pick(*i)?, gain: *gain },
        Action::Pan(i, pan) => Command::SetPan { id: pick(*i)?, pan: *pan },
        Action::Mute(i, muted) => Command::SetMuted { id: pick(*i)?, muted: *muted },
        Action::Solo(i, soloed) => Command::SetSoloed { id: pick(*i)?, soloed: *soloed },
        Action::Master(gain) => Command::SetMasterGain { gain: *gain },
        Action::AddAudio(t, f, s) => Command::AddAudioClip { track: pick(*t)?, asset: asset(*f), start: beats(*s) },
        Action::AddMidi(t, s, l) => {
            Command::AddMidiClip { track: pick(*t)?, name: "midi".into(), start: beats(*s), length: quarters(*l) }
        }
        Action::Clear(t) => Command::ClearTrack { id: pick(*t)? },
        Action::Move(p, t, s) => Command::MovePlacement { id: placement(*p)?, track: pick(*t)?, start: beats(*s) },
        Action::Window(p, o, l) => {
            let window = (*l != 0).then(|| Window { offset: quarters(*o), length: quarters(*l) });
            Command::SetWindow { id: placement(*p)?, window }
        }
        Action::Duplicate(p, t, s, linked) => {
            Command::DuplicatePlacement { id: placement(*p)?, track: pick(*t)?, start: beats(*s), linked: *linked }
        }
        Action::Unique(p) => Command::MakeUnique { id: placement(*p)? },
        Action::RemovePlacement(p) => Command::RemovePlacement { id: placement(*p)? },
        Action::ClipLength(c, l) => Command::SetClipLength { id: clip(*c)?, length: quarters(*l) },
        Action::AddNote(c, pitch, s, l) => {
            Command::AddNote { clip: clip(*c)?, note: Note::new(pitch % 128, 100, quarters(*s), quarters(*l)) }
        }
        Action::SetNote(c, n, s, l) => {
            let c = clip(*c)?;
            Command::SetNote { clip: c, id: note_of(c, *n)?, note: Note::new(60, 80, quarters(*s), quarters(*l)) }
        }
        Action::RemoveNote(c, n) => {
            let c = clip(*c)?;
            Command::RemoveNote { clip: c, id: note_of(c, *n)? }
        }
        // Built without the checked constructors, through serde, so an
        // out-of-range value reaches the command and has to be refused there.
        Action::Tempo(millis) => {
            let tempo: Tempo = serde_json::from_value(serde_json::json!(millis)).ok()?;
            Command::SetTempo { tempo }
        }
        Action::Meter(beats, unit) => {
            let meter: Meter = serde_json::from_value(serde_json::json!({ "beats": beats, "unit": unit })).ok()?;
            Command::SetMeter { meter }
        }
        Action::AddWithFile(n) => {
            Command::AddTrack { name: "file".into(), source: Source::Clips, file: Some(asset(*n)) }
        }
    })
}

proptest! {
    #[test]
    fn undoing_step_by_step_retraces_every_state(actions in prop::collection::vec(action(), 0..40)) {
        let mut session = Session::new();
        // The project as it stood before each change that actually happened.
        let mut before: Vec<Project> = Vec::new();

        // An add up front, so no case can pass by having edited nothing: a
        // sequence aimed at an empty project would undo an empty history and
        // look perfectly correct.
        let actions: Vec<Action> = std::iter::once(Action::Add).chain(actions).collect();
        for action in &actions {
            let Some(command) = to_command(session.project(), action) else { continue };
            let snapshot = session.project().clone();
            let depth = session.depth();
            // A change that continues the gesture already in progress folds
            // into it, so it is not a state undo will ever land on. Only a
            // change that made a new step gets a snapshot — which makes this
            // a check on the folding too.
            if session.apply(command).is_ok() && session.depth() > depth {
                before.push(snapshot);
            }
        }
        prop_assert_eq!(session.depth(), before.len());
        prop_assert!(!before.is_empty(), "nothing was applied, so nothing was proved");

        // Walked back one at a time rather than only checking the start: an
        // inverse that is wrong in the middle and wrong again in the opposite
        // direction would cancel out and pass an end-to-end check.
        while let Some(expected) = before.pop() {
            prop_assert!(session.undo());
            prop_assert_eq!(session.project(), &expected, "undo landed on the wrong state");
        }
        prop_assert!(!session.can_undo());
        prop_assert_eq!(session.project(), &Project::new());
    }

    #[test]
    fn redoing_everything_returns_to_where_undo_began(actions in prop::collection::vec(action(), 0..40)) {
        let mut session = Session::new();
        let actions: Vec<Action> = std::iter::once(Action::Add).chain(actions).collect();
        for action in &actions {
            if let Some(command) = to_command(session.project(), action) {
                let _ = session.apply(command);
            }
        }
        let finished = session.project().clone();
        let depth = session.depth();
        prop_assert!(depth > 0, "nothing was applied, so nothing was proved");

        while session.undo() {}
        prop_assert_eq!(session.depth(), 0);

        while session.redo() {}
        prop_assert_eq!(session.depth(), depth);
        prop_assert_eq!(session.project(), &finished, "redo did not arrive back where it started");
    }

    #[test]
    fn a_project_is_never_left_holding_a_number_that_is_not_one(actions in prop::collection::vec(action(), 0..40)) {
        // A NaN gain would survive into a saved file and silence the output
        // again tomorrow. Refusing it is only useful if nothing else can put
        // one there.
        let mut session = Session::new();
        for action in &actions {
            if let Some(command) = to_command(session.project(), action) {
                let _ = session.apply(command);
            }
            let project = session.project();
            prop_assert!(project.master_gain().is_finite());
            for track in project.tracks() {
                prop_assert!(track.gain.is_finite(), "a track ended up with gain {}", track.gain);
                prop_assert!((-1.0..=1.0).contains(&track.pan), "pan {} left the range", track.pan);
            }
        }
    }

    #[test]
    fn identifiers_stay_unique_however_tracks_come_and_go(actions in prop::collection::vec(action(), 0..40)) {
        // Ids come from a counter that undo never walks back, so they cannot
        // collide however the history is travelled.
        let mut session = Session::new();
        for action in &actions {
            if let Some(command) = to_command(session.project(), action) {
                let _ = session.apply(command);
            }
            let mut ids: Vec<_> = session.project().tracks().map(|t| t.id).collect();
            let total = ids.len();
            ids.sort_unstable();
            ids.dedup();
            prop_assert_eq!(ids.len(), total, "two tracks shared an id");
        }

        // And they must still be unique after the history is walked back and
        // forth, which is where a reused id would actually collide.
        while session.undo() {}
        while session.redo() {}
        let mut ids: Vec<_> = session.project().tracks().map(|t| t.id).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        prop_assert_eq!(ids.len(), total);
    }
}

proptest! {
    #[test]
    fn no_sequence_of_commands_breaks_a_promise(actions in prop::collection::vec(action(), 0..60)) {
        // The whole-project check, after every change and at every point of
        // the history: an order out of step with the tracks, a file a track
        // names but the pool lost, or one the pool kept for nobody.
        let mut session = Session::new();
        for action in &actions {
            if let Some(command) = to_command(session.project(), action) {
                let _ = session.apply(command);
            }
            prop_assert_eq!(session.check(), Ok(()));
        }
        while session.undo() {
            prop_assert_eq!(session.check(), Ok(()));
        }
        while session.redo() {
            prop_assert_eq!(session.check(), Ok(()));
        }
    }
}
