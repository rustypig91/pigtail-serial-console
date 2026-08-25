//! Floating tool windows (filters, highlight, plot extraction), toggled from the
//! console right-click menu so the main window stays uncluttered.

use crate::app::App;
use serialcore::config::{ExtractMode, ExtractRule, HighlightRule};
use serialcore::filter::{Combine, FilterRule};
use serialcore::reader::ErrorScope;

impl App {
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
                let Some(active) = self.active_index() else {
                    ui.weak("Connect a port to filter.");
                    return;
                };
                let conn = &mut self.connections[active];
                ui.horizontal(|ui| {
                    ui.label("Combine:");
                    let mut changed = ui
                        .selectable_value(&mut conn.filter_combine, Combine::And, "AND")
                        .changed();
                    changed |= ui
                        .selectable_value(&mut conn.filter_combine, Combine::Or, "OR")
                        .changed();
                    if changed {
                        conn.filter_dirty = true;
                    }
                });
                ui.label("Filter reveals matching history, not just new output.");

                let mut remove: Option<usize> = None;
                for (i, rule) in conn.filter_rules.iter_mut().enumerate() {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut rule.enabled, "").changed() {
                                conn.filter_dirty = true;
                            }
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut rule.pattern)
                                        .desired_width(160.0)
                                        .hint_text("pattern"),
                                )
                                .changed()
                            {
                                conn.filter_dirty = true;
                            }
                            if ui.small_button("🗑").clicked() {
                                remove = Some(i);
                            }
                        });
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut rule.is_regex, "regex").changed()
                                || ui.checkbox(&mut rule.case_sensitive, "case").changed()
                                || ui.checkbox(&mut rule.invert, "invert").changed()
                            {
                                conn.filter_dirty = true;
                            }
                        });
                    });
                }
                if let Some(i) = remove {
                    conn.filter_rules.remove(i);
                    conn.filter_dirty = true;
                }
                if ui.button("+ Add filter").clicked() {
                    conn.filter_rules.push(FilterRule::default());
                    conn.filter_dirty = true;
                }
                for (i, err) in &conn.filter_errors {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xff, 0x88, 0x55),
                        format!("rule {}: {err}", i + 1),
                    );
                }
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
                        changed |= ui.checkbox(&mut rule.bold, "b").changed();
                        if let Some(c) = crate::app::parse_hex_color(&rule.color) {
                            ui.colored_label(c, "■");
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
                        bold: true,
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
