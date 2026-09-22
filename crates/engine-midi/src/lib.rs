//! Standard MIDI files, turned into notes the engine can schedule.
//!
//! All of this runs on the control side: it reads files, allocates and sorts.
//! What crosses to the audio thread is the flat list of events it produces.
//!
//! The hard part is not the format, it is the clock. A MIDI file measures time
//! in ticks, and how long a tick lasts depends on a tempo that the file may
//! change as often as it likes. Reading the first tempo and using it throughout
//! is the classic mistake: everything after the first change drifts, and the
//! further in, the worse it gets.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use engine_core::{Note, PianoRoll};

#[cfg(feature = "live")]
mod live;
#[cfg(feature = "live")]
pub use live::{MidiInput, connect, list_inputs};
use midly::{Format, MetaMessage, MidiMessage, Smf, Timing, TrackEventKind};

/// The tempo a file is at until it says otherwise: 120 BPM, in microseconds
/// per beat, as the format defines the default.
const DEFAULT_TEMPO_US: f64 = 500_000.0;

/// Pitches on a MIDI channel, which is also the stride of the held-key map.
const PITCHES: usize = 128;

#[derive(Debug)]
pub enum MidiError {
    Io(std::io::Error),
    /// The bytes are not a MIDI file, or are damaged.
    Parse(String),
    /// Format 2: tracks that are separate pieces rather than parts of one.
    /// Merging them onto a single timeline would stack unrelated music.
    SequentialTracks,
    /// Timed in SMPTE frames rather than beats: it has no musical time to
    /// place on a timeline, only seconds.
    NotMetrical,
    /// The system's MIDI stack refused to open or enumerate.
    Backend(String),
    /// No input port, or none matching what was asked for.
    NoInput(String),
}

impl fmt::Display for MidiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Parse(e) => write!(f, "not a readable MIDI file: {e}"),
            Self::SequentialTracks => {
                write!(f, "format 2 files hold independent pieces, which have no common timeline")
            }
            Self::NotMetrical => write!(f, "timed in SMPTE frames, not in beats"),
            Self::Backend(e) => write!(f, "the system's MIDI stack refused: {e}"),
            Self::NoInput(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MidiError {}

impl From<std::io::Error> for MidiError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// A note from a file, with the channel it arrived on.
///
/// Channels are kept rather than flattened: channel 10 is percussion by
/// convention, and a caller that wants one instrument needs to be able to ask
/// for it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MidiNote {
    pub channel: u8,
    pub pitch: u8,
    pub velocity: f32,
    pub start: u64,
    pub length: u64,
}

/// A decoded file, in frames at one sample rate.
///
/// Frames rather than seconds because that is what the engine schedules
/// against, and converting once here is one rounding rather than one per note
/// at every use.
#[derive(Debug, Clone)]
pub struct MidiSequence {
    notes: Vec<MidiNote>,
    sample_rate: u32,
    duration: u64,
}

