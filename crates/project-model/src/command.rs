use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::history::Ids;
use crate::{
    Asset, AssetId, Clip, ClipId, Content, Meter, Note, NoteId, Placement, PlacementId, Project, Source, Tempo, Tick,
    Track, TrackId, Window, time,
};

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
    /// A track, and — when a file is given — an audio clip of it placed at the
    /// start. One command, so a file dropped onto the window is one entry in
    /// the history.
    AddTrack {
        name: String,
        source: Source,
        file: Option<Asset>,
    },
    /// Takes its placements with it.
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
    /// The project's tempo. Dragged like a fader, so a sweep is one step.
    SetTempo {
        tempo: Tempo,
    },
    SetMeter {
        meter: Meter,
    },
    /// What the track plays. A change of material, not of setting, so it never
    /// folds into a neighbouring gesture.
    SetSource {
        id: TrackId,
        source: Source,
    },
    /// Removes every placement on a track, keeping the track and its settings.
    ClearTrack {
        id: TrackId,
    },
    /// Places a file on a track as a new audio clip.
    AddAudioClip {
        track: TrackId,
        asset: Asset,
        start: Tick,
    },
    /// Places a new, empty MIDI clip on a track.
    AddMidiClip {
        track: TrackId,
        name: String,
        start: Tick,
        length: Tick,
    },
    /// Moves a placement in time, and to another track if asked. Dragged, so
    /// a move is one step however many frames it took.
    MovePlacement {
        id: PlacementId,
        track: TrackId,
        start: Tick,
    },
    /// Trims what part of its clip a placement plays, without touching the
    /// clip other placements may share. `None` plays all of it.
    SetWindow {
        id: PlacementId,
        window: Option<Window>,
    },
    /// Puts a placement's clip somewhere else too. Linked, the new placement
    /// plays the same clip and follows its edits; not linked, it gets a copy of
    /// its own.
    DuplicatePlacement {
        id: PlacementId,
        track: TrackId,
        start: Tick,
        linked: bool,
    },
    /// Gives a placement its own copy of a clip it shares, so editing it stops
    /// changing the others.
    MakeUnique {
        id: PlacementId,
    },
    /// Takes a clip off the timeline. The clip goes too once nothing plays it.
    RemovePlacement {
        id: PlacementId,
    },
    RenameClip {
        id: ClipId,
        name: String,
    },
    /// A MIDI clip's length. Refused if a note or a placement's window would
    /// be left outside it.
    SetClipLength {
        id: ClipId,
        length: Tick,
    },
    AddNote {
        clip: ClipId,
        note: Note,
    },
    /// Moves, resizes or re-pitches a note. Dragged, so one step per gesture.
    SetNote {
        clip: ClipId,
        id: NoteId,
        note: Note,
    },
    RemoveNote {
        clip: ClipId,
        id: NoteId,
    },
}

/// What a change is aimed at, for telling one gesture from the next.
///
/// Only continuous controls have one. A mute is a decision and deserves its own
/// place in the history; a fader dragged across the screen is one decision that
/// happens to arrive sixty times a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Target(u8, u32);

