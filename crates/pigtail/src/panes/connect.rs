//! Minimal chrome: the header (tabs + new/save/settings), the status footer,
//! and the modal new-connection dialog with preset management.

use crate::app::{available_port_is_added, App, ConfigDialog};
use serialcore::config::{
    DataBits, FlowControl, LineEnding, NamedConfig, Parity, PortConfig, StopBits, TerminalMode,
    TransmitMacro,
};
use serialcore::reader::ConnState;
use std::time::Instant;

const COMMON_BAUDS: &[u32] = &[
    9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600, 1_000_000, 2_000_000, 3_000_000,
];

#[derive(Clone, Copy)]
struct DraggedTab(serialcore::store::PortId);

impl App {
    /// Reserve Ctrl+Shift+Left/Right for cycling the visible connection tabs.
    /// This is deliberately handled before the console sees raw input, so the
    /// terminal never receives the corresponding escape sequence.
    pub(crate) fn consume_tab_switch_shortcut(&mut self, ctx: &egui::Context) {
        // Keep keyboard input with whichever control or overlay currently owns
        // it. This matches the console-only scope of the macro shortcuts.
        if self.floating_window_open()
            || self.config_dialog.is_some()
            || self.rename_dialog.is_some()
            || self.file_transfer_dialog.is_some()
            || ctx.is_context_menu_open()
            || ctx.memory(|memory| memory.any_popup_open() || memory.focused().is_some())
        {
            return;
        }

        let tab_count = self.connections.len() + usize::from(self.connections.len() >= 2);
        if tab_count == 0 {
            return;
        }
        let modifiers = egui::Modifiers::CTRL | egui::Modifiers::SHIFT;
        let previous = ctx.input_mut(|input| input.consume_key(modifiers, egui::Key::ArrowLeft));
        let next = ctx.input_mut(|input| input.consume_key(modifiers, egui::Key::ArrowRight));
        if !previous && !next {
            return;
        }

        let current = if self.merged_selected {
            self.connections.len()
        } else {
            self.active.min(self.connections.len() - 1)
        };
        let selected = if previous {
            (current + tab_count - 1) % tab_count
        } else {
            (current + 1) % tab_count
        };

        if selected == self.connections.len() {
            self.merged_selected = true;
            let selected_is_live = self.merged_tx_port.is_some_and(|id| {
                self.connections
                    .iter()
                    .any(|conn| conn.id == id && conn.state != ConnState::Closed)
            });
            if !selected_is_live {
                self.merged_tx_port = self
                    .connections
                    .get(self.active)
                    .filter(|conn| conn.state != ConnState::Closed)
                    .or_else(|| {
                        self.connections
                            .iter()
                            .find(|conn| conn.state != ConnState::Closed)
                    })
                    .map(|conn| conn.id);
            }
        } else {
            self.active = selected;
            self.merged_selected = false;
        }
    }

    /// A plain acknowledgement dialog for `connect_errors`: a one-off
    /// background operation — connect, reconnect, port-detection start, an
    /// export write — that failed outright rather than through the normal
    /// per-connection error path, either because there is no live connection
    /// to show it on or because the failure has nothing to do with a
    /// connection's health. Shows one message at a time, oldest first, so
    /// simultaneous failures all get seen rather than the latest silently
    /// replacing the others.
    pub(crate) fn show_connect_error(&mut self, ctx: &egui::Context) {
        // A drop confirmation is modal and already anchored at the centre;
        // keep an unrelated background error queued until it closes.
        if self.file_transfer_dialog.is_some() {
            return;
        }
        let Some(err) = self.connect_errors.front() else {
            return;
        };
        if super::update::show_ack_window(ctx, err.title, &err.message) {
            self.connect_errors.pop_front();
        }
    }

