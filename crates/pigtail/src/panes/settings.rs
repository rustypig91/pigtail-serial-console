//! Settings window (spec §7.14, §5 M5): max lines, retention, theme, updates.

use crate::app::{history_limits, App};
use serialcore::config::{MAX_CONSOLE_FONT_SIZE, MIN_CONSOLE_FONT_SIZE};

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
                        changed |= ui
                            .add(
                                egui::DragValue::new(
                                    &mut self.config.settings.session_retention_days,
                                )
                                .range(1..=3650),
                            )
                            .changed();
                        ui.end_row();

                        ui.label("Theme");
                        let mut dark = self.config.settings.theme != "light";
                        if ui.selectable_value(&mut dark, true, "dark").clicked()
                            || ui.selectable_value(&mut dark, false, "light").clicked()
                        {
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
                                 The only network request pigtail makes.",
                            )
                            .changed();
                        ui.end_row();
                    });

                ui.separator();
                ui.label(format!("Config: {}", self.paths.config_file.display()));
                ui.label(format!("Sessions: {}", self.paths.sessions.display()));
                ui.horizontal(|ui| {
                    ui.weak(concat!("pigtail v", env!("CARGO_PKG_VERSION")));
                    let checking = self.update_rx.is_some();
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
