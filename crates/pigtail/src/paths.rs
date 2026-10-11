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
    pub fn open_config_dir(&self) -> anyhow::Result<()> {
        let directory = self
            .config_file
            .parent()
            .context("configuration file has no parent directory")?;
        std::fs::create_dir_all(directory).context("creating configuration directory")?;
        open::that(directory).context("opening configuration directory")
    }

    pub fn open_logs_dir(&self) -> anyhow::Result<()> {
        let directory = self
            .crash_log
            .parent()
            .context("crash log has no parent directory")?;
        std::fs::create_dir_all(directory).context("creating logs directory")?;
        open::that(directory).context("opening logs directory")
    }
}

#[cfg(not(feature = "demo"))]
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn failed_launcher_reports_error() {
        // Isolate PATH in a subprocess instead of mutating the test process's
        // environment while other tests run concurrently.
        const CHILD_ENV: &str = "PIGTAIL_TEST_FOLDER_LAUNCHER";
        if let Some(directory) = std::env::var_os(CHILD_ENV) {
            let directory = PathBuf::from(directory);
            let paths = AppPaths {
                config_file: directory.join("config/pigtail.toml"),
                sessions: directory.join("data/sessions"),
                crash_log: directory.join("data/crash.log"),
            };
            assert!(paths.open_config_dir().is_err());
            assert!(paths.open_logs_dir().is_err());
            assert!(directory.join("config").is_dir());
            assert!(directory.join("data").is_dir());
            return;
        }
        use std::os::unix::fs::PermissionsExt as _;
        let directory =
            std::env::temp_dir().join(format!("pigtail-launcher-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let launcher = directory.join("xdg-open");
        std::fs::write(&launcher, "#!/bin/sh\nexit 7\n").unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "paths::tests::failed_launcher_reports_error",
                "--nocapture",
            ])
            .env(CHILD_ENV, &directory)
            .env("PATH", &directory)
            .status()
            .unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(status.success());
    }
}
