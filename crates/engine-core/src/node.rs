use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use engine_dsp::LinearSmoother;

use crate::AudioBuffer;

/// How long a parameter takes to reach a new value.
///
/// Every parameter here is ramped rather than applied at once: a gain written
/// straight into the multiply steps the output by the size of the change, which
/// is a click. Long enough to be inaudible, short enough to feel immediate.
pub const PARAM_RAMP_MS: f32 = 10.0;

/// One stage of the graph.
///
/// Processing happens in place: a node reads the buffer and writes it back,
/// which keeps the graph free of the copies a returned buffer would need.
/// Called from the audio thread, so no allocation, no locking, no I/O.
pub trait Node: Send {
    fn process(&mut self, buffer: &mut AudioBuffer<'_>);

    /// Drops any internal state, as when playback jumps somewhere else.
    fn reset(&mut self) {}

    /// How many frames this node delays its output by.
    ///
    /// Zero for anything that works sample by sample. A node that has to see
    /// ahead — a limiter, a linear-phase filter — reports what it holds back,
    /// so the graph can say how far behind its output runs.
    fn latency_frames(&self) -> usize {
        0
    }

    /// Tells the node where the block about to be processed starts on the
    /// engine's timeline.
    ///
    /// Only a node that schedules something against that timeline needs it — a
    /// synth placing a note on its frame. Everything else works on whatever it
    /// is handed and ignores this.
    fn set_frame(&mut self, _frame: u64) {}

    /// Applies an automation event aimed at this node.
    ///
    /// `param` is one of the node's own `PARAM_*` constants. A node ignores
    /// anything it does not recognise: an event whose target was replaced by a
    /// graph swap is better dropped than applied to whatever now sits there.
    fn set_param(&mut self, _param: u32, _value: f32) {}
}

/// Scales every channel by a ramped gain.
#[derive(Debug, Clone)]
pub struct Gain {
    gain: LinearSmoother,
}

impl Gain {
    /// The multiplier, linear rather than in decibels.
    pub const PARAM_GAIN: u32 = 0;

    pub fn new(gain: f32, sample_rate: f32) -> Self {
        Self { gain: LinearSmoother::with_duration(gain, PARAM_RAMP_MS, sample_rate) }
    }

    pub fn set_gain(&mut self, gain: f32) {
        self.gain.set_target(gain);
    }

    pub fn gain(&self) -> f32 {
        self.gain.target()
    }
}

impl Node for Gain {
    fn set_param(&mut self, param: u32, value: f32) {
        if param == Self::PARAM_GAIN {
            self.set_gain(value);
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        // The ramp advances once per frame, not once per channel, or the two
        // sides of a stereo track would drift apart while the gain moves.
        let (frames, channels) = (buffer.frames(), buffer.channels());
        for frame in 0..frames {
            let gain = self.gain.next_value();
            for channel in 0..channels {
                buffer.channel_mut(channel)[frame] *= gain;
            }
        }
    }
}

/// Places a stereo signal across the pair, at constant power.
///
/// The obvious law — scale one side by `1 - p` and the other by `p` — loses
/// 3 dB in the middle, so a sound panned across the image audibly dips as it
/// passes the centre. Taking the gains from a quarter circle instead keeps
/// `left² + right²` at one everywhere, which is what the ear follows.
///
/// A buffer with fewer than two channels passes through untouched: panning is
/// meaningless before the signal has a pair to sit in.
#[derive(Debug, Clone)]
pub struct Pan {
    position: LinearSmoother,
}

impl Pan {
    /// -1 hard left, 0 centre, 1 hard right.
    pub const PARAM_POSITION: u32 = 0;

    /// `position` runs from -1 (hard left) through 0 (centre) to 1 (hard right).
    pub fn new(position: f32, sample_rate: f32) -> Self {
        Self { position: LinearSmoother::with_duration(position.clamp(-1.0, 1.0), PARAM_RAMP_MS, sample_rate) }
    }

