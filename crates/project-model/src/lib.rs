//! The project as it is edited and saved — not as it is played.
//!
//! Deliberately outside the `engine-*` family. The engine's state lives in the
//! audio thread's graph and changes sample by sample; this is what a person
//! edits, undoes and stores on disk. Keeping them as one structure is the
//! mistake this crate exists to avoid: they have different lifetimes, different
//! threads and different definitions of "correct".
//!
//! Nothing here allocates on a deadline, locks, or knows what a sample is. It
//! is plain data, the edits that move it from one state to another, and the
//! file it is kept in.
//!
//! The only way to change a [`Project`] is a [`Command`]. That is not
//! ceremony: it is what makes undo possible, and it is the seam a feature is
//! added at later. One mechanism, two jobs.
//!
//! Entities live in tables keyed by identifier, and an order is a separate
//! list of identifiers where order means something (decision D4 in the plan).
//! Moving a track is then one line of a saved file rather than a rewritten
//! block, and a reference is always by name, never by position.

mod arrangement;
mod asset;
mod bus;
mod command;
mod effects;
mod eq;
mod file;
mod history;
mod id;
mod instrument;
mod time;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

pub use arrangement::{AudioWindow, Clip, ClipId, Content, NewClip, Note, NoteId, Placement, PlacementId, Window};
pub use asset::{Asset, AssetId};
pub use bus::{Bus, BusId, SEND};
pub use command::{Command, CommandError};
pub use effects::{CompSettings, DelaySettings, ReverbSettings};
pub use eq::{BANDS, BandShape, EqBand, default_eq};
pub use file::FileError;
pub use history::Session;
pub use instrument::{Instrument, Wave};
pub use time::{Meter, PPQ, Tempo, TempoChange, TempoMap, Tick};

/// Which version of the file format this build writes.
///
/// Here from the first commit rather than from the first time a file is
/// written. Adding a version to a format that already has files in the wild
/// means guessing what the unversioned ones were, and that guess is always
/// wrong for somebody.
pub const FORMAT_VERSION: u32 = 9;

/// Identifies a track for the life of a project.
///
/// Never reused, even after the track is gone (decision D2): a reference that
/// outlived its track must find nothing, not a stranger that took its number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct TrackId(pub u32);

id::deserialize_as_key!(TrackId);

/// Where a track's audio comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Nothing. What an empty track plays.
    #[default]
    Silent,
    /// A test tone, asked for on purpose. Useful for hearing what a fader or a
    /// pan does without loading anything.
    Tone,
    /// The clips placed on it.
    Clips,
}

/// One track's settings. What the mixer shows, and what a graph is built from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub name: String,
    /// Linear, not decibels: this is what multiplies a sample.
    pub gain: f32,
    /// -1 hard left, 0 centre, 1 hard right.
    pub pan: f32,
    pub muted: bool,
    pub soloed: bool,
    /// What the track plays. Switching to the tone and back leaves its clips
    /// where they are, which is what lets a person hear what a setting does
    /// and come back to their material.
    pub source: Source,
    /// Applied before the fader, in band order.
    pub eq: [EqBand; BANDS],
    /// After the equaliser, in this order: dynamics first, then the space.
    pub comp: CompSettings,
    pub delay: DelaySettings,
    pub reverb: ReverbSettings,
    /// How much of the track each bus hears, taken after its fader, pan, mute
    /// and solo — so the track's reverb follows the track.
    pub sends: BTreeMap<BusId, f32>,
    /// What its MIDI clips are played with.
    pub instrument: Instrument,
}

impl Track {
    pub fn new(id: TrackId, name: impl Into<String>, source: Source) -> Self {
        Self {
            id,
            name: name.into(),
            gain: 1.0,
            pan: 0.0,
            muted: false,
            soloed: false,
            source,
            eq: default_eq(),
            comp: CompSettings::default(),
            delay: DelaySettings::default(),
            reverb: ReverbSettings::default(),
            sends: BTreeMap::new(),
            instrument: Instrument::default(),
        }
    }
}

