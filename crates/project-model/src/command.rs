use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::history::Ids;
use crate::{
    Asset, AssetId, AudioWindow, BANDS, Bus, BusId, Clip, ClipId, CompSettings, Content, DelaySettings, EqBand,
    Instrument, Meter, NewClip, Note, NoteId, Placement, PlacementId, Project, ReverbSettings, Source, Tempo, TempoMap,
    Tick, Track, TrackId, Window, time,
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
    /// A track, and — when one is given — a clip placed at its start. One
    /// command, so a file dropped onto the window is one entry in the history.
    AddTrack {
        name: String,
        source: Source,
        clip: Option<NewClip>,
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
    /// A tempo change from `at` onwards. Refused where there is one already —
    /// that one is changed instead — and at the start, which always has one.
    AddTempoChange {
        at: Tick,
        tempo: Tempo,
    },
    /// The tempo of a change, named by its place in the map; 0 is the start,
    /// as [`Command::SetTempo`] sets. Dragged, so one step per gesture.
    SetTempoChange {
        index: usize,
        tempo: Tempo,
    },
    /// Moves a change, stopping at its neighbours so every change keeps its
    /// place in the map. The start cannot move.
    MoveTempoChange {
        index: usize,
        to: Tick,
    },
    /// The start cannot be removed: something has to hold at 0.
    RemoveTempoChange {
        index: usize,
    },
    /// What the track plays. A change of material, not of setting, so it never
    /// folds into a neighbouring gesture.
    SetSource {
        id: TrackId,
        source: Source,
    },
    /// One band of a track's equaliser. Dragged, so a sweep is one step per
    /// band.
    SetEqBand {
        track: TrackId,
        band: u8,
        value: EqBand,
    },
    /// A track's compressor. Dragged, so one step per gesture.
    SetCompressor {
        track: TrackId,
        value: CompSettings,
    },
    SetDelay {
        track: TrackId,
        value: DelaySettings,
    },
    SetReverb {
        track: TrackId,
        value: ReverbSettings,
    },
    /// What a track's MIDI clips are played with. Dragged, so one step per
    /// gesture.
    SetInstrument {
        track: TrackId,
        value: Instrument,
    },
    /// A new effects bus, a reverb return to begin with.
    AddBus {
        name: String,
    },
    /// Takes the sends to it with it, so no track is left sending nowhere.
    RemoveBus {
        id: BusId,
    },
    RenameBus {
        id: BusId,
        name: String,
    },
    SetBusGain {
        id: BusId,
        gain: f32,
    },
    SetBusMuted {
        id: BusId,
        muted: bool,
    },
    /// A bus's three effects together. Dragged, so one step per gesture.
    SetBusEffects {
        id: BusId,
        comp: CompSettings,
        delay: DelaySettings,
        reverb: ReverbSettings,
    },
    /// How much of a track a bus hears. `None` removes the send.
    SetSend {
        track: TrackId,
        bus: BusId,
        level: Option<f32>,
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
    /// Trims a placement from either end in one gesture: a new start and a new
    /// window together, so trimming the front moves the start and the offset
    /// by the same amount and the notes stay where they were in time.
    ///
    /// `window` trims a MIDI clip, `audio` an audio clip; each must be `None`
    /// on the other kind.
    TrimPlacement {
        id: PlacementId,
        start: Tick,
        window: Option<Window>,
        audio: Option<AudioWindow>,
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
pub(crate) struct Target(u8, u32, u32);

impl Command {
    /// The control this change moves, when it is one that can be dragged.
    pub(crate) fn target(&self) -> Option<Target> {
        // The project-wide controls have no object of their own to be named by.
        const PROJECT: u32 = u32::MAX;
        match self {
            Self::SetGain { id, .. } => Some(Target(0, id.0, 0)),
            Self::SetPan { id, .. } => Some(Target(1, id.0, 0)),
            Self::SetMasterGain { .. } => Some(Target(2, PROJECT, 0)),
            Self::SetTempo { .. } => Some(Target(3, PROJECT, 0)),
            Self::SetTempoChange { index, .. } => Some(Target(207, *index as u32, 0)),
            Self::MoveTempoChange { index, .. } => Some(Target(208, *index as u32, 0)),
            Self::MovePlacement { id, .. } => Some(Target(4, id.0, 0)),
            Self::SetWindow { id, .. } | Self::TrimPlacement { id, .. } => Some(Target(5, id.0, 0)),
            Self::SetNote { id, .. } => Some(Target(6, id.0, 0)),
            // One target per band, above every other kind.
            Self::SetEqBand { track, band, .. } => Some(Target(8u8.saturating_add(*band), track.0, 0)),
            Self::SetCompressor { track, .. } => Some(Target(200, track.0, 0)),
            Self::SetDelay { track, .. } => Some(Target(201, track.0, 0)),
            Self::SetReverb { track, .. } => Some(Target(202, track.0, 0)),
            Self::SetBusGain { id, .. } => Some(Target(203, id.0, 0)),
            Self::SetBusEffects { id, .. } => Some(Target(204, id.0, 0)),
            // Two ids: one track's sends to two buses are two controls.
            Self::SetSend { track, bus, .. } => Some(Target(205, track.0, bus.0)),
            Self::SetInstrument { track, .. } => Some(Target(206, track.0, 0)),
            Self::AddTrack { .. }
            | Self::RemoveTrack { .. }
            | Self::RenameTrack { .. }
            | Self::SetMuted { .. }
            | Self::SetSoloed { .. }
            | Self::SetMeter { .. }
            | Self::AddTempoChange { .. }
            | Self::RemoveTempoChange { .. }
            | Self::AddBus { .. }
            | Self::RemoveBus { .. }
            | Self::RenameBus { .. }
            | Self::SetBusMuted { .. }
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
    NoSuchBus(BusId),
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
            Self::NoSuchBus(BusId(id)) => write!(f, "there is no bus {id}"),
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
    SetTempoMap {
        map: TempoMap,
    },
    SetMeter {
        meter: Meter,
    },
    SetSource {
        id: TrackId,
        source: Source,
    },
    SetEqBand {
        id: TrackId,
        band: usize,
        value: EqBand,
    },
    SetInstrument {
        id: TrackId,
        value: Instrument,
    },
    InsertBus {
        at: usize,
        bus: Bus,
    },
    RemoveBus {
        id: BusId,
    },
    /// Replaces a bus's settings whole: renamed, faded, muted or re-effected.
    SetBus {
        bus: Bus,
    },
    SetSend {
        track: TrackId,
        bus: BusId,
        level: Option<f32>,
    },
    SetCompressor {
        id: TrackId,
        value: CompSettings,
    },
    SetDelay {
        id: TrackId,
        value: DelaySettings,
    },
    SetReverb {
        id: TrackId,
        value: ReverbSettings,
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
    let bus = |id: BusId| project.bus(id).ok_or(CommandError::NoSuchBus(id));
    let midi = |id: ClipId| {
        let found = clip(id)?;
        match &found.content {
            Content::Midi { length, notes } => Ok((*length, notes)),
            Content::Audio { .. } => Err(CommandError::NotMidi(id)),
        }
    };
    let one = |forward: Edit, backward: Edit| (vec![forward], vec![backward]);

    Ok(match command {
        Command::AddTrack { name, source, clip } => {
            // Checked before any id is taken: a refused command takes nothing.
            if let Some(NewClip::Midi { length, notes }) = &clip {
                if length.0 == 0 {
                    return Err(CommandError::OutOfRange("a clip's length"));
                }
                notes.iter().try_for_each(|n| fits(n, *length))?;
            }
            let mut change = Change::new(project);
            let at = project.track_count();
            let track = Track::new(ids.take_track(), name.clone(), source);
            let id = track.id;
            change.push(Edit::InsertTrack { at, track }, Edit::RemoveTrack { id });
            match clip {
                None => {}
                Some(NewClip::Audio { asset }) => change.place_audio(ids, id, asset, Tick(0), name),
                Some(NewClip::Midi { length, notes }) => {
                    let notes = notes.into_iter().map(|n| (ids.take_note(), n)).collect();
                    let clip = Clip { id: ids.take_clip(), name, content: Content::Midi { length, notes } };
                    change.place(ids, clip, id, Tick(0));
                }
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
        Command::SetTempo { tempo } => tempo_edit(project, project.tempo_map().with_tempo(0, tempo))?,
        Command::AddTempoChange { at, tempo } => tempo_edit(project, project.tempo_map().with_change(at, tempo))?,
        Command::SetTempoChange { index, tempo } => tempo_edit(project, project.tempo_map().with_tempo(index, tempo))?,
        Command::MoveTempoChange { index, to } => tempo_edit(project, project.tempo_map().with_moved(index, to))?,
        Command::RemoveTempoChange { index } => tempo_edit(project, project.tempo_map().without(index))?,
        Command::SetMeter { meter } => {
            if !time::meter_valid(meter) {
                return Err(CommandError::OutOfRange("a time signature"));
            }
            one(Edit::SetMeter { meter }, Edit::SetMeter { meter: project.meter() })
        }
        Command::SetSource { id, source } => {
            let was = track(id)?.source;
            one(Edit::SetSource { id, source }, Edit::SetSource { id, source: was })
        }
        Command::SetEqBand { track: id, band, value } => {
            let band = usize::from(band);
            let was = *track(id)?.eq.get(band).ok_or(CommandError::OutOfRange("a band number"))?;
            if !value.is_valid() {
                return Err(CommandError::OutOfRange("an equaliser band"));
            }
            one(Edit::SetEqBand { id, band, value }, Edit::SetEqBand { id, band, value: was })
        }
        Command::SetCompressor { track: id, value } => {
            let was = track(id)?.comp;
            if !value.is_valid() {
                return Err(CommandError::OutOfRange("a compressor setting"));
            }
            one(Edit::SetCompressor { id, value }, Edit::SetCompressor { id, value: was })
        }
        Command::SetDelay { track: id, value } => {
            let was = track(id)?.delay;
            if !value.is_valid() {
                return Err(CommandError::OutOfRange("a delay setting"));
            }
            one(Edit::SetDelay { id, value }, Edit::SetDelay { id, value: was })
        }
        Command::SetReverb { track: id, value } => {
            let was = track(id)?.reverb;
            if !value.is_valid() {
                return Err(CommandError::OutOfRange("a reverb setting"));
            }
            one(Edit::SetReverb { id, value }, Edit::SetReverb { id, value: was })
        }
        Command::SetInstrument { track: id, value } => {
            let was = track(id)?.instrument;
            if !value.is_valid() {
                return Err(CommandError::OutOfRange("an instrument setting"));
            }
            one(Edit::SetInstrument { id, value }, Edit::SetInstrument { id, value: was })
        }
        Command::AddBus { name } => {
            let at = project.bus_order.len();
            let bus = Bus::new(ids.take_bus(), name);
            let id = bus.id;
            one(Edit::InsertBus { at, bus }, Edit::RemoveBus { id })
        }
        Command::RemoveBus { id } => {
            let bus = bus(id)?.clone();
            let at = project.position_of_bus(id).ok_or(CommandError::NoSuchBus(id))?;
            let mut change = Change::new(project);
            for track in project.tracks() {
                if let Some(level) = track.sends.get(&id) {
                    let (t, level) = (track.id, *level);
                    change.push(
                        Edit::SetSend { track: t, bus: id, level: None },
                        Edit::SetSend { track: t, bus: id, level: Some(level) },
                    );
                }
            }
            change.push(Edit::RemoveBus { id }, Edit::InsertBus { at, bus });
            change.finish()
        }
        Command::RenameBus { id, name } => {
            let was = bus(id)?.clone();
            one(Edit::SetBus { bus: Bus { name, ..was.clone() } }, Edit::SetBus { bus: was })
        }
        Command::SetBusGain { id, gain } => {
            let was = bus(id)?.clone();
            finite(gain, "a bus's gain")?;
            one(Edit::SetBus { bus: Bus { gain, ..was.clone() } }, Edit::SetBus { bus: was })
        }
        Command::SetBusMuted { id, muted } => {
            let was = bus(id)?.clone();
            one(Edit::SetBus { bus: Bus { muted, ..was.clone() } }, Edit::SetBus { bus: was })
        }
        Command::SetBusEffects { id, comp, delay, reverb } => {
            let was = bus(id)?.clone();
            let now = Bus { comp, delay, reverb, ..was.clone() };
            if !now.is_valid() {
                return Err(CommandError::OutOfRange("a bus effect's settings"));
            }
            one(Edit::SetBus { bus: now }, Edit::SetBus { bus: was })
        }
        Command::SetSend { track: t, bus: b, level } => {
            let was = track(t)?.sends.get(&b).copied();
            bus(b)?;
            if level.is_some_and(|l| !crate::SEND.contains(&l)) {
                return Err(CommandError::OutOfRange("a send level"));
            }
            one(Edit::SetSend { track: t, bus: b, level }, Edit::SetSend { track: t, bus: b, level: was })
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
            window_fits(clip(was.clip)?, window)?;
            let now = Placement { window, ..was };
            one(Edit::SetPlacement { placement: now }, Edit::SetPlacement { placement: was })
        }
        Command::TrimPlacement { id, start, window, audio } => {
            let was = placement(id)?;
            let trimmed = clip(was.clip)?;
            window_fits(trimmed, window)?;
            if let Some(audio) = audio
                && (trimmed.asset().is_none() || audio.length == 0 || audio.rate == 0)
            {
                return Err(CommandError::OutOfRange("an audio window"));
            }
            let now = Placement { start, window, audio, ..was };
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

/// A window must be inside its clip, and only a MIDI clip has one.
fn window_fits(clip: &Clip, window: Option<Window>) -> Result<(), CommandError> {
    let Some(window) = window else { return Ok(()) };
    let fits = clip.length().is_some_and(|l| window.length.0 > 0 && window.offset.0 + window.length.0 <= l.0);
    if fits { Ok(()) } else { Err(CommandError::OutOfRange("a window")) }
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
        let placement = Placement { id: ids.take_placement(), clip: clip.id, track, start, window: None, audio: None };
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
        Edit::InsertBus { at, bus } => {
            project.bus_order.insert(*at, bus.id);
            project.buses.insert(bus.id, bus.clone());
        }
        Edit::RemoveBus { id } => {
            project.bus_order.retain(|b| b != id);
            project.buses.remove(id);
        }
        Edit::SetBus { bus } => {
            project.buses.insert(bus.id, bus.clone());
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
        Edit::SetTempoMap { map } => project.tempo_map = map.clone(),
        Edit::SetMeter { meter } => project.meter = *meter,
        Edit::SetSource { id, source } => on(*id, &mut |t| t.source = *source),
        Edit::SetCompressor { id, value } => on(*id, &mut |t| t.comp = *value),
        Edit::SetInstrument { id, value } => on(*id, &mut |t| t.instrument = *value),
        Edit::SetSend { track, bus, level } => on(*track, &mut |t| match level {
            Some(level) => {
                t.sends.insert(*bus, *level);
            }
            None => {
                t.sends.remove(bus);
            }
        }),
        Edit::SetDelay { id, value } => on(*id, &mut |t| t.delay = *value),
        Edit::SetReverb { id, value } => on(*id, &mut |t| t.reverb = *value),
        Edit::SetEqBand { id, band, value } => on(*id, &mut |t| {
            if *band < BANDS {
                t.eq[*band] = *value;
            }
        }),
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

/// A new tempo map as one edit and its inverse. `None` is a change the map
/// refused — a change on an occupied tick, the start moved or removed, a
/// place past its end — and every tempo must be in range.
fn tempo_edit(project: &Project, map: Option<TempoMap>) -> Result<Resolved, CommandError> {
    let map = map.filter(TempoMap::is_valid).ok_or(CommandError::OutOfRange("a tempo change"))?;
    let was = project.tempo_map().clone();
    Ok((vec![Edit::SetTempoMap { map }], vec![Edit::SetTempoMap { map: was }]))
}

fn finite(value: f32, what: &'static str) -> Result<(), CommandError> {
    if value.is_finite() { Ok(()) } else { Err(CommandError::NotFinite(what)) }
}
