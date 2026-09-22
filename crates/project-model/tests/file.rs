//! Saving and opening, held to the promises decision D7 makes.
//!
//! A project file is only worth keeping in Git if the same project always
//! gives the same bytes, and only worth keeping at all if reading it back gives
//! the same project. Both are checked on sequences nobody wrote by hand, and
//! the format itself is pinned by a reference file: a change to it has to be
//! made on purpose, in this test, where a reviewer sees it.

use std::path::{Path, PathBuf};

use project_model::{Asset, AssetId, Command, FileError, Invalid, Session, Source};
use proptest::prelude::*;

fn folder() -> PathBuf {
    // Absolute on either system, so the relative/absolute split is what the
    // test is about rather than an accident of where it runs.
    if cfg!(windows) { PathBuf::from(r"C:\songs\demo") } else { PathBuf::from("/songs/demo") }
}

fn demo() -> Session {
    let mut session = Session::new();
    let kick = Asset::new(AssetId::of_bytes(b"kick"), folder().join("audio").join("kick.wav"));
    session
        .apply(Command::AddTrack { name: "Kick".into(), source: Source::File, file: Some(kick) })
        .expect("adding works");
    session.apply(Command::AddTrack { name: "Pad".into(), source: Source::Tone, file: None }).expect("adding works");
    let pad = session.project().tracks().nth(1).expect("two tracks").id;
    session.apply(Command::SetGain { id: pad, gain: 0.5 }).expect("pad exists");
    session.apply(Command::SetPan { id: pad, pan: -0.3 }).expect("pad exists");
    session
}

/// The format, pinned. If this fails because the format was changed on
/// purpose, bump `FORMAT_VERSION`, write the migration, and update this.
const REFERENCE: &str = r#"{
  "format": "daw-project",
  "version": 1,
  "next_ids": {
    "track": 2
  },
  "project": {
    "master_gain": 1.0,
    "track_order": [
      0,
      1
    ],
    "tracks": {
      "0": {
        "id": 0,
        "name": "Kick",
        "gain": 1.0,
        "pan": 0.0,
        "muted": false,
        "soloed": false,
        "source": "file",
        "file": "f09b2136a68a271423955b63129a24cc00b1d326f31f481e965f786315000a26"
      },
      "1": {
        "id": 1,
        "name": "Pad",
        "gain": 0.5,
        "pan": -0.3,
        "muted": false,
        "soloed": false,
        "source": "tone",
        "file": null
      }
    },
    "assets": {
      "f09b2136a68a271423955b63129a24cc00b1d326f31f481e965f786315000a26": {
        "id": "f09b2136a68a271423955b63129a24cc00b1d326f31f481e965f786315000a26",
        "path": "audio/kick.wav"
      }
    }
  }
}
"#;

#[test]
fn a_project_is_written_exactly_as_the_reference() {
    assert_eq!(demo().to_json(Some(&folder())), REFERENCE);
}

#[test]
fn the_reference_reads_back_as_the_project_it_came_from() {
    let session = Session::from_json(REFERENCE, Some(&folder())).unwrap();
    assert_eq!(session.project(), demo().project());
    assert!(!session.can_undo(), "an opened project arrived with a history");
}

#[test]
fn an_opened_project_keeps_retiring_ids() {
    // The counter is saved, so a track removed before saving keeps its id
    // retired afterwards: the next track must not take it.
    let mut session = demo();
    let pad = session.project().tracks().nth(1).unwrap().id;
    session.apply(Command::RemoveTrack { id: pad }).unwrap();

    let mut opened = Session::from_json(&session.to_json(None), None).unwrap();
    opened.apply(Command::AddTrack { name: "new".into(), source: Source::Silent, file: None }).unwrap();
    assert_ne!(opened.project().tracks().last().unwrap().id, pad, "a retired id came back after opening");
}

#[test]
fn a_file_outside_the_project_folder_keeps_its_full_path() {
    let mut session = Session::new();
    let elsewhere =
        if cfg!(windows) { PathBuf::from(r"D:\samples\snare.wav") } else { PathBuf::from("/samples/snare.wav") };
    let snare = Asset::new(AssetId::of_bytes(b"snare"), &elsewhere);
    session.apply(Command::AddTrack { name: "Snare".into(), source: Source::File, file: Some(snare) }).unwrap();

    let opened = Session::from_json(&session.to_json(Some(&folder())), Some(&folder())).unwrap();
    assert_eq!(opened.project().assets().next().unwrap().path, elsewhere);
}

#[test]
fn a_file_from_a_newer_version_is_refused_by_name() {
    let newer = REFERENCE.replace("\"version\": 1", "\"version\": 99");
    assert!(matches!(Session::from_json(&newer, None), Err(FileError::Newer(99))));
}

