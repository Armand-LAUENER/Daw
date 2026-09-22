//! Where projects live, and finding them again.
//!
//! Every project is saved in one folder, `Daw` in the home directory, unless it
//! was opened from elsewhere. That is what lets the window offer them in a list
//! without a file dialog — a dependency, and on Linux a toolkit — to browse
//! with.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// What a project file is called. Anything else dropped on the window is
/// taken for audio or MIDI.
pub const EXTENSION: &str = "daw";

/// The folder projects are saved in, made if it is not there yet.
pub fn folder() -> std::io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| std::io::Error::other("no home directory"))?;
    let folder = PathBuf::from(home).join("Daw");
    std::fs::create_dir_all(&folder)?;
    Ok(folder)
}

/// A project found in the folder.
#[derive(Debug, Clone)]
pub struct Listed {
    pub path: PathBuf,
    /// The file name without its extension.
    pub name: String,
    pub modified: SystemTime,
}

/// The projects in `folder`, the most recently saved first. A folder that
/// cannot be read lists nothing rather than failing: the list is a
/// convenience, and dropping a file still works.
pub fn list(folder: &Path) -> Vec<Listed> {
    let Ok(entries) = std::fs::read_dir(folder) else { return Vec::new() };
    let mut found: Vec<Listed> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case(EXTENSION)))
        .filter_map(|path| {
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
            let name = path.file_stem()?.to_string_lossy().into_owned();
            Some(Listed { path, name, modified })
        })
        .collect();
    found.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name)));
    found
}

/// A file name from what a person typed: letters, digits, spaces, dashes and
/// underscores kept, anything a file system might refuse replaced by a dash.
/// `None` when nothing usable is left.
pub fn file_name(typed: &str) -> Option<String> {
    let cleaned: String =
        typed.trim().chars().map(|c| if c.is_alphanumeric() || " -_".contains(c) { c } else { '-' }).collect();
    let cleaned = cleaned.trim_matches(|c: char| c == '-' || c.is_whitespace()).to_string();
    (!cleaned.is_empty()).then(|| format!("{cleaned}.{EXTENSION}"))
}

/// `untitled.daw`, or the first `untitled-N.daw` that does not exist yet, so
/// an earlier project is never overwritten.
pub fn untitled(folder: &Path) -> PathBuf {
    (1..)
        .map(|n| {
            let name = if n == 1 { "untitled".into() } else { format!("untitled-{n}") };
            folder.join(format!("{name}.{EXTENSION}"))
        })
        .find(|path| !path.exists())
        .expect("some number is free")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("daw-projects-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn only_project_files_are_listed_newest_first() {
        let dir = scratch("list");
        std::fs::write(dir.join("old.daw"), "{}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("new.daw"), "{}").unwrap();
        std::fs::write(dir.join("kick.wav"), "").unwrap();
        let names: Vec<String> = list(&dir).into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["new", "old"]);
        assert!(list(&dir.join("missing")).is_empty(), "a missing folder should list nothing");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_typed_name_becomes_a_safe_file_name() {
        assert_eq!(file_name("My song"), Some("My song.daw".into()));
        assert_eq!(file_name("  demo/v2: final?  "), Some("demo-v2- final.daw".into()));
        assert_eq!(file_name("été"), Some("été.daw".into()), "accents are letters");
        assert_eq!(file_name("  "), None);
        assert_eq!(file_name("///"), None);
    }

    #[test]
    fn untitled_never_takes_a_name_already_used() {
        let dir = scratch("untitled");
        assert_eq!(untitled(&dir), dir.join("untitled.daw"));
        std::fs::write(dir.join("untitled.daw"), "{}").unwrap();
        assert_eq!(untitled(&dir), dir.join("untitled-2.daw"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