    pub fn set_position(&mut self, position: f32) {
        self.position.set_target(position.clamp(-1.0, 1.0));
    }

    pub fn position(&self) -> f32 {
        self.position.target()
    }

    /// The pair of gains for a position, as `(left, right)`.
    pub fn gains(position: f32) -> (f32, f32) {
        // -1..1 mapped onto a quarter turn: cos falls from 1 to 0 as sin rises.
        let angle = (position.clamp(-1.0, 1.0) + 1.0) * 0.25 * std::f32::consts::PI;
        (angle.cos(), angle.sin())
    }
}

impl Node for Pan {
    fn set_param(&mut self, param: u32, value: f32) {
        if param == Self::PARAM_POSITION {
            self.set_position(value);
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        let Some((left, right)) = buffer.stereo_mut() else {
            return;
        };
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let (gain_l, gain_r) = Self::gains(self.position.next_value());
            *l *= gain_l;
            *r *= gain_r;
        }
    }
}

/// Silences a signal without stopping it.
///
/// Muting is a ramp to zero, not a branch around the processing: the nodes
/// upstream keep running, so their filters and delays stay warm and unmuting
/// does not restart them mid-tail.
#[derive(Debug, Clone)]
pub struct Mute {
    gain: LinearSmoother,
    muted: bool,
}

impl Mute {
    /// Muted above the halfway mark, open below it.
    ///
    /// Automation carries floats, and a toggle read as `value != 0.0` would
    /// latch on the way through zero if anything ever ramped into it.
    pub const PARAM_MUTED: u32 = 0;

    pub fn new(muted: bool, sample_rate: f32) -> Self {
        let level = if muted { 0.0 } else { 1.0 };
        Self { gain: LinearSmoother::with_duration(level, PARAM_RAMP_MS, sample_rate), muted }
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        self.gain.set_target(if muted { 0.0 } else { 1.0 });
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }
}

impl Node for Mute {
    fn set_param(&mut self, param: u32, value: f32) {
        if param == Self::PARAM_MUTED {
            self.set_muted(value >= 0.5);
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        let (frames, channels) = (buffer.frames(), buffer.channels());
        for frame in 0..frames {
            let gain = self.gain.next_value();
            for channel in 0..channels {
                buffer.channel_mut(channel)[frame] *= gain;
            }
        }
    }
}

/// How many tracks are soloed, shared by every [`Solo`] on one graph.
///
/// Solo is the one control a track cannot answer on its own: a track is never
/// "un-soloed", it is silenced *because another track is soloed*. Something has
/// to hold that fact for all of them, and this is it.
///
/// One bus per graph. A graph built to replace another gets its own, since its
/// nodes are new; the control side carries the solo state across by building
/// the new graph with the same tracks soloed.
///
/// It is an atomic because a `Graph` crosses to the audio thread, so everything
/// in it must be `Send`. Only one thread reads or writes it at a time, so the
/// ordering can be the cheapest one.
#[derive(Debug, Clone, Default)]
pub struct SoloBus(Arc<AtomicU32>);

impl SoloBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether anything is soloed, which is what decides the other tracks.
    pub fn any(&self) -> bool {
        self.count() > 0
    }

