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

/// Makes the audio for a track.
///
/// Passed in at each call rather than held by the studio. Holding it would mean
/// the studio owning whatever it captures — a library of decoded files, say —
/// while the caller also needs it, which in safe Rust means interior mutability
/// and a borrow rule nobody checks until it panics. Handed over instead, the
/// compiler proves the two never overlap.
///
/// Called on the control side when a graph is built, so it may read files and
/// take its time; what crosses to the audio thread is the node it returns.
pub trait Sources {
    /// `project` is there to look up what the track names — its file lives in
    /// the project's pool, not on the track.
    fn node_for(&mut self, project: &Project, track: &Track) -> Box<dyn Node>;
}

/// The project, the history, and the engine playing it, kept in step.
pub struct Studio<E: Engine> {
    session: Session,
    bindings: Bindings,
    engine: E,
    channels: usize,
    max_frames: usize,
    sample_rate: f32,
    /// A rebuild the engine had no room for, to try again.
    owed_rebuild: bool,
    /// Rebuilds asked for and refused, worth showing rather than hiding.
    refusals: u64,
}

impl<E: Engine> Studio<E> {
    pub fn new(engine: E, sources: &mut impl Sources, channels: usize, max_frames: usize, sample_rate: f32) -> Self {
        let mut studio = Self {
            session: Session::new(),
            bindings: Bindings::default(),
            engine,
            channels,
            max_frames,
            sample_rate,
            owed_rebuild: false,
            refusals: 0,
        };
        studio.rebuild(sources);
        studio
    }

    pub fn project(&self) -> &Project {
        self.session.project()
    }

