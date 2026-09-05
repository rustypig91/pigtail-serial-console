//! Floating tool windows (filters, highlight, plot extraction), toggled from the
//! console right-click menu so the main window stays uncluttered.

use crate::app::{App, TabId};
use serialcore::config::{ExtractMode, ExtractRule, HighlightRule};
use serialcore::filter::{Combine, FilterRule};
use serialcore::reader::ErrorScope;

impl App {
    pub(crate) fn show_tab_close_confirmation(&mut self, ctx: &egui::Context) {
        let Some((id, mut do_not_ask)) = self.tab_close_confirmation else {
            return;
        };
        let label = match id {
            TabId::Connection(port) => self
                .connections
                .iter()
                .find(|conn| conn.id == port)
                .map(|conn| conn.display_label().to_owned()),
            TabId::Merged(tab_id) => self
                .merged_tabs
                .iter()
                .find(|tab| tab.id == tab_id)
                .map(|tab| tab.name.clone()),
        };
        let Some(label) = label else {
            self.tab_close_confirmation = None;
            return;
        };
        let mut open = true;
        let mut close = false;
        let mut cancel = false;
        egui::Window::new("Close tab?")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!("Are you sure you want to close \"{}\"?", label));
                if matches!(id, TabId::Connection(_)) {
                    ui.label("Closing the tab disconnects this connection.");
                } else {
                    ui.label("The connections included in this view will stay open.");
                }
                ui.checkbox(&mut do_not_ask, "Do not ask me again");
                ui.horizontal(|ui| {
                    cancel = ui.button("Cancel").clicked();
                    close = ui.button("Close tab").clicked();
                });
            });
        if close {
            self.tab_close_confirmation = None;
            if do_not_ask {
                self.config.settings.confirm_tab_close = false;
                self.write_config();
            }
            match id {
                TabId::Connection(port) => {
                    if let Some(index) = self.connections.iter().position(|conn| conn.id == port) {
                        self.close_connection(index);
                    }
                }
                TabId::Merged(tab_id) => {
                    if let Some(index) = self.merged_tabs.iter().position(|tab| tab.id == tab_id) {
                        self.close_merged_tab(index);
                    }
                }
            }
        } else if cancel || !open {
            self.tab_close_confirmation = None;
        } else {
            self.tab_close_confirmation = Some((id, do_not_ask));
        }
    }

    /// Handle Escape before console input or text fields can consume it.
    pub(crate) fn close_window_on_escape(&mut self, ctx: &egui::Context) {
        if ctx.is_context_menu_open() || !ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            return;
        }
        let top = ctx.memory(|m| {
            // Let dropdowns and context menus handle Escape first.
            if m.any_popup_open() {
                return None;
            }
            // The layer order retains closed windows; only consider visible
            // top-level windows when selecting the next Escape target.
            m.layer_ids()
                .filter(|layer| {
                    layer.order == egui::Order::Middle
                        && m.areas().is_visible(layer)
                        && m.areas().parent_layer(*layer).is_none()
                })
                .last()
        });
        let Some(top) = top else { return };
        let is_window = |name: &str| top.id == egui::Id::new(name);
        let mut closed = false;
        for (name, open) in [
            ("Settings", &mut self.show_settings),
            ("Keyboard shortcuts", &mut self.show_keyboard_shortcuts),
            ("Filters", &mut self.show_filters_win),
            ("Highlight rules", &mut self.show_highlight_win),
            ("Plot extraction", &mut self.show_extract_win),
            ("Transmit macros", &mut self.show_macros_win),
        ] {
            if *open && is_window(name) {
                *open = false;
                closed = true;
            }
        }
        if self.tab_close_confirmation.is_some() && is_window("Close tab?") {
            self.tab_close_confirmation = None;
            closed = true;
        } else if self.show_error_win.is_some() && is_window("error_window") {
            self.show_error_win = None;
            closed = true;
        } else if self.retention_cleanup_confirmation.is_some()
            && is_window("Remove expired session captures?")
        {
            self.retention_cleanup_confirmation = None;
            self.session_retention_draft = None;
            closed = true;
        } else if self.macro_running_edit_confirmation.is_some() && is_window("Macro is running") {
            self.macro_running_edit_confirmation = None;
            closed = true;
        } else if self.macro_editor.is_some() && (is_window("Edit macro") || is_window("Add macro"))
        {
            self.macro_editor = None;
            closed = true;
        } else if self.macro_shortcut_conflict.is_some() && is_window("Shortcut already used") {
            self.macro_shortcut_conflict = None;
            closed = true;
        } else if self.config_dialog.is_some()
            && (is_window("Port options") || is_window("New connection"))
        {
            self.config_dialog = None;
            closed = true;
        } else if self.rename_dialog.is_some() && is_window("Rename tab") {
            self.rename_dialog = None;
            closed = true;
        } else if self.file_transfer_dialog.is_some() && is_window("Send file") {
            self.file_transfer_dialog = None;
            closed = true;
        } else if self
            .connect_errors
            .front()
            .is_some_and(|error| is_window(error.title))
        {
            self.connect_errors.pop_front();
            closed = true;
        } else if self
            .update_dialog
            .as_ref()
            .is_some_and(|dialog| is_window(&dialog.title))
            && self.install_rx.is_none()
        {
            self.update_dialog = None;
            closed = true;
        } else if is_window("file_transfer_progress") {
            if let Some(conn) = self
                .connections
                .iter()
                .find(|conn| conn.transfer_progress.is_some())
            {
                conn.handle.cancel_transfer();
                closed = true;
            }
        }
        if closed {
            ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
            });
        }
    }

    pub(crate) fn show_tool_windows(&mut self, ctx: &egui::Context) {
        self.show_filters_window(ctx);
        self.show_highlight_window(ctx);
        self.show_extract_window(ctx);
        self.show_error_window(ctx);
    }

    /// The full error message, opened by clicking the footer's `⚠ error`
    /// indicator. Scoped to the connection it was opened for (not "whichever
    /// tab is active"), so switching tabs while it's open doesn't swap the
    /// message. Closes itself if that connection's error clears (e.g. a
    /// reconnect succeeds) or the tab is closed while it's open.
    fn show_error_window(&mut self, ctx: &egui::Context) {
        let Some(id) = self.show_error_win else {
            return;
        };
        let Some(conn) = self.connections.iter().find(|c| c.id == id) else {
            self.show_error_win = None;
            return;
        };
        let Some(err) = conn.last_error.clone() else {
            self.show_error_win = None;
            return;
        };
        let title = match err.scope {
            ErrorScope::Connection => "Connection error",
            ErrorScope::Session => "Session error",
        };
        let mut open = true;
        let mut dismiss = false;
        egui::Window::new(title)
            // Fixed, so the window keeps its position and size when the title
            // changes with the scope of the error being shown.
            .id(egui::Id::new("error_window"))
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.label(err.msg.as_str());
                // Nothing else ever clears a session-scoped error: the
                // connection recovering doesn't fix a capture file that
                // couldn't be written, so without this it would sit in the
                // footer for the rest of the run.
                if err.scope == ErrorScope::Session {
                    ui.separator();
                    dismiss = ui.button("Dismiss").clicked();
                }
            });
        if dismiss {
            if let Some(conn) = self.connections.iter_mut().find(|c| c.id == id) {
                conn.last_error = None;
            }
        }
        if !open || dismiss {
            self.show_error_win = None;
        }
    }

    fn show_filters_window(&mut self, ctx: &egui::Context) {
        if !self.show_filters_win {
            return;
        }
        let mut open = self.show_filters_win;
        egui::Window::new("Filters")
            .open(&mut open)
            .default_width(300.0)
            .show(ctx, |ui| {
                if self.merged_selected {
                    ui.weak("Rules here apply only to the merged view.");
                    show_filter_controls(
                        ui,
                        &mut self.merged_filter_rules,
                        &mut self.merged_filter_combine,
                        &mut self.merged_filter_dirty,
                        &self.merged_filter_errors,
                    );
                    return;
                }
                let Some(active) = self.active_index() else {
                    ui.weak("Connect a port to filter.");
                    return;
                };
                let conn = &mut self.connections[active];
                show_filter_controls(
                    ui,
                    &mut conn.filter_rules,
                    &mut conn.filter_combine,
                    &mut conn.filter_dirty,
                    &conn.filter_errors,
                );
            });
        self.show_filters_win = open;
    }

    fn show_highlight_window(&mut self, ctx: &egui::Context) {
        if !self.show_highlight_win {
            return;
        }
        let mut open = self.show_highlight_win;
        let mut changed = false;
        egui::Window::new("Highlight rules")
            .open(&mut open)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.weak("First matching rule wins. Applied to every connection.");
                let mut remove: Option<usize> = None;
                for (i, rule) in self.config.highlight.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        changed |= ui.checkbox(&mut rule.enabled, "").changed();
                        changed |= ui
                            .add(
                                egui::TextEdit::singleline(&mut rule.pattern)
                                    .desired_width(120.0)
                                    .hint_text("regex"),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::TextEdit::singleline(&mut rule.color)
                                    .desired_width(72.0)
                                    .hint_text("#rrggbb"),
                            )
                            .changed();
                        changed |= ui
                            .checkbox(&mut rule.case_sensitive, "case")
                            .on_hover_text("Match uppercase and lowercase exactly")
                            .changed();
                        let color = crate::app::parse_hex_color(&rule.color)
                            .unwrap_or(egui::Color32::from_rgb(0xff, 0x55, 0x55));
                        let mut rgb = [color.r(), color.g(), color.b()];
                        if ui
                            .color_edit_button_srgb(&mut rgb)
                            .on_hover_text("Choose a highlight color")
                            .changed()
                        {
                            rule.color = format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
                            changed = true;
                        }
                        if ui.small_button("🗑").clicked() {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    self.config.highlight.remove(i);
                    changed = true;
                }
                if ui.button("+ Add highlight").clicked() {
                    self.config.highlight.push(HighlightRule {
                        pattern: "ERROR|FATAL".into(),
                        color: "#ff5555".into(),
                        case_sensitive: false,
                        // Nothing draws bold (see `HighlightRule::bold`), so a
                        // new rule no longer arrives with it set.
                        bold: false,
                        enabled: true,
                    });
                    changed = true;
                }
            });
        self.show_highlight_win = open;
        if changed {
            self.highlight_dirty = true;
            self.write_config();
        }
    }

    fn show_extract_window(&mut self, ctx: &egui::Context) {
        if !self.show_extract_win {
            return;
        }
        let mut open = self.show_extract_win;
        egui::Window::new("Plot extraction")
            .open(&mut open)
            .default_width(360.0)
            .show(ctx, |ui| {
                let Some(active) = self.active_index() else {
                    ui.weak("Connect a port to extract series.");
                    return;
                };
                let conn = &mut self.connections[active];
                ui.weak(
                    "Rules pick numbers out of the console and plot them. Editing a rule \
                     re-reads this whole session, so the plot shows every line that matches \
                     — not only the ones still to come.",
                );
                ui.add_space(4.0);

                let mut remove: Option<usize> = None;
                for (i, rule) in conn.extract_rules.iter_mut().enumerate() {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.label("Read numbers as:");
                            // Named for what each mode does to a line, not for
                            // the config keyword behind it.
                            let mut mode = rule.mode;
                            ui.selectable_value(&mut mode, ExtractMode::Kv, "name = value pairs")
                                .on_hover_text(
                                    "Split the line into pairs at every space, comma or \
                                     semicolon, then read each pair as a name and a number.\n\n\
                                     `temp:23.4 rpm:1200` plots temp and rpm.",
                                );
                            ui.selectable_value(&mut mode, ExtractMode::Regex, "a regex pattern")
                                .on_hover_text(
                                    "Match the line against a regular expression; every named \
                                     capture group becomes a series.\n\n\
                                     `rpm=(?P<rpm>\\d+)` plots rpm.",
                                );
                            if mode != rule.mode {
                                rule.mode = mode;
                                conn.extract_dirty = true;
                            }
                            if ui
                                .small_button("🗑")
                                .on_hover_text("Delete this rule")
                                .clicked()
                            {
                                remove = Some(i);
                            }
                        });

                        // Written by the mode arms below, shown under the grid.
                        let mut example = String::new();
                        egui::Grid::new(("extract-rule", i))
                            .num_columns(2)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                ui.label("Only lines starting with")
                                    .on_hover_text(
                                        "Skip every line that does not start with this text; the \
                                         text itself is not parsed. Leave empty to read every line.",
                                    );
                                let mut prefix = rule.prefix.clone().unwrap_or_default();
                                if ui
                                    .add(
                                        egui::TextEdit::singleline(&mut prefix)
                                            .hint_text("any line")
                                            .desired_width(180.0),
                                    )
                                    .changed()
                                {
                                    rule.prefix = if prefix.is_empty() {
                                        None
                                    } else {
                                        Some(prefix)
                                    };
                                    conn.extract_dirty = true;
                                }
                                ui.end_row();

                                match rule.mode {
                                    ExtractMode::Kv => {
                                        let seps = rule
                                            .kv_separators
                                            .clone()
                                            .unwrap_or_else(|| vec![':', '=']);
                                        // The space lives in the same list, but
                                        // it gets a checkbox: it is invisible in
                                        // a text box, and it means something
                                        // different from the other separators.
                                        let mut spaced = seps.iter().any(|c| c.is_whitespace());
                                        let mut typed: String =
                                            seps.iter().filter(|c| !c.is_whitespace()).collect();

                                        ui.label("Joined by").on_hover_text(
                                            "The characters that may stand between a name and its \
                                             number. A space cannot go here — it already does the \
                                             other job, ending one pair and starting the next — so \
                                             it has its own box below.",
                                        );
                                        let edited = ui.add(
                                            egui::TextEdit::singleline(&mut typed)
                                                .hint_text(":= (default)")
                                                .desired_width(180.0),
                                        );
                                        ui.end_row();

                                        ui.label("");
                                        let toggled = ui
                                            .checkbox(&mut spaced, "…or just a space: temp 23.4")
                                            .on_hover_text(
                                                "Off by default: with it on, any word followed by \
                                                 a number is plotted, so a line like \
                                                 `Booting 42 modules` becomes a series too.",
                                            );
                                        ui.end_row();

                                        if edited.changed() || toggled.changed() {
                                            let mut chars: Vec<char> = typed
                                                .chars()
                                                .filter(|c| !c.is_whitespace())
                                                .collect();
                                            if spaced {
                                                chars.push(' ');
                                            }
                                            rule.kv_separators = Some(chars);
                                            conn.extract_dirty = true;
                                        }
                                        // An empty box means the built-in
                                        // `:` and `=`, so the example says so
                                        // rather than showing the space.
                                        let joiner = typed
                                            .chars()
                                            .next()
                                            .map(String::from)
                                            .unwrap_or_else(|| {
                                                if spaced { " " } else { ":" }.to_string()
                                            });
                                        example = format!(
                                            "e.g. `temp{joiner}23.4 rpm{joiner}1200` → series temp, rpm"
                                        );
                                    }
                                    ExtractMode::Regex => {
                                        ui.label("Pattern").on_hover_text(
                                            "A regular expression. Each `(?P<name>…)` group \
                                             becomes a series called `name`, and its text must \
                                             parse as a number.",
                                        );
                                        let mut pat = rule.pattern.clone().unwrap_or_default();
                                        if ui
                                            .add(
                                                egui::TextEdit::singleline(&mut pat)
                                                    .hint_text("rpm=(?P<rpm>\\d+)")
                                                    .desired_width(180.0),
                                            )
                                            .changed()
                                        {
                                            rule.pattern =
                                                if pat.is_empty() { None } else { Some(pat) };
                                            conn.extract_dirty = true;
                                        }
                                        ui.end_row();
                                        example =
                                            "e.g. `rpm=(?P<rpm>\\d+) duty=(?P<duty>[\\d.]+)` \
                                             → series rpm, duty"
                                                .to_string();
                                    }
                                }
                            });

                        ui.weak(example);
                        if rule.mode == ExtractMode::Kv {
                            ui.weak(
                                "One pair ends and the next begins at a space, comma, \
                                 semicolon or tab.",
                            );
                        }
                    });
                }
                if let Some(i) = remove {
                    conn.extract_rules.remove(i);
                    conn.extract_dirty = true;
                }
                if ui.button("+ Add extraction rule").clicked() {
                    conn.extract_rules.push(ExtractRule {
                        mode: ExtractMode::Kv,
                        prefix: None,
                        pattern: None,
                        kv_separators: None,
                    });
                    conn.extract_dirty = true;
                    // A rule is only ever added to see what it draws.
                    conn.show_plot = true;
                }

                // What the rules are actually producing, so a rule that matches
                // nothing is visible as such without hunting for the plot.
                ui.separator();
                for err in &conn.extract_errors {
                    ui.colored_label(egui::Color32::from_rgb(0xff, 0x88, 0x55), err);
                }
                if conn.extract_rules.is_empty() {
                    ui.weak("No rules yet.");
                } else if conn.series.is_empty() {
                    ui.weak("No series — nothing in this session's output matched.");
                } else {
                    let names: Vec<&str> =
                        conn.series.iter().map(|e| e.series.name()).collect();
                    ui.weak(format!("Plotting: {}", names.join(", ")));
                }
            });
        self.show_extract_win = open;
    }
}

