//! What a project sounds like, and what an edit costs.

use engine_core::{AudioBuffer, Node, automation_channel};
use project_engine::{CEILING, Change, Compiled, compile, route};
use project_model::{Command, Session, Source, TrackId};

const SR: f32 = 48_000.0;
const CHANNELS: usize = 2;
const MAX_FRAMES: usize = 512;
/// Low enough that the sum of a few tracks never reaches the ceiling, so what
/// is measured is the mixer rather than the limiter.
const LEVEL: f32 = 0.3;

/// A track's audio, standing in for a clip the model cannot hold yet.
struct Constant(f32);

impl Node for Constant {
    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        for channel in buffer.channels_mut() {
            channel.fill(self.0);
        }
    }
}

fn build(session: &Session) -> Compiled {
    compile(session.project(), |_| Box::new(Constant(LEVEL)), CHANNELS, MAX_FRAMES, SR)
        .expect("a mixer is acyclic by construction")
}

/// A session with `n` tracks, its history cleared.
fn with_tracks(n: usize) -> Session {
    let mut session = Session::new();
    for i in 0..n {
        session
            .apply(Command::AddTrack { name: format!("track {i}"), source: Source::default(), clip: None })
            .expect("adding always works");
    }
    Session::from_project(session.project().clone())
}

/// Renders `blocks` and returns the last frame, once the ramps have settled.
fn settled(compiled: &mut Compiled, blocks: usize) -> (f32, f32) {
    let mut out = vec![0.0f32; MAX_FRAMES * CHANNELS];
    for _ in 0..blocks {
        compiled.graph.process(&mut out);
    }
    let last = out.len() - CHANNELS;
    (out[last], out[last + 1])
}

fn ids(session: &Session) -> Vec<TrackId> {
    session.project().tracks().map(|t| t.id).collect()
}

#[test]
fn an_empty_project_plays_silence() {
    let mut compiled = build(&Session::new());
    assert_eq!(settled(&mut compiled, 4), (0.0, 0.0));
}

#[test]
fn a_track_arrives_at_the_level_the_project_gives_it() {
    let mut session = with_tracks(1);
    let id = ids(&session)[0];
    session.apply(Command::SetGain { id, gain: 0.5 }).unwrap();

    let mut compiled = build(&session);
    let (left, right) = settled(&mut compiled, 8);
    // Centre pan is -3 dB a side, which is the point of the constant-power law.
    let expected = LEVEL * 0.5 * std::f32::consts::FRAC_1_SQRT_2;
    assert!((left - expected).abs() < 1e-4, "left {left}, expected {expected}");
    assert!((right - expected).abs() < 1e-4);
}

#[test]
fn a_compiled_graph_starts_where_the_project_is_rather_than_ramping_to_it() {
    // Otherwise every compile would open with a swell from the default value,
    // which on a swap is audible as a pump.
    let mut session = with_tracks(1);
    let id = ids(&session)[0];
    session.apply(Command::SetGain { id, gain: 0.0 }).unwrap();

    let mut compiled = build(&session);
    let mut out = vec![0.0f32; 64 * CHANNELS];
    compiled.graph.process(&mut out);
    assert!(out.iter().all(|s| *s == 0.0), "a silent track leaked at the start: {:?}", &out[..4]);
}

#[test]
fn panning_hard_left_empties_the_right() {
    let mut session = with_tracks(1);
    let id = ids(&session)[0];
    session.apply(Command::SetPan { id, pan: -1.0 }).unwrap();

    let mut compiled = build(&session);
    let (left, right) = settled(&mut compiled, 8);
    assert!((left - LEVEL).abs() < 1e-4, "left {left}");
    assert!(right.abs() < 1e-4, "right {right}");
}

#[test]
fn soloing_one_track_silences_the_others() {
    // The project resolves this across every track, and the compiled graph has
    // to agree with it.
    let mut session = with_tracks(2);
    let [a, b] = ids(&session)[..] else { panic!("two tracks") };
    session.apply(Command::SetPan { id: a, pan: -1.0 }).unwrap();
    session.apply(Command::SetPan { id: b, pan: 1.0 }).unwrap();
    session.apply(Command::SetSoloed { id: a, soloed: true }).unwrap();

    let mut compiled = build(&session);
    let (left, right) = settled(&mut compiled, 8);
    assert!((left - LEVEL).abs() < 1e-4, "the soloed track should be the one heard");
    assert!(right.abs() < 1e-4, "the other track was not silenced");
}

#[test]
fn the_graph_reports_the_limiters_latency() {
    let compiled = build(&with_tracks(1));
    assert!(compiled.graph.latency_frames() > 0, "the master ceiling reported no lookahead");
}

#[test]
fn changing_the_shape_asks_for_a_rebuild_and_moving_a_fader_does_not() {
    // The distinction this crate exists for. Rebuilding on every fader
    // movement would mean a crossfade per mouse frame.
    let session = with_tracks(1);
    let id = ids(&session)[0];
    let compiled = build(&session);
    let b = &compiled.bindings;

    assert_eq!(
        route(b, &Command::AddTrack { name: "two".into(), source: Default::default(), clip: None }),
        Change::Rebuild
    );
    assert_eq!(route(b, &Command::RemoveTrack { id }), Change::Rebuild);
    assert_eq!(route(b, &Command::RenameTrack { id, name: "x".into() }), Change::Nothing);

    for command in [
        Command::SetGain { id, gain: 0.5 },
        Command::SetPan { id, pan: 0.5 },
        Command::SetMuted { id, muted: true },
        Command::SetSoloed { id, soloed: true },
        Command::SetMasterGain { gain: 0.5 },
    ] {
        assert!(matches!(route(b, &command), Change::Param { .. }), "{command:?} asked for a rebuild");
    }
}

