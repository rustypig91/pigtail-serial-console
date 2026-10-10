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
        let mut vt_limit_editing = false;
        if open {
            let response = super::chrome::app_modal(ctx, "Settings").show(ctx, |ui| {
                ui.set_width(420.0);
                super::chrome::dialog_style(ui);
                super::chrome::modal_header(ui, "Settings", &mut open);
                let max_height = (ctx.screen_rect().height() - 120.0).max(180.0);
                egui::ScrollArea::vertical()
                    .id_salt("settings_scroll")
                    .max_height(max_height)
                    .min_scrolled_height(max_height.min(690.0))
                    .show(ui, |ui| {
                        settings_section(ui, "Appearance", "Customize the console and application theme.", |ui| {
                        ui.label("Theme");
                        let mut dark = self.config.settings.theme != "light";
                        let theme_changed = ui.horizontal(|ui| {
                            let dark_clicked = ui.selectable_value(&mut dark, true, "Dark").clicked();
                            let light_clicked = ui.selectable_value(&mut dark, false, "Light").clicked();
                            dark_clicked || light_clicked
                        }).inner;
                        if theme_changed {
                            self.config.settings.theme =
                                if dark { "dark".into() } else { "light".into() };
                            ctx.set_visuals(super::app_visuals(dark));
                            changed = true;
                        }
                        ui.end_row();

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
                            .checkbox(&mut self.config.settings.wrap_lines, "Wrap long lines")
                            .on_hover_text(
                                "Off: a long line runs past the right edge and is clipped",
                            )
                            .changed();
                        ui.end_row();
                        });
                        ui.add_space(10.0);
                        settings_section(ui, "Interaction", "Choose how tabs close and data is sent.", |ui| {
                        ui.label("Closing tabs");
                        changed |= ui
                            .checkbox(
                                &mut self.config.settings.confirm_tab_close,
                                "Ask before closing a tab",
                            )
                            .changed();
                        ui.end_row();

                        ui.label("Send delay");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut self.config.settings.send_delay_ms)
                                    .range(0..=60_000)
                                    .suffix(" ms"),
                            )
                            .on_hover_text(
                                "Minimum delay between outgoing bytes for typing, paste, macros, \
                                 and file transfers. 0 sends without a delay. UTF-8 characters \
                                 may contain multiple bytes.",
                            )
                            .changed();
                        ui.end_row();
                        });
                        ui.add_space(10.0);
                        settings_section(ui, "History", "Manage retained console history and session captures.", |ui| {
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

                        ui.label("VT scrollback rows");
                        let response = ui.add(
                            egui::DragValue::new(&mut self.config.settings.vt_scrollback_rows)
                                .speed(100)
                                .range(0..=serialcore::config::MAX_VT_SCROLLBACK_ROWS),
                        ).on_hover_text("Rows of VT history kept per connection (0 disables scrollback). Applies when editing finishes, rebuilding from retained received bytes. Full capture stays on disk.");
                        vt_limit_editing = response.dragged() || response.has_focus();
                        changed |= response.changed();
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
                        });
                        ui.add_space(10.0);
                        settings_section(ui, "Updates", "Keep the application up to date.", |ui| {
                        ui.label("Updates");
                        changed |= ui
                            .checkbox(&mut self.config.settings.check_updates, "Check at startup")
                            .on_hover_text(
                                "Asks GitHub for the newest release. \
                                 Updates are downloaded only when you press Update.",
                            )
                            .changed();
                        ui.end_row();
                        });
                        ui.add_space(10.0);
                        ui.collapsing("Storage locations", |ui| {
                            ui.weak(format!("Config: {}", self.paths.config_file.display()));
                            ui.weak(format!("Sessions: {}", self.paths.sessions.display()));
                        });
                        ui.add_space(8.0);
                        ui.weak(concat!(
                            "Rusty's Pigtail · v",
                            env!("CARGO_PKG_VERSION")
                        ));
                    });
            });
            if self.retention_cleanup_confirmation.is_none() && response.should_close() {
                open = false;
            }
        }

        self.show_settings = open;
        if !vt_limit_editing || !open {
            for conn in &mut self.connections {
                conn.set_vt_scrollback_rows(self.config.settings.vt_scrollback_rows);
            }
        }
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
        let response = super::chrome::app_modal(ctx, "retention_confirmation").show(ctx, |ui| {
            super::chrome::modal_header(ui, "Remove expired session captures?", &mut open);
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

        if response.should_close() {
            cancel = true;
        }
        if confirm {
            self.retention_cleanup_confirmation = None;
            self.apply_session_retention(days, &paths);
        } else if cancel || !open {
            self.retention_cleanup_confirmation = None;
            self.session_retention_draft = None;
        }
    }
}

fn settings_section(
    ui: &mut egui::Ui,
    title: &str,
    description: &str,
    contents: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::none()
        .fill(ui.visuals().faint_bg_color)
        .stroke(ui.visuals().window_stroke)
        .rounding(8.0)
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.set_min_width((ui.available_width() - 1.0).max(0.0));
            ui.strong(title);
            ui.weak(description);
            ui.add_space(4.0);
            egui::Grid::new(("settings_section", title))
                .num_columns(2)
                .min_col_width(176.0)
                .spacing([24.0, 10.0])
                .min_row_height(32.0)
                .show(ui, contents);
        });
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
