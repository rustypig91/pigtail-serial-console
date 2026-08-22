//! Platform config/data paths via `directories`.

use anyhow::Context;
use std::path::PathBuf;

/// Resolved locations for config and session logs.
#[derive(Clone, Debug)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub sessions: PathBuf,
    /// Where a panic is recorded before the window disappears. A release build
    /// has no console attached (`windows_subsystem = "windows"`), so without a
    /// file a crash leaves nothing at all behind to look at.
    pub crash_log: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> anyhow::Result<AppPaths> {
        let dirs = directories::ProjectDirs::from("dev", "pigtail", "pigtail")
            .context("no valid home directory for config")?;
        let config_file = dirs.config_dir().join("pigtail.toml");
        let sessions = dirs.data_dir().join("sessions");
        let crash_log = dirs.data_dir().join("crash.log");
        Ok(AppPaths {
            config_file,
            sessions,
            crash_log,
        })
    }
}
