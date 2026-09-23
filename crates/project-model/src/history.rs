use serde::{Deserialize, Serialize};

use crate::command::{Command, CommandError, Edit, Target, apply, resolve};
use crate::{BusId, Clip, ClipId, Invalid, NoteId, PlacementId, Project, TrackId};

/// The next identifier of each kind, handed out once and never again.
///
/// Kept beside the project rather than in it, and never walked back by undo
/// (decision D2). Undoing an added track and adding another must not give the
/// second the first one's id: a saved file, or anything else that saw the
/// first, would take the stranger for it. Out of the project, it also stays out
/// of every comparison of two projects — a gap in the numbering is not a
/// difference in the music.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) struct Ids {
    track: u32,
    clip: u32,
    placement: u32,
    note: u32,
    /// Absent from files written before buses existed: none had been made.
    #[serde(default)]
    bus: u32,
}

impl Ids {
    /// The smallest counters that cannot collide with what a project holds.
    pub(crate) fn clearing(project: &Project) -> Self {
        let next = |ids: &mut dyn Iterator<Item = u32>| ids.map(|id| id + 1).max().unwrap_or(0);
        Self {
            track: next(&mut project.tracks().map(|t| t.id.0)),
            clip: next(&mut project.clips().map(|c| c.id.0)),
            placement: next(&mut project.placements().map(|p| p.id.0)),
            note: next(&mut project.clips().filter_map(Clip::notes).flat_map(|n| n.keys().map(|id| id.0))),
            bus: next(&mut project.buses().map(|b| b.id.0)),
        }
    }

    /// Whichever is further on, counter by counter, so a counter from a file
    /// edited by hand can never hand out an id the project already uses.
    pub(crate) fn max(self, other: Self) -> Self {
        Self {
            track: self.track.max(other.track),
            clip: self.clip.max(other.clip),
            placement: self.placement.max(other.placement),
            note: self.note.max(other.note),
            bus: self.bus.max(other.bus),
        }
    }

    pub(crate) fn take_track(&mut self) -> TrackId {
        TrackId(take(&mut self.track))
    }

    pub(crate) fn take_clip(&mut self) -> ClipId {
        ClipId(take(&mut self.clip))
    }

    pub(crate) fn take_placement(&mut self) -> PlacementId {
        PlacementId(take(&mut self.placement))
    }

    pub(crate) fn take_bus(&mut self) -> BusId {
        BusId(take(&mut self.bus))
    }

    pub(crate) fn take_note(&mut self) -> NoteId {
        NoteId(take(&mut self.note))
    }

    /// Whether every id the project holds is one these counters handed out.
    pub(crate) fn check(&self, project: &Project) -> Result<(), Invalid> {
        let clearing = Self::clearing(project);
        let ahead = [
            ("track", clearing.track, self.track),
            ("clip", clearing.clip, self.clip),
            ("placement", clearing.placement, self.placement),
            ("note", clearing.note, self.note),
            ("bus", clearing.bus, self.bus),
        ];
        match ahead.into_iter().find(|(_, needed, have)| needed > have) {
            Some((what, needed, _)) => Err(Invalid::IdAhead(what, needed - 1)),
            None => Ok(()),
        }
    }
}

fn take(counter: &mut u32) -> u32 {
    let id = *counter;
    *counter += 1;
    id
}

/// One entry in the history: what was done, and what puts it back.
#[derive(Debug, Clone)]
struct Step {
    forward: Vec<Edit>,
    backward: Vec<Edit>,
    /// The control it moved, when a following change could continue it.
    target: Option<Target>,
    /// Names the project as this step leaves it. Never reused, and renewed
    /// when a gesture continues the step, since the project changed again.
    serial: u64,
}

