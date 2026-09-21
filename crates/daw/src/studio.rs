//! What the window does, with no window in it.
//!
//! Everything here decides *what* a change means and leaves the carrying-out to
//! an [`Engine`]. That split is not tidiness: a real engine needs an audio
//! device, which neither CI nor WSL has, so without it none of this could be
//! checked at all. The window is a view over this; this is not a view over the
//! window.

use engine_core::{Graph, Node, ParamId};
use project_engine::{Bindings, Change, compile, route};
use project_model::{Command, CommandError, Project, Session, Track};

/// Where a decided change is actually carried out.
///
/// Both operations can refuse: the queues between here and the audio thread are
/// bounded, and blocking on them would stall the interface.
pub trait Engine {
    /// Aims a parameter at the graph that is already playing.
    fn send(&mut self, param: ParamId, value: f32) -> bool;
    /// Hands over a graph to replace the one playing.
    fn swap(&mut self, graph: Box<Graph>) -> bool;
}

/// Makes the audio for a track. The model has no clips yet, so this is where
/// the question is answered until it does.
pub type SourceFor = Box<dyn FnMut(&Track) -> Box<dyn Node> + Send>;

/// The project, the history, and the engine playing it, kept in step.
pub struct Studio<E: Engine> {
    session: Session,
    bindings: Bindings,
    engine: E,
    source_for: SourceFor,
    channels: usize,
    max_frames: usize,
    sample_rate: f32,
    /// A rebuild the engine had no room for, to try again.
    owed_rebuild: bool,
    /// Rebuilds asked for and refused, worth showing rather than hiding.
    refusals: u64,
}

impl<E: Engine> Studio<E> {
    pub fn new(engine: E, source_for: SourceFor, channels: usize, max_frames: usize, sample_rate: f32) -> Self {
        let mut studio = Self {
            session: Session::new(),
            bindings: Bindings::default(),
            engine,
            source_for,
            channels,
            max_frames,
            sample_rate,
            owed_rebuild: false,
            refusals: 0,
        };
        studio.rebuild();
        studio
    }

    pub fn project(&self) -> &Project {
        self.session.project()
    }

    pub fn can_undo(&self) -> bool {
        self.session.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.session.can_redo()
    }

    pub fn refusals(&self) -> u64 {
        self.refusals
    }

    /// Carries out a command on the project, and on what is playing.
    ///
    /// The project moves first. A command it refuses must never reach the
    /// engine, or the two would disagree about a change that never happened.
    pub fn apply(&mut self, command: Command) -> Result<(), CommandError> {
        let change = route(&self.bindings, &command);
        self.session.apply(command)?;

        match change {
            Change::Nothing => {}
            Change::Rebuild => self.rebuild(),
            Change::Param { param, value } => {
                if !self.engine.send(param, value) {
                    // The queue was full, so the graph no longer matches the
                    // project. Rebuilding is the one repair that is certain:
                    // a fresh graph starts at the project's values.
                    self.owed_rebuild = true;
                }
            }
        }
        Ok(())
    }

    /// Ends the gesture in progress, so the next change starts its own step.
    ///
    /// Called when a control is let go. Without it a fader dragged across the
    /// screen would leave one history entry per frame.
    pub fn seal(&mut self) {
        self.session.seal();
    }

