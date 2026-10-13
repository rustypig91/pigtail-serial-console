//! Rusty's Pigtail - Serial Terminal.
//!
//! This crate is a thin egui shell over `serialcore`. All logic that is not
//! about drawing widgets belongs in the core crate (spec §12).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod graphics;
mod panes;
mod paths;
mod search_history;
mod terminal;
#[cfg(test)]
mod test_ui;
mod wrap;

use anyhow::Context;

static RESTART_PATH: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

fn main() -> anyhow::Result<()> {
    // Keep demo builds completely separate from the user's config and captures.
    #[cfg(feature = "demo")]
    let demo_dir = tempfile::tempdir().context("creating demo directory")?;
    #[cfg(feature = "demo")]
    let dirs = paths::AppPaths {
        config_file: demo_dir.path().join("pigtail.toml"),
        sessions: demo_dir.path().join("sessions"),
        crash_log: demo_dir.path().join("crash.log"),
    };
    #[cfg(not(feature = "demo"))]
    let dirs = paths::AppPaths::resolve().context("resolving app directories")?;
    install_logging(&dirs.crash_log.with_file_name("app.log"));
    install_panic_hook(dirs.crash_log.clone());
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting Pigtail");

    // Load configuration before creating the UI.
    let cfg = if cfg!(feature = "demo") {
        serialcore::config::Config::default()
    } else {
        app::load_config(&dirs)
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(if cfg!(feature = "demo") {
            [1280.0, 900.0]
        } else {
            [1100.0, 720.0]
        })
        .with_min_inner_size([700.0, 400.0])
        .with_decorations(false)
        .with_title(concat!(
            "Rusty's Pigtail - Serial Terminal v",
            env!("CARGO_PKG_VERSION")
        ));
    // Application/window icon, embedded at build time.
    match eframe::icon_data::from_png_bytes(include_bytes!("icon.png")) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(e) => tracing::warn!("loading window icon: {e}"),
    }

    let graphics_context = std::sync::Arc::new(std::sync::OnceLock::new());
    let native_options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: graphics::configuration(graphics_context.clone()),
        ..Default::default()
    };

    eframe::run_native(
        "pigtail",
        native_options,
        Box::new(move |cc| {
            let _ = graphics_context.set(cc.egui_ctx.clone());
            if let Some(render_state) = &cc.wgpu_render_state {
                tracing::info!(adapter = ?render_state.adapter.get_info(), "graphics adapter selected");
            }
            Ok(Box::new(app::App::new(cc, dirs, cfg)))
        }),
    )
    .map_err(|e| {
        tracing::error!("eframe error: {e}");
        anyhow::anyhow!("eframe error: {e}")
    })?;
    // run_native has dropped App and its serial connections before restarting.
    if let Some(path) = RESTART_PATH.lock().expect("restart path lock").take() {
        let mut command = std::process::Command::new(&path);
        if let Some(directory) = path.parent() {
            command.current_dir(directory);
        }
        // An AppImage must establish a fresh runtime and mount for the new file.
        for key in ["APPIMAGE", "APPDIR", "OWD", "ARGV0"] {
            command.env_remove(key);
        }
        if let Err(error) = command.spawn() {
            tracing::error!(%error, "could not restart after update");
            rfd::MessageDialog::new()
                .set_title("Update installed")
                .set_description(format!(
                    "Rusty's Pigtail - Serial Terminal was updated, but could not restart: {error}. Please open it again."
                ))
                .set_level(rfd::MessageLevel::Error)
                .show();
        }
    }
    Ok(())
}

/// Keep diagnostics from the latest two normal launches alongside the panic log.
/// Demo builds log to stderr and never leave diagnostics in regular app data.
fn install_logging(path: &std::path::Path) {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;

    let file = if cfg!(feature = "demo") {
        None
    } else {
        if path.is_file() {
            if let Err(error) = std::fs::rename(path, path.with_file_name("app.previous.log")) {
                eprintln!("could not preserve {}: {error}", path.display());
            }
        }
        path.parent()
            .map(std::fs::create_dir_all)
            .transpose()
            .and_then(|_| std::fs::File::create(path))
            .map_err(|error| eprintln!("could not create {}: {error}", path.display()))
            .ok()
    };
    let writer = match file {
        Some(file) => BoxMakeWriter::new(std::sync::Mutex::new(file)),
        None => BoxMakeWriter::new(std::io::stderr),
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .with_writer(writer)
        .init();
}

/// Append every panic to a file, then let the default hook run.
///
/// Windows release builds have no console attached. Record the panic and its
/// backtrace separately from ordinary tracing, even if logging could not start.
/// Write straight through rather than buffering so the record survives exit.
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