impl MidiSequence {
    pub fn notes(&self) -> &[MidiNote] {
        &self.notes
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Frames until the last note ends.
    pub fn duration(&self) -> u64 {
        self.duration
    }

    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    /// Every channel, merged.
    pub fn piano_roll(&self) -> PianoRoll {
        self.roll(|_| true)
    }

    /// One channel, numbered from zero — so the percussion channel usually
    /// called 10 is 9 here, as it is in the wire format.
    pub fn channel_roll(&self, channel: u8) -> PianoRoll {
        self.roll(|note| note.channel == channel)
    }

    fn roll(&self, keep: impl Fn(&MidiNote) -> bool) -> PianoRoll {
        let mut roll = PianoRoll::new();
        for note in self.notes.iter().filter(|n| keep(n)) {
            roll.add(Note::new(note.pitch, note.velocity, note.start, note.length));
        }
        roll
    }
}

pub fn decode_file(path: impl AsRef<Path>, sample_rate: u32) -> Result<MidiSequence, MidiError> {
    decode_bytes(&std::fs::read(path)?, sample_rate)
}

pub fn decode_bytes(data: &[u8], sample_rate: u32) -> Result<MidiSequence, MidiError> {
    let smf = Smf::parse(data).map_err(|e| MidiError::Parse(e.to_string()))?;
    let events = merge(&smf)?;

    let timing = smf.header.timing;
    let ticks_per_beat = match timing {
        Timing::Metrical(tpq) => Some(f64::from(tpq.as_int())),
        Timing::Timecode(..) => None,
    };
    // A timecode file measures real time directly, so no tempo applies to it.
    let timecode_seconds_per_tick = match timing {
        Timing::Timecode(fps, subframes) => Some(1.0 / (f64::from(fps.as_f32()) * f64::from(subframes))),
        Timing::Metrical(_) => None,
    };
    let mut tempo_us = DEFAULT_TEMPO_US;
    let mut seconds = 0.0f64;
    let mut last_tick = 0u64;
    let frame_of = |seconds: f64| (seconds * f64::from(sample_rate)).round() as u64;

    let (notes, duration) = pair(&events, |tick, kind| {
        let seconds_per_tick =
            timecode_seconds_per_tick.unwrap_or_else(|| tempo_us / 1e6 / ticks_per_beat.unwrap_or(1.0).max(1.0));
        seconds += (tick - last_tick) as f64 * seconds_per_tick;
        last_tick = tick;
        // Read after the time has been advanced: a change applies to what
        // comes after it, never to the stretch already counted.
        if let TrackEventKind::Meta(MetaMessage::Tempo(us)) = kind {
            tempo_us = f64::from(us.as_int());
        }
        frame_of(seconds)
    });
    Ok(MidiSequence { notes, sample_rate, duration })
}

/// A file in its own musical time: notes in the file's ticks, before any tempo
/// has turned them into seconds.
///
/// What a timeline wants. A project keeps notes in beats and lets its own
/// tempo decide when they sound, so going through seconds and back would only
/// add a rounding and tie the notes to the file's tempo.
#[derive(Debug, Clone)]
pub struct MidiScore {
    /// The file's resolution: how many of its ticks make a quarter note.
    pub ticks_per_quarter: u16,
    /// `start` and `length` in the file's ticks, earliest first.
    pub notes: Vec<MidiNote>,
    /// Ticks until the last note ends.
    pub length: u64,
}

pub fn read_score(path: impl AsRef<Path>) -> Result<MidiScore, MidiError> {
    score_bytes(&std::fs::read(path)?)
}

pub fn score_bytes(data: &[u8]) -> Result<MidiScore, MidiError> {
    let smf = Smf::parse(data).map_err(|e| MidiError::Parse(e.to_string()))?;
    let Timing::Metrical(tpq) = smf.header.timing else {
        return Err(MidiError::NotMetrical);
    };
    let events = merge(&smf)?;
    let (notes, length) = pair(&events, |tick, _| tick);
    Ok(MidiScore { ticks_per_quarter: tpq.as_int().max(1), notes, length })
}

type Merged<'a> = Vec<(u64, usize, usize, TrackEventKind<'a>)>;

/// Every track's events on one timeline, in absolute ticks.
///
/// Tracks in a format 1 file run in parallel and the tempo usually lives in
/// the first one, so they have to be merged before the clock can be worked out
/// at all.
fn merge<'a>(smf: &Smf<'a>) -> Result<Merged<'a>, MidiError> {
    if smf.header.format == Format::Sequential {
        return Err(MidiError::SequentialTracks);
    }
    let mut merged: Merged<'a> = Vec::new();
    for (track_index, track) in smf.tracks.iter().enumerate() {
        let mut tick = 0u64;
        for (order, event) in track.iter().enumerate() {
            tick += u64::from(event.delta.as_int());
            merged.push((tick, track_index, order, event.kind));
        }
    }
    // Ties broken by track then position, so a tempo change and a note on the
    // same tick resolve the same way every time the file is read.
    merged.sort_by_key(|(tick, track, order, _)| (*tick, *track, *order));
    Ok(merged)
}

