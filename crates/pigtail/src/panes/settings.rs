//! Settings window (spec §7.14, §5 M5): max lines, theme, updates.

use crate::app::{history_limits, App};
use serialcore::config::{MAX_CONSOLE_FONT_SIZE, MIN_CONSOLE_FONT_SIZE};

impl App {
    pub(crate) fn show_settings_window(&mut self, ctx: &egui::Context) {
        let defaults = serialcore::config::Settings::default();
        let mut open = self.show_settings;
        let mut changed = false;
        let mut history_limit_changed = false;
        let mut history_limit_dragged = false;
        let mut history_limit_editing = false;
        let mut vt_limit_editing = false;
        let mut folder_to_open = None;
        if open {
            let response = super::chrome::app_modal(ctx, "Settings").show(ctx, |ui| {
                ui.set_width(420.0);
                super::chrome::dialog_style(ui);
                super::chrome::modal_header(ui, "Settings", &mut open);
                let max_height = (ctx.content_rect().height() - 120.0).max(180.0);
                egui::ScrollArea::vertical()
                    .id_salt("settings_scroll")
                    .max_height(max_height)
                    .min_scrolled_height(max_height.min(690.0))
                    .show(ui, |ui| {
                        settings_section(ui, "Appearance", "Customize the console and application theme.", |ui| {
                        if reset_setting(ui, "Theme", "Choose a light or dark appearance for the application and console. Each theme keeps its own base color.", &mut self.config.settings.theme, defaults.theme.clone()) {
                            super::chrome::apply_settings_visuals(ctx, &self.config.settings);
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
                            super::chrome::apply_settings_visuals(ctx, &self.config.settings);
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
                            super::chrome::apply_settings_visuals(ctx, &self.config.settings);
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

                        });
                        ui.add_space(10.0);
                        settings_section(ui, "History", "Manage retained console history and session captures.", |ui| {
                        let history_reset = reset_setting(ui, "Max log lines", "Limit retained console history and the related Hex and plot memory budgets. Stored captures are deleted once all their lines fall outside this limit.", &mut self.config.settings.max_lines, defaults.max_lines);
                        changed |= history_reset;
                        let response = ui.add(
                            egui::DragValue::new(&mut self.config.settings.max_lines)
                                .speed(10_000)
                                .range(10_000..=10_000_000),
                        );
                        let limits = history_limits(self.config.settings.max_lines);
                        history_limit_dragged = response.dragged();
                        history_limit_editing = response.dragged() || response.has_focus();
                        history_limit_changed = history_reset || response
                            .on_hover_text(format!(
                                "Also keeps up to {} of raw bytes in Hex, {} points per plotted \
                                 series. Stored captures follow the log line limit.",
                                format_bytes(limits.raw_bytes),
                                limits.series_points,
                            ))
                            .changed();
                        changed |= history_limit_changed;
                        ui.end_row();

                        changed |= reset_setting(ui, "VT scrollback rows", "Set how many previous rows the ANSI/VT terminal retains for scrolling and search. Use 0 to disable scrollback. Changes apply when editing finishes.", &mut self.config.settings.vt_scrollback_rows, defaults.vt_scrollback_rows);
                        let response = ui.add(
                            egui::DragValue::new(&mut self.config.settings.vt_scrollback_rows)
                                .speed(100)
                                .range(0..=serialcore::config::MAX_VT_SCROLLBACK_ROWS),
                        ).on_hover_text("Rows of VT history kept per connection (0 disables scrollback). Applies when editing finishes, rebuilding from retained received bytes. Captures follow the log line limit.");
                        vt_limit_editing = response.dragged() || response.has_focus();
                        changed |= response.changed();
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
                        egui::Frame::NONE
                            .fill(ui.visuals().faint_bg_color)
                            .stroke(ui.visuals().window_stroke)
                            .corner_radius(8.0)
                            .inner_margin(12.0)
                            .show(ui, |ui| {
                                ui.set_min_width((ui.available_width() - 1.0).max(0.0));
                                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                                ui.strong("Folders");
                                ui.weak("Click a path to open it in your file manager.");
                                for (label, file, logs) in [
                                    ("Configuration", &self.paths.config_file, false),
                                    ("Logs and session captures", &self.paths.crash_log, true),
                                ] {
                                    if let Some(directory) = file.parent() {
                                        ui.add_space(8.0);
                                        ui.label(label);
                                        let path = directory.display().to_string();
                                        if ui.link(&path).on_hover_text(format!("Open {path}")).clicked() {
                                            // Defer until after the immutable paths borrow ends.
                                            folder_to_open = Some(logs);
                                        }
                                    }
                                }
                            });
                        ui.add_space(8.0);
                        ui.weak(concat!(
                            "Rusty's Pigtail · v",
                            env!("CARGO_PKG_VERSION")
                        ));
                        ui.add_space(12.0);
                        if ui.button("Reset all")
                            .on_hover_text("Restore all settings to their defaults, including both color palettes and update preferences.")
                            .clicked()
                        {
                            self.reset_all_settings();
                            super::chrome::apply_settings_visuals(ctx, &self.config.settings);
                            history_limit_changed = true;
                            history_limit_dragged = false;
                            history_limit_editing = false;
                            vt_limit_editing = false;
                            changed = true;
                        }
                    });
            });
            if response.should_close() {
                open = false;
            }
        }

        if let Some(logs) = folder_to_open {
            self.open_folder(logs);
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
        // Commit deletion only after the line-limit edit finishes, avoiding
        // intermediate values while typing or dragging.
        if !history_limit_editing || !open {
            self.request_session_cleanup();
        }
        if changed {
            ctx.request_repaint();
            self.write_config();
        }
    }

    fn reset_all_settings(&mut self) {
        self.config.settings = serialcore::config::Settings::default();
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
    egui::Frame::NONE
        .fill(ui.visuals().faint_bg_color)
        .stroke(ui.visuals().window_stroke)
        .corner_radius(8.0)
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
    fn reset_all_restores_settings() {
        let (mut app, _enum_tx) = test_app("reset-all");
        app.config.settings.theme = "light".into();
        app.config.settings.max_lines = 50_000;
        app.config.settings.check_updates = false;
        app.reset_all_settings();
        assert_eq!(app.config.settings, serialcore::config::Settings::default());
    }
}