    pub fn count(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Silences a track unless it, or nothing at all, is soloed.
///
/// Like [`Mute`], this is a ramp rather than a branch around the processing:
/// the nodes upstream keep running, so releasing a solo does not restart a
/// filter or cut off a tail that was decaying behind it. That is what makes a
/// solo safe to press and let go while something is playing.
///
/// Soloing is not exclusive — several tracks can be soloed and they all play.
/// Nor does it override a mute: with both nodes in series their gains multiply,
/// so a track the user muted stays silent when soloed, which is what a mixer
/// does.
#[derive(Debug)]
pub struct Solo {
    bus: SoloBus,
    soloed: bool,
    /// Whether this track is currently audible, to spot a real change: a
    /// smoother restarts its ramp on every `set_target`, so retargeting each
    /// block would crawl towards the value and never land on it.
    open: bool,
    /// False until the first block, where the bus is finally complete.
    settled: bool,
    gain: LinearSmoother,
    ramp_len: u32,
}

impl Solo {
    /// Soloed above the halfway mark, not below it.
    pub const PARAM_SOLOED: u32 = 0;

    pub fn new(bus: &SoloBus, soloed: bool, sample_rate: f32) -> Self {
        let ramp_len = (PARAM_RAMP_MS * 1e-3 * sample_rate).round().max(0.0) as u32;
        if soloed {
            bus.0.fetch_add(1, Ordering::Relaxed);
        }
        Self {
            bus: bus.clone(),
            soloed,
            open: true,
            settled: false,
            gain: LinearSmoother::new(1.0, ramp_len),
            ramp_len,
        }
    }

    pub fn set_soloed(&mut self, soloed: bool) {
        // Guarded rather than unconditional: the bus counts tracks, so writing
        // the same state twice would count this one twice and leave every other
        // track silenced after it is released.
        if soloed == self.soloed {
            return;
        }
        self.soloed = soloed;
        if soloed {
            self.bus.0.fetch_add(1, Ordering::Relaxed);
        } else {
            self.bus.0.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub fn is_soloed(&self) -> bool {
        self.soloed
    }

    pub fn bus(&self) -> &SoloBus {
        &self.bus
    }
}

impl Drop for Solo {
    /// Hands the count back, so a graph retired while one of its tracks was
    /// soloed cannot leave a bus it shares stuck above zero.
    fn drop(&mut self) {
        if self.soloed {
            self.bus.0.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Node for Solo {
    fn process(&mut self, buffer: &mut AudioBuffer<'_>) {
        let open = self.soloed || !self.bus.any();
        if !self.settled {
            // The bus only finishes counting once every track has been built,
            // which is after this node was. The first block is the earliest
            // point the starting value can be known — and starting at the wrong
            // one would let a soloed-out track through for the length of a ramp.
            self.gain = LinearSmoother::new(if open { 1.0 } else { 0.0 }, self.ramp_len);
            self.settled = true;
        } else if open != self.open {
            self.gain.set_target(if open { 1.0 } else { 0.0 });
        }
        self.open = open;

        let (frames, channels) = (buffer.frames(), buffer.channels());
        for frame in 0..frames {
            let gain = self.gain.next_value();
            for channel in 0..channels {
                buffer.channel_mut(channel)[frame] *= gain;
            }
        }
    }

    fn reset(&mut self) {
        // Re-resolve against the bus rather than ramping from wherever playback
        // stopped, the same way a fresh graph does.
        self.settled = false;
    }

    fn set_param(&mut self, param: u32, value: f32) {
        if param == Self::PARAM_SOLOED {
            self.set_soloed(value >= 0.5);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    /// The ramp covers full scale in `PARAM_RAMP_MS`, so this is its steepest step.
    const MAX_RAMP_STEP: f32 = 1.0 / (PARAM_RAMP_MS * 1e-3 * SR) * 1.001;

    fn run(node: &mut impl Node, data: &mut [f32], channels: usize) {
        let mut buffer = AudioBuffer::new(data, channels);
        node.process(&mut buffer);
    }

    #[test]
    fn a_settled_gain_scales_exactly() {
        let mut gain = Gain::new(0.5, SR);
        let mut data = [1.0; 8];
        run(&mut gain, &mut data, 2);
        assert!(data.iter().all(|s| *s == 0.5), "{data:?}");
    }

    #[test]
    fn a_gain_change_ramps_instead_of_stepping() {
        let mut gain = Gain::new(1.0, SR);
        let mut data = [1.0; 960];
        gain.set_gain(0.0);
        run(&mut gain, &mut data, 1);
        for (i, w) in data.windows(2).enumerate() {
            assert!((w[1] - w[0]).abs() <= MAX_RAMP_STEP, "step at {i}: {} -> {}", w[0], w[1]);
        }
        assert_eq!(data[959], 0.0, "the ramp never arrived");
    }

    #[test]
    fn both_channels_of_a_frame_get_the_same_gain() {
        // Advancing the ramp per channel instead of per frame would tilt the
        // image every time the gain moved.
        let mut gain = Gain::new(1.0, SR);
        gain.set_gain(0.0);
        let mut data = [1.0; 64];
        run(&mut gain, &mut data, 2);
        let (left, right) = data.split_at(32);
        assert_eq!(left, right, "the channels drifted apart during the ramp");
    }

    #[test]
    fn panning_holds_its_power_across_the_image() {
        for step in 0..=20 {
            let position = -1.0 + step as f32 / 10.0;
            let (left, right) = Pan::gains(position);
            let power = left * left + right * right;
            assert!((power - 1.0).abs() < 1e-6, "power {power} at position {position}");
        }
    }

    #[test]
    fn the_centre_is_three_db_down_not_unity() {
        // The whole point of the law: a linear pan reads 1.0 here, so a sound
        // crossing the middle would bump by 3 dB.
        let (left, right) = Pan::gains(0.0);
        assert!((left - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6, "left {left}");
        assert!((right - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6, "right {right}");
    }

    #[test]
    fn hard_left_silences_the_right() {
        let (left, right) = Pan::gains(-1.0);
        assert!((left - 1.0).abs() < 1e-6);
        assert!(right.abs() < 1e-6);
    }

    #[test]
    fn a_mono_buffer_passes_through_a_pan_untouched() {
        let mut pan = Pan::new(-1.0, SR);
        let mut data = [1.0; 4];
        run(&mut pan, &mut data, 1);
        assert_eq!(data, [1.0; 4]);
    }

    #[test]
    fn muting_fades_rather_than_cutting() {
        let mut mute = Mute::new(false, SR);
        let mut data = [1.0; 960];
        mute.set_muted(true);
        run(&mut mute, &mut data, 1);
        assert!(data.windows(2).all(|w| (w[1] - w[0]).abs() <= MAX_RAMP_STEP));
        assert_eq!(data[959], 0.0);
        assert!(mute.is_muted());
    }

    /// Runs one block of unit signal through a track's solo and reports the
    /// level it settled at.
    fn solo_level(solo: &mut Solo, blocks: usize) -> f32 {
        let mut last = 0.0;
        for _ in 0..blocks {
            let mut data = [1.0; 960];
            run(solo, &mut data, 1);
            last = data[959];
        }
        last
    }

    #[test]
    fn nothing_soloed_leaves_every_track_open() {
        let bus = SoloBus::new();
        let mut a = Solo::new(&bus, false, SR);
        let mut b = Solo::new(&bus, false, SR);
        assert!(!bus.any());
        assert_eq!(solo_level(&mut a, 1), 1.0);
        assert_eq!(solo_level(&mut b, 1), 1.0);
    }

    #[test]
    fn soloing_one_track_silences_the_others() {
        let bus = SoloBus::new();
        let mut soloed = Solo::new(&bus, true, SR);
        let mut other = Solo::new(&bus, false, SR);

        assert_eq!(bus.count(), 1);
        assert_eq!(solo_level(&mut soloed, 1), 1.0, "the soloed track should be the one you hear");
        assert_eq!(solo_level(&mut other, 1), 0.0, "the other track should have been silenced");
    }

    #[test]
    fn a_track_silenced_by_solo_starts_that_way() {
        // The bus is not finished counting when a node is built, so the value
        // it starts on can only be resolved on the first block. Getting this
        // wrong lets a soloed-out track through for a whole ramp.
        let bus = SoloBus::new();
        let _soloed = Solo::new(&bus, true, SR);
        let mut other = Solo::new(&bus, false, SR);

        let mut data = [1.0; 64];
        run(&mut other, &mut data, 1);
        assert!(data.iter().all(|s| *s == 0.0), "leaked at the start: {:?}", &data[..4]);
    }

    #[test]
    fn releasing_the_last_solo_reopens_everything() {
        let bus = SoloBus::new();
        let mut soloed = Solo::new(&bus, true, SR);
        let mut other = Solo::new(&bus, false, SR);
        assert_eq!(solo_level(&mut other, 1), 0.0);

        soloed.set_soloed(false);
        assert!(!bus.any());
        assert_eq!(solo_level(&mut other, 2), 1.0, "the track never came back");
    }

    #[test]
    fn several_tracks_can_be_soloed_at_once() {
        // Solo is not exclusive: pressing a second one adds to the first
        // rather than stealing it.
        let bus = SoloBus::new();
        let mut first = Solo::new(&bus, true, SR);
        let mut second = Solo::new(&bus, false, SR);
        let mut rest = Solo::new(&bus, false, SR);

        second.set_soloed(true);
        assert_eq!(bus.count(), 2);
        assert_eq!(solo_level(&mut first, 2), 1.0);
        assert_eq!(solo_level(&mut second, 2), 1.0);
        assert_eq!(solo_level(&mut rest, 2), 0.0);
    }

    #[test]
    fn soloing_twice_does_not_count_twice() {
        // The bus counts tracks, not presses. A double count would leave every
        // other track silenced after this one is released.
        let bus = SoloBus::new();
        let mut track = Solo::new(&bus, false, SR);
        track.set_soloed(true);
        track.set_soloed(true);
        assert_eq!(bus.count(), 1);
        track.set_soloed(false);
        assert_eq!(bus.count(), 0, "the bus never came back to zero");
    }

    #[test]
    fn a_retired_track_hands_its_solo_back() {
        let bus = SoloBus::new();
        let track = Solo::new(&bus, true, SR);
        assert_eq!(bus.count(), 1);
        drop(track);
        assert_eq!(bus.count(), 0, "a dropped track left the bus stuck above zero");
    }

    #[test]
    fn solo_fades_rather_than_cutting() {
        let bus = SoloBus::new();
        let mut other = Solo::new(&bus, false, SR);
        let mut soloed = Solo::new(&bus, false, SR);
        // Settle both while everything is open.
        assert_eq!(solo_level(&mut other, 1), 1.0);

        soloed.set_soloed(true);
        let mut data = [1.0; 960];
        run(&mut other, &mut data, 1);
        for (i, w) in data.windows(2).enumerate() {
            assert!((w[1] - w[0]).abs() <= MAX_RAMP_STEP, "step at {i}: {} -> {}", w[0], w[1]);
        }
        assert_eq!(data[959], 0.0, "the fade never arrived");
    }

    #[test]
    fn a_muted_track_stays_silent_when_soloed() {
        // The two nodes sit in series, so their gains multiply. A mixer works
        // the same way: soloing something you muted does not unmute it.
        let bus = SoloBus::new();
        let mut mute = Mute::new(true, SR);
        let mut solo = Solo::new(&bus, true, SR);

        let mut data = [1.0; 960];
        run(&mut mute, &mut data, 1);
        run(&mut solo, &mut data, 1);
        assert_eq!(data[959], 0.0);
    }

    #[test]
    fn solo_is_automatable() {
        let bus = SoloBus::new();
        let mut track = Solo::new(&bus, false, SR);
        track.set_param(Solo::PARAM_SOLOED, 1.0);
        assert!(track.is_soloed());
        track.set_param(999, 0.0);
        assert!(track.is_soloed(), "an unknown parameter released the solo");
        track.set_param(Solo::PARAM_SOLOED, 0.0);
        assert!(!track.is_soloed());
    }

    #[test]
    fn resetting_re_resolves_against_the_bus() {
        let bus = SoloBus::new();
        let mut track = Solo::new(&bus, false, SR);
        assert_eq!(solo_level(&mut track, 1), 1.0);

        let _soloed = Solo::new(&bus, true, SR);
        track.reset();
        let mut data = [1.0; 64];
        run(&mut track, &mut data, 1);
        assert!(data.iter().all(|s| *s == 0.0), "reset ramped instead of landing on the answer");
    }
}
