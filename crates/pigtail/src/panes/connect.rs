//! Minimal chrome: the header (tabs + new/save/settings), the status footer,
//! and the modal new-connection dialog with preset management.

use crate::app::{available_port_is_added, App, ConfigDialog};
use serialcore::config::{
    DataBits, FlowControl, LineEnding, NamedConfig, Parity, PortConfig, StopBits, TerminalMode,
};
use serialcore::reader::ConnState;

const COMMON_BAUDS: &[u32] = &[
    9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600, 1_000_000, 2_000_000, 3_000_000,
];

impl App {
    /// A plain acknowledgement dialog for `connect_errors`: a one-off
    /// background operation — connect, reconnect, port-detection start, an
    /// export write — that failed outright rather than through the normal
    /// per-connection error path, either because there is no live connection
    /// to show it on or because the failure has nothing to do with a
    /// connection's health. Shows one message at a time, oldest first, so
    /// simultaneous failures all get seen rather than the latest silently
    /// replacing the others.
    pub(crate) fn show_connect_error(&mut self, ctx: &egui::Context) {
        let Some(err) = self.connect_errors.front() else {
            return;
        };
        if super::update::show_ack_window(ctx, err.title, &err.message) {
            self.connect_errors.pop_front();
        }
    }

    /// The top header: one tab per connection, a merged tab, and `+`/save/⚙.
    pub(crate) fn show_header(&mut self, ctx: &egui::Context) {
        let mut to_close: Option<usize> = None;
        let mut set_active: Option<usize> = None;
        let mut select_merged = false;
        let mut new_tab = false;
        let mut save_text = false;
        let mut port_options: Option<usize> = None;
        let mut rename_tab: Option<usize> = None;

        // The config dialog is meant to be modal, but an `egui::Window` does
        // not block input to what it covers, so the header would keep acting
        // on clicks behind it: "+" or a tab's "Port options…" would replace
        // the in-progress dialog with a fresh one, silently discarding a
        // half-filled form, and "Close tab" would leave the dialog's
        // `editing` pointing at a tab that no longer exists (issue #16).
        // Disabled rather than merely ignored so the greying-out shows *why*
        // the clicks do nothing.
        let modal_open = self.config_dialog.is_some() || self.rename_dialog.is_some();

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // Only the tab strip and "+" are disabled: the settings gear
                // and save-view button below act on neither the dialog nor
                // the set of tabs, so there is nothing for them to corrupt.
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
                            .on_hover_text(&tooltip)
                            .on_disabled_hover_text(format!(
                                "{}\n(finish or cancel the open dialog first)",
                                tooltip
                            ));
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

                    if self.connections.len() >= 2
                        && ui
                            .selectable_label(self.merged_selected, "Merged")
                            .clicked()
                    {
                        select_merged = true;
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
                    if ui.button("⚙").on_hover_text("Settings").clicked() {
                        self.show_settings = true;
                    }
                    if ui
                        .add_enabled(self.active_index().is_some(), egui::Button::new("💾"))
                        .on_hover_text(if self.merged_selected {
                            "Save the merged view to a text file"
                        } else {
                            "Save this tab's view to a text file"
                        })
                        .clicked()
                    {
                        save_text = true;
                    }
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
        if let Some(i) = to_close {
            self.close_connection(i);
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
        if save_text {
            if self.merged_selected {
                self.export_merged_view(false);
            } else if let Some(active) = self.active_index() {
                self.export_active_view(active, false);
            }
        }
    }

    /// The bottom status footer: connection state, view details, and the view
    /// toggles — hex, plot and Pin (autoscroll).
    pub(crate) fn show_footer(&mut self, ctx: &egui::Context) {
        let mut toggle_pin = false;
        let mut toggle_plot = false;
        let mut toggle_hex = false;
        let mut merged_tx_port = self.merged_tx_port;
        let mut open_error_win: Option<serialcore::store::PortId> = None;
        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if self.merged_selected {
                    let shown = self.merged_view().len();
                    ui.label(format!("merged · {} lines", self.merged.len()));
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
            let editing = dialog.editing.is_some();
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
                                let added = !editing
                                    && available_port_is_added(index, available, connections);
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
                    } else if !editing
                        && available.iter().enumerate().all(|(index, _)| {
                            available_port_is_added(index, available, connections)
                        })
                    {
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
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
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
    use super::port_choice_text;

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
}
