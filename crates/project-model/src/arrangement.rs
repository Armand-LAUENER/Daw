//! What plays when: clips, where they are placed, and the notes inside them
//! (decision D3 in the plan).
//!
//! Content and placement are kept apart. A [`Clip`] is material — a file, or a
//! list of notes — and a [`Placement`] puts it on a track at a moment. Two
//! placements naming the same clip are a *linked* clip: editing its notes
//! changes both, which is what a pattern is in other programs. Copying the clip
//! instead gives each placement its own, which is what a region is. The choice
//! is made per object, never by a global mode.
//!
//! Every position and length is in ticks (decision D1). An audio clip has no
//! length in ticks: a recording lasts a fixed time whatever the tempo, so only
//! where it starts is musical.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Asset, AssetId, Tick, TrackId};

/// Identifies a clip for the life of a project. Never reused (decision D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ClipId(pub u32);

crate::id::deserialize_as_key!(ClipId);

/// Identifies a placement for the life of a project. Never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct PlacementId(pub u32);

crate::id::deserialize_as_key!(PlacementId);

/// Identifies a note for the life of a project. Never reused, and unique
/// across clips, so a copied clip's notes are new notes rather than aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct NoteId(pub u32);

crate::id::deserialize_as_key!(NoteId);

/// A note in a MIDI clip, positioned from the clip's start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    /// MIDI note number, 0..=127.
    pub pitch: u8,
    /// MIDI velocity, 1..=127. Zero is not a quiet note but, in MIDI, a
    /// note-off, so it is refused rather than kept as a trap.
    pub velocity: u8,
    pub start: Tick,
    /// At least one tick: a note of no length never sounds and cannot be
    /// selected.
    pub length: Tick,
}

impl Note {
    pub fn new(pitch: u8, velocity: u8, start: Tick, length: Tick) -> Self {
        Self { pitch, velocity, start, length }
    }

    pub fn end(&self) -> Tick {
        Tick(self.start.0 + self.length.0)
    }

    /// Whether the note makes sense on its own, before asking whether it fits
    /// its clip.
    pub(crate) fn is_valid(&self) -> bool {
        self.pitch <= 127 && (1..=127).contains(&self.velocity) && self.length.0 > 0
    }
}

/// What a clip holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Content {
    /// A file from the project's pool, played from its start.
    Audio { asset: AssetId },
    /// Notes, each inside `0..length`.
    Midi { length: Tick, notes: BTreeMap<NoteId, Note> },
}

/// Material that can be placed on the timeline, once or many times.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: ClipId,
    pub name: String,
    pub content: Content,
}

impl Clip {
    /// The file it plays, for an audio clip.
    pub fn asset(&self) -> Option<AssetId> {
        match self.content {
            Content::Audio { asset } => Some(asset),
            Content::Midi { .. } => None,
        }
    }

    /// The length of a MIDI clip. An audio clip lasts as long as its file,
    /// which is a matter of seconds, not ticks.
    pub fn length(&self) -> Option<Tick> {
        match self.content {
            Content::Midi { length, .. } => Some(length),
            Content::Audio { .. } => None,
        }
    }

    pub fn notes(&self) -> Option<&BTreeMap<NoteId, Note>> {
        match &self.content {
            Content::Midi { notes, .. } => Some(notes),
            Content::Audio { .. } => None,
        }
    }
}

/// The part of a MIDI clip a placement plays: from `offset` into the clip, for
/// `length`. What trimming the ends of a clip on the timeline changes, without
/// touching the clip that other placements may share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub offset: Tick,
    pub length: Tick,
}

/// A clip put on a track at a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub id: PlacementId,
    pub clip: ClipId,
    pub track: TrackId,
    pub start: Tick,
    /// `None` plays the whole clip. Only a MIDI clip can be windowed: an audio
    /// clip has no length in ticks to window.
    pub window: Option<Window>,
}

/// A clip made together with the track it goes on, placed at the start.
///
/// What a file dropped onto the window becomes: one command, so one undo takes
/// the track and its material back together.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NewClip {
    Audio {
        asset: Asset,
    },
    /// Notes from the clip's start; each must fit inside `length`.
    Midi {
        length: Tick,
        notes: Vec<Note>,
    },
}