/// Pairs note-ons with their note-offs, in whatever unit `now_of` measures
/// time in. Returns the notes, earliest first, and when the last one ends.
///
/// `now_of` is called once per event, in order, with its tick — so a clock
/// that follows tempo changes can keep its own state.
fn pair(events: &Merged<'_>, mut now_of: impl FnMut(u64, &TrackEventKind<'_>) -> u64) -> (Vec<MidiNote>, u64) {
    // Keyed by channel and pitch, and ordered rather than hashed so that the
    // sweep of still-held keys at the end comes out the same way every time the
    // same file is read.
    let mut pending: BTreeMap<usize, (u64, f32)> = BTreeMap::new();
    let mut notes: Vec<MidiNote> = Vec::new();
    let mut now = 0;

    for (tick, _, _, kind) in events {
        now = now_of(*tick, kind);
        if let TrackEventKind::Midi { channel, message } = kind {
            let channel = channel.as_int();
            match message {
                // A note on with no velocity is a note off. Plenty of files
                // never send a real one.
                MidiMessage::NoteOn { key, vel } if vel.as_int() > 0 => {
                    let slot = slot(channel, key.as_int());
                    // The same key struck again before it was let go: end
                    // the first rather than losing track of it.
                    if let Some((start, velocity)) = pending.remove(&slot) {
                        notes.push(finish(channel, key.as_int(), velocity, start, now));
                    }
                    pending.insert(slot, (now, f32::from(vel.as_int()) / 127.0));
                }
                MidiMessage::NoteOff { key, .. } | MidiMessage::NoteOn { key, .. } => {
                    let slot = slot(channel, key.as_int());
                    if let Some((start, velocity)) = pending.remove(&slot) {
                        notes.push(finish(channel, key.as_int(), velocity, start, now));
                    }
                }
                _ => {}
            }
        }
    }

    // A file that ends without letting go of a key: close it at the end rather
    // than dropping the note, which would silently lose the last chord.
    for (slot, (start, velocity)) in pending {
        notes.push(finish((slot / PITCHES) as u8, (slot % PITCHES) as u8, velocity, start, now));
    }

    notes.sort_by_key(|n| (n.start, n.channel, n.pitch));
    let end = notes.iter().map(|n| n.start + n.length).max().unwrap_or(0);
    (notes, end)
}

fn slot(channel: u8, pitch: u8) -> usize {
    usize::from(channel) * PITCHES + usize::from(pitch)
}

