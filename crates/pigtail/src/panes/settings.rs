//! Settings window (spec §7.14, §5 M5): max lines, retention, theme, updates.

use crate::app::App;
use serialcore::config::{MAX_CONSOLE_FONT_SIZE, MIN_CONSOLE_FONT_SIZE};

impl App {
    pub(crate) fn show_settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        let mut changed = false;
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
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut self.config.settings.max_lines)
                                    .speed(10_000)
                                    .range(10_000..=10_000_000),
                            )
                            .on_hover_text("Full capture always remains on disk")
                            .changed();
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
                            .checkbox(
                                &mut self.config.settings.check_updates,
                                "check at startup",
                            )
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
        if changed {
            self.write_config();
        }
        if check_now {
            self.start_update_check(true);
        }
    }
}