    /// The top header: one tab per connection, a merged tab, `+`, and global
    /// actions collected under an ellipsis menu.
    pub(crate) fn show_header(&mut self, ctx: &egui::Context) {
        let mut to_close: Option<usize> = None;
        let mut set_active: Option<usize> = None;
        let mut select_merged = false;
        let mut new_tab = false;
        let mut choose_file = false;
        let mut port_options: Option<usize> = None;
        let mut rename_tab: Option<usize> = None;
        let mut reorder = None;
        let macros_tooltip = macro_tooltip(&self.config.macros);

        // The config dialog is meant to be modal, but an `egui::Window` does
        // not block input to what it covers, so the header would keep acting
        // on clicks behind it: "+" or a tab's "Port options…" would replace
        // the in-progress dialog with a fresh one, silently discarding a
        // half-filled form, and "Close tab" would leave the dialog's
        // `editing` pointing at a tab that no longer exists (issue #16).
        // Disabled rather than merely ignored so the greying-out shows *why*
        // the clicks do nothing.
        let modal_open = self.config_dialog.is_some()
            || self.rename_dialog.is_some()
            || self.file_transfer_dialog.is_some();

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // Only the tab strip and "+" are disabled: global actions
                // below act on neither the dialog nor the set of tabs, so
                // there is nothing for them to corrupt.
                ui.add_enabled_ui(!modal_open, |ui| {
                    for (i, conn) in self.connections.iter().enumerate() {
                        let selected = !self.merged_selected && self.active == i;
                        let display_label = conn.display_label();
                        let label = egui::RichText::new(short_label(display_label))
                            .color(state_color(conn.state))
                            .strong();
                        let device_details = if conn.name.is_some() {
                            format!("{}\n{}", display_label, conn.label)
                        } else {
                            conn.label.clone()
                        };
                        let tooltip = format!("Status: {}\n{}", conn.state, device_details);
                        // `on_hover_text` only fires on an *enabled* widget,
                        // so a disabled tab needs its own tooltip to keep the
                        // detected device name and port available.
                        let resp = ui
                            .selectable_label(selected, label)
                            .interact(egui::Sense::click_and_drag())
                            .on_hover_text(&tooltip)
                            .on_disabled_hover_text(format!(
                                "{}\n(finish or cancel the open dialog first)",
                                tooltip
                            ));
                        if !modal_open {
                            // Other buttons belong to the context menu and
                            // close-tab action, not tab reordering.
                            if resp.drag_started_by(egui::PointerButton::Primary) {
                                resp.dnd_set_drag_payload(DraggedTab(conn.id));
                            }
                            if resp.dnd_hover_payload::<DraggedTab>().is_some() {
                                if let Some(pointer) = ctx.pointer_interact_pos() {
                                    let after = pointer.x >= resp.rect.center().x;
                                    let boundary = i + usize::from(after);
                                    let x = if after {
                                        resp.rect.right()
                                    } else {
                                        resp.rect.left()
                                    };
                                    ui.painter().line_segment(
                                        [
                                            egui::pos2(x, resp.rect.top()),
                                            egui::pos2(x, resp.rect.bottom()),
                                        ],
                                        egui::Stroke::new(2.0_f32, ui.visuals().selection.bg_fill),
                                    );
                                    if let Some(tab) = resp.dnd_release_payload::<DraggedTab>() {
                                        reorder = Some((tab.0, boundary));
                                    }
                                }
                            }
                        }
                        if resp.clicked() {
                            set_active = Some(i);
                        }
                        // Middle-click closes the tab.
                        if resp.middle_clicked() {
                            to_close = Some(i);
                        }
                        // Right-click menu on the tab.
                        resp.context_menu(|ui| {
                            if ui.button("Rename…").clicked() {
                                rename_tab = Some(i);
                                ui.close_menu();
                            }
                            if ui.button("Port options…").clicked() {
                                port_options = Some(i);
                                ui.close_menu();
                            }
                            if ui.button("Close tab").clicked() {
                                to_close = Some(i);
                                ui.close_menu();
                            }
                        });
                    }

                    if self.connections.len() >= 2 {
                        let resp = ui.selectable_label(self.merged_selected, "Merged");
                        if resp.clicked() {
                            select_merged = true;
                        }
                    }

                    if ui
                        .button("+")
                        .on_hover_text("New connection")
                        .on_disabled_hover_text("Finish or cancel the open dialog first")
                        .clicked()
                    {
                        new_tab = true;
                    }
                });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.menu_button("…", |ui| {
                        if ui
                            .button("Send file…")
                            .on_hover_text("Choose a file to send to the active console")
                            .clicked()
                        {
                            choose_file = true;
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button("Macros").on_hover_text(&macros_tooltip).clicked() {
                            self.show_macros_win = true;
                            ui.close_menu();
                        }
                        if ui.button("Show keyboard shortcuts").clicked() {
                            self.show_keyboard_shortcuts = true;
                            ui.close_menu();
                        }
                        if ui.button("Settings").clicked() {
                            self.show_settings = true;
                            ui.close_menu();
                        }
                        let updating = self.update_rx.is_some() || self.install_rx.is_some();
                        if ui
                            .add_enabled(!updating, egui::Button::new("Check for updates"))
                            .on_disabled_hover_text(
                                "An update check or installation is in progress",
                            )
                            .clicked()
                        {
                            self.start_update_check(true);
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui
                            .button("Support developer")
                            .on_hover_text("Opens Buy Me a Coffee in your browser")
                            .clicked()
                        {
                            ctx.open_url(egui::OpenUrl::new_tab(
                                "https://buymeacoffee.com/rustypig91g",
                            ));
                            ui.close_menu();
                        }
                    });
                });
            });
        });

        if let Some(i) = set_active {
            self.active = i;
            self.merged_selected = false;
        }
        if select_merged {
            self.merged_selected = true;
            let selected_is_live = self.merged_tx_port.is_some_and(|id| {
                self.connections
                    .iter()
                    .any(|conn| conn.id == id && conn.state != ConnState::Closed)
            });
            if !selected_is_live {
                self.merged_tx_port = self
                    .connections
                    .get(self.active)
                    .filter(|conn| conn.state != ConnState::Closed)
                    .or_else(|| {
                        self.connections
                            .iter()
                            .find(|conn| conn.state != ConnState::Closed)
                    })
                    .map(|conn| conn.id);
            }
        }
        if to_close.is_none() {
            if let Some((id, boundary)) = reorder {
                self.reorder_connection(id, boundary);
            }
        }
        if let Some(i) = to_close {
            self.request_close_connection(i);
        }
        if let Some(i) = port_options {
            self.open_port_options(i);
        }
        if let Some(i) = rename_tab {
            self.open_rename_dialog(i);
        }
        if new_tab {
            self.open_config_dialog();
        }
        if choose_file {
            self.choose_file_transfer();
        }
    }

    /// A compact reference for the application-wide keyboard commands. Device
    /// input remains intentionally separate: unlisted keystrokes go to the
    /// active serial console.
    pub(crate) fn show_keyboard_shortcuts_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_keyboard_shortcuts;
        egui::Window::new("Keyboard shortcuts")
            .open(&mut open)
            .resizable(false)
            .show(ctx, |ui| {
                egui::Grid::new("keyboard_shortcuts_grid")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.strong("Ctrl+Shift+Left / Right");
                        ui.label("Previous / next tab");
                        ui.end_row();
                        ui.strong("Ctrl+Shift+F");
                        ui.label("Show or hide search");
                        ui.end_row();
                        ui.strong("Ctrl+Shift+C / V");
                        ui.label("Copy selected text / paste to console");
                        ui.end_row();
                        ui.strong("Ctrl+mouse wheel");
                        ui.label("Change console text size");
                        ui.end_row();
                        ui.strong("Ctrl+Shift+0–9");
                        ui.label("Run the assigned macro");
                        ui.end_row();
                    });
                ui.separator();
                ui.weak("All other keystrokes are sent to the active serial console.");
            });
        self.show_keyboard_shortcuts = open;
    }

    /// The bottom status footer: connection state, view details, and the view
    /// toggles — hex, plot and Pin (autoscroll).
    pub(crate) fn show_footer(&mut self, ctx: &egui::Context) {
        let mut toggle_pin = false;
        let mut toggle_plot = false;
        let mut toggle_hex = false;
        let mut toggle_highlights = false;
        let has_highlights = self.config.highlight.iter().any(|rule| rule.enabled);
        let mut merged_tx_port = self.merged_tx_port;
        let mut open_error_win: Option<serialcore::store::PortId> = None;
        let long_running_macros =
            self.long_running_macro_indicators(Instant::now(), self.macro_target_port());
        let mut stop_macro_run = None;
        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if self.merged_selected {
                    let shown = self.merged_view().len();
                    ui.label(format!("merged · {} lines", self.merged.len()));
                    show_macro_run_indicators(ui, &long_running_macros, &mut stop_macro_run);
                    if self.merged_filter_active() {
                        ui.separator();
                        ui.label(format!("{shown} shown"));
                    }
                    if !self.merged_search_matches.is_empty() {
                        ui.separator();
                        let n = self.merged_search_pos.map(|p| p + 1).unwrap_or(0);
                        ui.label(format!("match {n}/{}", self.merged_search_matches.len()));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if self.merged_follow { "Pinned" } else { "Pin" };
                        if ui
                            .selectable_label(self.merged_follow, label)
                            .on_hover_text("Pin the merged view to the bottom and autoscroll")
                            .clicked()
                        {
                            toggle_pin = true;
                        }
                        let selected_label = merged_tx_port
                            .and_then(|id| self.connections.iter().find(|conn| conn.id == id))
                            .map(|conn| short_label(conn.display_label()))
                            .unwrap_or_else(|| "Select device".to_string());
                        egui::ComboBox::from_id_salt("merged_tx_device")
                            .selected_text(format!("Send to: {selected_label}"))
                            .show_ui(ui, |ui| {
                                for conn in &self.connections {
                                    let text = short_label(conn.display_label());
                                    ui.add_enabled_ui(conn.state != ConnState::Closed, |ui| {
                                        ui.selectable_value(
                                            &mut merged_tx_port,
                                            Some(conn.id),
                                            text,
                                        );
                                    });
                                }
                            });
                        if !self.merged_follow && self.merged_new_since_scroll > 0 {
                            ui.label(format!("{} new", self.merged_new_since_scroll));
                            ui.separator();
                        }
                        if has_highlights
                            && ui
                                .selectable_label(self.highlights_visible, "Highlights")
                                .on_hover_text("Enable or disable all highlights")
                                .clicked()
                        {
                            toggle_highlights = true;
                        }
                    });
                    return;
                }
                let Some(active) = self.active_index() else {
                    ui.weak("no connection — press + to add one");
                    return;
                };
                let conn = &self.connections[active];
                // A live error takes priority over the raw state: the reader
                // keeps retrying in the background (state stays Connecting /
                // Reconnecting so it can recover on its own), but the status
                // bar should say what's actually wrong rather than keep
                // claiming to be "connecting" while it fails over and over.
                //
                // While the link *is* up, though, the state is still worth
                // showing, so an error raised alongside a working connection
                // (a capture-file write that failed, say) sits next to it
                // rather than replacing it — this footer is the only place
                // such an error ever surfaces.
                if conn.state == ConnState::Connected || conn.last_error.is_none() {
                    ui.colored_label(
                        state_color(conn.state),
                        format!("{} {}", state_dot(conn.state), conn.state),
                    );
                }
                if let Some(err) = &conn.last_error {
                    let resp = ui.add(
                        egui::Label::new(
                            egui::RichText::new("⚠ error")
                                .color(egui::Color32::from_rgb(0xff, 0x55, 0x55)),
                        )
                        .sense(egui::Sense::click()),
                    );
                    let resp = resp
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(err.msg.as_str());
                    if resp.clicked() {
                        open_error_win = Some(conn.id);
                    }
                }
                ui.separator();
                ui.monospace(conn.port_config.summary());
                ui.separator();
                ui.label(format!("{} lines", conn.store.next_abs_index()));
                show_macro_run_indicators(ui, &long_running_macros, &mut stop_macro_run);
                if conn.filter_index_active() {
                    ui.separator();
                    ui.label(format!("{} shown", conn.filter_index.len()));
                }
                if !conn.search_matches.is_empty() {
                    ui.separator();
                    let n = conn.search_pos.map(|p| p + 1).unwrap_or(0);
                    ui.label(format!("match {n}/{}", conn.search_matches.len()));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Pin toggle: pinned = follow tail / autoscroll.
                    let label = if conn.follow { "Pinned" } else { "Pin" };
                    if ui
                        .selectable_label(conn.follow, label)
                        .on_hover_text("Pin to bottom and autoscroll")
                        .clicked()
                    {
                        toggle_pin = true;
                    }
                    // Added after the pin in a right-to-left layout, so they
                    // land to its left: Hex | Plot | Pinned.
                    if ui
                        .selectable_label(conn.show_plot, "Plot")
                        .on_hover_text("Show the plot pane below the console")
                        .clicked()
                    {
                        toggle_plot = true;
                    }
                    if ui
                        .selectable_label(conn.hex_view, "Hex")
                        .on_hover_text("Show raw bytes instead of decoded lines")
                        .clicked()
                    {
                        toggle_hex = true;
                    }
                    if has_highlights
                        && ui
                            .selectable_label(self.highlights_visible, "Highlights")
                            .on_hover_text("Enable or disable all highlights")
                            .clicked()
                    {
                        toggle_highlights = true;
                    }
                    ui.separator();
                    if !conn.follow && conn.new_since_scroll > 0 {
                        ui.label(format!("{} new", conn.new_since_scroll));
                    }
                    let mut evicted = Vec::new();
                    if conn.store.evicted_any() {
                        evicted.push("console");
                    }
                    if conn.raw_evicted_any {
                        evicted.push("hex");
                    }
                    if conn.series_evicted_any {
                        evicted.push("plot");
                    }
                    if !evicted.is_empty() {
                        ui.separator();
                        ui.colored_label(
                            egui::Color32::from_rgb(0xe5, 0xc0, 0x40),
                            format!(
                                "{} history evicted (full capture on disk)",
                                evicted.join("/")
                            ),
                        );
                    }
                });
            });
        });

        self.merged_tx_port = merged_tx_port;
        if let Some(run_index) = stop_macro_run {
            self.stop_macro_run(run_index);
        }
        if toggle_highlights {
            self.highlights_visible = !self.highlights_visible;
        }
        if toggle_pin && self.merged_selected {
            self.merged_follow = !self.merged_follow;
            if self.merged_follow {
                self.merged_new_since_scroll = 0;
            }
        }
        if (!self.merged_selected && toggle_pin) || toggle_plot || toggle_hex {
            if let Some(active) = self.active_index() {
                let conn = &mut self.connections[active];
                if toggle_pin {
                    conn.follow = !conn.follow;
                    if conn.follow {
                        conn.new_since_scroll = 0;
                    }
                }
                if toggle_plot {
                    conn.show_plot = !conn.show_plot;
                }
                if toggle_hex {
                    conn.hex_view = !conn.hex_view;
                }
            }
        }
        if let Some(id) = open_error_win {
            self.show_error_win = Some(id);
        }
    }

    /// The modal new-connection dialog (opening a tab first configures the port).
    pub(crate) fn show_config_dialog(&mut self, ctx: &egui::Context) {
        if self.config_dialog.is_none() {
            return;
        }
        // Both this and `show_connect_error` anchor at CENTER_CENTER (see the
        // note in `show_update_dialog`). A connect error can land while this
        // dialog is already open (e.g. a background export failing), so defer
        // to it the same way the update notice does rather than stacking the
        // two windows.
        if self.defer_to_connect_error() {
            return;
        }
        let mut do_connect = false;
        let mut do_cancel = false;
        let mut persist = false;

        {
            let App {
                config_dialog,
                available,
                connections,
                config,
                ..
            } = self;
            let dialog: &mut ConfigDialog = config_dialog.as_mut().unwrap();
            let mut load_preset: Option<usize> = None;
            let editing_port = dialog.editing;
            let editing = editing_port.is_some();
            let title = if editing {
                "Port options"
            } else {
                "New connection"
            };

            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label("Port");
                    egui::ComboBox::from_id_salt("dlg_port")
                        .width(260.0)
                        .selected_text(
                            dialog
                                .selected_path
                                .clone()
                                .unwrap_or_else(|| "select a port…".into()),
                        )
                        .show_ui(ui, |ui| {
                            for (index, p) in available.iter().enumerate() {
                                let added = available_port_is_added(
                                    index,
                                    available,
                                    connections,
                                    editing_port,
                                );
                                let text = port_choice_text(&p.path, &p.identity.label(), added);
                                ui.add_enabled_ui(!added, |ui| {
                                    ui.selectable_value(
                                        &mut dialog.selected_path,
                                        Some(p.path.clone()),
                                        text,
                                    );
                                });
                            }
                        });
                    if available.is_empty() {
                        ui.weak("No serial ports detected.");
                    } else if available.iter().enumerate().all(|(index, _)| {
                        available_port_is_added(index, available, connections, editing_port)
                    }) {
                        ui.weak("All detected ports have already been added.");
                    }

                    ui.separator();
                    connect_controls(ui, &mut dialog.config);

                    ui.separator();
                    ui.label("Presets");
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("dlg_preset")
                            .selected_text("Load…")
                            .show_ui(ui, |ui| {
                                for (i, preset) in config.presets.iter().enumerate() {
                                    if ui.selectable_label(false, &preset.name).clicked() {
                                        load_preset = Some(i);
                                    }
                                }
                            });
                        ui.add(
                            egui::TextEdit::singleline(&mut dialog.preset_name)
                                .hint_text("preset name")
                                .desired_width(120.0),
                        );
                        if ui.button("Save preset").clicked() {
                            persist = true;
                        }
                    });

                    ui.separator();
                    ui.horizontal(|ui| {
                        // When editing an existing tab we can reconnect by
                        // identity even if the device isn't currently listed, so
                        // the apply button need not require a selected path.
                        let can = editing || dialog.selected_path.is_some();
                        let apply_label = if editing {
                            "Apply & reconnect"
                        } else {
                            "Connect"
                        };
                        if ui
                            .add_enabled(can, egui::Button::new(apply_label))
                            .clicked()
                        {
                            do_connect = true;
                        }
                        if ui.button("Cancel").clicked() {
                            do_cancel = true;
                        }
                    });
                });

            if let Some(i) = load_preset {
                if let Some(p) = config.presets.get(i) {
                    dialog.config = p.config.clone();
                    dialog.preset_name = p.name.clone();
                }
            }
            if persist && !dialog.preset_name.trim().is_empty() {
                let name = dialog.preset_name.trim().to_string();
                if let Some(existing) = config.presets.iter_mut().find(|p| p.name == name) {
                    existing.config = dialog.config.clone();
                } else {
                    config.presets.push(NamedConfig {
                        name,
                        config: dialog.config.clone(),
                    });
                }
            }
        }

        if persist {
            self.write_config();
        }
        if do_cancel {
            self.config_dialog = None;
        }
        if do_connect {
            match self.config_dialog.as_ref().and_then(|d| d.editing) {
                Some(port_id) => {
                    let (path, config) = self
                        .config_dialog
                        .take()
                        .map(|d| (d.selected_path, d.config))
                        .unwrap();
                    self.reconnect_with_config(port_id, path, config);
                }
                None => self.connect_from_dialog(),
            }
        }
    }

    /// Modal editor for a tab's user-facing name. This does not reconnect the
    /// serial port; it only updates display state and the remembered session.
    pub(crate) fn show_rename_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.rename_dialog.as_ref() else {
            return;
        };
        if self.defer_to_connect_error() {
            return;
        }

        let detected_label = self
            .connections
            .iter()
            .find(|conn| conn.id == dialog.port)
            .map(|conn| conn.label.clone())
            .unwrap_or_default();
        let mut save = false;
        let mut cancel = false;

        let dialog = self.rename_dialog.as_mut().unwrap();
        egui::Window::new("Rename tab")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("Tab name");
                let response = ui.add(
                    egui::TextEdit::singleline(&mut dialog.name)
                        .hint_text(&detected_label)
                        .desired_width(280.0),
                );
                ui.weak("Leave empty to use the detected device name.");
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                if response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    save = true;
                }
            });

        if save {
            let dialog = self.rename_dialog.take().unwrap();
            self.rename_connection(dialog.port, &dialog.name);
        } else if cancel {
            self.rename_dialog = None;
        }
    }
}