/// Everything a project holds.
///
/// Read through accessors and changed only through a [`Session`], so the
/// tables and the order that indexes them cannot drift apart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    tempo_map: TempoMap,
    meter: Meter,
    master_gain: f32,
    /// The mixer's order. Every entry names a track in `tracks`, and every
    /// track appears here once.
    track_order: Vec<TrackId>,
    tracks: BTreeMap<TrackId, Track>,
    /// Every clip that is placed somewhere, and nothing else: a clip is removed
    /// with its last placement.
    clips: BTreeMap<ClipId, Clip>,
    placements: BTreeMap<PlacementId, Placement>,
    /// Every file an audio clip plays, and nothing else: an asset is removed
    /// with the last clip that used it.
    assets: BTreeMap<AssetId, Asset>,
    /// The mixer's order of buses, under the tracks.
    bus_order: Vec<BusId>,
    buses: BTreeMap<BusId, Bus>,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            tempo_map: TempoMap::default(),
            meter: Meter::default(),
            master_gain: 1.0,
            track_order: Vec::new(),
            tracks: BTreeMap::new(),
            clips: BTreeMap::new(),
            placements: BTreeMap::new(),
            assets: BTreeMap::new(),
            bus_order: Vec::new(),
            buses: BTreeMap::new(),
        }
    }
}

impl Project {
    pub fn new() -> Self {
        Self::default()
    }

    /// The tempo the project starts at.
    pub fn tempo(&self) -> Tempo {
        self.tempo_map.start()
    }

    /// How the tempo moves through the project.
    pub fn tempo_map(&self) -> &TempoMap {
        &self.tempo_map
    }

    pub fn meter(&self) -> Meter {
        self.meter
    }

    pub fn master_gain(&self) -> f32 {
        self.master_gain
    }

    /// The tracks, in the mixer's order.
    pub fn tracks(&self) -> impl DoubleEndedIterator<Item = &Track> + ExactSizeIterator {
        self.track_order.iter().map(|id| &self.tracks[id])
    }

    /// The buses, in the mixer's order.
    pub fn buses(&self) -> impl DoubleEndedIterator<Item = &Bus> + ExactSizeIterator {
        self.bus_order.iter().map(|id| &self.buses[id])
    }

    pub fn bus(&self, id: BusId) -> Option<&Bus> {
        self.buses.get(&id)
    }

    pub fn position_of_bus(&self, id: BusId) -> Option<usize> {
        self.bus_order.iter().position(|b| *b == id)
    }