    /// The project with its history, for saving.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Swaps in another project — one just opened — and plays it.
    ///
    /// Its history starts empty: undoing past an open would land on a project
    /// that is no longer on screen or on disk.
    pub fn replace(&mut self, session: Session, sources: &mut impl Sources) {
        self.session = session;
        self.rebuild(sources);
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
    pub fn apply(&mut self, command: Command, sources: &mut impl Sources) -> Result<(), CommandError> {
        let change = route(&self.bindings, &command);
        self.session.apply(command)?;

        match change {
            Change::Nothing => {}
            Change::Rebuild => self.rebuild(sources),
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
    pub fn undo(&mut self, sources: &mut impl Sources) -> bool {
        if self.session.undo() {
            self.rebuild(sources);
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self, sources: &mut impl Sources) -> bool {
        if self.session.redo() {
            self.rebuild(sources);
            true
        } else {
            false
        }
    }

    /// Retries anything the engine had no room for.
    ///
    /// Called every frame. Until it succeeds the graph is out of date, which is
    /// why the attempt is repeated rather than dropped.
    pub fn poll(&mut self, sources: &mut impl Sources) {
        if self.owed_rebuild {
            self.rebuild(sources);
        }
    }

    fn rebuild(&mut self, sources: &mut impl Sources) {
        let Self { session, channels, max_frames, sample_rate, .. } = self;
        let project = session.project();
        let compiled = compile(project, |track| sources.node_for(project, track), *channels, *max_frames, *sample_rate);
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

    /// Answers every track with silence, and counts how often it was asked —
    /// which is how often the graph was built.
    #[derive(Default)]
    struct Quiet {
        asked: usize,
    }

    impl Sources for Quiet {
        fn node_for(&mut self, _project: &Project, _track: &Track) -> Box<dyn Node> {
            self.asked += 1;
            Box::new(Silence)
        }
    }

    struct Fixture {
        studio: Studio<Recorder>,
        sources: Quiet,
    }

    impl Fixture {
        fn new() -> Self {
            let mut sources = Quiet::default();
            let studio = Studio::new(Recorder::default(), &mut sources, 2, 512, 48_000.0);
            Self { studio, sources }
        }

        fn apply(&mut self, command: Command) -> Result<(), CommandError> {
            self.studio.apply(command, &mut self.sources)
        }

        fn add_track(&mut self) -> TrackId {
            self.apply(Command::AddTrack { name: "track".into(), source: Default::default(), file: None })
                .expect("adding always works");
            self.studio.project().tracks().last().expect("it was just added").id
        }

        fn swaps(&self) -> usize {
            self.studio.engine.swaps
        }

        fn params(&self) -> usize {
            self.studio.engine.params.len()
        }
    }

    #[test]
    fn a_fader_reaches_the_graph_without_rebuilding_it() {
        let mut f = Fixture::new();
        let id = f.add_track();
        let before = f.swaps();

        f.apply(Command::SetGain { id, gain: 0.5 }).unwrap();
        assert_eq!(f.swaps(), before, "a fader movement rebuilt the graph");
        assert_eq!(f.studio.engine.params.last().map(|(_, v)| *v), Some(0.5));
    }

    #[test]
    fn adding_a_track_rebuilds() {
        let mut f = Fixture::new();
        let before = f.swaps();
        f.add_track();
        assert_eq!(f.swaps(), before + 1);
    }

    #[test]
    fn renaming_asks_nothing_of_the_engine() {
        let mut f = Fixture::new();
        let id = f.add_track();
        let (swaps, params) = (f.swaps(), f.params());

        f.apply(Command::RenameTrack { id, name: "drums".into() }).unwrap();
        assert_eq!(f.studio.project().track(id).unwrap().name, "drums");
        assert_eq!((f.swaps(), f.params()), (swaps, params));
    }

    #[test]
    fn changing_what_a_track_plays_asks_for_its_audio_again() {
        // A different source is a different node, so the graph has to be built
        // again and the new material fetched.
        let mut f = Fixture::new();
        let id = f.add_track();
        let (swaps, asked) = (f.swaps(), f.sources.asked);

        f.apply(Command::SetSource { id, source: project_model::Source::File }).unwrap();
        assert_eq!(f.swaps(), swaps + 1);
        assert!(f.sources.asked > asked, "the graph was rebuilt without asking what to play");
    }

    #[test]
    fn a_command_the_project_refuses_never_reaches_the_engine() {
        // Otherwise the graph would be carrying a change the project says did
        // not happen, and no undo would ever take it back.
        let mut f = Fixture::new();
        f.add_track();
        let (swaps, params) = (f.swaps(), f.params());

        assert!(f.apply(Command::SetGain { id: TrackId(999), gain: 0.5 }).is_err());
        assert_eq!((f.swaps(), f.params()), (swaps, params));
    }

    #[test]
    fn a_refused_swap_leaves_the_old_bindings_in_place() {
        // The old graph is still playing. Adopting the new bindings would aim
        // every later fader at nodes that are not there.
        let mut f = Fixture::new();
        f.add_track();
        f.studio.engine.refuse_swap = true;

        let id = f.add_track();
        assert!(f.studio.owed_rebuild, "the refusal was forgotten");
        assert_eq!(f.studio.refusals(), 1);
        assert!(f.studio.project().track(id).is_some());

        f.studio.engine.refuse_swap = false;
        f.studio.poll(&mut f.sources);
        assert!(!f.studio.owed_rebuild, "the retry never happened");
    }

    #[test]
    fn a_fader_the_queue_had_no_room_for_is_repaired_by_a_rebuild() {
        // A dropped parameter would leave the graph quietly out of step with
        // the mixer the person is looking at.
        let mut f = Fixture::new();
        let id = f.add_track();
        f.studio.engine.refuse_send = true;

        f.apply(Command::SetGain { id, gain: 0.25 }).unwrap();
        assert!(f.studio.owed_rebuild, "the dropped change was not noticed");

        let before = f.swaps();
        f.studio.poll(&mut f.sources);
        assert_eq!(f.swaps(), before + 1, "the repair never ran");
    }

    #[test]
    fn undo_puts_the_graph_back_as_well_as_the_project() {
        let mut f = Fixture::new();
        let id = f.add_track();
        f.apply(Command::SetGain { id, gain: 0.1 }).unwrap();
        let before = f.swaps();

        assert!(f.studio.undo(&mut f.sources));
        assert_eq!(f.studio.project().track(id).unwrap().gain, 1.0);
        assert_eq!(f.swaps(), before + 1, "the project moved but the graph did not");

        assert!(f.studio.redo(&mut f.sources));
        assert_eq!(f.studio.project().track(id).unwrap().gain, 0.1);
    }

    #[test]
    fn there_is_nothing_to_undo_at_the_start() {
        let mut f = Fixture::new();
        assert!(!f.studio.can_undo() && !f.studio.can_redo());
        assert!(!f.studio.undo(&mut f.sources));
    }

    #[test]
    fn opening_a_project_plays_it_and_starts_a_fresh_history() {
        let mut f = Fixture::new();
        f.add_track();
        let mut other = Session::new();
        for name in ["a", "b", "c"] {
            other.apply(Command::AddTrack { name: name.into(), source: Default::default(), file: None }).unwrap();
        }
        let other = Session::from_project(other.project().clone());
        let swaps = f.swaps();

        f.studio.replace(other.clone(), &mut f.sources);
        assert_eq!(f.studio.project(), other.project());
        assert_eq!(f.swaps(), swaps + 1, "the opened project was not handed to the engine");
        assert!(!f.studio.can_undo(), "undo could reach back past the open");
    }
}