/// Serial-parameter grid, operating on a borrowed [`PortConfig`].
fn connect_controls(ui: &mut egui::Ui, cfg: &mut PortConfig) {
    egui::Grid::new("conn_grid")
        .num_columns(2)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            ui.label("Baud");
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("baud")
                    .selected_text(cfg.baud.to_string())
                    .show_ui(ui, |ui| {
                        for &b in COMMON_BAUDS {
                            ui.selectable_value(&mut cfg.baud, b, b.to_string());
                        }
                    });
                ui.label("or");
                ui.add(
                    egui::DragValue::new(&mut cfg.baud)
                        .speed(0.0)
                        .range(50..=6_000_000),
                )
                .on_hover_text("Click to type any baud rate");
            });
            ui.end_row();

            ui.label("Data bits");
            egui::ComboBox::from_id_salt("databits")
                .selected_text(format!("{}", u8::from(cfg.data_bits)))
                .show_ui(ui, |ui| {
                    for b in [
                        DataBits::Five,
                        DataBits::Six,
                        DataBits::Seven,
                        DataBits::Eight,
                    ] {
                        ui.selectable_value(&mut cfg.data_bits, b, format!("{}", u8::from(b)));
                    }
                });
            ui.end_row();

            ui.label("Parity");
            egui::ComboBox::from_id_salt("parity")
                .selected_text(parity_label(cfg.parity))
                .show_ui(ui, |ui| {
                    for p in [Parity::None, Parity::Odd, Parity::Even] {
                        ui.selectable_value(&mut cfg.parity, p, parity_label(p));
                    }
                });
            ui.end_row();

            ui.label("Stop bits");
            egui::ComboBox::from_id_salt("stopbits")
                .selected_text(format!("{}", u8::from(cfg.stop_bits)))
                .show_ui(ui, |ui| {
                    for s in [StopBits::One, StopBits::Two] {
                        ui.selectable_value(&mut cfg.stop_bits, s, format!("{}", u8::from(s)));
                    }
                });
            ui.end_row();

            ui.label("Flow control");
            egui::ComboBox::from_id_salt("flow")
                .selected_text(flow_label(cfg.flow_control))
                .show_ui(ui, |ui| {
                    for f in [
                        FlowControl::None,
                        FlowControl::Software,
                        FlowControl::Hardware,
                    ] {
                        ui.selectable_value(&mut cfg.flow_control, f, flow_label(f));
                    }
                });
            ui.end_row();

            ui.label("Terminal");
            egui::ComboBox::from_id_salt("terminal")
                .selected_text(cfg.terminal.label())
                .show_ui(ui, |ui| {
                    for m in [
                        TerminalMode::Vt100,
                        TerminalMode::LfOnly,
                        TerminalMode::Classic,
                    ] {
                        ui.selectable_value(&mut cfg.terminal, m, m.label())
                            .on_hover_text(match m {
                                TerminalMode::Vt100 => "Linux/VT100: \\r overwrites the line",
                                TerminalMode::LfOnly => "Break on \\n only; strip \\r",
                                TerminalMode::Classic => "\\n, \\r\\n, or \\r each break a line",
                            });
                    }
                });
            ui.end_row();

            ui.label("Send ending");
            egui::ComboBox::from_id_salt("line_ending")
                .selected_text(cfg.line_ending.label())
                .show_ui(ui, |ui| {
                    for e in [
                        LineEnding::None,
                        LineEnding::Lf,
                        LineEnding::CrLf,
                        LineEnding::Cr,
                    ] {
                        ui.selectable_value(&mut cfg.line_ending, e, e.label());
                    }
                });
            ui.end_row();
        });

    ui.checkbox(
        &mut cfg.dtr_on_open,
        "Assert DTR on open (resets many boards)",
    );
    ui.checkbox(&mut cfg.rts_on_open, "Assert RTS on open");
    ui.checkbox(
        &mut cfg.local_echo,
        "Local echo (show sent input in the log)",
    );
    ui.checkbox(
        &mut cfg.local_history,
        "Local history (Up/Down recall sent input, never sent)",
    );
}

