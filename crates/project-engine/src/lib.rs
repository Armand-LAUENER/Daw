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
use project_model::{BANDS, BandShape, BusId, Command, Project, Track, TrackId};

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

/// Where one bus's controls ended up in a compiled graph.
#[derive(Debug, Clone, Copy)]
pub struct BusBinding {
    pub id: BusId,
    /// Where its sends arrive.
    pub input: NodeId,
    pub comp: Option<NodeId>,
    pub delay: Option<NodeId>,
    pub reverb: Option<NodeId>,
    pub gain: ParamId,
    pub muted: ParamId,
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
    buses: Vec<BusBinding>,
    /// Each send's level, by track and bus.
    sends: Vec<(TrackId, BusId, ParamId)>,
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

    pub fn bus(&self, id: BusId) -> Option<&BusBinding> {
        self.buses.iter().find(|b| b.id == id)
    }

    pub fn send(&self, track: TrackId, bus: BusId) -> Option<ParamId> {
        self.sends.iter().find(|(t, b, _)| *t == track && *b == bus).map(|(.., p)| *p)
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
    let timing = Timing::new(project.tempo(), sample_rate as u32);

    // Buses first, so tracks have somewhere to send. Each: a summing point
    // for its sends, its effects, its fader and mute, into the master. No
    // solo node: a return stays open when a track is soloed, or soloing a
    // track would take its reverb away.
    let mut buses = Vec::with_capacity(project.buses().len());
    for bus in project.buses() {
        let input = builder.add(Box::new(Gain::new(1.0, sample_rate)));
        let (last, [comp, delay, reverb]) =
            effect_chain(&mut builder, input, &bus.comp, &bus.delay, &bus.reverb, true, timing, channels, sample_rate);
        let gain = builder.add(Box::new(Gain::new(bus.gain, sample_rate)));
        let muted = builder.add(Box::new(Mute::new(bus.muted, sample_rate)));
        builder.connect(last, gain).connect(gain, muted).connect(muted, master);
        buses.push(BusBinding {
            id: bus.id,
            input,
            comp,
            delay,
            reverb,
            gain: ParamId::new(gain, Gain::PARAM_GAIN),
            muted: ParamId::new(muted, Mute::PARAM_MUTED),
        });
    }
    let mut sends = Vec::new();

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
        builder.connect(source, eq);
        // The effects that are on, in order, after the equaliser.
        let (last, [comp, delay, reverb]) = effect_chain(
            &mut builder,
            eq,
            &track.comp,
            &track.delay,
            &track.reverb,
            false,
            timing,
            channels,
            sample_rate,
        );
        let gain = builder.add(Box::new(Gain::new(track.gain, sample_rate)));
        let pan = builder.add(Box::new(Pan::new(track.pan, sample_rate)));
        let muted = builder.add(Box::new(Mute::new(track.muted, sample_rate)));
        let soloed = builder.add(Box::new(Solo::new(&bus, track.soloed, sample_rate)));
        builder
            .connect(last, gain)
            .connect(gain, pan)
            .connect(pan, muted)
            .connect(muted, soloed)
            .connect(soloed, master);

        // Sends, after everything the track does: its fader, pan, mute and
        // solo all reach what it sends.
        for (bus_id, level) in &track.sends {
            let Some(bus) = buses.iter().find(|b| b.id == *bus_id) else { continue };
            let send = builder.add(Box::new(Gain::new(*level, sample_rate)));
            builder.connect(soloed, send).connect(send, bus.input);
            sends.push((track.id, *bus_id, ParamId::new(send, Gain::PARAM_GAIN)));
        }

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
    let bindings = Bindings {
        master_gain: Some(ParamId::new(master, Gain::PARAM_GAIN)),
        timing: Some(timing),
        tracks,
        buses,
        sends,
    };
    Ok(Compiled { graph: Box::new(graph), bindings })
}

/// The effects that are on, in series after `from`, and the node the chain
/// now ends on. On a bus the delay and the reverb are wet only: the dry sound
/// already reaches the mix through its own track.
#[allow(clippy::too_many_arguments)]
fn effect_chain(
    builder: &mut GraphBuilder,
    from: NodeId,
    comp: &project_model::CompSettings,
    delay: &project_model::DelaySettings,
    reverb: &project_model::ReverbSettings,
    wet_only: bool,
    timing: Timing,
    channels: usize,
    sample_rate: f32,
) -> (NodeId, [Option<NodeId>; 3]) {
    let comp = comp.on.then(|| builder.add(Box::new(Compress::new(comp_settings(comp), sample_rate))));
    let delay = delay.on.then(|| {
        let time = timing.frame_of(delay.time) as f32;
        let echo = Echo::new(channels, time, delay.feedback, delay.mix, sample_rate);
        builder.add(Box::new(if wet_only { echo.wet_only() } else { echo }))
    });
    let reverb = reverb.on.then(|| {
        let room = Room::new(channels, reverb.size, reverb.damping, reverb.mix, sample_rate);
        builder.add(Box::new(if wet_only { room.wet_only() } else { room }))
    });
    let mut last = from;
    for effect in [comp, delay, reverb].into_iter().flatten() {
        builder.connect(last, effect);
        last = effect;
    }
    (last, [comp, delay, reverb])
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

fn comp_params(node: NodeId, c: &project_model::CompSettings) -> Vec<(ParamId, f32)> {
    let p = |which| ParamId::new(node, which);
    vec![
        (p(Compress::PARAM_THRESHOLD), c.threshold),
        (p(Compress::PARAM_RATIO), c.ratio),
        (p(Compress::PARAM_ATTACK), c.attack),
        (p(Compress::PARAM_RELEASE), c.release),
        (p(Compress::PARAM_MAKEUP), c.makeup),
    ]
}

fn delay_params(node: NodeId, d: &project_model::DelaySettings, timing: Option<Timing>) -> Vec<(ParamId, f32)> {
    let p = |which| ParamId::new(node, which);
    let time = timing.map_or(0.0, |t| t.frame_of(d.time) as f32);
    vec![(p(Echo::PARAM_TIME), time), (p(Echo::PARAM_FEEDBACK), d.feedback), (p(Echo::PARAM_MIX), d.mix)]
}

fn reverb_params(node: NodeId, r: &project_model::ReverbSettings) -> Vec<(ParamId, f32)> {
    let p = |which| ParamId::new(node, which);
    vec![(p(Room::PARAM_SIZE), r.size), (p(Room::PARAM_DAMPING), r.damping), (p(Room::PARAM_MIX), r.mix)]
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
        | Command::SetTempo { .. }
        // A different instrument is a different synth.
        | Command::SetInstrument { .. } => Change::Rebuild,
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
        Command::SetCompressor { track, value } => {
            effect(bindings, *track, value.on, |b| b.comp, |node| comp_params(node, value))
        }
        // The time is sent in ticks' worth of frames at the tempo the graph
        // was built for; a new tempo rebuilds, and recomputes it.
        Command::SetDelay { track, value } => {
            effect(bindings, *track, value.on, |b| b.delay, |node| delay_params(node, value, bindings.timing))
        }
        Command::SetReverb { track, value } => {
            effect(bindings, *track, value.on, |b| b.reverb, |node| reverb_params(node, value))
        }
        // A bus appearing or going is a different graph; its name is not a sound.
        Command::AddBus { .. } | Command::RemoveBus { .. } => Change::Rebuild,
        Command::RenameBus { .. } => Change::Nothing,
        Command::SetBusGain { id, gain } => {
            bindings.bus(*id).map_or(Change::Rebuild, |b| Change::Param { param: b.gain, value: *gain })
        }
        Command::SetBusMuted { id, muted } => {
            bindings.bus(*id).map_or(Change::Rebuild, |b| Change::Param { param: b.muted, value: flag(*muted) })
        }
        Command::SetBusEffects { id, comp, delay, reverb } => match bindings.bus(*id) {
            // The same effects on as when built: their settings are
            // parameters. Any switched on or off: a rebuild.
            Some(b)
                if b.comp.is_some() == comp.on && b.delay.is_some() == delay.on && b.reverb.is_some() == reverb.on =>
            {
                let mut params = Vec::new();
                if let Some(node) = b.comp {
                    params.extend(comp_params(node, comp));
                }
                if let Some(node) = b.delay {
                    params.extend(delay_params(node, delay, bindings.timing));
                }
                if let Some(node) = b.reverb {
                    params.extend(reverb_params(node, reverb));
                }
                if params.is_empty() { Change::Nothing } else { Change::Params(params) }
            }
            _ => Change::Rebuild,
        },
        Command::SetSend { track, bus, level } => match (bindings.send(*track, *bus), level) {
            (Some(param), Some(level)) => Change::Param { param, value: *level },
            // A send made or removed changes the graph's shape.
            _ => Change::Rebuild,
        },
        Command::SetMasterGain { gain } => {
            bindings.master_gain().map_or(Change::Rebuild, |param| Change::Param { param, value: *gain })
        }
    }
}
