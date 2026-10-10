//! Settings window (spec §7.14, §5 M5): max lines, retention, theme, updates.

use crate::app::{history_limits, App, RetentionCleanupConfirmation};
use serialcore::config::{MAX_CONSOLE_FONT_SIZE, MIN_CONSOLE_FONT_SIZE};
use std::path::PathBuf;

impl App {
    pub(crate) fn show_settings_window(&mut self, ctx: &egui::Context) {
        let defaults = serialcore::config::Settings::default();
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
                        if reset_setting(ui, "Theme", "Choose a light or dark appearance for the application and console. Each theme keeps its own base color.", &mut self.config.settings.theme, defaults.theme.clone()) {
                            ctx.set_visuals(super::chrome::settings_visuals(&self.config.settings));
                            changed = true;
                        }
                        let mut dark = self.config.settings.theme != "light";
                        let theme_changed = ui.horizontal(|ui| {
                            let dark_clicked = ui.selectable_value(&mut dark, true, "Dark").on_hover_text("Use dark backgrounds and light text. Your dark theme base color is restored when you switch back.").clicked();
                            let light_clicked = ui.selectable_value(&mut dark, false, "Light").on_hover_text("Use light backgrounds and dark text. Your light theme base color is restored when you switch back.").clicked();
                            dark_clicked || light_clicked
                        }).inner;
                        if theme_changed {
                            self.config.settings.theme =
                                if dark { "dark".into() } else { "light".into() };
                            ctx.set_visuals(super::chrome::settings_visuals(&self.config.settings));
                            changed = true;
                        }
                        ui.end_row();

                        let base = if dark {
                            &mut self.config.settings.dark_base_color
                        } else {
                            &mut self.config.settings.light_base_color
                        };
                        let color_reset = reset_setting(ui, "Base color", "Choose the color palette for backgrounds, tabs, and controls in the current theme. The other theme keeps its own choice.", base, if dark { defaults.dark_base_color } else { defaults.light_base_color });
                        let color_changed = ui.horizontal(|ui| {
                            use serialcore::config::BaseColor;
                            let mut changed = false;
                            for (value, label) in [(BaseColor::Blue, "Blue"), (BaseColor::Green, "Green"), (BaseColor::Red, "Red")] {
                                changed |= base_color_swatch(ui, base, value, label, dark);
                            }
                            changed
                        }).inner;
                        if color_changed || color_reset {
                            ctx.set_visuals(super::chrome::settings_visuals(&self.config.settings));
                            changed = true;
                        }
                        ui.end_row();

                        changed |= reset_setting(ui, "Header opacity", "Control how much console text shows through the active tab and lower toolbar. At 100%, these surfaces are fully opaque.", &mut self.config.settings.header_opacity, defaults.header_opacity);
                        let mut opacity = (f32::from(self.config.settings.header_opacity) * 100.0 / 255.0).round();
                        if ui.add(egui::Slider::new(&mut opacity, 0.0..=100.0).suffix("%").integer())
                            .on_hover_text("Opacity of the active tab and lower toolbar/search area. 100% is fully opaque.")
                            .changed()
                        {
                            self.config.settings.header_opacity = (opacity * 255.0 / 100.0).round() as u8;
                            changed = true;
                        }
                        ui.end_row();

                        changed |= reset_setting(ui, "Console text size", "Set the size of console text in points. You can also hold Ctrl and scroll over the console to adjust it.", &mut self.config.settings.console_font_size, defaults.console_font_size);
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

                        changed |= reset_setting(ui, "Long lines", "Wrap long console lines onto additional rows so they remain readable. When off, text beyond the right edge is clipped.", &mut self.config.settings.wrap_lines, defaults.wrap_lines);
                        changed |= ui
                            .checkbox(&mut self.config.settings.wrap_lines, "Wrap long lines")
                            .on_hover_text(
                                "Wrap long console lines onto additional rows so they remain readable. When off, text beyond the right edge is clipped.",
                            )
                            .changed();
                        ui.end_row();
                        });
                        ui.add_space(10.0);
                        settings_section(ui, "Interaction", "Choose how tabs close and data is sent.", |ui| {
                        changed |= reset_setting(ui, "Closing tabs", "Ask for confirmation before closing a tab, helping prevent accidental closure of an active connection or merged view.", &mut self.config.settings.confirm_tab_close, defaults.confirm_tab_close);
                        changed |= ui
                            .checkbox(
                                &mut self.config.settings.confirm_tab_close,
                                "Ask before closing a tab",
                            )
                            .on_hover_text("Ask before closing a connection or merged tab to help prevent accidental closure.")
                            .changed();
                        ui.end_row();

                        changed |= reset_setting(ui, "Send delay", "Set a minimum delay between outgoing bytes for typing, paste, macros, and file transfers. Use 0 for no delay; this can help slower devices keep up.", &mut self.config.settings.send_delay_ms, defaults.send_delay_ms);
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
                        let history_reset = reset_setting(ui, "Max lines in memory", "Limit retained console history and the related Hex and plot memory budgets. Older data leaves the live view, while full session captures remain on disk.", &mut self.config.settings.max_lines, defaults.max_lines);
                        changed |= history_reset;
                        let response = ui.add(
                            egui::DragValue::new(&mut self.config.settings.max_lines)
                                .speed(10_000)
                                .range(10_000..=10_000_000),
                        );
                        let limits = history_limits(self.config.settings.max_lines);
                        history_limit_dragged = response.dragged();
                        history_limit_changed = history_reset || response
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

                        changed |= reset_setting(ui, "VT scrollback rows", "Set how many previous rows the ANSI/VT terminal retains for scrolling and search. Use 0 to disable scrollback. Changes apply when editing finishes.", &mut self.config.settings.vt_scrollback_rows, defaults.vt_scrollback_rows);
                        let response = ui.add(
                            egui::DragValue::new(&mut self.config.settings.vt_scrollback_rows)
                                .speed(100)
                                .range(0..=serialcore::config::MAX_VT_SCROLLBACK_ROWS),
                        ).on_hover_text("Rows of VT history kept per connection (0 disables scrollback). Applies when editing finishes, rebuilding from retained received bytes. Full capture stays on disk.");
                        vt_limit_editing = response.dragged() || response.has_focus();
                        changed |= response.changed();
                        ui.end_row();

                        if setting_label(ui, "Session retention (days)", "Choose how long stored session captures are kept. Changing this setting checks for expired captures and asks before deleting any.", self.config.settings.session_retention_days != defaults.session_retention_days && self.retention_cleanup_confirmation.is_none()) {
                            self.session_retention_draft = None;
                            self.request_session_retention_change(defaults.session_retention_days);
                        }
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
                        changed |= reset_setting(ui, "Updates", "Check GitHub for a newer release when the application starts. Downloads begin only when you choose Update.", &mut self.config.settings.check_updates, defaults.check_updates);
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
                        ui.add_space(12.0);
                        if ui.add_enabled(self.retention_cleanup_confirmation.is_none(), egui::Button::new("Reset all"))
                            .on_hover_text("Restore all settings to their defaults, including both color palettes and update preferences. Any capture deletion still requires confirmation.")
                            .clicked()
                        {
                            self.reset_all_settings();
                            ctx.set_visuals(super::chrome::settings_visuals(&self.config.settings));
                            history_limit_changed = true;
                            history_limit_dragged = false;
                            vt_limit_editing = false;
                            changed = true;
                        }
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
            ctx.request_repaint();
            self.write_config();
        }
        self.show_retention_cleanup_confirmation(ctx);
    }

    fn reset_all_settings(&mut self) {
        let saved_retention = self.config.settings.session_retention_days;
        self.config.settings = serialcore::config::Settings::default();
        let default_retention = self.config.settings.session_retention_days;
        // Keep the saved retention until its existing cleanup flow succeeds
        // or the user approves any capture deletion.
        self.config.settings.session_retention_days = saved_retention;
        self.session_retention_draft = None;
        if saved_retention != default_retention {
            self.request_session_retention_change(default_retention);
        }
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

/// Show a compact, muted reset control only for customized settings.
fn setting_label(ui: &mut egui::Ui, label: &str, description: &str, customized: bool) -> bool {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(label).on_hover_text(description);
        if !customized {
            return false;
        }
        ui.spacing_mut().button_padding = egui::vec2(2.0, 1.0);
        ui.scope(|ui| {
            let muted = ui.visuals().weak_text_color();
            let foreground = ui.visuals().text_color();
            let widgets = &mut ui.visuals_mut().widgets;
            widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
            widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
            widgets.inactive.bg_stroke = egui::Stroke::NONE;
            widgets.inactive.fg_stroke.color = muted;
            widgets.hovered.fg_stroke.color = foreground;
            ui.add(
                egui::Button::new(egui::RichText::new("↺").size(12.0))
                    .small()
                    .min_size(egui::vec2(16.0, 16.0)),
            )
            .on_hover_text(format!("Reset {label} to default"))
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        })
        .inner
    })
    .inner
}