/// A project together with everything done to it.
///
/// The project can be read from anywhere, but only changed through
/// [`Session::apply`], so there is no way to make a change the history does not
/// know about — which is the only way undo can be trusted.
#[derive(Debug, Clone, Default)]
pub struct Session {
    project: Project,
    ids: Ids,
    steps: Vec<Step>,
    /// How many steps are currently in effect. The rest are redoable.
    applied: usize,
    /// Whether the last step can still absorb a change that continues it.
    ///
    /// A fader dragged across the screen arrives as a change every frame. Left
    /// alone that is sixty entries for one gesture, and an undo that walks back
    /// through the drag frame by frame. They fold into one step until something
    /// [`seals`](Session::seal) it — which is what letting go of the mouse does.
    open: bool,
    /// The serial the next step, or the next continuation of one, is given.
    next_serial: u64,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts from a project that already exists, with nothing to undo.
    pub fn from_project(project: Project) -> Self {
        let ids = Ids::clearing(&project);
        Self { project, ids, steps: Vec::new(), applied: 0, open: false, next_serial: 0 }
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Checks the project's promises, and that the id counter is ahead of
    /// every id in it.
    pub fn check(&self) -> Result<(), Invalid> {
        self.project.check()?;
        self.ids.check(&self.project)
    }

    pub(crate) fn ids(&self) -> Ids {
        self.ids
    }

    /// A session opened from a file: its project, and the counter the file
    /// kept, so ids retired before it was saved stay retired.
    pub(crate) fn restored(project: Project, ids: Ids) -> Self {
        let ids = ids.max(Ids::clearing(&project));
        Self { project, ids, steps: Vec::new(), applied: 0, open: false, next_serial: 0 }
    }

    /// Carries out a command, and remembers how to take it back.
    ///
    /// A command that cannot be carried out changes nothing and records
    /// nothing, so it never leaves an undo that undoes nothing.
    pub fn apply(&mut self, command: Command) -> Result<(), CommandError> {
        let target = command.target();
        let (forward, backward) = resolve(&self.project, &mut self.ids, command)?;

        // A change continuing the gesture already in progress replaces its
        // forward edit and keeps its backward one — which is where the gesture
        // began, and so where undo has to land.
        if self.open
            && target.is_some()
            && self.applied == self.steps.len()
            && let Some(last) = self.steps.last_mut()
            && last.target == target
        {
            apply(&mut self.project, &forward);
            last.forward = forward;
            self.next_serial += 1;
            last.serial = self.next_serial;
            return Ok(());
        }

        // Anything that was redoable is now a branch nobody can reach.
        self.steps.truncate(self.applied);
        apply(&mut self.project, &forward);
        self.next_serial += 1;
        self.steps.push(Step { forward, backward, target, serial: self.next_serial });
        self.applied += 1;
        self.open = target.is_some();
        Ok(())
    }

    /// Ends the gesture in progress, so the next change starts a new step.
    ///
    /// The interface calls this when a control is let go. Anything that does
    /// not drag never needs it.
    pub fn seal(&mut self) {
        self.open = false;
    }

    /// Takes back the last change. False when there is nothing to take back.
    pub fn undo(&mut self) -> bool {
        let Some(index) = self.applied.checked_sub(1) else {
            return false;
        };
        apply(&mut self.project, &self.steps[index].backward);
        self.applied = index;
        // Whatever gesture was in progress is over; a change now is a new one.
        self.open = false;
        true
    }

    /// Puts back the last undone change. False when there is none.
    pub fn redo(&mut self) -> bool {
        let Some(step) = self.steps.get(self.applied) else {
            return false;
        };
        apply(&mut self.project, &step.forward);
        self.applied += 1;
        self.open = false;
        true
    }

    pub fn can_undo(&self) -> bool {
        self.applied > 0
    }

    pub fn can_redo(&self) -> bool {
        self.applied < self.steps.len()
    }

    /// How many changes are in effect, for a status line or a test.
    pub fn depth(&self) -> usize {
        self.applied
    }

    /// Which state the project is in, as a number: equal to one taken before
    /// only if the project is exactly as it was then.
    ///
    /// Remembered at a save and compared later, it says whether there are
    /// changes to save. Undoing back to the saved state makes the two equal
    /// again; changing, undoing and changing something else does not, since
    /// serials are never reused.
    pub fn revision(&self) -> u64 {
        self.applied.checked_sub(1).map_or(0, |i| self.steps[i].serial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Command, CommandError, Source, TrackId};

    /// A session with `n` tracks and its history already cleared, so a test
    /// starts from a known project rather than from an empty one.
    fn with_tracks(n: usize) -> Session {
        let mut session = Session::new();
        for i in 0..n {
            session
                .apply(Command::AddTrack { name: format!("track {i}"), source: Source::default(), clip: None })
                .expect("adding always works");
        }
        Session::from_project(session.project().clone())
    }

    fn first(session: &Session) -> TrackId {
        session.project().tracks().next().unwrap().id
    }

    #[test]
    fn a_new_session_has_nothing_to_undo() {
        let mut session = Session::new();
        assert!(!session.can_undo() && !session.can_redo());
        assert!(!session.undo(), "undoing nothing should say so");
        assert!(!session.redo());
    }

    #[test]
    fn undo_puts_a_value_back_exactly() {
        let mut session = with_tracks(1);
        let id = first(&session);
        let before = session.project().clone();

        session.apply(Command::SetGain { id, gain: 0.25 }).unwrap();
        assert_eq!(session.project().track(id).unwrap().gain, 0.25);

        assert!(session.undo());
        assert_eq!(session.project(), &before, "undo did not restore the project");
    }

    #[test]
    fn a_removed_track_comes_back_where_it_was() {
        // Not at the end: a track restored in the wrong place would look
        // undone while quietly reordering the mixer.
        let mut session = with_tracks(3);
        let middle = session.project().tracks().nth(1).unwrap().id;
        let before = session.project().clone();

        session.apply(Command::RemoveTrack { id: middle }).unwrap();
        assert_eq!(session.project().track_count(), 2);

        assert!(session.undo());
        assert_eq!(session.project(), &before);
        assert_eq!(session.project().tracks().nth(1).unwrap().id, middle, "it came back in the wrong place");
    }

    #[test]
    fn a_redone_add_keeps_the_same_identifier() {
        // Replaying the command instead of the resolved edit would mint a new
        // id, and every later step naming the old one would miss.
        let mut session = Session::new();
        session.apply(Command::AddTrack { name: "one".into(), source: Default::default(), clip: None }).unwrap();
        let id = first(&session);

        assert!(session.undo());
        assert!(session.redo());
        assert_eq!(first(&session), id, "the track came back as a different track");
    }

    #[test]
    fn an_id_is_never_handed_out_twice() {
        // Undoing an add and adding again must not give the new track the old
        // one's number: a saved file, or a reference taken in between, would
        // mistake the stranger for it (decision D2).
        let mut session = Session::new();
        session.apply(Command::AddTrack { name: "one".into(), source: Default::default(), clip: None }).unwrap();
        let undone = first(&session);
        assert!(session.undo());
        session.apply(Command::AddTrack { name: "two".into(), source: Default::default(), clip: None }).unwrap();
        assert_ne!(first(&session), undone, "an undone track's id was handed out again");

        // Nor after the highest track is removed, which is where deriving the
        // next id from what exists would reuse it.
        let last = first(&session);
        session.apply(Command::RemoveTrack { id: last }).unwrap();
        session.apply(Command::AddTrack { name: "three".into(), source: Default::default(), clip: None }).unwrap();
        assert_ne!(first(&session), last);
    }

    #[test]
    fn the_revision_says_whether_the_project_is_as_it_was() {
        let mut session = with_tracks(1);
        let id = first(&session);
        let saved = session.revision();

        session.apply(Command::SetGain { id, gain: 0.5 }).unwrap();
        assert_ne!(session.revision(), saved, "a change left the revision alone");
        // Undone back to the saved state: the same revision again.
        session.undo();
        assert_eq!(session.revision(), saved, "undoing to the saved state still looked changed");

        // A change, saved; then a drag continuing it. The step is the same,
        // the project is not, so the revision moves.
        session.apply(Command::SetGain { id, gain: 0.5 }).unwrap();
        let saved = session.revision();
        session.apply(Command::SetGain { id, gain: 0.4 }).unwrap();
        assert_eq!(session.depth(), 1);
        assert_ne!(session.revision(), saved, "a continued gesture looked unchanged");

        // A different change on another branch never matches an old revision.
        session.undo();
        session.apply(Command::SetPan { id, pan: 0.2 }).unwrap();
        assert_ne!(session.revision(), saved);

        // A refused command changes nothing, the revision included.
        let before = session.revision();
        assert!(session.apply(Command::SetGain { id: TrackId(999), gain: 0.5 }).is_err());
        assert_eq!(session.revision(), before);
    }

    #[test]
    fn a_refused_command_leaves_no_trace() {
        let mut session = with_tracks(1);
        let before = session.project().clone();

        let missing = TrackId(999);
        assert_eq!(session.apply(Command::SetGain { id: missing, gain: 0.5 }), Err(CommandError::NoSuchTrack(missing)));
        assert_eq!(session.project(), &before);
        assert!(!session.can_undo(), "a refused command took a slot in the history");
    }

    #[test]
    fn a_gain_that_is_not_a_number_is_refused() {
        let mut session = with_tracks(1);
        let id = first(&session);
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(session.apply(Command::SetGain { id, gain: bad }).is_err(), "{bad} was accepted");
        }
        assert_eq!(session.project().track(id).unwrap().gain, 1.0);
    }

    #[test]
    fn a_new_change_drops_what_could_have_been_redone() {
        let mut session = with_tracks(1);
        let id = first(&session);
        session.apply(Command::SetGain { id, gain: 0.5 }).unwrap();
        session.undo();
        assert!(session.can_redo());

        session.apply(Command::SetPan { id, pan: 0.5 }).unwrap();
        assert!(!session.can_redo(), "a branch nobody can reach was kept");
        assert_eq!(session.project().track(id).unwrap().gain, 1.0);
    }

    #[test]
    fn a_pan_outside_the_range_is_stored_as_what_took_effect() {
        let mut session = with_tracks(1);
        let id = first(&session);
        session.apply(Command::SetPan { id, pan: 4.0 }).unwrap();
        assert_eq!(session.project().track(id).unwrap().pan, 1.0);
        // Redo has to land on the same value, not clamp a second time from 4.0.
        session.undo();
        session.redo();
        assert_eq!(session.project().track(id).unwrap().pan, 1.0);
    }

    #[test]
    fn solo_is_resolved_across_the_project_not_per_track() {
        // A track cannot answer "am I heard" alone; that is the whole reason
        // the graph has a solo bus.
        let mut session = with_tracks(2);
        let (a, b) = (first(&session), session.project().tracks().nth(1).unwrap().id);
        assert!(session.project().is_audible(a) && session.project().is_audible(b));

        session.apply(Command::SetSoloed { id: a, soloed: true }).unwrap();
        assert!(session.project().is_audible(a));
        assert!(!session.project().is_audible(b), "soloing one should silence the other");

        session.apply(Command::SetMuted { id: a, muted: true }).unwrap();
        assert!(!session.project().is_audible(a), "a muted track stays silent when soloed");
    }

    #[test]
    fn a_drag_folds_into_one_step() {
        // Sixty changes a second, one gesture. Without this an undo walks back
        // through the drag frame by frame.
        let mut session = with_tracks(1);
        let id = first(&session);
        let before = session.project().clone();

        for step in 1..=60 {
            session.apply(Command::SetGain { id, gain: step as f32 / 60.0 }).unwrap();
        }
        assert_eq!(session.depth(), 1, "the drag left {} steps", session.depth());
        assert_eq!(session.project().track(id).unwrap().gain, 1.0);

        assert!(session.undo());
        assert_eq!(session.project(), &before, "undo landed inside the drag instead of before it");
    }

    #[test]
    fn letting_go_ends_the_gesture() {
        let mut session = with_tracks(1);
        let id = first(&session);
        session.apply(Command::SetGain { id, gain: 0.5 }).unwrap();
        session.seal();
        session.apply(Command::SetGain { id, gain: 0.25 }).unwrap();
        assert_eq!(session.depth(), 2, "the second gesture folded into the first");

        session.undo();
        assert_eq!(session.project().track(id).unwrap().gain, 0.5, "undo skipped a gesture");
    }

    #[test]
    fn two_faders_never_fold_together() {
        let mut session = with_tracks(2);
        let (a, b) = (first(&session), session.project().tracks().nth(1).unwrap().id);
        session.apply(Command::SetGain { id: a, gain: 0.5 }).unwrap();
        session.apply(Command::SetGain { id: b, gain: 0.5 }).unwrap();
        assert_eq!(session.depth(), 2);
    }

    #[test]
    fn gain_and_pan_on_one_track_are_separate_gestures() {
        let mut session = with_tracks(1);
        let id = first(&session);
        session.apply(Command::SetGain { id, gain: 0.5 }).unwrap();
        session.apply(Command::SetPan { id, pan: 0.5 }).unwrap();
        assert_eq!(session.depth(), 2);
    }

    #[test]
    fn a_switch_is_never_folded_away() {
        // Muting twice is two decisions, and both belong in the history.
        let mut session = with_tracks(1);
        let id = first(&session);
        session.apply(Command::SetMuted { id, muted: true }).unwrap();
        session.apply(Command::SetMuted { id, muted: false }).unwrap();
        assert_eq!(session.depth(), 2);
    }

    #[test]
    fn a_loop_region_drag_folds_into_one_step_and_the_switches_do_not() {
        use crate::{LoopRegion, Tick};

        let mut session = with_tracks(0);
        let before = session.project().clone();
        for end in 1..=10u64 {
            let region = Some(LoopRegion { start: Tick(0), end: Tick(end * 960) });
            session.apply(Command::SetLoopRegion { region }).unwrap();
        }
        assert_eq!(session.depth(), 1, "resizing the loop left {} steps", session.depth());
        assert!(session.undo());
        assert_eq!(session.project(), &before);

        // Turning the loop and the click on and off are decisions, not a drag.
        session.apply(Command::SetLoopEnabled { enabled: true }).unwrap();
        session.apply(Command::SetLoopEnabled { enabled: false }).unwrap();
        session.apply(Command::SetMetronome { enabled: true }).unwrap();
        assert_eq!(session.depth(), 3);
    }

    #[test]
    fn a_loop_region_that_does_not_end_after_it_starts_is_refused() {
        use crate::{LoopRegion, Tick};

        let mut session = with_tracks(0);
        let region = Some(LoopRegion { start: Tick(960), end: Tick(960) });
        assert_eq!(session.apply(Command::SetLoopRegion { region }), Err(CommandError::OutOfRange("a loop region")));
        let backwards = Some(LoopRegion { start: Tick(960), end: Tick(0) });
        assert!(session.apply(Command::SetLoopRegion { region: backwards }).is_err());
    }

    #[test]
    fn undoing_ends_the_gesture_so_a_later_change_is_its_own_step() {
        let mut session = with_tracks(1);
        let id = first(&session);
        session.apply(Command::SetGain { id, gain: 0.5 }).unwrap();
        session.undo();
        session.apply(Command::SetGain { id, gain: 0.25 }).unwrap();
        assert_eq!(session.depth(), 1);
        assert!(session.undo());
        assert_eq!(session.project().track(id).unwrap().gain, 1.0);
    }
}