    pub fn track_count(&self) -> usize {
        self.track_order.len()
    }

    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks.get(&id)
    }

    /// Where a track sits in the mixer.
    pub fn position_of(&self, id: TrackId) -> Option<usize> {
        self.track_order.iter().position(|t| *t == id)
    }

    /// Every file the project uses.
    pub fn assets(&self) -> impl ExactSizeIterator<Item = &Asset> {
        self.assets.values()
    }

    pub fn asset(&self, id: AssetId) -> Option<&Asset> {
        self.assets.get(&id)
    }

    pub fn clips(&self) -> impl ExactSizeIterator<Item = &Clip> {
        self.clips.values()
    }

    pub fn clip(&self, id: ClipId) -> Option<&Clip> {
        self.clips.get(&id)
    }

    pub fn placements(&self) -> impl ExactSizeIterator<Item = &Placement> {
        self.placements.values()
    }

    pub fn placement(&self, id: PlacementId) -> Option<&Placement> {
        self.placements.get(&id)
    }

    /// A track's placements, earliest first; ties by id, so the order is the
    /// same every time it is asked.
    pub fn placements_on(&self, track: TrackId) -> Vec<&Placement> {
        let mut on: Vec<_> = self.placements.values().filter(|p| p.track == track).collect();
        on.sort_by_key(|p| (p.start, p.id));
        on
    }

    /// How many placements play a clip: more than one is a linked clip.
    pub fn uses_of_clip(&self, clip: ClipId) -> usize {
        self.placements.values().filter(|p| p.clip == clip).count()
    }

    /// A short description of what a track is playing, for the mixer.
    pub fn playing(&self, track: &Track) -> String {
        match track.source {
            Source::Silent => "silent".into(),
            Source::Tone => "tone".into(),
            Source::Clips => {
                let on = self.placements_on(track.id);
                match on.as_slice() {
                    [] => "empty".into(),
                    [one] => self.clip(one.clip).map_or_else(|| "clip".into(), |c| c.name.clone()),
                    many => format!("{} clips", many.len()),
                }
            }
        }
    }

    /// Whether anything is soloed, which is what decides the other tracks.
    pub fn any_soloed(&self) -> bool {
        self.tracks.values().any(|t| t.soloed)
    }

    /// Whether a track is actually heard, taking the whole project into account.
    ///
    /// A track is silent if it is muted, or if something else is soloed and it
    /// is not. Resolved here rather than per track, because a track cannot
    /// answer it alone.
    pub fn is_audible(&self, id: TrackId) -> bool {
        let Some(track) = self.track(id) else {
            return false;
        };
        !track.muted && (track.soloed || !self.any_soloed())
    }

    /// How many clips play a given file.
    pub(crate) fn uses_of_asset(&self, asset: AssetId) -> usize {
        self.clips.values().filter(|c| c.asset() == Some(asset)).count()
    }

    /// Checks everything a project promises about itself.
    ///
    /// Commands keep these true one at a time; this is the whole-project
    /// statement of them, for a file read from disk — which anybody could have
    /// edited — and for the tests that prove no sequence of commands breaks
    /// them.
    pub fn check(&self) -> Result<(), Invalid> {
        if !(self.tempo_map.is_valid() && time::meter_valid(self.meter)) {
            return Err(Invalid::OutOfRange("the tempo or time signature"));
        }
        if !self.master_gain.is_finite() {
            return Err(Invalid::OutOfRange("the master gain"));
        }
        if self.track_order.len() != self.tracks.len() {
            return Err(Invalid::Order);
        }
        let mut seen = BTreeSet::new();
        for id in &self.track_order {
            if !self.tracks.contains_key(id) || !seen.insert(*id) {
                return Err(Invalid::Order);
            }
        }
        for (key, track) in &self.tracks {
            if track.id != *key {
                return Err(Invalid::MisfiledTrack(*key));
            }
            if !track.gain.is_finite() {
                return Err(Invalid::OutOfRange("a track's gain"));
            }
            if !(-1.0..=1.0).contains(&track.pan) {
                return Err(Invalid::OutOfRange("a pan position"));
            }
            if !track.eq.iter().all(EqBand::is_valid) {
                return Err(Invalid::OutOfRange("an equaliser band"));
            }
            if !(track.comp.is_valid() && track.delay.is_valid() && track.reverb.is_valid()) {
                return Err(Invalid::OutOfRange("an effect's settings"));
            }
            if !track.instrument.is_valid() {
                return Err(Invalid::OutOfRange("an instrument's settings"));
            }
            for (bus, level) in &track.sends {
                if !self.buses.contains_key(bus) {
                    return Err(Invalid::Dangling("bus", bus.0));
                }
                if !SEND.contains(level) {
                    return Err(Invalid::OutOfRange("a send level"));
                }
            }
        }
        for (key, clip) in &self.clips {
            if clip.id != *key {
                return Err(Invalid::Misfiled("clip", key.0));
            }
            if self.uses_of_clip(*key) == 0 {
                return Err(Invalid::Unused("clip", key.0));
            }
            match &clip.content {
                Content::Audio { asset } => {
                    if !self.assets.contains_key(asset) {
                        return Err(Invalid::MissingAsset(*asset));
                    }
                }
                Content::Midi { length, notes } => {
                    if length.0 == 0 {
                        return Err(Invalid::OutOfRange("a clip's length"));
                    }
                    if notes.values().any(|n| !n.is_valid() || n.end() > *length) {
                        return Err(Invalid::NoteOutside(*key));
                    }
                }
            }
        }
        for (key, placement) in &self.placements {
            if placement.id != *key {
                return Err(Invalid::Misfiled("placement", key.0));
            }
            if !self.tracks.contains_key(&placement.track) {
                return Err(Invalid::Dangling("track", placement.track.0));
            }
            let Some(clip) = self.clips.get(&placement.clip) else {
                return Err(Invalid::Dangling("clip", placement.clip.0));
            };
            if let Some(window) = placement.window {
                let fits =
                    clip.length().is_some_and(|l| window.length.0 > 0 && window.offset.0 + window.length.0 <= l.0);
                if !fits {
                    return Err(Invalid::WindowOutside(*key));
                }
            }
            // An audio window: on an audio clip, and not empty. Whether it
            // ends inside the file is for whoever has read the file to say.
            if let Some(audio) = placement.audio
                && (clip.asset().is_none() || audio.length == 0 || audio.rate == 0)
            {
                return Err(Invalid::WindowOutside(*key));
            }
        }
        if self.bus_order.len() != self.buses.len() {
            return Err(Invalid::Order);
        }
        let mut seen = BTreeSet::new();
        for id in &self.bus_order {
            if !self.buses.contains_key(id) || !seen.insert(*id) {
                return Err(Invalid::Order);
            }
        }
        for (key, bus) in &self.buses {
            if bus.id != *key {
                return Err(Invalid::Misfiled("bus", key.0));
            }
            if !bus.is_valid() {
                return Err(Invalid::OutOfRange("a bus's settings"));
            }
        }
        for (key, asset) in &self.assets {
            if asset.id != *key {
                return Err(Invalid::MisfiledAsset(*key));
            }
            if self.uses_of_asset(*key) == 0 {
                return Err(Invalid::UnusedAsset(*key));
            }
        }
        Ok(())
    }
}

