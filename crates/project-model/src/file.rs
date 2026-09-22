//! The project on disk (decision D7 in the plan).
//!
//! Indented JSON, and the same project always gives the same bytes: tables are
//! sorted by identifier, floats are written in the shortest form that reads
//! back to the same value, and nothing that varies from run to run — a time,
//! a machine name — is written at all. That is what lets a project live in Git
//! and a change show up as the lines it changed.
//!
//! The header is read before anything else, so a file from a newer build is
//! refused with a sentence rather than half-read with a parse error, and an
//! older one can be migrated in the one place that knows how.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::history::Ids;
use crate::{FORMAT_VERSION, Invalid, Meter, Project, Session, Tempo, default_eq};

/// What the first field of every project file says, so a JSON file that
/// happens to parse is not taken for a project.
const FORMAT: &str = "daw-project";

#[derive(Serialize)]
struct Written<'a> {
    format: &'static str,
    version: u32,
    next_ids: Ids,
    project: &'a Project,
}

#[derive(Deserialize)]
struct Header {
    format: String,
    version: u32,
}

#[derive(Deserialize)]
struct Read {
    next_ids: Ids,
    project: Project,
}

/// Why a project could not be saved or opened.
#[derive(Debug)]
pub enum FileError {
    Io(io::Error),
    /// Not JSON, or JSON that is not shaped like a project.
    Parse(serde_json::Error),
    /// JSON, but some other kind of file.
    NotAProject,
    /// Written by a newer build, which may hold things this one would drop.
    Newer(u32),
    /// Shaped like a project but breaking one of its promises — a file edited
    /// by hand, or damaged. Opened anyway it would fail later and further from
    /// the cause.
    Invalid(Invalid),
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Parse(e) => write!(f, "not a readable project: {e}"),
            Self::NotAProject => write!(f, "not a project file"),
            Self::Newer(v) => {
                write!(f, "written by a newer version (format {v}, this build reads up to {FORMAT_VERSION})")
            }
            Self::Invalid(e) => write!(f, "damaged project: {e}"),
        }
    }
}

impl std::error::Error for FileError {}

impl From<io::Error> for FileError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for FileError {
    fn from(e: serde_json::Error) -> Self {
        Self::Parse(e)
    }
}

impl Session {
    /// The project as it would be saved in `folder`.
    ///
    /// Files inside the folder are written relative to it, so the folder can be
    /// moved or copied to another machine whole. Files elsewhere keep their
    /// full path; the hash still identifies them if that path goes stale.
    pub fn to_json(&self, folder: Option<&Path>) -> String {
        let mut project = self.project().clone();
        if let Some(folder) = folder {
            for asset in project.assets.values_mut() {
                asset.path = portable(&asset.path, folder);
            }
        }
        let written = Written { format: FORMAT, version: FORMAT_VERSION, next_ids: self.ids(), project: &project };
        // Only fails for a map with non-string keys or a failing writer, and
        // this model has neither.
        let mut text = serde_json::to_string_pretty(&written).expect("a project always serializes");
        text.push('\n');
        text
    }

    /// Reads a project saved in `folder`, with nothing to undo.
    pub fn from_json(text: &str, folder: Option<&Path>) -> Result<Self, FileError> {
        let header: Header = serde_json::from_str(text).map_err(|_| FileError::NotAProject)?;
        if header.format != FORMAT {
            return Err(FileError::NotAProject);
        }
        if header.version > FORMAT_VERSION {
            return Err(FileError::Newer(header.version));
        }
        let mut value: Value = serde_json::from_str(text)?;
        migrate(&mut value, header.version)?;
        let Read { next_ids, mut project } = serde_json::from_value(value)?;
        if let Some(folder) = folder {
            for asset in project.assets.values_mut() {
                if asset.path.is_relative() {
                    asset.path = folder.join(&asset.path);
                }
            }
        }
        let session = Self::restored(project, next_ids);
        session.check().map_err(FileError::Invalid)?;
        Ok(session)
    }

    /// Writes the project to `path`.
    ///
    /// Written beside it first and moved into place, so a crash or a full disk
    /// part-way through leaves the previous save intact rather than half of a
    /// new one.
    pub fn save(&self, path: &Path) -> Result<(), FileError> {
        let text = self.to_json(path.parent());
        let mut staging = path.as_os_str().to_owned();
        staging.push(".saving");
        let staging = PathBuf::from(staging);

        let written = (|| {
            let mut file = fs::File::create(&staging)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            fs::rename(&staging, path)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&staging);
        }
        Ok(written?)
    }

