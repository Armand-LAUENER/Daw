use std::fmt;

use std::path::PathBuf;

use crate::{Project, Source, Track, TrackId};

/// A change someone asked for.
///
/// The whole editing surface. A feature added later is a variant here plus the
/// arm that resolves it — which is also what makes it undoable, for free.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// The source and the file are given up front rather than set afterwards,
    /// so a file dropped onto the window is one entry in the history.
    AddTrack {
        name: String,
        source: Source,
        file: Option<PathBuf>,
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
        file: Option<PathBuf>,
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

/// A change with everything already decided.
///
/// A [`Command`] says what to do; this says exactly what was done, and can be
/// read backwards. Resolving once and storing the result is what keeps a redo
/// identical to the original: an `AddTrack` replayed from the command would
/// mint a fresh id, and every later step naming the old one would miss.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Edit {
    InsertTrack { index: usize, track: Track },
    RemoveTrack { index: usize },
    SetName { id: TrackId, name: String },
    SetGain { id: TrackId, gain: f32 },
    SetPan { id: TrackId, pan: f32 },
    SetMuted { id: TrackId, muted: bool },
    SetSoloed { id: TrackId, soloed: bool },
    SetMasterGain { gain: f32 },
    SetSource { id: TrackId, source: Source },
    SetFile { id: TrackId, file: Option<PathBuf> },
}

/// Works out what a command does, and what would undo it.
///
/// Reads the project without touching it, so a command that cannot be carried
/// out leaves nothing half done.
pub(crate) fn resolve(project: &Project, command: Command) -> Result<(Edit, Edit), CommandError> {
    let found = |id: TrackId| project.track(id).ok_or(CommandError::NoSuchTrack(id));

    Ok(match command {
        Command::AddTrack { name, source, file } => {
            let index = project.tracks.len();
            let track = Track::new(project.next_id(), name, source, file);
            (Edit::InsertTrack { index, track }, Edit::RemoveTrack { index })
        }
        Command::RemoveTrack { id } => {
            let index = project.index_of(id).ok_or(CommandError::NoSuchTrack(id))?;
            (Edit::RemoveTrack { index }, Edit::InsertTrack { index, track: project.tracks[index].clone() })
        }
        Command::RenameTrack { id, name } => {
            let was = found(id)?.name.clone();
            (Edit::SetName { id, name }, Edit::SetName { id, name: was })
        }
        Command::SetGain { id, gain } => {
            let was = found(id)?.gain;
            finite(gain, "a track's gain")?;
            (Edit::SetGain { id, gain }, Edit::SetGain { id, gain: was })
        }
        Command::SetPan { id, pan } => {
            let was = found(id)?.pan;
            finite(pan, "a pan position")?;
            // Clamped here, so what is stored is what took effect and a redo
            // lands on the same value.
            (Edit::SetPan { id, pan: pan.clamp(-1.0, 1.0) }, Edit::SetPan { id, pan: was })
        }
        Command::SetMuted { id, muted } => {
            let was = found(id)?.muted;
            (Edit::SetMuted { id, muted }, Edit::SetMuted { id, muted: was })
        }
        Command::SetSoloed { id, soloed } => {
            let was = found(id)?.soloed;
            (Edit::SetSoloed { id, soloed }, Edit::SetSoloed { id, soloed: was })
        }
        Command::SetMasterGain { gain } => {
            finite(gain, "the master gain")?;
            (Edit::SetMasterGain { gain }, Edit::SetMasterGain { gain: project.master_gain })
        }
        Command::SetSource { id, source } => {
            let was = found(id)?.source;
            (Edit::SetSource { id, source }, Edit::SetSource { id, source: was })
        }
        Command::SetFile { id, file } => {
            let was = found(id)?.file.clone();
            (Edit::SetFile { id, file }, Edit::SetFile { id, file: was })
        }
    })
}

/// Carries out an edit. Every one of these is reversible by another.
pub(crate) fn apply(project: &mut Project, edit: &Edit) {
    let mut on = |id: TrackId, f: &mut dyn FnMut(&mut Track)| {
        if let Some(track) = project.tracks.iter_mut().find(|t| t.id == id) {
            f(track);
        }
    };

    match edit {
        Edit::InsertTrack { index, track } => project.tracks.insert(*index, track.clone()),
        Edit::RemoveTrack { index } => {
            project.tracks.remove(*index);
        }
        Edit::SetName { id, name } => on(*id, &mut |t| t.name = name.clone()),
        Edit::SetGain { id, gain } => on(*id, &mut |t| t.gain = *gain),
        Edit::SetPan { id, pan } => on(*id, &mut |t| t.pan = *pan),
        Edit::SetMuted { id, muted } => on(*id, &mut |t| t.muted = *muted),
        Edit::SetSoloed { id, soloed } => on(*id, &mut |t| t.soloed = *soloed),
        Edit::SetMasterGain { gain } => project.master_gain = *gain,
        Edit::SetSource { id, source } => on(*id, &mut |t| t.source = *source),
        Edit::SetFile { id, file } => on(*id, &mut |t| t.file.clone_from(file)),
    }
}

fn finite(value: f32, what: &'static str) -> Result<(), CommandError> {
    if value.is_finite() { Ok(()) } else { Err(CommandError::NotFinite(what)) }
}
