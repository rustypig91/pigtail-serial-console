//! Settings window (spec §7.14, §5 M5): max lines, retention, theme, updates.

use crate::app::{history_limits, App, RetentionCleanupConfirmation};
use serialcore::config::{MAX_CONSOLE_FONT_SIZE, MIN_CONSOLE_FONT_SIZE};
use std::path::PathBuf;

impl App {
    pub(crate) fn show_settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        let mut changed = false;
        let mut history_limit_changed = false;
        let mut history_limit_dragged = false;
        // Started after the window closes its borrow on `self`.
        let mut check_now = false;
        egui::Window::new("Settings")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                egui::Grid::new("settings_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Console text size");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut self.config.settings.console_font_size)
                                    .speed(0.2)
                                    .range(MIN_CONSOLE_FONT_SIZE..=MAX_CONSOLE_FONT_SIZE)
                                    .suffix(" pt"),
                            )
                            .on_hover_text("Ctrl+scroll over the console changes this too")
                            .changed();
                        ui.end_row();

                        ui.label("Long lines");
                        changed |= ui
                            .checkbox(&mut self.config.settings.wrap_lines, "wrap")
                            .on_hover_text(
                                "Off: a long line runs past the right edge and is clipped",
                            )
                            .changed();
                        ui.end_row();

                        ui.label("Max lines in memory");
                        let response = ui.add(
                            egui::DragValue::new(&mut self.config.settings.max_lines)
                                .speed(10_000)
                                .range(10_000..=10_000_000),
                        );
                        let limits = history_limits(self.config.settings.max_lines);
                        history_limit_dragged = response.dragged();
                        history_limit_changed = response
                            .on_hover_text(format!(
                                "Also keeps up to {} of raw bytes in Hex, {} points per plotted \
                                 series, and preloads up to {} when reopening a tab. Full capture \
                                 always remains on disk.",
                                format_bytes(limits.raw_bytes),
                                limits.series_points,
                                format_bytes(limits.preload_bytes),
                            ))
                            .changed();
                        changed |= history_limit_changed;
                        ui.end_row();

                        ui.label("Session retention (days)");
                        let saved_retention = self.config.settings.session_retention_days;
                        let draft = self.session_retention_draft.get_or_insert(saved_retention);
                        let response = ui.add_enabled(
                            self.retention_cleanup_confirmation.is_none(),
                            egui::DragValue::new(draft).range(1..=3650),
                        );
                        let days = *draft;
                        // Typing produces an edit event for every character.
                        // Wait until the user leaves the field before previewing
                        // deletion, so e.g. entering 20 never prompts at 2.
                        if days != saved_retention
                            && (response.lost_focus()
                                || (response.changed() && !response.has_focus()))
                        {
                            self.request_session_retention_change(days);
                        }
                        ui.end_row();

                        ui.label("Theme");
                        let mut dark = self.config.settings.theme != "light";
                        let mut theme_changed =
                            ui.selectable_value(&mut dark, true, "dark").clicked();
                        theme_changed |= ui.selectable_value(&mut dark, false, "light").clicked();
                        if theme_changed {
                            self.config.settings.theme =
                                if dark { "dark".into() } else { "light".into() };
                            ctx.set_visuals(if dark {
                                egui::Visuals::dark()
                            } else {
                                egui::Visuals::light()
                            });
                            changed = true;
                        }
                        ui.end_row();

                        ui.label("Updates");
                        changed |= ui
                            .checkbox(&mut self.config.settings.check_updates, "check at startup")
                            .on_hover_text(
                                "Asks GitHub for the newest release. \
                                 Updates are downloaded only when you press Update.",
                            )
                            .changed();
                        ui.end_row();
                    });

                ui.separator();
                ui.label(format!("Config: {}", self.paths.config_file.display()));
                ui.label(format!("Sessions: {}", self.paths.sessions.display()));
                ui.horizontal(|ui| {
                    ui.weak(concat!("pigtail v", env!("CARGO_PKG_VERSION")));
                    let checking = self.update_rx.is_some() || self.install_rx.is_some();
                    if ui
                        .add_enabled(!checking, egui::Button::new("Check for updates"))
                        .clicked()
                    {
                        check_now = true;
                    }
                    if checking {
                        ui.spinner();
                    }
                });
            });

        self.show_settings = open;
        if history_limit_changed {
            let limits = history_limits(self.config.settings.max_lines);
            for conn in &mut self.connections {
                conn.apply_history_limits(limits);
            }
        }
        // `dragged()` becomes false on the release frame. Any increases made
        // across the drag are now backfilled once at the final capacity. This
        // also settles keyboard edits and a pending change if the window closes.
        if !history_limit_dragged && self.finish_history_capacity_changes() {
            // Capacity settling happens after the plot, console, and merged
            // caches were drawn for this frame. A quiet connection has no
            // reader wake to show the backfill or cache rebuild, so schedule
            // the one follow-up frame that consumes the settled state.
            ctx.request_repaint();
        }
        if changed {
            self.write_config();
        }
        if check_now {
            self.start_update_check(true);
        }
        self.show_retention_cleanup_confirmation(ctx);
    }

    fn apply_session_retention(&mut self, days: u32, paths: &[PathBuf]) {
        self.config.settings.session_retention_days = days;
        self.session_retention_draft = None;
        match serialcore::session::remove_session_paths(paths) {
            Ok(removed) => tracing::info!("removed {removed} expired session capture(s)"),
            Err(error) => tracing::warn!("session cleanup failed: {error}"),
        }
        self.write_config();
    }

    fn request_session_retention_change(&mut self, days: u32) {
        match serialcore::session::old_session_paths(&self.paths.sessions, days) {
            Ok(old) if old.is_empty() => self.apply_session_retention(days, &old),
            Ok(old) => {
                self.retention_cleanup_confirmation =
                    Some(RetentionCleanupConfirmation { days, paths: old });
            }
            Err(error) => {
                tracing::warn!("couldn't preview session cleanup: {error}");
                // A preview is the user's chance to approve a destructive
                // change. Do not retry it as cleanup here: a transient error
                // could make that second scan succeed and delete captures the
                // user never saw or approved.
                self.session_retention_draft = None;
            }
        }
    }

    fn show_retention_cleanup_confirmation(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.retention_cleanup_confirmation else {
            return;
        };
        let days = pending.days;
        let captures = pending.paths.len();
        let paths = pending.paths.clone();
        let mut open = true;
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("Remove expired session captures?")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Changing retention to {days} days will delete {captures} stored session {}.",
                    if captures == 1 { "capture" } else { "captures" },
                ));
                ui.label("This cannot be undone.");
                ui.horizontal(|ui| {
                    confirm = ui.button("Remove captures").clicked();
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if confirm {
            self.retention_cleanup_confirmation = None;
            self.apply_session_retention(days, &paths);
        } else if cancel || !open {
            self.retention_cleanup_confirmation = None;
            self.session_retention_draft = None;
        }
    }
}

fn format_bytes(bytes: usize) -> String {
    const MIB: f64 = (1024 * 1024) as f64;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use crate::app::tests::test_app;

    #[test]
    fn failed_retention_preview_keeps_the_saved_setting() {
        let (mut app, _enum_tx) = test_app("retention-preview-failure");
        let saved = app.config.settings.session_retention_days;
        std::fs::create_dir_all(app.paths.sessions.parent().unwrap()).unwrap();
        std::fs::write(&app.paths.sessions, b"not a directory").unwrap();
        app.session_retention_draft = Some(1);

        app.request_session_retention_change(1);

        assert_eq!(app.config.settings.session_retention_days, saved);
        assert!(app.session_retention_draft.is_none());
        std::fs::remove_dir_all(app.paths.sessions.parent().unwrap()).ok();
    }
}