fn reset_setting<T: PartialEq>(
    ui: &mut egui::Ui,
    label: &str,
    description: &str,
    value: &mut T,
    default: T,
) -> bool {
    if setting_label(ui, label, description, *value != default) {
        *value = default;
        true
    } else {
        false
    }
}

fn base_color_swatch(
    ui: &mut egui::Ui,
    selected: &mut serialcore::config::BaseColor,
    value: serialcore::config::BaseColor,
    label: &str,
    dark: bool,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(32.0, 32.0), egui::Sense::click());
    let changed = response.clicked() && *selected != value;
    if changed {
        *selected = value;
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            *selected == value,
            label,
        )
    });
    let palette = super::chrome::palette_visuals(dark, value);
    // Preview a colored control surface: the panel background is nearly black
    // in dark mode, making the three palettes indistinguishable at this size.
    let color = if dark {
        palette.widgets.active.bg_fill
    } else {
        palette.widgets.inactive.bg_fill
    };
    ui.painter().circle_filled(rect.center(), 10.0, color);
    ui.painter().circle_stroke(
        rect.center(),
        10.0,
        egui::Stroke::new(1.0_f32, ui.visuals().weak_text_color()),
    );
    if *selected == value || response.has_focus() {
        ui.painter().circle_stroke(
            rect.center(),
            14.0,
            egui::Stroke::new(2.0_f32, ui.visuals().text_color()),
        );
    } else if response.hovered() {
        ui.painter().circle_stroke(
            rect.center(),
            14.0,
            egui::Stroke::new(1.0_f32, ui.visuals().weak_text_color()),
        );
    }
    response
        .on_hover_text(format!("Use the {label} palette for the {} theme. This changes application surfaces and accents immediately.", if dark { "dark" } else { "light" }))
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    changed
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
    fn reset_all_restores_settings_but_keeps_retention_when_preview_fails() {
        let (mut app, _enum_tx) = test_app("reset-all-preview-failure");
        let defaults = serialcore::config::Settings::default();
        app.config.settings.theme = "light".into();
        app.config.settings.dark_base_color = serialcore::config::BaseColor::Red;
        app.config.settings.send_delay_ms = 123;
        app.config.settings.max_lines = 50_000;
        app.config.settings.check_updates = false;
        app.config.settings.skipped_version = Some("9.0.0".into());
        let retained_days = defaults.session_retention_days + 1;
        app.config.settings.session_retention_days = retained_days;
        std::fs::create_dir_all(app.paths.sessions.parent().unwrap()).unwrap();
        std::fs::write(&app.paths.sessions, b"not a directory").unwrap();

        app.reset_all_settings();

        let expected = serialcore::config::Settings {
            session_retention_days: retained_days,
            ..defaults
        };
        assert_eq!(app.config.settings, expected);
        assert!(app.session_retention_draft.is_none());
        std::fs::remove_dir_all(app.paths.sessions.parent().unwrap()).ok();
    }

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
