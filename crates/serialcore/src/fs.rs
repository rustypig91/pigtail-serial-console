//! Small filesystem helpers shared by the core and desktop crates.

use std::io::Write;
use std::path::Path;

/// Replace `path` without ever exposing a partially-written destination.
///
/// The temporary file lives beside the destination, so the final rename stays
/// within one filesystem and is atomic. Its unique name also keeps concurrent
/// writers from sharing or deleting one another's temporary files. A failed
/// write or persist leaves the previous destination untouched.
pub fn atomic_write(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::Builder::new()
        .prefix(".pigtail-write-")
        .tempfile_in(parent)?;
    temp.write_all(contents.as_ref())?;
    temp.flush()?;
    temp.persist(path).map(|_| ()).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pigtail-{name}-{}", std::process::id()))
    }

    #[test]
    fn replaces_the_destination_repeatedly_and_removes_temporary_files() {
        let dir = scratch("atomic-replace");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");
        std::fs::write(&path, "old").unwrap();

        atomic_write(&path, "new").unwrap();
        atomic_write(&path, "newer").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "newer");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_persist_preserves_the_destination_and_removes_the_temporary_file() {
        let dir = scratch("atomic-failure");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("old"), "old").unwrap();

        assert!(atomic_write(&path, "new").is_err());
        assert_eq!(std::fs::read_to_string(path.join("old")).unwrap(), "old");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
