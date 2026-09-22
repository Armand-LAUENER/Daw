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

pub use plan::{AudioSpan, audio_spans, note_plan};
pub use time::{Position, Timing};

use engine_core::{
    BandSettings, Compress, Echo, Eq, Gain, Graph, GraphBuilder, GraphError, Limiter, Mute, Node, NodeId, Pan, ParamId,
    Room, Solo, SoloBus,
};
use engine_dsp::{CompressorSettings, Shape};
use project_model::{BANDS, BandShape, Command, Project, Track, TrackId};

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
    /// The equaliser, and the shape and state each band was built with: a
    /// band still built the same way can take new values as parameters, one
    /// that is not needs the graph rebuilt.
    pub eq: NodeId,
    pub eq_built: [(BandShape, bool); BANDS],
    /// Each effect's node, when it is on. An effect that is off is not in
    /// the graph, so its settings have nowhere to go until it is switched on,
    /// which rebuilds.
    pub comp: Option<NodeId>,
    pub delay: Option<NodeId>,
    pub reverb: Option<NodeId>,
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
    /// The tempo and rate the graph was built at, to turn a delay's ticks
    /// into frames.
    timing: Option<Timing>,
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
        let bands = track.eq.map(|b| BandSettings {
            shape: dsp_shape(b.shape),
            freq_hz: b.freq,
            gain_db: b.gain,
            q: b.q,
            on: b.on,
        });
        let eq = builder.add(Box::new(Eq::new(&bands, channels, sample_rate)));
        // The effects that are on, in order, after the equaliser.
        let timing = Timing::new(project.tempo(), sample_rate as u32);
        let comp = track.comp.on.then(|| builder.add(Box::new(Compress::new(comp_settings(&track.comp), sample_rate))));
        let delay = track.delay.on.then(|| {
            let d = &track.delay;
            let time = timing.frame_of(d.time) as f32;
            builder.add(Box::new(Echo::new(channels, time, d.feedback, d.mix, sample_rate)))
        });
        let reverb = track.reverb.on.then(|| {
            let r = &track.reverb;
            builder.add(Box::new(Room::new(channels, r.size, r.damping, r.mix, sample_rate)))
        });
        let gain = builder.add(Box::new(Gain::new(track.gain, sample_rate)));
        let pan = builder.add(Box::new(Pan::new(track.pan, sample_rate)));
        let muted = builder.add(Box::new(Mute::new(track.muted, sample_rate)));
        let soloed = builder.add(Box::new(Solo::new(&bus, track.soloed, sample_rate)));
        builder.connect(source, eq);
        // Each effect that is on takes the chain's end; whatever is last feeds
        // the fader.
        let mut last = eq;
        for effect in [comp, delay, reverb].into_iter().flatten() {
            builder.connect(last, effect);
            last = effect;
        }
        builder
            .connect(last, gain)
            .connect(gain, pan)
            .connect(pan, muted)
            .connect(muted, soloed)
            .connect(soloed, master);

        tracks.push(TrackBinding {
            id: track.id,
            eq,
            eq_built: track.eq.map(|b| (b.shape, b.on)),
            comp,
            delay,
            reverb,
            gain: ParamId::new(gain, Gain::PARAM_GAIN),
            pan: ParamId::new(pan, Pan::PARAM_POSITION),
            muted: ParamId::new(muted, Mute::PARAM_MUTED),
            soloed: ParamId::new(soloed, Solo::PARAM_SOLOED),
        });
    }

    let ceiling = builder.add(Box::new(Limiter::new(CEILING, channels, sample_rate)));
    builder.connect(master, ceiling).output(ceiling);

    let graph = builder.build(channels, max_frames)?;
    let timing = Some(Timing::new(project.tempo(), sample_rate as u32));
    let bindings = Bindings { master_gain: Some(ParamId::new(master, Gain::PARAM_GAIN)), timing, tracks };
    Ok(Compiled { graph: Box::new(graph), bindings })
}

/// How an effect's settings reach the engine: as parameters when it is in the
/// graph and stays on, a rebuild when it is switched on or off, and nothing
/// while it is off — there is nothing to hear.
fn effect(
    bindings: &Bindings,
    track: TrackId,
    on: bool,
    node: impl Fn(&TrackBinding) -> Option<NodeId>,
    params: impl Fn(NodeId) -> Vec<(ParamId, f32)>,
) -> Change {
    let Some(binding) = bindings.track(track) else { return Change::Rebuild };
    match (node(binding), on) {
        (Some(id), true) => Change::Params(params(id)),
        (None, false) => Change::Nothing,
        _ => Change::Rebuild,
    }
}