fn show_filter_controls(
    ui: &mut egui::Ui,
    rules: &mut Vec<FilterRule>,
    combine: &mut Combine,
    dirty: &mut bool,
    errors: &[(usize, String)],
) {
    ui.horizontal(|ui| {
        ui.label("Combine:");
        let mut changed = ui.selectable_value(combine, Combine::And, "AND").changed();
        changed |= ui.selectable_value(combine, Combine::Or, "OR").changed();
        *dirty |= changed;
    });
    ui.label("Filter reveals matching history, not just new output.");

    let mut remove: Option<usize> = None;
    for (i, rule) in rules.iter_mut().enumerate() {
        ui.group(|ui| {
            ui.horizontal(|ui| {
                *dirty |= ui.checkbox(&mut rule.enabled, "").changed();
                *dirty |= ui
                    .add(
                        egui::TextEdit::singleline(&mut rule.pattern)
                            .desired_width(160.0)
                            .hint_text("pattern"),
                    )
                    .changed();
                if ui.small_button("🗑").clicked() {
                    remove = Some(i);
                }
            });
            ui.horizontal(|ui| {
                let mut changed = ui.checkbox(&mut rule.is_regex, "regex").changed();
                changed |= ui.checkbox(&mut rule.case_sensitive, "case").changed();
                changed |= ui.checkbox(&mut rule.invert, "invert").changed();
                *dirty |= changed;
            });
        });
    }
    if let Some(i) = remove {
        rules.remove(i);
        *dirty = true;
    }
    if ui.button("+ Add filter").clicked() {
        rules.push(FilterRule::default());
        *dirty = true;
    }
    for (i, err) in errors {
        ui.colored_label(
            egui::Color32::from_rgb(0xff, 0x88, 0x55),
            format!("rule {}: {err}", i + 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::app::tests::test_app;

    #[test]
    fn escape_closes_only_frontmost_window_and_consumes_key() {
        let (mut app, _enum_tx) = test_app("escape-windows");
        let ctx = egui::Context::default();
        app.show_settings = true;
        app.show_filters_win = true;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            app.show_tool_windows(ctx);
            app.show_settings_window(ctx);
        });
        ctx.memory_mut(|m| {
            m.areas_mut().move_to_top(egui::LayerId::new(
                egui::Order::Middle,
                egui::Id::new("Settings"),
            ));
        });
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let _ = ctx.run(input.clone(), |ctx| {
            app.close_window_on_escape(ctx);
            assert!(!app.show_settings);
            assert!(app.show_filters_win);
            assert!(!ctx.input(|i| i.key_pressed(egui::Key::Escape)));
            app.show_tool_windows(ctx);
        });
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            app.show_tool_windows(ctx);
        });
        ctx.memory_mut(|m| m.open_popup(egui::Id::new("dropdown")));
        let _ = ctx.run(input.clone(), |ctx| {
            app.close_window_on_escape(ctx);
            assert!(app.show_filters_win);
            assert!(ctx.input(|i| i.key_pressed(egui::Key::Escape)));
            app.show_tool_windows(ctx);
        });
        ctx.memory_mut(|m| m.close_popup());
        let _ = ctx.run(input, |ctx| {
            app.close_window_on_escape(ctx);
            assert!(!app.show_filters_win);
            assert!(!ctx.input(|i| i.key_pressed(egui::Key::Escape)));
        });
    }
}
