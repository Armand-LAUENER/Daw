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

use crate::history::Ids;
use crate::{FORMAT_VERSION, Invalid, Project, Session};

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
        // Every version so far is this one. A migration from an older one
        // goes here, as a function from its shape to the next.

        let Read { next_ids, mut project } = serde_json::from_str(text)?;
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
