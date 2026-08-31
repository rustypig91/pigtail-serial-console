//! Small filesystem helpers shared by the core and desktop crates.

use std::path::{Path, PathBuf};

/// Replace `path` without ever exposing a partially-written destination.
///
/// The temporary file lives beside the destination, so the final rename stays
/// within one filesystem and is atomic. A failed write or rename leaves the
/// previous destination untouched.
pub fn atomic_write(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);

    let result = (|| {
        std::fs::write(&tmp_path, contents)?;
        std::fs::rename(&tmp_path, path)
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pigtail-{name}-{}", std::process::id()))
    }

    #[test]
    fn replaces_the_destination_and_removes_the_temporary_file() {
        let dir = scratch("atomic-replace");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");
        std::fs::write(&path, "old").unwrap();

        atomic_write(&path, "new").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(!dir.join("settings.toml.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_temporary_write_preserves_the_destination() {
        let dir = scratch("atomic-failure");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");
        std::fs::write(&path, "old").unwrap();
        // A directory cannot be truncated as the adjacent temporary file.
        std::fs::create_dir(dir.join("settings.toml.tmp")).unwrap();

        assert!(atomic_write(&path, "new").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        std::fs::remove_dir_all(&dir).ok();
    }
}