fn parity_label(p: Parity) -> &'static str {
    match p {
        Parity::None => "none",
        Parity::Odd => "odd",
        Parity::Even => "even",
    }
}

fn flow_label(f: FlowControl) -> &'static str {
    match f {
        FlowControl::None => "none",
        FlowControl::Software => "software (XON/XOFF)",
        FlowControl::Hardware => "hardware (RTS/CTS)",
    }
}

fn state_dot(state: ConnState) -> char {
    match state {
        ConnState::Connected | ConnState::Connecting | ConnState::Reconnecting => '•',
        ConnState::Lost | ConnState::Disconnected | ConnState::Closed => '·',
    }
}

fn state_color(state: ConnState) -> egui::Color32 {
    match state {
        ConnState::Connected => egui::Color32::from_rgb(0x33, 0xcc, 0x66),
        ConnState::Connecting | ConnState::Reconnecting => {
            egui::Color32::from_rgb(0xe5, 0xc0, 0x40)
        }
        ConnState::Lost => egui::Color32::from_rgb(0xff, 0x55, 0x55),
        ConnState::Disconnected | ConnState::Closed => egui::Color32::GRAY,
    }
}

fn short_label(label: &str) -> String {
    if label.chars().count() > 24 {
        let s: String = label.chars().take(23).collect();
        format!("{s}…")
    } else {
        label.to_string()
    }
}

