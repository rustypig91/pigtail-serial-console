//! Platform config/data paths via `directories`.

use anyhow::Context;
use std::path::PathBuf;

/// Resolved locations for config and session logs.
#[derive(Clone, Debug)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub sessions: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> anyhow::Result<AppPaths> {
        let dirs = directories::ProjectDirs::from("dev", "pigtail", "pigtail")
            .context("no valid home directory for config")?;
        let config_file = dirs.config_dir().join("pigtail.toml");
        let sessions = dirs.data_dir().join("sessions");
        Ok(AppPaths {
            config_file,
            sessions,
        })
    }
}