fn finish(channel: u8, pitch: u8, velocity: f32, start: u64, end: u64) -> MidiNote {
    // At least one frame. A note whose start and end land on the same frame
    // would have its stop sorted before its start, so the voice would be told
    // to stop before it existed and would then sound for ever.
    MidiNote { channel, pitch, velocity, start, length: end.saturating_sub(start).max(1) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use midly::num::{u4, u7, u15, u24, u28};
    use midly::{Header, Track, TrackEvent};

    const SR: u32 = 48_000;
    const TPQ: u16 = 480;

    fn note_on(delta: u32, key: u8, vel: u8) -> TrackEvent<'static> {
        TrackEvent {
            delta: u28::new(delta),
            kind: TrackEventKind::Midi {
                channel: u4::new(0),
                message: MidiMessage::NoteOn { key: u7::new(key), vel: u7::new(vel) },
            },
        }
    }

    fn note_off(delta: u32, key: u8) -> TrackEvent<'static> {
        TrackEvent {
            delta: u28::new(delta),
            kind: TrackEventKind::Midi {
                channel: u4::new(0),
                message: MidiMessage::NoteOff { key: u7::new(key), vel: u7::new(0) },
            },
        }
    }

    fn tempo(delta: u32, us_per_beat: u32) -> TrackEvent<'static> {
        TrackEvent { delta: u28::new(delta), kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::new(us_per_beat))) }
    }

    fn end() -> TrackEvent<'static> {
        TrackEvent { delta: u28::new(0), kind: TrackEventKind::Meta(MetaMessage::EndOfTrack) }
    }

    /// Writes a file in memory, so the tests carry their own fixtures rather
    /// than binaries nobody can read in a diff.
    fn write(format: Format, timing: Timing, tracks: Vec<Track<'static>>) -> Vec<u8> {
        let mut smf = Smf::new(Header::new(format, timing));
        smf.tracks = tracks;
        let mut bytes = Vec::new();
        smf.write(&mut bytes).expect("writing to a Vec cannot fail");
        bytes
    }

    fn metrical(tracks: Vec<Track<'static>>) -> Vec<u8> {
        write(Format::Parallel, Timing::Metrical(u15::new(TPQ)), tracks)
    }

    /// Frames in one beat at 120 BPM.
    fn beat() -> u64 {
        u64::from(SR) / 2
    }

    #[test]
    fn a_note_lands_where_the_default_tempo_puts_it() {
        // No tempo event at all, so the file is at the 120 BPM the format
        // defines: one beat is half a second.
        let bytes = metrical(vec![vec![note_on(0, 60, 100), note_off(u32::from(TPQ), 60), end()]]);
        let seq = decode_bytes(&bytes, SR).unwrap();

        assert_eq!(seq.notes().len(), 1);
        assert_eq!(seq.notes()[0].start, 0);
        assert_eq!(seq.notes()[0].length, beat());
    }

    #[test]
    fn velocity_comes_through_scaled() {
        let bytes = metrical(vec![vec![note_on(0, 60, 127), note_off(10, 60), end()]]);
        assert!((decode_bytes(&bytes, SR).unwrap().notes()[0].velocity - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_tempo_change_only_applies_to_what_follows_it() {
        // The bug this whole clock exists for. One beat at 120, then the tempo
        // doubles and the next beat takes half as long. Reading only the first
        // tempo would put the second note a quarter of a second late, and
        // everything after it further out still.
        let bytes = metrical(vec![vec![
            note_on(0, 60, 100),
            note_off(u32::from(TPQ), 60),
            tempo(0, 250_000), // 240 BPM
            note_on(0, 64, 100),
            note_off(u32::from(TPQ), 64),
            end(),
        ]]);
        let seq = decode_bytes(&bytes, SR).unwrap();

        let second = seq.notes().iter().find(|n| n.pitch == 64).expect("the second note");
        assert_eq!(second.start, beat(), "the second note should start one 120 BPM beat in");
        assert_eq!(second.length, beat() / 2, "it should last one 240 BPM beat");
    }

    #[test]
    fn tempo_in_one_track_governs_notes_in_another() {
        // Where a format 1 file puts it: a conductor track with no notes.
        let bytes = metrical(vec![
            vec![tempo(0, 250_000), end()],
            vec![note_on(0, 60, 100), note_off(u32::from(TPQ), 60), end()],
        ]);
        let seq = decode_bytes(&bytes, SR).unwrap();
        assert_eq!(seq.notes()[0].length, beat() / 2, "the conductor track was ignored");
    }

    #[test]
    fn a_note_on_with_no_velocity_is_a_note_off() {
        let bytes = metrical(vec![vec![note_on(0, 60, 100), note_on(u32::from(TPQ), 60, 0), end()]]);
        let seq = decode_bytes(&bytes, SR).unwrap();
        assert_eq!(seq.notes().len(), 1);
        assert_eq!(seq.notes()[0].length, beat());
    }

    #[test]
    fn striking_a_held_key_again_ends_the_first_note() {
        let bytes =
            metrical(vec![vec![note_on(0, 60, 100), note_on(u32::from(TPQ), 60, 100), note_off(240, 60), end()]]);
        let seq = decode_bytes(&bytes, SR).unwrap();
        assert_eq!(seq.notes().len(), 2, "one of the two strikes was lost");
        assert_eq!(seq.notes()[0].length, beat());
    }

    #[test]
    fn a_key_never_let_go_is_closed_at_the_end() {
        let bytes = metrical(vec![vec![note_on(0, 60, 100), tempo(u32::from(TPQ), 500_000), end()]]);
        let seq = decode_bytes(&bytes, SR).unwrap();
        assert_eq!(seq.notes().len(), 1, "the dangling note was dropped");
        assert_eq!(seq.notes()[0].length, beat());
    }

    #[test]
    fn a_zero_length_note_still_gets_a_frame() {
        // Otherwise its stop sorts before its start, the voice is told to stop
        // before it exists, and it then sounds for ever.
        let bytes = metrical(vec![vec![note_on(0, 60, 100), note_off(0, 60), end()]]);
        assert_eq!(decode_bytes(&bytes, SR).unwrap().notes()[0].length, 1);
    }

    #[test]
    fn a_score_keeps_the_files_ticks_whatever_its_tempo() {
        // The same two notes as the tempo test: as a score, the tempo change
        // moves nothing. A beat is TPQ ticks before it and after it.
        let bytes = metrical(vec![vec![
            note_on(0, 60, 100),
            note_off(u32::from(TPQ), 60),
            tempo(0, 250_000),
            note_on(0, 64, 100),
            note_off(u32::from(TPQ), 64),
            end(),
        ]]);
        let score = score_bytes(&bytes).unwrap();
        assert_eq!(score.ticks_per_quarter, TPQ);
        let starts: Vec<(u8, u64, u64)> = score.notes.iter().map(|n| (n.pitch, n.start, n.length)).collect();
        let tpq = u64::from(TPQ);
        assert_eq!(starts, vec![(60, 0, tpq), (64, tpq, tpq)]);
        assert_eq!(score.length, 2 * tpq);
    }

    #[test]
    fn a_timecode_file_has_no_score() {
        let bytes = write(Format::Parallel, Timing::Timecode(midly::Fps::Fps25, 40), vec![vec![end()]]);
        assert!(matches!(score_bytes(&bytes), Err(MidiError::NotMetrical)));
    }

    #[test]
    fn timecode_files_ignore_tempo_because_they_measure_real_time() {
        let bytes = write(
            Format::Parallel,
            Timing::Timecode(midly::Fps::Fps25, 40),
            vec![vec![note_on(0, 60, 100), tempo(0, 250_000), note_off(1_000, 60), end()]],
        );
        let seq = decode_bytes(&bytes, SR).unwrap();
        // 25 fps at 40 subframes is 1000 ticks a second, so 1000 ticks is one
        // second whatever any tempo event claims.
        assert_eq!(seq.notes()[0].length, u64::from(SR));
    }

    #[test]
    fn sequential_files_are_refused_rather_than_stacked() {
        let bytes = write(
            Format::Sequential,
            Timing::Metrical(u15::new(TPQ)),
            vec![vec![note_on(0, 60, 100), note_off(10, 60), end()]],
        );
        assert!(matches!(decode_bytes(&bytes, SR), Err(MidiError::SequentialTracks)));
    }

    #[test]
    fn nonsense_is_refused_with_a_reason() {
        assert!(matches!(decode_bytes(b"not a midi file at all", SR), Err(MidiError::Parse(_))));
    }

    #[test]
    fn a_roll_can_be_taken_one_channel_at_a_time() {
        let on_channel = |ch: u8, key: u8, delta: u32| TrackEvent {
            delta: u28::new(delta),
            kind: TrackEventKind::Midi {
                channel: u4::new(ch),
                message: MidiMessage::NoteOn { key: u7::new(key), vel: u7::new(100) },
            },
        };
        let bytes =
            metrical(vec![vec![on_channel(0, 60, 0), on_channel(9, 38, 0), note_off(u32::from(TPQ), 60), end()]]);
        let seq = decode_bytes(&bytes, SR).unwrap();

        assert_eq!(seq.piano_roll().notes().len(), 2);
        assert_eq!(seq.channel_roll(0).notes().len(), 1);
        assert_eq!(seq.channel_roll(9).notes()[0].pitch, 38, "the percussion channel went missing");
    }

    #[test]
    fn an_empty_file_decodes_to_nothing() {
        let seq = decode_bytes(&metrical(vec![vec![end()]]), SR).unwrap();
        assert!(seq.is_empty());
        assert_eq!(seq.duration(), 0);
    }
}