fn show_macro_run_indicators(
    ui: &mut egui::Ui,
    runs: &[(usize, String)],
    stop_run: &mut Option<usize>,
) {
    for (run_index, name) in runs {
        ui.separator();
        if ui
            .small_button(format!("⏳ {}", short_label(name)))
            .on_hover_text("Macro is running. Click to stop it.")
            .clicked()
        {
            *stop_run = Some(*run_index);
        }
    }
}

/// A compact catalog for the Macros header button. Keeping every field labeled
/// makes several macros easy to scan without opening the editor.
fn macro_tooltip(macros: &[TransmitMacro]) -> String {
    if macros.is_empty() {
        return "No macros configured. Click to add one.".to_owned();
    }

    let mut tooltip = String::from("Transmit macros\n");
    for (index, macro_def) in macros.iter().enumerate() {
        if index > 0 {
            tooltip.push('\n');
        }
        let name = if macro_def.name.trim().is_empty() {
            "(unnamed)"
        } else {
            macro_def.name.trim()
        };
        let description = if macro_def.description.trim().is_empty() {
            "—"
        } else {
            macro_def.description.trim()
        };
        let shortcut = macro_def.shortcut.filter(|digit| *digit <= 9).map_or_else(
            || "Unassigned".to_owned(),
            |digit| format!("Ctrl+Shift+{digit}"),
        );
        let runs = if macro_def.repeat_indefinitely {
            "Indefinitely".to_owned()
        } else if macro_def.repeat_count <= 1 {
            "Once".to_owned()
        } else {
            format!("{} times", macro_def.repeat_count)
        };
        tooltip.push_str(&format!(
            "Name: {name}\nDescription: {description}\nShortcut: {shortcut}\nRuns: {runs}\n"
        ));
    }
    tooltip.push_str("\nClick to edit or run macros.");
    tooltip
}