impl Command {
    /// The control this change moves, when it is one that can be dragged.
    pub(crate) fn target(&self) -> Option<Target> {
        // The project-wide controls have no object of their own to be named by.
        const PROJECT: u32 = u32::MAX;
        match self {
            Self::SetGain { id, .. } => Some(Target(0, id.0)),
            Self::SetPan { id, .. } => Some(Target(1, id.0)),
            Self::SetMasterGain { .. } => Some(Target(2, PROJECT)),
            Self::SetTempo { .. } => Some(Target(3, PROJECT)),
            Self::MovePlacement { id, .. } => Some(Target(4, id.0)),
            Self::SetWindow { id, .. } => Some(Target(5, id.0)),
            Self::SetNote { id, .. } => Some(Target(6, id.0)),
            Self::AddTrack { .. }
            | Self::RemoveTrack { .. }
            | Self::RenameTrack { .. }
            | Self::SetMuted { .. }
            | Self::SetSoloed { .. }
            | Self::SetMeter { .. }
            | Self::SetSource { .. }
            | Self::ClearTrack { .. }
            | Self::AddAudioClip { .. }
            | Self::AddMidiClip { .. }
            | Self::DuplicatePlacement { .. }
            | Self::MakeUnique { .. }
            | Self::RemovePlacement { .. }
            | Self::RenameClip { .. }
            | Self::SetClipLength { .. }
            | Self::AddNote { .. }
            | Self::RemoveNote { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CommandError {
    /// The track was removed, or never existed. Refused rather than ignored: a
    /// command that quietly did nothing would still take a slot in the history
    /// and give the person an undo that undoes nothing.
    NoSuchTrack(TrackId),
    NoSuchClip(ClipId),
    NoSuchPlacement(PlacementId),
    NoSuchNote(NoteId),
    /// A gain that is NaN or infinite would silence or destroy the output, and
    /// would survive into a saved file to do it again tomorrow.
    NotFinite(&'static str),
    /// A value outside what the project accepts — a tempo of 2 BPM, a note
    /// past the end of its clip, a window wider than what it windows.
    OutOfRange(&'static str),
    /// Notes asked of an audio clip.
    NotMidi(ClipId),
    /// Would change nothing — clearing an empty track, making a clip unique
    /// that is not shared — and so would leave an undo that undoes nothing.
    Unchanged,
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchTrack(TrackId(id)) => write!(f, "there is no track {id}"),
            Self::NoSuchClip(ClipId(id)) => write!(f, "there is no clip {id}"),
            Self::NoSuchPlacement(PlacementId(id)) => write!(f, "there is no placement {id}"),
            Self::NoSuchNote(NoteId(id)) => write!(f, "there is no note {id}"),
            Self::NotFinite(what) => write!(f, "{what} must be a finite number"),
            Self::OutOfRange(what) => write!(f, "{what} is out of range"),
            Self::NotMidi(ClipId(id)) => write!(f, "clip {id} is audio and has no notes"),
            Self::Unchanged => write!(f, "that would change nothing"),
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
/// A command may take several: removing a track removes its placements, the
/// clips only they played, and the files only those clips used — and one undo
/// brings all of it back.
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
    InsertClip {
        clip: Clip,
    },
    RemoveClip {
        id: ClipId,
    },
    InsertPlacement {
        placement: Placement,
    },
    RemovePlacement {
        id: PlacementId,
    },
    /// Replaces a placement whole: moved, re-windowed or re-pointed.
    SetPlacement {
        placement: Placement,
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
    SetTempo {
        tempo: Tempo,
    },
    SetMeter {
        meter: Meter,
    },
    SetSource {
        id: TrackId,
        source: Source,
    },
    SetClipName {
        id: ClipId,
        name: String,
    },
    SetClipLength {
        id: ClipId,
        length: Tick,
    },
    InsertNote {
        clip: ClipId,
        id: NoteId,
        note: Note,
    },
    RemoveNote {
        clip: ClipId,
        id: NoteId,
    },
    SetNote {
        clip: ClipId,
        id: NoteId,
        note: Note,
    },
}

/// What a command does, and what undoes it — each in the order it is applied.
pub(crate) type Resolved = (Vec<Edit>, Vec<Edit>);

/// Works out what a command does, and what would undo it.
///
/// Reads the project without touching it, so a command that cannot be carried
/// out leaves nothing half done. Ids are only taken once nothing can fail.
pub(crate) fn resolve(project: &Project, ids: &mut Ids, command: Command) -> Result<Resolved, CommandError> {
    let track = |id: TrackId| project.track(id).ok_or(CommandError::NoSuchTrack(id));
    let placement = |id: PlacementId| project.placement(id).copied().ok_or(CommandError::NoSuchPlacement(id));
    let clip = |id: ClipId| project.clip(id).ok_or(CommandError::NoSuchClip(id));
    let midi = |id: ClipId| {
        let found = clip(id)?;
        match &found.content {
            Content::Midi { length, notes } => Ok((*length, notes)),
            Content::Audio { .. } => Err(CommandError::NotMidi(id)),
        }
    };
    let one = |forward: Edit, backward: Edit| (vec![forward], vec![backward]);

    Ok(match command {
        Command::AddTrack { name, source, file } => {
            let mut change = Change::new(project);
            let at = project.track_count();
            let track = Track::new(ids.take_track(), name.clone(), source);
            let id = track.id;
            change.push(Edit::InsertTrack { at, track }, Edit::RemoveTrack { id });
            if let Some(asset) = file {
                change.place_audio(ids, id, asset, Tick(0), name);
            }
            change.finish()
        }
        Command::RemoveTrack { id } => {
            let removed = track(id)?.clone();
            let at = project.position_of(id).ok_or(CommandError::NoSuchTrack(id))?;
            let mut change = Change::new(project);
            for placement in project.placements_on(id) {
                change.remove_placement(placement.id);
            }
            change.push(Edit::RemoveTrack { id }, Edit::InsertTrack { at, track: removed });
            change.finish()
        }
        Command::RenameTrack { id, name } => {
            let was = track(id)?.name.clone();
            one(Edit::SetName { id, name }, Edit::SetName { id, name: was })
        }
        Command::SetGain { id, gain } => {
            let was = track(id)?.gain;
            finite(gain, "a track's gain")?;
            one(Edit::SetGain { id, gain }, Edit::SetGain { id, gain: was })
        }
        Command::SetPan { id, pan } => {
            let was = track(id)?.pan;
            finite(pan, "a pan position")?;
            // Clamped here, so what is stored is what took effect and a redo
            // lands on the same value.
            one(Edit::SetPan { id, pan: pan.clamp(-1.0, 1.0) }, Edit::SetPan { id, pan: was })
        }
        Command::SetMuted { id, muted } => {
            let was = track(id)?.muted;
            one(Edit::SetMuted { id, muted }, Edit::SetMuted { id, muted: was })
        }
        Command::SetSoloed { id, soloed } => {
            let was = track(id)?.soloed;
            one(Edit::SetSoloed { id, soloed }, Edit::SetSoloed { id, soloed: was })
        }
        Command::SetMasterGain { gain } => {
            finite(gain, "the master gain")?;
            one(Edit::SetMasterGain { gain }, Edit::SetMasterGain { gain: project.master_gain() })
        }
        Command::SetTempo { tempo } => {
            if !time::valid(tempo, project.meter()) {
                return Err(CommandError::OutOfRange("a tempo"));
            }
            one(Edit::SetTempo { tempo }, Edit::SetTempo { tempo: project.tempo() })
        }
        Command::SetMeter { meter } => {
            if !time::valid(project.tempo(), meter) {
                return Err(CommandError::OutOfRange("a time signature"));
            }
            one(Edit::SetMeter { meter }, Edit::SetMeter { meter: project.meter() })
        }
        Command::SetSource { id, source } => {
            let was = track(id)?.source;
            one(Edit::SetSource { id, source }, Edit::SetSource { id, source: was })
        }
        Command::ClearTrack { id } => {
            track(id)?;
            let on = project.placements_on(id);
            if on.is_empty() {
                return Err(CommandError::Unchanged);
            }
            let mut change = Change::new(project);
            for placement in on {
                change.remove_placement(placement.id);
            }
            change.finish()
        }
        Command::AddAudioClip { track: id, asset, start } => {
            track(id)?;
            let mut change = Change::new(project);
            let name = asset.path.file_stem().map_or_else(|| "audio".into(), |s| s.to_string_lossy().into_owned());
            change.place_audio(ids, id, asset, start, name);
            change.finish()
        }
        Command::AddMidiClip { track: id, name, start, length } => {
            track(id)?;
            if length.0 == 0 {
                return Err(CommandError::OutOfRange("a clip's length"));
            }
            let mut change = Change::new(project);
            let clip = Clip { id: ids.take_clip(), name, content: Content::Midi { length, notes: BTreeMap::new() } };
            change.place(ids, clip, id, start);
            change.finish()
        }
        Command::MovePlacement { id, track: to, start } => {
            let was = placement(id)?;
            track(to)?;
            let now = Placement { track: to, start, ..was };
            one(Edit::SetPlacement { placement: now }, Edit::SetPlacement { placement: was })
        }
        Command::SetWindow { id, window } => {
            let was = placement(id)?;
            if let Some(window) = window {
                let length = clip(was.clip)?.length();
                let fits = length.is_some_and(|l| window.length.0 > 0 && window.offset.0 + window.length.0 <= l.0);
                if !fits {
                    return Err(CommandError::OutOfRange("a window"));
                }
            }
            let now = Placement { window, ..was };
            one(Edit::SetPlacement { placement: now }, Edit::SetPlacement { placement: was })
        }
        Command::DuplicatePlacement { id, track: to, start, linked } => {
            let source = placement(id)?;
            track(to)?;
            let mut change = Change::new(project);
            let clip_id = if linked {
                source.clip
            } else {
                let copy = copy_of(clip(source.clip)?, ids);
                let copied = copy.id;
                change.push(Edit::InsertClip { clip: copy }, Edit::RemoveClip { id: copied });
                copied
            };
            let placement = Placement { id: ids.take_placement(), clip: clip_id, track: to, start, ..source };
            let new = placement.id;
            change.push(Edit::InsertPlacement { placement }, Edit::RemovePlacement { id: new });
            change.finish()
        }
        Command::MakeUnique { id } => {
            let was = placement(id)?;
            if project.uses_of_clip(was.clip) < 2 {
                return Err(CommandError::Unchanged);
            }
            let copy = copy_of(clip(was.clip)?, ids);
            let copied = copy.id;
            let mut change = Change::new(project);
            change.push(Edit::InsertClip { clip: copy }, Edit::RemoveClip { id: copied });
            let now = Placement { clip: copied, ..was };
            change.push(Edit::SetPlacement { placement: now }, Edit::SetPlacement { placement: was });
            change.finish()
        }
        Command::RemovePlacement { id } => {
            placement(id)?;
            let mut change = Change::new(project);
            change.remove_placement(id);
            change.finish()
        }
        Command::RenameClip { id, name } => {
            let was = clip(id)?.name.clone();
            one(Edit::SetClipName { id, name }, Edit::SetClipName { id, name: was })
        }
        Command::SetClipLength { id, length } => {
            let (was, notes) = midi(id)?;
            let windows_fit = project
                .placements()
                .filter(|p| p.clip == id)
                .filter_map(|p| p.window)
                .all(|w| w.offset.0 + w.length.0 <= length.0);
            if length.0 == 0 || notes.values().any(|n| n.end() > length) || !windows_fit {
                return Err(CommandError::OutOfRange("a clip's length"));
            }
            one(Edit::SetClipLength { id, length }, Edit::SetClipLength { id, length: was })
        }
        Command::AddNote { clip, note } => {
            let (length, _) = midi(clip)?;
            fits(&note, length)?;
            let id = ids.take_note();
            one(Edit::InsertNote { clip, id, note }, Edit::RemoveNote { clip, id })
        }
        Command::SetNote { clip, id, note } => {
            let (length, notes) = midi(clip)?;
            let was = *notes.get(&id).ok_or(CommandError::NoSuchNote(id))?;
            fits(&note, length)?;
            one(Edit::SetNote { clip, id, note }, Edit::SetNote { clip, id, note: was })
        }
        Command::RemoveNote { clip, id } => {
            let (_, notes) = midi(clip)?;
            let was = *notes.get(&id).ok_or(CommandError::NoSuchNote(id))?;
            one(Edit::RemoveNote { clip, id }, Edit::InsertNote { clip, id, note: was })
        }
    })
}

/// A copy of a clip under a new id, with new ids for its notes: the copy's
/// notes are different notes from the original's, edited separately.
fn copy_of(clip: &Clip, ids: &mut Ids) -> Clip {
    let content = match &clip.content {
        Content::Audio { asset } => Content::Audio { asset: *asset },
        Content::Midi { length, notes } => {
            Content::Midi { length: *length, notes: notes.values().map(|n| (ids.take_note(), *n)).collect() }
        }
    };
    Clip { id: ids.take_clip(), name: clip.name.clone(), content }
}

fn fits(note: &Note, length: Tick) -> Result<(), CommandError> {
    if note.is_valid() && note.end() <= length { Ok(()) } else { Err(CommandError::OutOfRange("a note")) }
}

/// A command's edits being put together, each paired with its inverse.
///
/// Each edit is also applied to a working copy of the project as it is added,
/// so a later decision — is this the clip's last placement, is this the file's
/// last clip — is taken against the project as the earlier edits leave it.
/// Removing a track with three placements of one clip removes the clip once,
/// after the third, and never before.
struct Change {
    forward: Vec<Edit>,
    backward: Vec<Edit>,
    work: Project,
}

impl Change {
    fn new(project: &Project) -> Self {
        Self { forward: Vec::new(), backward: Vec::new(), work: project.clone() }
    }

    fn push(&mut self, forward: Edit, backward: Edit) {
        apply_one(&mut self.work, &forward);
        self.forward.push(forward);
        self.backward.push(backward);
    }

    /// Adds a file to the pool unless it is already there.
    ///
    /// Already there means the same content, wherever it was found: the entry
    /// the pool has keeps its path, since other clips are playing from it.
    fn bring_in(&mut self, asset: Asset) {
        if self.work.asset(asset.id).is_none() {
            let id = asset.id;
            self.push(Edit::InsertAsset { asset }, Edit::RemoveAsset { id });
        }
    }

    /// A new audio clip of `asset`, placed on `track` at `start`.
    fn place_audio(&mut self, ids: &mut Ids, track: TrackId, asset: Asset, start: Tick, name: String) {
        let content = Content::Audio { asset: asset.id };
        self.bring_in(asset);
        let clip = Clip { id: ids.take_clip(), name, content };
        self.place(ids, clip, track, start);
    }

    /// Adds a clip and one placement of it.
    fn place(&mut self, ids: &mut Ids, clip: Clip, track: TrackId, start: Tick) {
        let placement = Placement { id: ids.take_placement(), clip: clip.id, track, start, window: None };
        let (clip_id, placement_id) = (clip.id, placement.id);
        self.push(Edit::InsertClip { clip }, Edit::RemoveClip { id: clip_id });
        self.push(Edit::InsertPlacement { placement }, Edit::RemovePlacement { id: placement_id });
    }

    /// Removes a placement, then its clip if nothing else plays it, then the
    /// clip's file if no other clip uses it.
    fn remove_placement(&mut self, id: PlacementId) {
        let Some(placement) = self.work.placement(id).copied() else {
            return;
        };
        self.push(Edit::RemovePlacement { id }, Edit::InsertPlacement { placement });
        if self.work.uses_of_clip(placement.clip) > 0 {
            return;
        }
        let Some(clip) = self.work.clip(placement.clip).cloned() else {
            return;
        };
        let asset = clip.asset();
        self.push(Edit::RemoveClip { id: clip.id }, Edit::InsertClip { clip });
        if let Some(asset) = asset
            && self.work.uses_of_asset(asset) == 0
            && let Some(entry) = self.work.asset(asset).cloned()
        {
            self.push(Edit::RemoveAsset { id: asset }, Edit::InsertAsset { asset: entry });
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
        Edit::InsertClip { clip } => {
            project.clips.insert(clip.id, clip.clone());
        }
        Edit::RemoveClip { id } => {
            project.clips.remove(id);
        }
        Edit::InsertPlacement { placement } | Edit::SetPlacement { placement } => {
            project.placements.insert(placement.id, *placement);
        }
        Edit::RemovePlacement { id } => {
            project.placements.remove(id);
        }
        Edit::SetName { id, name } => on(*id, &mut |t| t.name = name.clone()),
        Edit::SetGain { id, gain } => on(*id, &mut |t| t.gain = *gain),
        Edit::SetPan { id, pan } => on(*id, &mut |t| t.pan = *pan),
        Edit::SetMuted { id, muted } => on(*id, &mut |t| t.muted = *muted),
        Edit::SetSoloed { id, soloed } => on(*id, &mut |t| t.soloed = *soloed),
        Edit::SetMasterGain { gain } => project.master_gain = *gain,
        Edit::SetTempo { tempo } => project.tempo = *tempo,
        Edit::SetMeter { meter } => project.meter = *meter,
        Edit::SetSource { id, source } => on(*id, &mut |t| t.source = *source),
        Edit::SetClipName { id, name } => {
            if let Some(clip) = project.clips.get_mut(id) {
                clip.name.clone_from(name);
            }
        }
        Edit::SetClipLength { id, length } => {
            if let Some(Content::Midi { length: l, .. }) = project.clips.get_mut(id).map(|c| &mut c.content) {
                *l = *length;
            }
        }
        Edit::InsertNote { clip, id, note } | Edit::SetNote { clip, id, note } => {
            if let Some(Content::Midi { notes, .. }) = project.clips.get_mut(clip).map(|c| &mut c.content) {
                notes.insert(*id, *note);
            }
        }
        Edit::RemoveNote { clip, id } => {
            if let Some(Content::Midi { notes, .. }) = project.clips.get_mut(clip).map(|c| &mut c.content) {
                notes.remove(id);
            }
        }
    }
}

fn finite(value: f32, what: &'static str) -> Result<(), CommandError> {
    if value.is_finite() { Ok(()) } else { Err(CommandError::NotFinite(what)) }
}