#[test]
fn a_command_aimed_at_a_track_this_graph_never_had_asks_for_a_rebuild() {
    // A graph that does not know the track is a graph that is already out of
    // date; moving whatever sits at that index would be worse than rebuilding.
    let compiled = build(&with_tracks(1));
    let change = route(&compiled.bindings, &Command::SetGain { id: TrackId(999), gain: 0.5 });
    assert_eq!(change, Change::Rebuild);
}

#[test]
fn a_fader_moved_on_the_running_graph_lands_where_a_rebuild_would_have() {
    // The whole point of the seam: the cheap path and the expensive one must
    // agree, or the mixer drifts away from the project it is showing.
    let mut session = with_tracks(2);
    let [a, _b] = ids(&session)[..] else { panic!("two tracks") };

    let mut playing = build(&session);
    let (mut sender, mut queue) = automation_channel(16);

    let command = Command::SetGain { id: a, gain: 0.25 };
    let Change::Param { param, value } = route(&playing.bindings, &command) else {
        panic!("a gain change should never need a rebuild");
    };
    session.apply(command).unwrap();
    sender.send(engine_core::ParamEvent::now(param, value)).unwrap();

    // Long enough for the ramp to have settled, so what is compared is the
    // value each path arrived at rather than where it was on the way.
    let mut out = vec![0.0f32; MAX_FRAMES * CHANNELS];
    let mut frame = 0u64;
    for _ in 0..16 {
        playing.graph.process_automated(&mut out, &mut queue, frame);
        frame += MAX_FRAMES as u64;
    }
    let last = out.len() - CHANNELS;
    let automated = (out[last], out[last + 1]);

    let rebuilt = settled(&mut build(&session), 16);
    assert!((automated.0 - rebuilt.0).abs() < 1e-4, "left: automated {:?}, rebuilt {:?}", automated, rebuilt);
    assert!((automated.1 - rebuilt.1).abs() < 1e-4, "right: automated {:?}, rebuilt {:?}", automated, rebuilt);
}

#[test]
fn the_master_holds_a_stack_of_tracks_under_the_ceiling() {
    // Nothing in the model stops someone stacking twenty tracks at unity, and
    // clipping is the one failure that cannot be undone afterwards.
    let mut session = Session::new();
    for i in 0..20 {
        session.apply(Command::AddTrack { name: format!("{i}"), source: Source::default(), clip: None }).unwrap();
    }
    let mut compiled = compile(session.project(), |_| Box::new(Constant(1.0)), CHANNELS, MAX_FRAMES, SR).unwrap();

    let mut out = vec![0.0f32; MAX_FRAMES * CHANNELS];
    for _ in 0..32 {
        compiled.graph.process(&mut out);
        for sample in &out {
            assert!(sample.abs() <= CEILING + 1e-6, "{sample} broke the ceiling");
        }
    }
    assert!(out.iter().any(|s| s.abs() > 0.5), "the mix was too quiet to prove anything");
}

#[test]
fn changing_what_a_track_plays_asks_for_a_rebuild() {
    // A different source is a different node, not a value on one already
    // there, so there is nothing to aim at.
    let session = with_tracks(1);
    let id = ids(&session)[0];
    let compiled = build(&session);
    let source = Source::Clips;
    assert_eq!(route(&compiled.bindings, &Command::SetSource { id, source }), Change::Rebuild);
}

#[test]
fn the_source_survives_being_undone_and_redone() {
    let mut session = with_tracks(1);
    let id = ids(&session)[0];
    let source = Source::Clips;
    session.apply(Command::SetSource { id, source }).unwrap();
    assert_eq!(session.project().track(id).unwrap().source, source);

    session.undo();
    assert_eq!(session.project().track(id).unwrap().source, Source::Silent, "an ejected track should be silent");
    session.redo();
    assert_eq!(session.project().track(id).unwrap().source, source);
}

#[test]
fn a_tracks_equaliser_is_in_its_chain() {
    // The test source is a constant: DC. A low cut removes it entirely, which
    // it can only do if the equaliser sits between the source and the output.
    let mut session = with_tracks(1);
    let id = ids(&session)[0];
    let cut =
        project_model::EqBand { shape: project_model::BandShape::LowCut, freq: 80.0, gain: 0.0, q: 0.707, on: true };
    session.apply(Command::SetEqBand { track: id, band: 0, value: cut }).unwrap();
    let mut compiled = build(&session);
    let (left, _) = settled(&mut compiled, 400);
    assert!(left.abs() < 1e-3, "DC came through a low cut: {left}");

    // And the default bands, flat, leave the level exactly as it was.
    let mut flat = build(&with_tracks(1));
    let (left, _) = settled(&mut flat, 8);
    assert!((left - LEVEL * std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-4, "a flat equaliser changed the level");
}

#[test]
fn moving_a_band_is_parameters_but_reshaping_it_is_a_rebuild() {
    let session = with_tracks(1);
    let id = ids(&session)[0];
    let compiled = build(&session);
    let band = session.project().track(id).unwrap().eq[1];

    let moved = project_model::EqBand { freq: 1_200.0, gain: 4.0, ..band };
    assert!(matches!(
        route(&compiled.bindings, &Command::SetEqBand { track: id, band: 1, value: moved }),
        Change::Params(_)
    ));

    let reshaped = project_model::EqBand { shape: project_model::BandShape::HighCut, ..band };
    assert_eq!(route(&compiled.bindings, &Command::SetEqBand { track: id, band: 1, value: reshaped }), Change::Rebuild);
    let off = project_model::EqBand { on: false, ..band };
    assert_eq!(route(&compiled.bindings, &Command::SetEqBand { track: id, band: 1, value: off }), Change::Rebuild);
}