/// Text for one detected-port choice. Path-only devices use their path as the
/// identity label too; repeating it adds no information and is especially
/// noisy for ordinary `/dev/tty*` and Windows `COM*` ports.
fn port_choice_text(path: &str, device_label: &str, added: bool) -> String {
    let suffix = if added { "  (added)" } else { "" };
    if same_displayed_port(path, device_label) {
        format!("{path}{suffix}")
    } else {
        format!("{path}  {device_label}{suffix}")
    }
}

fn same_displayed_port(path: &str, device_label: &str) -> bool {
    path == device_label
        || windows_com_name(path).is_some_and(|path| {
            windows_com_name(device_label).is_some_and(|label| path.eq_ignore_ascii_case(label))
        })
}

/// Return the canonical `COM<number>` part of both normal (`COM3`) and Win32
/// device-namespace (`\\.\COM10`) spellings.
fn windows_com_name(value: &str) -> Option<&str> {
    let value = value.strip_prefix(r"\\.\").unwrap_or(value);
    let digits = value.get(3..)?;
    (value.get(..3)?.eq_ignore_ascii_case("COM")
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit()))
    .then_some(value)
}

#[cfg(test)]
mod tests {
    use super::{macro_tooltip, port_choice_text};
    use crate::app::tests::{inert_handle, test_app};
    use egui::{Event, Key};
    use serialcore::config::TransmitMacro;
    use serialcore::store::PortId;

