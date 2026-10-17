//! Floating tool windows for highlighting, plot setup and error details.

use crate::app::{App, TabId};
use serialcore::config::HighlightRule;
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
            TabId::Plot(id) => self
                .plot_tabs
                .iter()
                .find(|t| t.saved.id == id)
                .map(|t| t.saved.name.clone()),
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
        let response = super::chrome::app_modal(ctx, "Close tab?").show(ctx, |ui| {
            ui.set_width(420.0);
            ui.spacing_mut().button_padding = egui::vec2(10.0, 7.0);
            ui.spacing_mut().interact_size.y = 32.0;
            ui.spacing_mut().item_spacing = egui::vec2(12.0, 10.0);
            super::chrome::modal_header(ui, "Close tab?", &mut open);
            ui.label(format!("Are you sure you want to close \"{}\"?", label));
            if matches!(id, TabId::Connection(_)) {
                ui.weak("Closing the tab disconnects this connection.");
            } else {
                ui.weak("The connections included in this view will stay open.");
            }
            ui.add_space(8.0);
            ui.checkbox(&mut do_not_ask, "Do not ask me again");
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = ui
                    .add(egui::Button::new("Close tab").fill(ui.visuals().selection.bg_fill))
                    .clicked();
                cancel = ui.button("Cancel").clicked();
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
                TabId::Plot(id) => self.close_plot(id),
                TabId::Merged(tab_id) => {
                    if let Some(index) = self.merged_tabs.iter().position(|tab| tab.id == tab_id) {
                        self.close_merged_tab(index);
                    }
                }
            }
        } else if cancel || !open || response.should_close() {
            self.tab_close_confirmation = None;
        } else {
            self.tab_close_confirmation = Some((id, do_not_ask));
        }
    }

    /// Handle Escape before console input or text fields can consume it.
    pub(crate) fn close_window_on_escape(&mut self, ctx: &egui::Context) {
        if self.show_about
            || self.show_settings
            || self.show_keyboard_shortcuts
            || self.update_dialog.is_some()
            || !self.connect_errors.is_empty()
            || self.tab_close_confirmation.is_some()
        {
            return; // Modal dialogs handle Escape themselves.
        }
        if egui::Popup::is_any_open(ctx) || !ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            return;
        }
        let open_windows = [
            ("Settings", self.show_settings),
            ("About", self.show_about),
            ("Keyboard shortcuts", self.show_keyboard_shortcuts),
            ("Highlight rules", self.show_highlight_win),
            ("plot_setup", self.plot_draft.is_some()),
            ("Transmit macros", self.show_macros_win),
            ("Close tab?", self.tab_close_confirmation.is_some()),
            ("error_window", self.show_error_win.is_some()),
            (
                "Macro is running",
                self.macro_running_edit_confirmation.is_some(),
            ),
            ("Edit macro", self.macro_editor.is_some()),
            ("Add macro", self.macro_editor.is_some()),
            (
                "Shortcut already used",
                self.macro_shortcut_conflict.is_some(),
            ),
            ("Port options", self.config_dialog.is_some()),
            ("New connection", self.config_dialog.is_some()),
            ("Rename tab", self.rename_dialog.is_some()),
            ("new_merged_view", self.merged_dialog.is_some()),
            ("Send file", self.file_transfer_dialog.is_some()),
            (
                "file_transfer_progress",
                self.connections
                    .iter()
                    .any(|conn| conn.transfer_progress.is_some()),
            ),
        ];
        let matches_window =
            |id: egui::Id, name: &str| id == egui::Id::new(name) || id == egui::Id::new(Some(name));
        let top = ctx.memory(|m| {
            // Let dropdowns and context menus handle Escape first.

            // The layer order retains closed windows; only consider visible
            // top-level windows when selecting the next Escape target.
            m.layer_ids()
                .filter(|layer| {
                    layer.order == egui::Order::Middle
                        && m.areas().is_visible(layer)
                        && m.areas().parent_layer(*layer).is_none()
                        && (open_windows
                            .iter()
                            .any(|(name, open)| *open && matches_window(layer.id, name))
                            || self
                                .connect_errors
                                .front()
                                .is_some_and(|error| matches_window(layer.id, error.title))
                            || self
                                .update_dialog
                                .as_ref()
                                .is_some_and(|dialog| matches_window(layer.id, &dialog.title)))
                })
                .last()
        });
        let Some(top) = top else { return };
        let is_window = |name: &str| matches_window(top.id, name);
        let mut closed = false;
        for (name, open) in [
            ("Settings", &mut self.show_settings),
            ("About", &mut self.show_about),
            ("Keyboard shortcuts", &mut self.show_keyboard_shortcuts),
            ("Highlight rules", &mut self.show_highlight_win),
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
        } else if self.merged_dialog.is_some() && is_window("new_merged_view") {
            self.merged_dialog = None;
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
        if self.plot_draft.is_some() && is_window("plot_setup") {
            self.plot_draft = None;
            closed = true;
        }
        if closed {
            ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
            });
        }
    }

    pub(crate) fn show_tool_windows(&mut self, ctx: &egui::Context) {
        self.show_highlight_window(ctx);
        self.show_plot_setup(ctx);
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
            .frame(super::chrome::dialog_frame(ctx))
            .show(ctx, |ui| {
                super::chrome::dialog_style(ui);
                ui.label(err.msg.as_str());
                // Session failures need explicit dismissal: reconnecting
                // doesn't fix a capture file that couldn't be written.
                // Dropped-command warnings can also be dismissed early.
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

    fn show_highlight_window(&mut self, ctx: &egui::Context) {
        if !self.show_highlight_win {
            return;
        }
        let Some(index) = self
            .highlight_port
            .and_then(|port| self.connections.iter().position(|conn| conn.id == port))
        else {
            self.show_highlight_win = false;
            return;
        };
        let conn = &mut self.connections[index];
        let title = format!("Highlight rules — {}", conn.display_label());
        let mut open = self.show_highlight_win;
        let mut changed = false;
        egui::Window::new(title)
            .id(egui::Id::new("Highlight rules"))
            .open(&mut open)
            .default_width(320.0)
            .frame(super::chrome::dialog_frame(ctx))
            .show(ctx, |ui| {
                super::chrome::dialog_style(ui);
                ui.weak("First matching rule wins. Applies only to this connection.");
                let mut remove: Option<usize> = None;
                for (i, rule) in conn.highlight.iter_mut().enumerate() {
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
                    conn.highlight.remove(i);
                    changed = true;
                }
                if ui.button("+ Add highlight").clicked() {
                    conn.highlight.push(HighlightRule {
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
            conn.highlight_dirty = true;
            self.save_session();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::tests::test_app;
    use crate::test_ui::ContextTestExt as _;

    fn add_highlight_connection(app: &mut crate::app::App) {
        let id = serialcore::store::PortId(7);
        app.connections.push(app.make_connection(
            id,
            "Device 7".into(),
            Default::default(),
            Default::default(),
            crate::app::tests::inert_handle(id),
        ));
        app.highlight_port = Some(id);
        app.show_highlight_win = true;
    }

    #[test]
    fn highlight_editor_keeps_its_connection_when_switching_tabs_and_closes_with_it() {
        let (mut app, _tx) = test_app("highlight-editor-target");
        add_highlight_connection(&mut app);
        let other = serialcore::store::PortId(8);
        app.connections.push(app.make_connection(
            other,
            "Other port".into(),
            Default::default(),
            Default::default(),
            crate::app::tests::inert_handle(other),
        ));
        app.active = 1;
        app.connections[0].name = Some("Sensor".into());
        let ctx = egui::Context::default();
        let mut output =
            ctx.run_test_ui(Default::default(), |ui| app.show_highlight_window(ui.ctx()));
        for _ in 0..2 {
            output = ctx.run_test_ui(Default::default(), |ui| app.show_highlight_window(ui.ctx()));
        }
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == "Highlight rules — Sensor")));
        let add = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "+ Add highlight" => {
                    Some(text.pos + text.galley.size() / 2.0)
                }
                _ => None,
            })
            .unwrap();
        let _ = ctx.run_test_ui(
            egui::RawInput {
                events: vec![
                    egui::Event::PointerMoved(add),
                    egui::Event::PointerButton {
                        pos: add,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::PointerButton {
                        pos: add,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ui| app.show_highlight_window(ui.ctx()),
        );
        assert_eq!(app.connections[0].highlight.len(), 4);
        assert_eq!(app.connections[1].highlight.len(), 3);
        assert_eq!(app.config.last_open[0].highlight.len(), 4);
        app.connections.remove(0);
        let _ = ctx.run_test_ui(Default::default(), |ui| app.show_highlight_window(ui.ctx()));
        assert!(!app.show_highlight_win);
    }

    #[test]
    fn escape_closes_merged_options_above_a_tool_window() {
        let (mut app, _tx) = test_app("escape-merged-options");
        add_highlight_connection(&mut app);
        app.merged_dialog = Some(crate::app::MergedDialog {
            editing: None,
            name: "Merged".into(),
            ports: Vec::new(),
        });
        let ctx = egui::Context::default();
        let _ = ctx.run_test_ui(Default::default(), |ui| {
            app.show_tool_windows(ui.ctx());
            app.show_merged_dialog(ui.ctx());
        });
        let _ = ctx.run_test_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| {
                app.close_window_on_escape(ui.ctx());
                assert!(app.merged_dialog.is_none());
                assert!(app.show_highlight_win);
                assert!(!ui.input(|input| input.key_pressed(egui::Key::Escape)));
            },
        );
    }

    #[test]
    fn escape_closes_modal_over_focused_search_without_closing_search() {
        for merged in [false, true] {
            for modal_key in [egui::Key::F1, egui::Key::F2] {
                let (mut app, _enum_tx) = test_app("search-modal-escape");
                let id = serialcore::store::PortId(1);
                app.connections.push(app.make_connection(
                    id,
                    "Device 1".into(),
                    Default::default(),
                    Default::default(),
                    crate::app::tests::inert_handle(id),
                ));
                if merged {
                    app.create_merged_tab(vec![id]);
                    app.merged_search_query = "needle".into();
                } else {
                    app.connections[0].search_query = "needle".into();
                }
                app.show_search = true;
                app.search_focus_request = true;
                let ctx = egui::Context::default();
                let frame = |app: &mut crate::app::App, events| {
                    ctx.run_test_ui(
                        egui::RawInput {
                            events,
                            ..Default::default()
                        },
                        |ui| {
                            let ctx = &ui.ctx().clone();
                            app.consume_app_shortcuts(ctx);
                            app.close_window_on_escape(ctx);
                            app.show_workspace(ui, false);
                            app.show_settings_window(ctx);
                            app.show_about_window(ctx);
                        },
                    )
                };
                frame(&mut app, vec![]);
                frame(&mut app, vec![]);
                assert_eq!(
                    ctx.memory(|memory| memory.focused()),
                    Some(app.pane_widget_id("search_query"))
                );
                let key = |key| egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                };
                frame(&mut app, vec![key(modal_key)]);
                assert!(app.show_about || app.show_settings);
                frame(&mut app, vec![key(egui::Key::Escape)]);
                assert!(!app.show_settings && !app.show_about);
                assert!(app.show_search);
                let query = if merged {
                    &app.merged_search_query
                } else {
                    &app.connections[0].search_query
                };
                assert_eq!(query, "needle");
                assert!(!ctx.input(|input| input.key_pressed(egui::Key::Escape)));
            }
        }
    }

    #[test]
    fn escape_cancels_close_modal_without_changing_connection_or_preferences() {
        let (mut app, _enum_tx) = test_app("close-modal-escape");
        let id = serialcore::store::PortId(1);
        app.connections.push(app.make_connection(
            id,
            "Device 1".into(),
            Default::default(),
            Default::default(),
            crate::app::tests::inert_handle(id),
        ));
        app.tab_close_confirmation = Some((crate::app::TabId::Connection(id), true));
        let ctx = egui::Context::default();
        let _ = ctx.run_test_ui(egui::RawInput::default(), |ui| {
            let ctx = &ui.ctx().clone();
            app.show_tab_close_confirmation(ctx);
        });
        let _ = ctx.run_test_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| {
                let ctx = &ui.ctx().clone();
                app.close_window_on_escape(ctx);
                app.show_tab_close_confirmation(ctx);
                assert!(!ctx.input(|input| input.key_pressed(egui::Key::Escape)));
            },
        );
        assert!(app.tab_close_confirmation.is_none());
        assert_eq!(app.connections.len(), 1);
        assert!(app.config.settings.confirm_tab_close);
    }

    #[test]
    fn escape_closes_only_frontmost_window_and_consumes_key() {
        let (mut app, _enum_tx) = test_app("escape-windows");
        let ctx = egui::Context::default();
        app.show_settings = true;
        add_highlight_connection(&mut app);
        let _ = ctx.run_test_ui(egui::RawInput::default(), |ui| {
            let ctx = &ui.ctx().clone();
            app.show_tool_windows(ctx);
            app.show_settings_window(ctx);
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
        let _ = ctx.run_test_ui(input.clone(), |ui| {
            let ctx = &ui.ctx().clone();
            app.close_window_on_escape(ctx);
            app.show_settings_window(ctx);
            assert!(!app.show_settings);
            assert!(app.show_highlight_win);
            assert!(!ctx.input(|i| i.key_pressed(egui::Key::Escape)));
            app.show_tool_windows(ctx);
        });
        let _ = ctx.run_test_ui(egui::RawInput::default(), |ui| {
            let ctx = &ui.ctx().clone();
            app.show_tool_windows(ctx);
        });
        egui::Popup::open_id(&ctx, egui::Id::new("dropdown"));
        let _ = ctx.run_test_ui(input.clone(), |ui| {
            let ctx = &ui.ctx().clone();
            app.close_window_on_escape(ctx);
            assert!(app.show_highlight_win);
            assert!(ctx.input(|i| i.key_pressed(egui::Key::Escape)));
            app.show_tool_windows(ctx);
        });
        egui::Popup::close_all(&ctx);
        let _ = ctx.run_test_ui(input, |ui| {
            let ctx = &ui.ctx().clone();
            app.close_window_on_escape(ctx);
            assert!(!app.show_highlight_win);
            assert!(!ctx.input(|i| i.key_pressed(egui::Key::Escape)));
        });
    }
}
