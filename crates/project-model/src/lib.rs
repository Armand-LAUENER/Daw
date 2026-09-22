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

mod asset;
mod command;
mod file;
mod history;
mod time;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

pub use asset::{Asset, AssetId};
pub use command::{Command, CommandError};
pub use file::FileError;
pub use history::Session;
pub use time::{Meter, PPQ, Tempo, Tick};

/// Which version of the file format this build writes.
///
/// Here from the first commit rather than from the first time a file is
/// written. Adding a version to a format that already has files in the wild
/// means guessing what the unversioned ones were, and that guess is always
/// wrong for somebody.
pub const FORMAT_VERSION: u32 = 2;

/// Identifies a track for the life of a project.
///
/// Never reused, even after the track is gone (decision D2): a reference that
/// outlived its track must find nothing, not a stranger that took its number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrackId(pub u32);

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
    /// The track's file, which it keeps whether or not it is playing it.
    File,
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
    /// What the track plays.
    pub source: Source,
    /// The file the track holds, kept whether or not it is the thing being
    /// played. Separating the two is what lets a person listen to the test tone
    /// and come back to their audio instead of having lost it.
    ///
    /// Named by content rather than by path: the path lives in the project's
    /// pool, where it can be corrected once for every track that uses it.
    pub file: Option<AssetId>,
}

impl Track {
    pub fn new(id: TrackId, name: impl Into<String>, source: Source, file: Option<AssetId>) -> Self {
        Self { id, name: name.into(), gain: 1.0, pan: 0.0, muted: false, soloed: false, source, file }
    }
}

/// Everything a project holds.
///
/// Read through accessors and changed only through a [`Session`], so the
/// tables and the order that indexes them cannot drift apart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    tempo: Tempo,
    meter: Meter,
    master_gain: f32,
    /// The mixer's order. Every entry names a track in `tracks`, and every
    /// track appears here once.
    track_order: Vec<TrackId>,
    tracks: BTreeMap<TrackId, Track>,
    /// Every file a track holds, and nothing else: an asset no track uses is
    /// removed with the last track that did.
    assets: BTreeMap<AssetId, Asset>,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            tempo: Tempo::default(),
            meter: Meter::default(),
            master_gain: 1.0,
            track_order: Vec::new(),
            tracks: BTreeMap::new(),
            assets: BTreeMap::new(),
        }
    }
}

impl Project {
    pub fn new() -> Self {
        Self::default()
    }

    /// One tempo for the whole project. A map of changes comes with the
    /// timeline; until then there is nothing to place a change on.
    pub fn tempo(&self) -> Tempo {
        self.tempo
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

    /// The file a track holds, if it holds one.
    pub fn file_of(&self, track: &Track) -> Option<&Asset> {
        self.asset(track.file?)
    }

    /// A short description of what a track is playing, for the mixer.
    pub fn playing(&self, track: &Track) -> String {
        match track.source {
            Source::Silent => "silent".into(),
            Source::Tone => "tone".into(),
            Source::File => self.file_of(track).map_or_else(|| "no file".into(), Asset::name),
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

    /// How many tracks hold a given file.
    fn uses_of(&self, asset: AssetId) -> usize {
        self.tracks.values().filter(|t| t.file == Some(asset)).count()
    }

    /// Checks everything a project promises about itself.
    ///
    /// Commands keep these true one at a time; this is the whole-project
    /// statement of them, for a file read from disk — which anybody could have
    /// edited — and for the tests that prove no sequence of commands breaks
    /// them.
    pub fn check(&self) -> Result<(), Invalid> {
        if !time::valid(self.tempo, self.meter) {
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
            if let Some(file) = track.file
                && !self.assets.contains_key(&file)
            {
                return Err(Invalid::MissingAsset(file));
            }
        }
        for (key, asset) in &self.assets {
            if asset.id != *key {
                return Err(Invalid::MisfiledAsset(*key));
            }
            if self.uses_of(*key) == 0 {
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
    /// A track holds a file the pool does not have.
    MissingAsset(AssetId),
    /// The pool holds a file no track uses.
    UnusedAsset(AssetId),
    /// A file is filed under a hash that is not its own.
    MisfiledAsset(AssetId),
    /// A value that must be a finite number in range is not.
    OutOfRange(&'static str),
    /// A track's id is not below the counter that hands them out, so a later
    /// track would be given the same one.
    IdAhead(TrackId),
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Order => write!(f, "the track order does not match the tracks"),
            Self::MisfiledTrack(TrackId(id)) => write!(f, "track {id} is filed under another id"),
            Self::MissingAsset(id) => write!(f, "a track uses file {id}, which the project does not list"),
            Self::UnusedAsset(id) => write!(f, "file {id} is listed but no track uses it"),
            Self::MisfiledAsset(id) => write!(f, "file {id} is filed under another hash"),
            Self::OutOfRange(what) => write!(f, "{what} is not a valid number"),
            Self::IdAhead(TrackId(id)) => write!(f, "track {id} is past the id counter"),
        }
    }
}

impl std::error::Error for Invalid {}
