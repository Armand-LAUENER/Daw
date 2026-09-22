//! Audio a project uses, named by what it contains (decision D6 in the plan).
//!
//! A path is a poor name for a file: it changes when the project is copied to
//! another machine, when a folder is renamed, when the same sample is imported
//! from two places. The hash of the content does not. So the hash is the
//! identity and the path is only where the file was last seen — a hint for
//! finding it, which can be corrected without touching anything that uses it.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};

/// The BLAKE3 hash of a file's bytes.
///
/// Written as 64 hex digits: long, but a file that is read and diffed by people
/// has no use for a shorter form that might one day collide.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AssetId([u8; 32]);

impl AssetId {
    /// The id of some bytes already in memory.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// The id of a file on disk, read in pieces rather than all at once.
    ///
    /// Takes as long as reading the file does, so it belongs on the control
    /// side, never near the audio thread.
    pub fn of_file(path: &Path) -> io::Result<Self> {
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(File::open(path)?)?;
        Ok(Self(*hasher.finalize().as_bytes()))
    }

    /// The first few digits, enough to tell files apart on screen.
    pub fn short(&self) -> String {
        self.0[..4].iter().map(|b| format!("{b:02x}")).collect()
    }

    fn parse(text: &str) -> Option<Self> {
        if text.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(Self(bytes))
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl fmt::Debug for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AssetId({})", self.short())
    }
}

impl Serialize for AssetId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for AssetId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| de::Error::custom(format!("{text:?} is not a 64-digit hex hash")))
    }
}

/// A file in the project's pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub id: AssetId,
    /// Where the file was last seen. Absolute in memory; relative to the
    /// project in a saved file whenever the file sits beside it.
    pub path: PathBuf,
}

impl Asset {
    pub fn new(id: AssetId, path: impl Into<PathBuf>) -> Self {
        Self { id, path: path.into() }
    }

    /// Hashes a file and names it by the result.
    pub fn from_file(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        Ok(Self { id: AssetId::of_file(&path)?, path })
    }

    /// Whether the file at `path` is still the one this asset names.
    ///
    /// False for a file that was edited or replaced since: playing it anyway
    /// would be playing something the project never contained.
    pub fn matches_file(&self) -> io::Result<bool> {
        Ok(AssetId::of_file(&self.path)? == self.id)
    }

    /// The file's name, without the folders leading to it.
    pub fn name(&self) -> String {
        self.path.file_name().map_or_else(|| self.path.display().to_string(), |n| n.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_survives_being_written_as_text() {
        let id = AssetId::of_bytes(b"kick");
        assert_eq!(AssetId::parse(&id.to_string()), Some(id));
    }

    #[test]
    fn text_that_is_not_a_hash_is_refused() {
        assert_eq!(AssetId::parse("abc"), None);
        assert_eq!(AssetId::parse(&"zz".repeat(32)), None);
        // Multi-byte characters must not be split in the middle.
        assert_eq!(AssetId::parse(&"é".repeat(32)), None);
    }

    #[test]
    fn a_file_is_named_by_its_content_not_its_place() {
        let dir = std::env::temp_dir().join(format!("daw-asset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.wav"), dir.join("b.wav"));
        std::fs::write(&a, b"same bytes").unwrap();
        std::fs::write(&b, b"same bytes").unwrap();

        let (first, second) = (Asset::from_file(&a).unwrap(), Asset::from_file(&b).unwrap());
        assert_eq!(first.id, second.id, "one sample in two places is one asset");
        assert_eq!(first.id, AssetId::of_bytes(b"same bytes"));
        assert!(first.matches_file().unwrap());

        std::fs::write(&a, b"edited").unwrap();
        assert!(!first.matches_file().unwrap(), "an edited file still passed for the original");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
