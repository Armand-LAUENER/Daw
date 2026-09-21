use crate::Project;
use crate::command::{Command, CommandError, Edit, apply, resolve};

/// One entry in the history: what was done, and what puts it back.
#[derive(Debug, Clone)]
struct Step {
    forward: Edit,
    backward: Edit,
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
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts from a project that already exists, with nothing to undo.
    pub fn from_project(project: Project) -> Self {
        Self { project, steps: Vec::new(), applied: 0 }
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Carries out a command, and remembers how to take it back.
    ///
    /// A command that cannot be carried out changes nothing and records
    /// nothing, so it never leaves an undo that undoes nothing.
    pub fn apply(&mut self, command: Command) -> Result<(), CommandError> {
        let (forward, backward) = resolve(&self.project, command)?;
        // Anything that was redoable is now a branch nobody can reach.
        self.steps.truncate(self.applied);
        apply(&mut self.project, &forward);
        self.steps.push(Step { forward, backward });
        self.applied += 1;
        Ok(())
    }

    /// Takes back the last change. False when there is nothing to take back.
    pub fn undo(&mut self) -> bool {
        let Some(index) = self.applied.checked_sub(1) else {
            return false;
        };
        apply(&mut self.project, &self.steps[index].backward);
        self.applied = index;
        true
    }

    /// Puts back the last undone change. False when there is none.
    pub fn redo(&mut self) -> bool {
        let Some(step) = self.steps.get(self.applied) else {
            return false;
        };
        apply(&mut self.project, &step.forward);
        self.applied += 1;
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
    use crate::{Command, CommandError, TrackId};

    /// A session with `n` tracks and its history already cleared, so a test
    /// starts from a known project rather than from an empty one.
    fn with_tracks(n: usize) -> Session {
        let mut session = Session::new();
        for i in 0..n {
            session.apply(Command::AddTrack { name: format!("track {i}") }).expect("adding always works");
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
        session.apply(Command::AddTrack { name: "one".into() }).unwrap();
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
}