fn comp_settings(c: &project_model::CompSettings) -> CompressorSettings {
    CompressorSettings {
        threshold_db: c.threshold,
        ratio: c.ratio,
        attack_ms: c.attack,
        release_ms: c.release,
        makeup_db: c.makeup,
    }
}

/// The model's band shape as the DSP knows it. Two enums rather than one, so
/// the saved format and the filter code can each change without the other.
fn dsp_shape(shape: BandShape) -> Shape {
    match shape {
        BandShape::LowCut => Shape::LowCut,
        BandShape::LowShelf => Shape::LowShelf,
        BandShape::Bell => Shape::Bell,
        BandShape::HighShelf => Shape::HighShelf,
        BandShape::HighCut => Shape::HighCut,
    }
}

/// What one band does at `freq_hz`, in dB — from the same coefficients the
/// compiled graph uses, so a curve drawn from this is what is heard. A band
/// that is off does nothing.
pub fn band_gain_db(band: &project_model::EqBand, freq_hz: f32, sample_rate: f32) -> f32 {
    if !band.on {
        return 0.0;
    }
    let coefficients = engine_dsp::coefficients(dsp_shape(band.shape), band.freq, band.gain, band.q, sample_rate);
    20.0 * coefficients.magnitude(freq_hz, sample_rate).max(1e-9).log10()
}

/// What the engine has to do about a command.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// Nothing the audio thread can hear. A name is not a sound.
    Nothing,
    /// Can be aimed at the graph that is already playing.
    Param { param: ParamId, value: f32 },
    /// Several values aimed at once — an equaliser band's frequency, gain and
    /// Q, a compressor's settings — which only mean anything together.
    Params(Vec<(ParamId, f32)>),
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
        Command::SetEqBand { track, band, value } => {
            let band_index = usize::from(*band);
            match bindings.track(*track) {
                Some(b) if b.eq_built.get(band_index) == Some(&(value.shape, value.on)) => {
                    let param = |which| ParamId::new(b.eq, Eq::param(band_index, which));
                    Change::Params(vec![
                        (param(Eq::PARAM_FREQ), value.freq),
                        (param(Eq::PARAM_GAIN), value.gain),
                        (param(Eq::PARAM_Q), value.q),
                    ])
                }
                // A different shape, or a band switched on or off, is a
                // different filter: rebuilt, and crossfaded, rather than
                // jumped to.
                _ => Change::Rebuild,
            }
        }
        Command::SetCompressor { track, value } => effect(bindings, *track, value.on, |b| b.comp, |node| {
            let p = |which| ParamId::new(node, which);
            vec![
                (p(Compress::PARAM_THRESHOLD), value.threshold),
                (p(Compress::PARAM_RATIO), value.ratio),
                (p(Compress::PARAM_ATTACK), value.attack),
                (p(Compress::PARAM_RELEASE), value.release),
                (p(Compress::PARAM_MAKEUP), value.makeup),
            ]
        }),
        // The time is sent in ticks' worth of frames at the tempo the graph
        // was built for; a new tempo rebuilds, and recomputes it.
        Command::SetDelay { track, value } => effect(bindings, *track, value.on, |b| b.delay, |node| {
            let p = |which| ParamId::new(node, which);
            let time = bindings.timing.map_or(0.0, |t| t.frame_of(value.time) as f32);
            vec![(p(Echo::PARAM_TIME), time), (p(Echo::PARAM_FEEDBACK), value.feedback), (p(Echo::PARAM_MIX), value.mix)]
        }),
        Command::SetReverb { track, value } => effect(bindings, *track, value.on, |b| b.reverb, |node| {
            let p = |which| ParamId::new(node, which);
            vec![(p(Room::PARAM_SIZE), value.size), (p(Room::PARAM_DAMPING), value.damping), (p(Room::PARAM_MIX), value.mix)]
        }),
        Command::SetMasterGain { gain } => {
            bindings.master_gain().map_or(Change::Rebuild, |param| Change::Param { param, value: *gain })
        }
    }
}
