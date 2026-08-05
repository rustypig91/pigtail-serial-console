//! Floating tool windows (filters, highlight, plot extraction), toggled from the
//! console right-click menu so the main window stays uncluttered.

use crate::app::App;
use serialcore::config::{ExtractMode, ExtractRule, HighlightRule};
use serialcore::filter::{Combine, FilterRule};

impl App {
    pub(crate) fn show_tool_windows(&mut self, ctx: &egui::Context) {
        self.show_filters_window(ctx);
        self.show_highlight_window(ctx);
        self.show_extract_window(ctx);
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
            .default_width(320.0)
            .show(ctx, |ui| {
                let Some(active) = self.active_index() else {
                    ui.weak("Connect a port to extract series.");
                    return;
                };
                let conn = &mut self.connections[active];
                ui.weak("Applies to lines that arrive after the rule is added.");

                let mut remove: Option<usize> = None;
                for (i, rule) in conn.extract_rules.iter_mut().enumerate() {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            let mut is_regex = rule.mode == ExtractMode::Regex;
                            if ui.selectable_label(!is_regex, "kv").clicked() {
                                is_regex = false;
                                conn.extract_dirty = true;
                            }
                            if ui.selectable_label(is_regex, "regex").clicked() {
                                is_regex = true;
                                conn.extract_dirty = true;
                            }
                            rule.mode = if is_regex {
                                ExtractMode::Regex
                            } else {
                                ExtractMode::Kv
                            };
                            if ui.small_button("🗑").clicked() {
                                remove = Some(i);
                            }
                        });
                        let mut prefix = rule.prefix.clone().unwrap_or_default();
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut prefix)
                                    .hint_text("prefix gate, e.g. PLOT:")
                                    .desired_width(200.0),
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
                        if rule.mode == ExtractMode::Regex {
                            let mut pat = rule.pattern.clone().unwrap_or_default();
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut pat)
                                        .hint_text("(?P<rpm>\\d+)")
                                        .desired_width(200.0),
                                )
                                .changed()
                            {
                                rule.pattern = if pat.is_empty() { None } else { Some(pat) };
                                conn.extract_dirty = true;
                            }
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
                }
            });
        self.show_extract_win = open;
    }
}
