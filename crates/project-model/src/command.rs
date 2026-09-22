use std::fmt;

use serde::{Deserialize, Serialize};

use crate::history::Ids;
use crate::{Asset, AssetId, Project, Source, Track, TrackId};

/// A change someone asked for.
///
/// The whole editing surface. A feature added later is a variant here plus the
/// arm that resolves it — which is also what makes it undoable, for free.
///
/// Serializable although nothing writes a journal of them yet (decision D8):
/// commands replayed from disk are what recovery after a crash, and any later
/// collaboration, would be built from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// The source and the file are given up front rather than set afterwards,
    /// so a file dropped onto the window is one entry in the history.
    AddTrack {
        name: String,
        source: Source,
        file: Option<Asset>,
    },
    RemoveTrack {
        id: TrackId,
    },
    RenameTrack {
        id: TrackId,
        name: String,
    },
    /// Linear. Above one is makeup gain and below zero inverts, both of which
    /// are things people legitimately ask for.
    SetGain {
        id: TrackId,
        gain: f32,
    },
    /// Brought into -1..=1, so the stored value is the one that took effect.
    SetPan {
        id: TrackId,
        pan: f32,
    },
    SetMuted {
        id: TrackId,
        muted: bool,
    },
    SetSoloed {
        id: TrackId,
        soloed: bool,
    },
    SetMasterGain {
        gain: f32,
    },
    /// What the track plays. A change of material, not of setting, so it never
    /// folds into a neighbouring gesture.
    SetSource {
        id: TrackId,
        source: Source,
    },
    /// The file the track holds. Setting it does not start it playing, and
    /// clearing it does not silence the track — they are separate questions,
    /// which is what lets someone listen to the tone and come back.
    SetFile {
        id: TrackId,
        file: Option<Asset>,
    },
}

/// What a change is aimed at, for telling one gesture from the next.
///
/// Only continuous controls have one. A mute is a decision and deserves its own
/// place in the history; a fader dragged across the screen is one decision that
/// happens to arrive sixty times a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Target(u8, TrackId);

