use crate::Project;
use crate::command::{Command, CommandError, Edit, Target, apply, resolve};

/// One entry in the history: what was done, and what puts it back.
#[derive(Debug, Clone)]
struct Step {
    forward: Edit,
    backward: Edit,
    /// The control it moved, when a following change could continue it.
    target: Option<Target>,
}

/// A project together with everything done to it.
///
/// The project can be read from anywhere, but only changed through
/// [`Session::apply`], so there is no way to make a change the history does not
/// know about — which is the only way undo can be trusted.
#[derive(Debug, Clone, Default)]
pub struct Session {
    project: Project,
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
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts from a project that already exists, with nothing to undo.
    pub fn from_project(project: Project) -> Self {
        Self { project, steps: Vec::new(), applied: 0, open: false }
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Carries out a command, and remembers how to take it back.
    ///
    /// A command that cannot be carried out changes nothing and records
    /// nothing, so it never leaves an undo that undoes nothing.
    pub fn apply(&mut self, command: Command) -> Result<(), CommandError> {
        let target = command.target();
        let (forward, backward) = resolve(&self.project, command)?;

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
            return Ok(());
        }

        // Anything that was redoable is now a branch nobody can reach.
        self.steps.truncate(self.applied);
        apply(&mut self.project, &forward);
        self.steps.push(Step { forward, backward, target });
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
                .apply(Command::AddTrack { name: format!("track {i}"), source: Source::default() })
                .expect("adding always works");
        }
        Session::from_project(session.project().clone())
    }

    fn first(session: &Session) -> TrackId {
        session.project().tracks[0].id
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
        let middle = session.project().tracks[1].id;
        let before = session.project().clone();

        session.apply(Command::RemoveTrack { id: middle }).unwrap();
        assert_eq!(session.project().tracks.len(), 2);

        assert!(session.undo());
        assert_eq!(session.project(), &before);
        assert_eq!(session.project().tracks[1].id, middle, "it came back in the wrong place");
    }

    #[test]
    fn a_redone_add_keeps_the_same_identifier() {
        // Replaying the command instead of the resolved edit would mint a new
        // id, and every later step naming the old one would miss.
        let mut session = Session::new();
        session.apply(Command::AddTrack { name: "one".into(), source: Default::default() }).unwrap();
        let id = first(&session);

        assert!(session.undo());
        assert!(session.redo());
        assert_eq!(first(&session), id, "the track came back as a different track");
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
        let (a, b) = (session.project().tracks[0].id, session.project().tracks[1].id);
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
        let (a, b) = (session.project().tracks[0].id, session.project().tracks[1].id);
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