    /// Takes back the last change.
    ///
    /// Always rebuilds, even for a fader. The history knows which edit it
    /// reversed, but not in a form this side can aim, so the coarse repair is
    /// the correct one until it does — a crossfade on undo, never a mismatch.
    pub fn undo(&mut self) -> bool {
        if self.session.undo() {
            self.rebuild();
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self) -> bool {
        if self.session.redo() {
            self.rebuild();
            true
        } else {
            false
        }
    }

    /// Retries anything the engine had no room for.
    ///
    /// Called every frame. Until it succeeds the graph is out of date, which is
    /// why the attempt is repeated rather than dropped.
    pub fn poll(&mut self) {
        if self.owed_rebuild {
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        let Self { session, source_for, channels, max_frames, sample_rate, .. } = self;
        let compiled = compile(session.project(), |track| source_for(track), *channels, *max_frames, *sample_rate);
        let Ok(compiled) = compiled else {
            // A graph the builder refuses means the project describes routing
            // that cannot run. Keeping the old graph is better than silence.
            self.owed_rebuild = false;
            return;
        };

        if self.engine.swap(compiled.graph) {
            // Adopted only once the engine has taken it: until then the old
            // graph is still playing, and its bindings are the live ones.
            self.bindings = compiled.bindings;
            self.owed_rebuild = false;
        } else {
            self.owed_rebuild = true;
            self.refusals += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::AudioBuffer;
    use project_model::TrackId;

    /// An engine that records instead of playing, and can be told to refuse.
    #[derive(Default)]
    struct Recorder {
        params: Vec<(ParamId, f32)>,
        swaps: usize,
        refuse_swap: bool,
        refuse_send: bool,
    }

    impl Engine for Recorder {
        fn send(&mut self, param: ParamId, value: f32) -> bool {
            if self.refuse_send {
                return false;
            }
            self.params.push((param, value));
            true
        }

        fn swap(&mut self, _graph: Box<Graph>) -> bool {
            if self.refuse_swap {
                return false;
            }
            self.swaps += 1;
            true
        }
    }

    struct Silence;
    impl Node for Silence {
        fn process(&mut self, _buffer: &mut AudioBuffer<'_>) {}
    }

    fn studio() -> Studio<Recorder> {
        Studio::new(Recorder::default(), Box::new(|_| Box::new(Silence)), 2, 512, 48_000.0)
    }

    fn add_track(studio: &mut Studio<Recorder>) -> TrackId {
        studio.apply(Command::AddTrack { name: "track".into() }).expect("adding always works");
        studio.project().tracks.last().expect("it was just added").id
    }

    #[test]
    fn a_fader_reaches_the_graph_without_rebuilding_it() {
        let mut studio = studio();
        let id = add_track(&mut studio);
        let swaps_after_add = studio.engine.swaps;

        studio.apply(Command::SetGain { id, gain: 0.5 }).unwrap();
        assert_eq!(studio.engine.swaps, swaps_after_add, "a fader movement rebuilt the graph");
        assert_eq!(studio.engine.params.last().map(|(_, v)| *v), Some(0.5));
    }

    #[test]
    fn adding_a_track_rebuilds() {
        let mut studio = studio();
        let before = studio.engine.swaps;
        add_track(&mut studio);
        assert_eq!(studio.engine.swaps, before + 1);
    }

    #[test]
    fn renaming_asks_nothing_of_the_engine() {
        let mut studio = studio();
        let id = add_track(&mut studio);
        let (swaps, params) = (studio.engine.swaps, studio.engine.params.len());

        studio.apply(Command::RenameTrack { id, name: "drums".into() }).unwrap();
        assert_eq!(studio.project().track(id).unwrap().name, "drums");
        assert_eq!((studio.engine.swaps, studio.engine.params.len()), (swaps, params));
    }

    #[test]
    fn a_command_the_project_refuses_never_reaches_the_engine() {
        // Otherwise the graph would be carrying a change the project says did
        // not happen, and no undo would ever take it back.
        let mut studio = studio();
        add_track(&mut studio);
        let (swaps, params) = (studio.engine.swaps, studio.engine.params.len());

        assert!(studio.apply(Command::SetGain { id: TrackId(999), gain: 0.5 }).is_err());
        assert_eq!((studio.engine.swaps, studio.engine.params.len()), (swaps, params));
    }

    #[test]
    fn a_refused_swap_leaves_the_old_bindings_in_place() {
        // The old graph is still playing. Adopting the new bindings would aim
        // every later fader at nodes that are not there.
        let mut studio = studio();
        add_track(&mut studio);
        studio.engine.refuse_swap = true;

        let id = add_track(&mut studio);
        assert!(studio.owed_rebuild, "the refusal was forgotten");
        assert_eq!(studio.refusals(), 1);
        // The new track is in the project but not in the graph, so aiming at it
        // has to ask for a rebuild rather than move something else.
        assert!(studio.project().track(id).is_some());

        studio.engine.refuse_swap = false;
        studio.poll();
        assert!(!studio.owed_rebuild, "the retry never happened");
    }

    #[test]
    fn a_fader_the_queue_had_no_room_for_is_repaired_by_a_rebuild() {
        // A dropped parameter would leave the graph quietly out of step with
        // the mixer the person is looking at.
        let mut studio = studio();
        let id = add_track(&mut studio);
        studio.engine.refuse_send = true;

        studio.apply(Command::SetGain { id, gain: 0.25 }).unwrap();
        assert!(studio.owed_rebuild, "the dropped change was not noticed");

        let before = studio.engine.swaps;
        studio.poll();
        assert_eq!(studio.engine.swaps, before + 1, "the repair never ran");
    }

    #[test]
    fn undo_puts_the_graph_back_as_well_as_the_project() {
        let mut studio = studio();
        let id = add_track(&mut studio);
        studio.apply(Command::SetGain { id, gain: 0.1 }).unwrap();
        let before = studio.engine.swaps;

        assert!(studio.undo());
        assert_eq!(studio.project().track(id).unwrap().gain, 1.0);
        assert_eq!(studio.engine.swaps, before + 1, "the project moved but the graph did not");

        assert!(studio.redo());
        assert_eq!(studio.project().track(id).unwrap().gain, 0.1);
    }

    #[test]
    fn there_is_nothing_to_undo_at_the_start() {
        let mut studio = studio();
        assert!(!studio.can_undo() && !studio.can_redo());
        assert!(!studio.undo());
    }
}