impl Command {
    /// The control this change moves, when it is one that can be dragged.
    pub(crate) fn target(&self) -> Option<Target> {
        match self {
            Self::SetGain { id, .. } => Some(Target(0, *id)),
            Self::SetPan { id, .. } => Some(Target(1, *id)),
            // The master has no track of its own to be named by.
            Self::SetMasterGain { .. } => Some(Target(2, TrackId(u32::MAX))),
            Self::AddTrack { .. }
            | Self::RemoveTrack { .. }
            | Self::RenameTrack { .. }
            | Self::SetMuted { .. }
            | Self::SetSoloed { .. }
            | Self::SetSource { .. }
            | Self::SetFile { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CommandError {
    /// The track was removed, or never existed. Refused rather than ignored: a
    /// command that quietly did nothing would still take a slot in the history
    /// and give the person an undo that undoes nothing.
    NoSuchTrack(TrackId),
    /// A gain that is NaN or infinite would silence or destroy the output, and
    /// would survive into a saved file to do it again tomorrow.
    NotFinite(&'static str),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchTrack(TrackId(id)) => write!(f, "there is no track {id}"),
            Self::NotFinite(what) => write!(f, "{what} must be a finite number"),
        }
    }
}

impl std::error::Error for CommandError {}

/// One elementary change with everything already decided.
///
/// A [`Command`] says what to do; a list of these says exactly what was done,
/// and can be read backwards. Resolving once and storing the result is what
/// keeps a redo identical to the original: an `AddTrack` replayed from the
/// command would mint a fresh id, and every later step naming the old one
/// would miss.
///
/// A command may take several: a track given a file the pool has never seen
/// also adds that file to the pool, and both come back out with one undo.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Edit {
    /// `at` is the position in the mixer's order.
    InsertTrack {
        at: usize,
        track: Track,
    },
    RemoveTrack {
        id: TrackId,
    },
    InsertAsset {
        asset: Asset,
    },
    RemoveAsset {
        id: AssetId,
    },
    SetName {
        id: TrackId,
        name: String,
    },
    SetGain {
        id: TrackId,
        gain: f32,
    },
    SetPan {
        id: TrackId,
        pan: f32,
    },
    SetMuted {
        id: TrackId,
        muted: bool,
    },
    SetSoloed {
        id: TrackId,
        soloed: bool,
    },
    SetMasterGain {
        gain: f32,
    },
    SetSource {
        id: TrackId,
        source: Source,
    },
    SetFile {
        id: TrackId,
        file: Option<AssetId>,
    },
}

/// What a command does, and what undoes it — each in the order it is applied.
pub(crate) type Resolved = (Vec<Edit>, Vec<Edit>);

/// Works out what a command does, and what would undo it.
///
/// Reads the project without touching it, so a command that cannot be carried
/// out leaves nothing half done. `ids` is only moved on by a command that
/// cannot fail after taking one.
pub(crate) fn resolve(project: &Project, ids: &mut Ids, command: Command) -> Result<Resolved, CommandError> {
    let found = |id: TrackId| project.track(id).ok_or(CommandError::NoSuchTrack(id));
    let one = |forward: Edit, backward: Edit| (vec![forward], vec![backward]);

    Ok(match command {
        Command::AddTrack { name, source, file } => {
            let at = project.track_count();
            let track = Track::new(ids.take_track(), name, source, file.as_ref().map(|a| a.id));
            let id = track.id;
            let mut change = Change::default();
            if let Some(asset) = file {
                change.bring_in(project, asset);
            }
            change.push(Edit::InsertTrack { at, track }, Edit::RemoveTrack { id });
            change.finish()
        }
        Command::RemoveTrack { id } => {
            let track = found(id)?.clone();
            let at = project.position_of(id).ok_or(CommandError::NoSuchTrack(id))?;
            let file = track.file;
            let mut change = Change::default();
            change.push(Edit::RemoveTrack { id }, Edit::InsertTrack { at, track });
            if let Some(file) = file {
                change.let_go(project, file);
            }
            change.finish()
        }
        Command::RenameTrack { id, name } => {
            let was = found(id)?.name.clone();
            one(Edit::SetName { id, name }, Edit::SetName { id, name: was })
        }
        Command::SetGain { id, gain } => {
            let was = found(id)?.gain;
            finite(gain, "a track's gain")?;
            one(Edit::SetGain { id, gain }, Edit::SetGain { id, gain: was })
        }
        Command::SetPan { id, pan } => {
            let was = found(id)?.pan;
            finite(pan, "a pan position")?;
            // Clamped here, so what is stored is what took effect and a redo
            // lands on the same value.
            one(Edit::SetPan { id, pan: pan.clamp(-1.0, 1.0) }, Edit::SetPan { id, pan: was })
        }
        Command::SetMuted { id, muted } => {
            let was = found(id)?.muted;
            one(Edit::SetMuted { id, muted }, Edit::SetMuted { id, muted: was })
        }
        Command::SetSoloed { id, soloed } => {
            let was = found(id)?.soloed;
            one(Edit::SetSoloed { id, soloed }, Edit::SetSoloed { id, soloed: was })
        }
        Command::SetMasterGain { gain } => {
            finite(gain, "the master gain")?;
            one(Edit::SetMasterGain { gain }, Edit::SetMasterGain { gain: project.master_gain() })
        }
        Command::SetSource { id, source } => {
            let was = found(id)?.source;
            one(Edit::SetSource { id, source }, Edit::SetSource { id, source: was })
        }
        Command::SetFile { id, file } => {
            let was = found(id)?.file;
            let now = file.as_ref().map(|a| a.id);
            let mut change = Change::default();
            if let Some(asset) = file {
                change.bring_in(project, asset);
            }
            change.push(Edit::SetFile { id, file: now }, Edit::SetFile { id, file: was });
            // Let go only once the track has moved off it, and only if it was
            // really a different file: setting the same one again keeps it.
            if let Some(was) = was
                && now != Some(was)
            {
                change.let_go(project, was);
            }
            change.finish()
        }
    })
}

/// A command's edits being put together, each paired with its inverse.
#[derive(Default)]
struct Change {
    forward: Vec<Edit>,
    backward: Vec<Edit>,
}

impl Change {
    fn push(&mut self, forward: Edit, backward: Edit) {
        self.forward.push(forward);
        self.backward.push(backward);
    }

    /// Adds a file to the pool unless it is already there.
    ///
    /// Already there means the same content, wherever it was found: the entry
    /// the pool has keeps its path, since other tracks are playing from it.
    fn bring_in(&mut self, project: &Project, asset: Asset) {
        if project.asset(asset.id).is_none() {
            let id = asset.id;
            self.push(Edit::InsertAsset { asset }, Edit::RemoveAsset { id });
        }
    }

    /// Removes a file from the pool when the track about to leave it was its
    /// last user.
    fn let_go(&mut self, project: &Project, id: AssetId) {
        if project.uses_of(id) == 1
            && let Some(asset) = project.asset(id)
        {
            self.push(Edit::RemoveAsset { id }, Edit::InsertAsset { asset: asset.clone() });
        }
    }

    /// Undoing runs the inverses last-first, so each one finds the project the
    /// way its forward edit left it.
    fn finish(mut self) -> Resolved {
        self.backward.reverse();
        (self.forward, self.backward)
    }
}

/// Carries out edits in order. Every one of these is reversible by another.
pub(crate) fn apply(project: &mut Project, edits: &[Edit]) {
    for edit in edits {
        apply_one(project, edit);
    }
}

fn apply_one(project: &mut Project, edit: &Edit) {
    let mut on = |id: TrackId, f: &mut dyn FnMut(&mut Track)| {
        if let Some(track) = project.tracks.get_mut(&id) {
            f(track);
        }
    };

    match edit {
        Edit::InsertTrack { at, track } => {
            project.track_order.insert(*at, track.id);
            project.tracks.insert(track.id, track.clone());
        }
        Edit::RemoveTrack { id } => {
            project.track_order.retain(|t| t != id);
            project.tracks.remove(id);
        }
        Edit::InsertAsset { asset } => {
            project.assets.insert(asset.id, asset.clone());
        }
        Edit::RemoveAsset { id } => {
            project.assets.remove(id);
        }
        Edit::SetName { id, name } => on(*id, &mut |t| t.name = name.clone()),
        Edit::SetGain { id, gain } => on(*id, &mut |t| t.gain = *gain),
        Edit::SetPan { id, pan } => on(*id, &mut |t| t.pan = *pan),
        Edit::SetMuted { id, muted } => on(*id, &mut |t| t.muted = *muted),
        Edit::SetSoloed { id, soloed } => on(*id, &mut |t| t.soloed = *soloed),
        Edit::SetMasterGain { gain } => project.master_gain = *gain,
        Edit::SetSource { id, source } => on(*id, &mut |t| t.source = *source),
        Edit::SetFile { id, file } => on(*id, &mut |t| t.file = *file),
    }
}

fn finite(value: f32, what: &'static str) -> Result<(), CommandError> {
    if value.is_finite() { Ok(()) } else { Err(CommandError::NotFinite(what)) }
}
