//! The seam between what a person edits and what the audio thread runs.
//!
//! Its own crate on purpose. `project-model` must not depend on the engine —
//! the project outlives any particular way of playing it — and `engine-core`
//! must not know what a project is, since the audio thread has no business
//! holding editable state. Something has to translate between them, and this is
//! the only place that knows both.
//!
//! The translation has two directions and they cost very different amounts:
//!
//! - **Structure** — a track added or removed — means the graph is a different
//!   shape, so a new one is built here and handed over, crossfading over
//!   `SWAP_FADE_MS`.
//! - **Parameters** — gain, pan, mute, solo — reach the graph that is already
//!   playing as scheduled events, landing on the sample they were aimed at.
//!
//! Confusing the two is the expensive mistake: rebuilding the graph for every
//! movement of a fader would mean a ten-millisecond crossfade per mouse frame.
//! [`route`] is where that distinction is made, once.

mod plan;
mod time;

pub use plan::{audio_starts, note_plan};
pub use time::{Position, Timing};

use engine_core::{Gain, Graph, GraphBuilder, GraphError, Limiter, Mute, Node, Pan, ParamId, Solo, SoloBus};
use project_model::{Command, Project, Track, TrackId};

/// The ceiling the master sits under.
///
/// Tracks sum, and nothing in the model stops someone from stacking twenty of
/// them at unity. Clipping is the one failure that cannot be undone after the
/// fact, so the protection is not optional.
pub const CEILING: f32 = 0.95;

/// Where one track's controls ended up in a compiled graph.
#[derive(Debug, Clone, Copy)]
pub struct TrackBinding {
    pub id: TrackId,
    pub gain: ParamId,
    pub pan: ParamId,
    pub muted: ParamId,
    pub soloed: ParamId,
}

/// Where every control ended up, so an edit can be aimed without rebuilding.
///
/// Belongs to one compiled graph. A graph built to replace it has its own, and
/// aiming the old ones at the new graph would move whatever happens to sit at
/// those indices.
#[derive(Debug, Clone, Default)]
pub struct Bindings {
    master_gain: Option<ParamId>,
    tracks: Vec<TrackBinding>,
}

impl Bindings {
    pub fn track(&self, id: TrackId) -> Option<&TrackBinding> {
        self.tracks.iter().find(|b| b.id == id)
    }

    pub fn master_gain(&self) -> Option<ParamId> {
        self.master_gain
    }

    pub fn tracks(&self) -> &[TrackBinding] {
        &self.tracks
    }
}

/// A project as the audio thread will run it.
pub struct Compiled {
    pub graph: Box<Graph>,
    pub bindings: Bindings,
}

/// Builds the graph a project describes.
///
/// `source_for` supplies what each track plays. The model does not say yet —
/// it has no clips — so the caller decides, which is also what lets the same
/// project be compiled with real audio in the app and with a test tone here.
///
/// Runs on the control side: it allocates, and it may fail.
pub fn compile(
    project: &Project,
    mut source_for: impl FnMut(&Track) -> Box<dyn Node>,
    channels: usize,
    max_frames: usize,
    sample_rate: f32,
) -> Result<Compiled, GraphError> {
    let mut builder = GraphBuilder::new();
    // One bus per graph, shared by this graph's solo nodes and no others.
    let bus = SoloBus::new();
    let master = builder.add(Box::new(Gain::new(project.master_gain(), sample_rate)));
    let mut tracks = Vec::with_capacity(project.track_count());

    for track in project.tracks() {
        let source = builder.add(source_for(track));
        // Built at the values the project already holds, so a compiled graph
        // starts where the model is rather than ramping towards it.
        let gain = builder.add(Box::new(Gain::new(track.gain, sample_rate)));
        let pan = builder.add(Box::new(Pan::new(track.pan, sample_rate)));
        let muted = builder.add(Box::new(Mute::new(track.muted, sample_rate)));
        let soloed = builder.add(Box::new(Solo::new(&bus, track.soloed, sample_rate)));
        builder
            .connect(source, gain)
            .connect(gain, pan)
            .connect(pan, muted)
            .connect(muted, soloed)
            .connect(soloed, master);

        tracks.push(TrackBinding {
            id: track.id,
            gain: ParamId::new(gain, Gain::PARAM_GAIN),
            pan: ParamId::new(pan, Pan::PARAM_POSITION),
            muted: ParamId::new(muted, Mute::PARAM_MUTED),
            soloed: ParamId::new(soloed, Solo::PARAM_SOLOED),
        });
    }

    let ceiling = builder.add(Box::new(Limiter::new(CEILING, channels, sample_rate)));
    builder.connect(master, ceiling).output(ceiling);

    let graph = builder.build(channels, max_frames)?;
    let bindings = Bindings { master_gain: Some(ParamId::new(master, Gain::PARAM_GAIN)), tracks };
    Ok(Compiled { graph: Box::new(graph), bindings })
}

/// What the engine has to do about a command.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Change {
    /// Nothing the audio thread can hear. A name is not a sound.
    Nothing,
    /// Can be aimed at the graph that is already playing.
    Param { param: ParamId, value: f32 },
    /// The graph is a different shape now and has to be built again.
    Rebuild,
}

/// Decides how a command reaches the engine.
///
/// The one place the cheap path and the expensive one are told apart.
pub fn route(bindings: &Bindings, command: &Command) -> Change {
    // A command aimed at a track this graph does not have is a graph that is
    // already out of date; rebuilding is the honest answer.
    let aimed = |id: TrackId, pick: fn(&TrackBinding) -> ParamId, value: f32| {
        bindings.track(id).map_or(Change::Rebuild, |b| Change::Param { param: pick(b), value })
    };
    // Booleans travel as numbers, read back above the halfway mark.
    let flag = |on: bool| if on { 1.0 } else { 0.0 };

    match command {
        // A different source is a different node in the graph, not a value on
        // one that is already there.
        Command::AddTrack { .. }
        | Command::RemoveTrack { .. }
        | Command::SetSource { .. }
        | Command::ClearTrack { .. }
        | Command::AddAudioClip { .. }
        | Command::AddMidiClip { .. }
        | Command::MovePlacement { .. }
        | Command::SetWindow { .. }
        | Command::TrimPlacement { .. }
        | Command::DuplicatePlacement { .. }
        | Command::MakeUnique { .. }
        | Command::RemovePlacement { .. }
        | Command::SetClipLength { .. }
        | Command::AddNote { .. }
        | Command::SetNote { .. }
        | Command::RemoveNote { .. }
        // Every clip's start is a tick; a new tempo puts it on another frame.
        | Command::SetTempo { .. } => Change::Rebuild,
        // Names, and a meter that only changes how bars are counted.
        Command::RenameTrack { .. } | Command::RenameClip { .. } | Command::SetMeter { .. } => Change::Nothing,
        Command::SetGain { id, gain } => aimed(*id, |b| b.gain, *gain),
        Command::SetPan { id, pan } => aimed(*id, |b| b.pan, pan.clamp(-1.0, 1.0)),
        Command::SetMuted { id, muted } => aimed(*id, |b| b.muted, flag(*muted)),
        Command::SetSoloed { id, soloed } => aimed(*id, |b| b.soloed, flag(*soloed)),
        Command::SetMasterGain { gain } => {
            bindings.master_gain().map_or(Change::Rebuild, |param| Change::Param { param, value: *gain })
        }
    }
}
