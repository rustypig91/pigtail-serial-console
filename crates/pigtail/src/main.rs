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