    /// Reads the project at `path`.
    pub fn open(path: &Path) -> Result<Self, FileError> {
        let text = fs::read_to_string(path)?;
        Self::from_json(&text, path.parent())
    }
}

/// Brings a file written in an older format up to this one, one version at a
/// time.
///
/// Works on the JSON rather than on old Rust types: an old shape then needs
/// no struct kept alive for it, only the few lines that say what changed.
fn migrate(value: &mut Value, from: u32) -> Result<(), FileError> {
    let mut version = from;
    while version < FORMAT_VERSION {
        match version {
            // Version 2 gave the project a tempo and a time signature. A
            // project from before had neither, and played at the default.
            1 => {
                let project = project_of(value)?;
                project.insert("tempo".into(), json!(Tempo::default()));
                project.insert("meter".into(), json!(Meter::default()));
            }
            // Version 3 moved files off tracks and into clips placed on them.
            // A track's file becomes an audio clip of the same name at the
            // start of the timeline — where version 2 played it from.
            2 => {
                let project = project_of(value)?;
                let order: Vec<Value> =
                    project.get("track_order").and_then(Value::as_array).cloned().unwrap_or_default();
                let tracks = project.get_mut("tracks").and_then(Value::as_object_mut).ok_or(FileError::NotAProject)?;
                let (mut clips, mut placements) = (serde_json::Map::new(), serde_json::Map::new());
                // In mixer order, so the ids given out do not depend on how
                // the file happened to sort its keys.
                for id in order {
                    let Some(track) = tracks.get_mut(&id.to_string()).and_then(Value::as_object_mut) else {
                        return Err(FileError::NotAProject);
                    };
                    if track.get("source") == Some(&json!("file")) {
                        track.insert("source".into(), json!("clips"));
                    }
                    if let Some(Value::String(asset)) = track.remove("file") {
                        let n = clips.len();
                        let name = track.get("name").cloned().unwrap_or(json!(""));
                        let content = json!({ "kind": "audio", "asset": asset });
                        clips.insert(n.to_string(), json!({ "id": n, "name": name, "content": content }));
                        let placement = json!({ "id": n, "clip": n, "track": id, "start": 0, "window": null });
                        placements.insert(n.to_string(), placement);
                    }
                }
                let made = clips.len();
                project.insert("clips".into(), Value::Object(clips));
                project.insert("placements".into(), Value::Object(placements));
                let next = value.get_mut("next_ids").and_then(Value::as_object_mut).ok_or(FileError::NotAProject)?;
                next.insert("clip".into(), json!(made));
                next.insert("placement".into(), json!(made));
                next.insert("note".into(), json!(0));
            }
            // Version 4 gave placements of audio clips a window of their own,
            // in real time. Nothing was trimmed before it existed.
            3 => {
                let placements = project_of(value)?
                    .get_mut("placements")
                    .and_then(Value::as_object_mut)
                    .ok_or(FileError::NotAProject)?;
                for placement in placements.values_mut().filter_map(Value::as_object_mut) {
                    placement.insert("audio".into(), Value::Null);
                }
            }
            // Version 5 gave every track an equaliser. A track from before had
            // none, which is what the default's flat bands sound like.
            4 => {
                let tracks = project_of(value)?
                    .get_mut("tracks")
                    .and_then(Value::as_object_mut)
                    .ok_or(FileError::NotAProject)?;
                for track in tracks.values_mut().filter_map(Value::as_object_mut) {
                    track.insert("eq".into(), json!(default_eq()));
                }
            }
            // Version 0 never existed: no file was written before the header.
            _ => return Err(FileError::NotAProject),
        }
        version += 1;
    }
    Ok(())
}

fn project_of(value: &mut Value) -> Result<&mut serde_json::Map<String, Value>, FileError> {
    value.get_mut("project").and_then(Value::as_object_mut).ok_or(FileError::NotAProject)
}

/// A path as written in a project in `folder`: relative with forward slashes
/// when it is inside, unchanged when it is not.
///
/// Forward slashes because the same project is opened from Windows and from
/// Linux, and Windows reads them where Linux would take a backslash for part
/// of a name.
fn portable(path: &Path, folder: &Path) -> PathBuf {
    let Ok(inside) = path.strip_prefix(folder) else {
        return path.to_path_buf();
    };
    let parts: Vec<_> = inside
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy()),
            _ => None,
        })
        .collect();
    PathBuf::from(parts.join("/"))
}