    fn tab_switch(key: Key) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
        }
    }

    #[test]
    fn mouse_drag_reorders_tabs_without_changing_selection() {
        check_mouse_tab_drag(egui::PointerButton::Primary, true);
    }

    #[test]
    fn secondary_and_middle_drags_do_not_reorder_tabs() {
        check_mouse_tab_drag(egui::PointerButton::Secondary, false);
        check_mouse_tab_drag(egui::PointerButton::Middle, false);
    }

    fn check_mouse_tab_drag(drag_button: egui::PointerButton, should_reorder: bool) {
        let (mut app, _enum_tx) = test_app("mouse-tab-reorder");
        for id in [PortId(1), PortId(2), PortId(3)] {
            let mut conn = app.make_connection(
                id,
                format!("probe-{}", id.0),
                Default::default(),
                Default::default(),
                inert_handle(id),
            );
            conn.name = Some(format!("Tab {}", id.0));
            app.connections.push(conn);
        }
        app.active = 1;
        let ctx = egui::Context::default();
        let mut frame = |events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.show_header(ctx),
            )
        };
        let output = frame(vec![]);
        let tab_center = |name: &str| {
            output
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape {
                        (text.galley.text() == name).then_some(text.pos + text.galley.size() / 2.0)
                    } else {
                        None
                    }
                })
                .unwrap()
        };
        let start = tab_center("Tab 1");
        let end = tab_center("Tab 3") + egui::vec2(5.0, 0.0);
        let button = |pos, pressed| Event::PointerButton {
            pos,
            button: drag_button,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(vec![Event::PointerMoved(start), button(start, true)]);
        frame(vec![Event::PointerMoved(end)]);
        frame(vec![button(end, false)]);
        assert_eq!(
            app.connections.iter().map(|c| c.id).collect::<Vec<_>>(),
            if should_reorder {
                vec![PortId(2), PortId(3), PortId(1)]
            } else {
                vec![PortId(1), PortId(2), PortId(3)]
            }
        );
        assert_eq!(app.connections[app.active].id, PortId(2));
    }

    #[test]
    fn reordering_tabs_preserves_selection_and_saves_order() {
        let (mut app, _enum_tx) = test_app("tab-reorder");
        for id in [PortId(1), PortId(2), PortId(3)] {
            let mut conn = app.make_connection(
                id,
                format!("probe-{id:?}"),
                Default::default(),
                Default::default(),
                inert_handle(id),
            );
            conn.name = Some(format!("{}", id.0));
            app.connections.push(conn);
        }
        app.active = 1;
        app.reorder_connection(PortId(1), 3);
        assert_eq!(
            app.connections.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![PortId(2), PortId(3), PortId(1)]
        );
        assert_eq!(app.connections[app.active].id, PortId(2));
        assert_eq!(
            app.config
                .last_open
                .iter()
                .map(|c| c.name.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("2"), Some("3"), Some("1")]
        );

        app.merged_selected = true;
        app.merged_tx_port = Some(PortId(3));
        app.reorder_connection(PortId(1), 0);
        assert_eq!(
            app.connections.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![PortId(1), PortId(2), PortId(3)]
        );
        assert_eq!(app.connections[app.active].id, PortId(2));
        assert!(app.merged_selected);
        assert_eq!(app.merged_tx_port, Some(PortId(3)));

        app.reorder_connection(PortId(2), 2);
        app.reorder_connection(PortId(99), 0);
        assert_eq!(app.connections[app.active].id, PortId(2));
        assert_eq!(app.connections[1].id, PortId(2));
    }

    #[test]
    fn ctrl_shift_arrows_cycle_tabs_and_consume_the_terminal_input() {
        let (mut app, _enum_tx) = test_app("tab-switch-shortcuts");
        for id in [PortId(1), PortId(2)] {
            app.connections.push(app.make_connection(
                id,
                format!("probe-{id:?}"),
                Default::default(),
                Default::default(),
                inert_handle(id),
            ));
        }

        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput {
            events: vec![tab_switch(Key::ArrowLeft)],
            ..Default::default()
        });
        app.consume_tab_switch_shortcut(&ctx);
        assert!(
            app.merged_selected,
            "left from the first tab wraps to Merged"
        );
        assert!(ctx.input(|input| input.events.is_empty()));
        let _ = ctx.end_pass();

        let ctx = egui::Context::default();
        app.merged_selected = false;
        app.active = 0;
        ctx.begin_pass(egui::RawInput {
            events: vec![tab_switch(Key::ArrowRight)],
            ..Default::default()
        });
        app.consume_tab_switch_shortcut(&ctx);
        assert_eq!(app.active, 1, "right selects the next connection tab");
        assert!(!app.merged_selected);
        assert!(ctx.input(|input| input.events.is_empty()));
        let _ = ctx.end_pass();
    }

    #[test]
    fn duplicate_unix_path_label_is_shown_once() {
        assert_eq!(
            port_choice_text("/dev/ttyUSB0", "/dev/ttyUSB0", false),
            "/dev/ttyUSB0"
        );
        assert_eq!(
            port_choice_text("/dev/ttyUSB0", "/dev/ttyUSB0", true),
            "/dev/ttyUSB0  (added)"
        );
    }

    #[test]
    fn duplicate_windows_port_label_is_shown_once() {
        assert_eq!(port_choice_text("COM3", "COM3", false), "COM3");
        assert_eq!(port_choice_text("COM3", "com3", true), "COM3  (added)");
        assert_eq!(port_choice_text(r"\\.\COM10", "COM10", false), r"\\.\COM10");
    }

    #[test]
    fn useful_device_name_is_kept() {
        assert_eq!(
            port_choice_text("COM3", "ST-Link Virtual COM Port", false),
            "COM3  ST-Link Virtual COM Port"
        );
    }

    #[test]
    fn macro_tooltip_lists_every_macro_and_field() {
        let tooltip = macro_tooltip(&[
            TransmitMacro {
                name: "Boot".into(),
                description: "Restart the target".into(),
                shortcut: Some(2),
                ..Default::default()
            },
            TransmitMacro {
                name: "Status".into(),
                description: String::new(),
                shortcut: None,
                ..Default::default()
            },
        ]);

        assert!(
            tooltip.contains("Name: Boot\nDescription: Restart the target\nShortcut: Ctrl+Shift+2")
        );
        assert!(tooltip.contains("Name: Status\nDescription: —\nShortcut: Unassigned"));
        assert_eq!(tooltip.matches("Runs: Once").count(), 2);
    }
}