#[test]
fn json_that_is_not_a_project_is_refused() {
    assert!(matches!(Session::from_json("{\"hello\": 1}", None), Err(FileError::NotAProject)));
    assert!(matches!(Session::from_json("not json", None), Err(FileError::NotAProject)));
    let other = REFERENCE.replace("daw-project", "something-else");
    assert!(matches!(Session::from_json(&other, None), Err(FileError::NotAProject)));
}

#[test]
fn a_damaged_project_is_refused_rather_than_opened() {
    // A track naming a file the pool does not list: opened anyway it would
    // play the tone and say nothing.
    let hash = "f09b2136a68a271423955b63129a24cc00b1d326f31f481e965f786315000a26";
    let other = "0".repeat(64);
    let dangling = REFERENCE.replacen(&format!("\"file\": \"{hash}\""), &format!("\"file\": \"{other}\""), 1);
    assert!(matches!(Session::from_json(&dangling, None), Err(FileError::Invalid(Invalid::MissingAsset(_)))));

    // A track missing from the order would never be shown, or played.
    let unordered = REFERENCE.replace("      0,\n      1\n", "      0\n");
    assert!(matches!(Session::from_json(&unordered, None), Err(FileError::Invalid(Invalid::Order))));
}

#[test]
fn saving_replaces_the_file_whole_and_leaves_nothing_beside_it() {
    let dir = std::env::temp_dir().join(format!("daw-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("song.daw");

    let mut session = Session::new();
    session.apply(Command::AddTrack { name: "one".into(), source: Source::Tone, file: None }).unwrap();
    session.save(&path).unwrap();
    session.apply(Command::AddTrack { name: "two".into(), source: Source::Tone, file: None }).unwrap();
    session.save(&path).unwrap();

    assert_eq!(Session::open(&path).unwrap().project(), session.project());
    let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left, vec![std::ffi::OsString::from("song.daw")], "the staging file was left behind");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Random edits, including files shared between tracks.
#[derive(Debug, Clone)]
enum Edit {
    Add(Option<u8>),
    Remove(u8),
    Gain(u8, f32),
    Pan(u8, f32),
    Master(f32),
    Rename(u8, String),
}

fn finite() -> impl Strategy<Value = f32> {
    any::<f32>().prop_filter("finite", |g| g.is_finite())
}

fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        3 => proptest::option::of(any::<u8>()).prop_map(Edit::Add),
        1 => any::<u8>().prop_map(Edit::Remove),
        // Any finite f32 at all, not a tidy range: the shortest-form writer has
        // to survive subnormals and values near the limits too.
        2 => (any::<u8>(), finite()).prop_map(|(i, g)| Edit::Gain(i, g)),
        2 => (any::<u8>(), -1.0f32..=1.0).prop_map(|(i, p)| Edit::Pan(i, p)),
        1 => finite().prop_map(Edit::Master),
        // Quotes, backslashes and non-ASCII: the characters JSON escapes.
        1 => (any::<u8>(), "[a-zé\"\\\\ ]{0,10}").prop_map(|(i, n)| Edit::Rename(i, n)),
    ]
}

fn build(edits: &[Edit], folder: &Path) -> Session {
    let mut session = Session::new();
    for edit in edits {
        let count = session.project().track_count();
        let pick =
            |i: u8| (count > 0).then(|| session.project().tracks().nth(usize::from(i) % count).expect("in range").id);
        let command = match edit {
            Edit::Add(file) => Some(Command::AddTrack {
                name: "t".into(),
                source: Source::File,
                file: file.map(|n| Asset::new(AssetId::of_bytes(&[n % 3]), folder.join(format!("{}.wav", n % 3)))),
            }),
            Edit::Remove(i) => pick(*i).map(|id| Command::RemoveTrack { id }),
            Edit::Gain(i, gain) => pick(*i).map(|id| Command::SetGain { id, gain: *gain }),
            Edit::Pan(i, pan) => pick(*i).map(|id| Command::SetPan { id, pan: *pan }),
            Edit::Master(gain) => Some(Command::SetMasterGain { gain: *gain }),
            Edit::Rename(i, name) => pick(*i).map(|id| Command::RenameTrack { id, name: name.clone() }),
        };
        if let Some(command) = command {
            session.apply(command).expect("commands aimed at existing tracks work");
        }
    }
    session
}

proptest! {
    #[test]
    fn saving_then_opening_gives_back_the_same_project(edits in prop::collection::vec(edit(), 0..30)) {
        let folder = folder();
        let session = build(&edits, &folder);
        let text = session.to_json(Some(&folder));
        let opened = Session::from_json(&text, Some(&folder)).unwrap();

        prop_assert_eq!(opened.project(), session.project());
        // And writing the opened one gives the same bytes: nothing drifts
        // across a save, however many times it is made.
        prop_assert_eq!(opened.to_json(Some(&folder)), text);
    }
}
