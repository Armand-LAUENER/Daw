//! The project as it is edited and saved — not as it is played.
//!
//! Deliberately outside the `engine-*` family. The engine's state lives in the
//! audio thread's graph and changes sample by sample; this is what a person
//! edits, undoes and stores on disk. Keeping them as one structure is the
//! mistake this crate exists to avoid: they have different lifetimes, different
//! threads and different definitions of "correct".
//!
//! Nothing here allocates on a deadline, locks, or knows what a sample is. It
//! is plain data and the edits that move it from one state to another.
//!
//! The only way to change a [`Project`] is a [`Command`]. That is not
//! ceremony: it is what makes undo possible, and it is the seam a feature is
//! added at later. One mechanism, two jobs.

mod command;
mod history;

use std::path::PathBuf;

pub use command::{Command, CommandError};
pub use history::Session;

/// Which version of the format a project was written in.
///
/// Here from the first commit rather than from the first time a file is
/// written. Adding a version to a format that already has files in the wild
/// means guessing what the unversioned ones were, and that guess is always
/// wrong for somebody.
pub const FORMAT_VERSION: u32 = 1;

/// Identifies a track for the life of a project.
///
/// Stable across undo and redo, so a command recorded now still names the same
/// track after the history has been walked back and forth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrackId(pub u32);

/// Where a track's audio comes from.
///
/// The model names the source but does not hold it: decoding belongs to
/// whoever has a sample rate to decode for, and a project should survive being
/// opened on a machine where the file has moved.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Source {
    /// Nothing. What an empty track plays, and what a track plays once its
    /// file has been taken off it.
    #[default]
    Silent,
    /// A test tone, asked for on purpose.
    ///
    /// Useful for hearing that a fader or a pan is doing what it says without
    /// loading anything, which is what it was for before files worked. Not a
    /// default any more: a track that buzzes until told otherwise is a track
    /// nobody wants.
    Tone,
    /// Audio read from a file.
    File { path: PathBuf },
}

impl Source {
    /// A short name for the mixer, taken from the file rather than the path.
    pub fn label(&self) -> String {
        match self {
            Self::Silent => "silent".into(),
            Self::Tone => "tone".into(),
            Self::File { path } => {
                path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
            }
        }
    }
}

/// One track's settings. What the mixer shows, and what a graph is built from.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub id: TrackId,
    pub name: String,
    /// Linear, not decibels: this is what multiplies a sample.
    pub gain: f32,
    /// -1 hard left, 0 centre, 1 hard right.
    pub pan: f32,
    pub muted: bool,
    pub soloed: bool,
    pub source: Source,
}

impl Track {
    pub fn new(id: TrackId, name: impl Into<String>, source: Source) -> Self {
        Self { id, name: name.into(), gain: 1.0, pan: 0.0, muted: false, soloed: false, source }
    }
}

/// Everything a project holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub version: u32,
    pub tracks: Vec<Track>,
    pub master_gain: f32,
}

impl Default for Project {
    fn default() -> Self {
        Self { version: FORMAT_VERSION, tracks: Vec::new(), master_gain: 1.0 }
    }
}

impl Project {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    pub fn index_of(&self, id: TrackId) -> Option<usize> {
        self.tracks.iter().position(|t| t.id == id)
    }

    /// Whether anything is soloed, which is what decides the other tracks.
    pub fn any_soloed(&self) -> bool {
        self.tracks.iter().any(|t| t.soloed)
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

    /// The id a new track would take.
    ///
    /// Derived from what exists rather than kept in a counter. A counter would
    /// be one more piece of state for undo to restore, and one more way for it
    /// to be restored wrongly; there is nothing to restore here.
    fn next_id(&self) -> TrackId {
        TrackId(self.tracks.iter().map(|t| t.id.0).max().map_or(0, |max| max + 1))
    }
}
