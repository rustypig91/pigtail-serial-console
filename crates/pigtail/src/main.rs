//! pigtail — a desktop serial terminal for embedded firmware development.
//!
//! This crate is a thin egui shell over `serialcore`. All logic that is not
//! about drawing widgets belongs in the core crate (spec §12).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod panes;
mod paths;
mod wrap;

use anyhow::Context;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let dirs = paths::AppPaths::resolve().context("resolving app directories")?;
    install_panic_hook(dirs.crash_log.clone());

    // Best-effort retention cleanup at startup (spec §7.5).
    let cfg = app::load_config(&dirs);
    if let Err(e) = serialcore::session::cleanup_old_sessions(
        &dirs.sessions,
        cfg.settings.session_retention_days,
    ) {
        tracing::warn!("session cleanup failed: {e}");
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1100.0, 720.0])
        .with_min_inner_size([700.0, 400.0])
        .with_title(concat!("Pigtail v", env!("CARGO_PKG_VERSION")));
    // Application/window icon, embedded at build time.
    match eframe::icon_data::from_png_bytes(include_bytes!("icon.png")) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(e) => tracing::warn!("loading window icon: {e}"),
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "pigtail",
        native_options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, dirs, cfg)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))
}

/// Append every panic to a file, then let the default hook run.
///
/// A release build is a `windows` subsystem binary with no console attached
/// and no log file, so a panic on the UI thread takes the window down leaving
/// the user with nothing to report but "it crashed". This is the whole record
/// of what happened, and it survives the process — which is why it is written
/// straight through rather than buffered.
fn install_panic_hook(path: std::path::PathBuf) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write;
        let entry = format!(
            "{} v{} thread {:?}\n{info}\n{}\n\n",
            chrono::Utc::now().to_rfc3339(),
            env!("CARGO_PKG_VERSION"),
            std::thread::current().name().unwrap_or("unnamed"),
            std::backtrace::Backtrace::force_capture(),
        );
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(entry.as_bytes());
        }
        default(info);
    }));
}