/// A promise a project breaks.
#[derive(Debug, Clone, PartialEq)]
pub enum Invalid {
    /// The mixer's order and the track table disagree.
    Order,
    /// A track is filed under an id that is not its own.
    MisfiledTrack(TrackId),
    /// An audio clip plays a file the pool does not have.
    MissingAsset(AssetId),
    /// The pool holds a file no clip uses.
    UnusedAsset(AssetId),
    /// A clip or placement is filed under an id that is not its own.
    Misfiled(&'static str, u32),
    /// A clip no placement plays.
    Unused(&'static str, u32),
    /// A placement names a track or a clip that does not exist.
    Dangling(&'static str, u32),
    /// A note that is invalid, or runs past the end of its clip.
    NoteOutside(ClipId),
    /// A placement's window reaches outside its clip, or windows an audio clip.
    WindowOutside(PlacementId),
    /// A file is filed under a hash that is not its own.
    MisfiledAsset(AssetId),
    /// A value that must be a finite number in range is not.
    OutOfRange(&'static str),
    /// An id is not below the counter that hands them out, so a later object
    /// would be given the same one.
    IdAhead(&'static str, u32),
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Order => write!(f, "the track order does not match the tracks"),
            Self::MisfiledTrack(TrackId(id)) => write!(f, "track {id} is filed under another id"),
            Self::MissingAsset(id) => write!(f, "a clip uses file {id}, which the project does not list"),
            Self::UnusedAsset(id) => write!(f, "file {id} is listed but no clip uses it"),
            Self::Misfiled(what, id) => write!(f, "{what} {id} is filed under another id"),
            Self::Unused(what, id) => write!(f, "{what} {id} is listed but nothing uses it"),
            Self::Dangling(what, id) => write!(f, "a placement names {what} {id}, which does not exist"),
            Self::NoteOutside(ClipId(id)) => write!(f, "clip {id} has a note that is invalid or past its end"),
            Self::WindowOutside(PlacementId(id)) => write!(f, "placement {id} plays outside its clip"),
            Self::MisfiledAsset(id) => write!(f, "file {id} is filed under another hash"),
            Self::OutOfRange(what) => write!(f, "{what} is not a valid number"),
            Self::IdAhead(what, id) => write!(f, "{what} {id} is past the id counter"),
        }
    }
}

impl std::error::Error for Invalid {}
