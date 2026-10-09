//! Custom chrome: device tabs, console toolbar,
//! and the modal new-connection dialog with preset management.

use crate::app::{available_port_is_added, App, ConfigDialog, MergedDialog, TabId};
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
struct DraggedTab(TabId);

fn tab_drag(
    ui: &egui::Ui,
    ctx: &egui::Context,
    resp: &egui::Response,
    id: TabId,
    position: usize,
) -> Option<(TabId, usize)> {
    if resp.drag_started_by(egui::PointerButton::Primary) {
        resp.dnd_set_drag_payload(DraggedTab(id));
    }
    if resp.dnd_hover_payload::<DraggedTab>().is_some() {
        if let Some(pointer) = ctx.pointer_interact_pos() {
            let after = pointer.x >= resp.rect.center().x;
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
                return Some((tab.0, position + usize::from(after)));
            }
        }
    }
    None
}

impl App {
    /// Application accelerators are handled before terminal input, including
    /// when a text field is focused or no connection is open.
    pub(crate) fn consume_app_shortcuts(&mut self, ctx: &egui::Context) {
        if self.tab_close_confirmation.is_some()
            || self.retention_cleanup_confirmation.is_some()
            || self.show_keyboard_shortcuts
            || self.update_dialog.is_some()
            || !self.connect_errors.is_empty()
        {
            return;
        }
        let plain_key = |key| {
            ctx.input_mut(|input| {
            let mut consumed = false;
            input.events.retain(|event| {
                let matches = matches!(event, egui::Event::Key { key: pressed_key, pressed: true, modifiers, .. }
                    if *pressed_key == key && modifiers.is_none());
                consumed |= matches;
                !matches
            });
            consumed
        })
        };
        if plain_key(egui::Key::F1) {
            self.show_about = true;
            self.show_settings = false;
        }
        if plain_key(egui::Key::F2) {
            self.show_settings = true;
            self.show_about = false;
        }
        if self.show_about || self.show_settings {
            return;
        }
        if ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::S)
        }) {
            if self.merged_selected {
                self.export_merged_view(false);
            } else if let Some(active) = self.active_index() {
                self.export_active_view(active, false);
            }
        }
        if ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::M)
        }) {
            self.show_macros_win = true;
        }
    }

    /// Reserve Ctrl+Shift+Left/Right and Ctrl+Shift+Tab for cycling pane tabs.
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

        let tabs = self.visible_pane_tabs();
        let tab_count = tabs.len();
        if tab_count == 0 {
            return;
        }
        let modifiers = egui::Modifiers::CTRL | egui::Modifiers::SHIFT;
        let previous = ctx.input_mut(|input| input.consume_key(modifiers, egui::Key::ArrowLeft));
        let next = ctx.input_mut(|input| {
            let right = input.consume_key(modifiers, egui::Key::ArrowRight);
            let tab = input.consume_key(modifiers, egui::Key::Tab);
            right || tab
        });
        if !previous && !next {
            return;
        }

        let active_tab = if self.merged_selected {
            self.loaded_merged_tab
                .and_then(|i| self.merged_tabs.get(i))
                .map(|t| TabId::Merged(t.id))
        } else {
            self.connections
                .get(self.active)
                .map(|c| TabId::Connection(c.id))
        };
        let current = tabs
            .iter()
            .position(|id| Some(*id) == active_tab)
            .unwrap_or(0);
        let selected = if previous {
            (current + tab_count - 1) % tab_count
        } else {
            (current + 1) % tab_count
        };

        match tabs[selected] {
            TabId::Merged(id) => {
                let index = self.merged_tabs.iter().position(|t| t.id == id).unwrap();
                self.select_merged_tab(index);
            }
            TabId::Connection(id) => {
                self.active = self.connections.iter().position(|c| c.id == id).unwrap();
                self.merged_selected = false;
            }
        }
    }

    pub(crate) fn show_merged_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.merged_dialog.take() else {
            return;
        };
        let selected = &mut dialog.ports;
        selected.retain(|id| self.connections.iter().any(|conn| conn.id == *id));
        let mut open = true;
        let mut create = false;
        let mut cancel = false;
        egui::Window::new(if dialog.editing.is_some() {
            "Options"
        } else {
            "New merged view"
        })
        .id(egui::Id::new("new_merged_view"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .open(&mut open)
        .frame(super::chrome::dialog_frame(ctx))
        .show(ctx, |ui| {
            super::chrome::dialog_style(ui);
            ui.horizontal(|ui| {
                ui.label("Name:");
                ui.text_edit_singleline(&mut dialog.name);
            });
            ui.label("Choose connections to include in this view:");
            if self.connections.is_empty() {
                ui.label(
                    "No connections are open. You can add them later using this tab's options.",
                );
            }
            egui::ScrollArea::vertical()
                .max_height(300.0)
                .show(ui, |ui| {
                    for conn in &self.connections {
                        let mut checked = selected.contains(&conn.id);
                        if ui
                            .checkbox(&mut checked, conn.display_label())
                            .on_hover_text(&conn.label)
                            .changed()
                        {
                            if checked {
                                selected.push(conn.id);
                            } else {
                                selected.retain(|id| *id != conn.id);
                            }
                        }
                    }
                });
            ui.horizontal(|ui| {
                create = ui
                    .button(if dialog.editing.is_some() {
                        "Save"
                    } else {
                        "Create merged view"
                    })
                    .clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        if create {
            if let Some(id) = dialog.editing {
                self.edit_merged_tab(id, dialog.ports);
                self.name_merged_tab(id, &dialog.name);
            } else {
                self.create_merged_tab(dialog.ports);
                let id = self.merged_tabs.last().unwrap().id;
                self.name_merged_tab(id, &dialog.name);
            }
        } else if open && !cancel && !ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.merged_dialog = Some(dialog);
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

    /// The top header: one tab per connection or merged view, `+`, and global
    /// actions collected under an ellipsis menu.
    pub(crate) fn show_header(&mut self, ctx: &egui::Context) {
        self.show_header_in(ctx, None);
    }

    pub(crate) fn show_header_in(&mut self, ctx: &egui::Context, parent: Option<&mut egui::Ui>) {
        let painter = parent
            .as_ref()
            .map(|ui| ui.painter().clone())
            .unwrap_or_else(|| ctx.layer_painter(egui::LayerId::background()));
        let mut backdrop = super::chrome::HeaderBackdrop::reserve(painter);
        let mut to_close: Option<usize> = None;
        let mut set_active: Option<usize> = None;
        let mut select_merged = None;
        let mut close_merged = None;
        let mut edit_merged = None;
        let mut new_merged = false;
        let mut new_tab = false;
        let mut port_options: Option<usize> = None;
        let mut rename_tab: Option<usize> = None;
        let mut reorder = None;
        let tabs = self.visible_pane_tabs();

        // The config dialog is meant to be modal, but an `egui::Window` does
        // not block input to what it covers, so the header would keep acting
        // on clicks behind it: "+" or a tab's "Port options…" would replace
        // the in-progress dialog with a fresh one, silently discarding a
        // half-filled form, and "Close tab" would leave the dialog's
        // `editing` pointing at a tab that no longer exists (issue #16).
        // Disabled rather than merely ignored so the greying-out shows *why*
        // the clicks do nothing.
        // A pending close must also block new editors and replacement close
        // requests until the user confirms or cancels its original target.
        let modal_open = self.merged_dialog.is_some()
            || self.tab_close_confirmation.is_some()
            || self.config_dialog.is_some()
            || self.rename_dialog.is_some()
            || self.file_transfer_dialog.is_some();

        let header_rect = super::workspace::show_panel(
            egui::TopBottomPanel::top(self.pane_widget_id("header")).frame(
                egui::Frame::none()
                    .fill(super::chrome::header_fill(
                        ctx.style().visuals.dark_mode,
                        false,
                    ))
                    .inner_margin(egui::Margin {
                        left: 10.0,
                        right: 8.0,
                        top: 6.0,
                        bottom: 0.0,
                    }),
            ),
            ctx,
            parent,
            |ui| {
                // Status and navigation labels must not join console text selection.
                ui.style_mut().interaction.selectable_labels = false;
                ui.spacing_mut().interact_size.y = 30.0;
                let header = ui.horizontal(|ui| {
                    // Only the tab strip and "+" are disabled: global actions
                    // below act on neither the dialog nor the set of tabs, so
                    // there is nothing for them to corrupt.
                    ui.add_enabled_ui(!modal_open, |ui| {
                        let reserve = if self.workspace.split.is_some() {
                            32.0
                        } else {
                            145.0
                        };
                        egui::ScrollArea::horizontal()
                            .drag_to_scroll(false)
                            .id_salt(self.pane_widget_id("tabs_scroll"))
                            .max_width((ui.available_width() - reserve).max(40.0))
                            .auto_shrink([true, true])
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    for (position, tab_id) in tabs.iter().copied().enumerate() {
                                        match tab_id {
                                            TabId::Connection(id) => {
                                                let i = self
                                                    .connections
                                                    .iter()
                                                    .position(|c| c.id == id)
                                                    .unwrap();
                                                let conn = &self.connections[i];
                                                let selected =
                                                    !self.merged_selected && self.active == i;
                                                let display_label = conn.display_label();
                                                let device_details = if conn.name.is_some() {
                                                    format!("{}\n{}", display_label, conn.label)
                                                } else {
                                                    conn.label.clone()
                                                };
                                                let tooltip = format!(
                                                    "Status: {}\n{}",
                                                    conn.state, device_details
                                                );
                                                // `on_hover_text` only fires on an *enabled* widget,
                                                // so a disabled tab needs its own tooltip to keep the
                                                // detected device name and port available.
                                                let (resp, close) = super::chrome::device_tab(ui, &short_label(display_label), selected, state_color(conn.state));
                                                let resp = resp.on_hover_text(&tooltip).on_disabled_hover_text(format!("{tooltip}\n(finish or cancel the open dialog first)"));
                                                if close { to_close = Some(i); }
                                                if !modal_open {
                                                    reorder =
                                                        tab_drag(ui, ctx, &resp, tab_id, position)
                                                            .or(reorder);
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
                                                    super::chrome::menu_style(ui);
                                                    self.show_tab_layout_menu(ui, tab_id);
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

                                            TabId::Merged(id) => {
                                                let i = self
                                                    .merged_tabs
                                                    .iter()
                                                    .position(|t| t.id == id)
                                                    .unwrap();
                                                let tab = &self.merged_tabs[i];
                                                let (resp, close) = super::chrome::device_tab(ui, &short_label(&tab.name), self.merged_selected && self.loaded_merged_tab == Some(i), egui::Color32::from_rgb(120, 145, 180));
                                                let resp = resp.on_hover_text(&tab.name);
                                                if close { close_merged = Some(i); }
                                                if !modal_open {
                                                    reorder =
                                                        tab_drag(ui, ctx, &resp, tab_id, position)
                                                            .or(reorder);
                                                }
                                                if resp.clicked() {
                                                    select_merged = Some(i);
                                                }
                                                if resp.middle_clicked() {
                                                    close_merged = Some(i);
                                                }
                                                resp.context_menu(|ui| {
                                                    super::chrome::menu_style(ui);
                                                    self.show_tab_layout_menu(ui, tab_id);
                                                    if ui.button("Options").clicked() {
                                                        edit_merged = Some(i);
                                                        ui.close_menu();
                                                    }
                                                    if ui.button("Close tab").clicked() {
                                                        close_merged = Some(i);
                                                        ui.close_menu();
                                                    }
                                                });
                                            }
                                        }
                                    }
                                });
                            });
                        ui.menu_button(egui::RichText::new("+").size(20.0), |ui| {
                            super::chrome::menu_style(ui);
                            if ui.button("New connection").clicked() {
                                new_tab = true;
                                ui.close_menu();
                            }
                            if ui.button("New merged view").clicked() {
                                new_merged = true;
                                ui.close_menu();
                            }
                        });
                    });

                    if self.workspace.split.is_none() {
                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width(), 36.0),
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                super::chrome::window_controls(ui);
                                super::chrome::drag_window(ui);
                            },
                        );
                    }
                });
                // The unused header space accepts a drop after the last tab.
                // Individual tabs consume their payload first, preserving the
                // before/after insertion marker when dropping on a tab.
                if !modal_open {
                    let rect = egui::Rect::from_min_max(
                        header.response.rect.min,
                        egui::pos2(ui.max_rect().right(), header.response.rect.bottom()),
                    );
                    let target =
                        ui.interact(rect, ui.id().with("tab_drop_end"), egui::Sense::hover());
                    if target.dnd_hover_payload::<DraggedTab>().is_some() {
                        ui.painter().line_segment(
                            [rect.left_bottom(), rect.right_bottom()],
                            egui::Stroke::new(2.0_f32, ui.visuals().selection.bg_fill),
                        );
                    }
                    if let Some(tab) = target.dnd_release_payload::<DraggedTab>() {
                        reorder = Some((tab.0, tabs.len()));
                    }
                }
            },
        );

        backdrop.rect = header_rect;
        ctx.data_mut(|data| data.insert_temp(self.pane_widget_id("header_backdrop"), backdrop));

        if let Some(i) = set_active {
            self.active = i;
            self.merged_selected = false;
        }
        if let Some(i) = select_merged {
            self.select_merged_tab(i);
        }
        if let Some(i) = close_merged {
            self.request_close_merged_tab(i);
        }
        if let Some(i) = edit_merged {
            let tab = &self.merged_tabs[i];
            self.merged_dialog = Some(MergedDialog {
                name: tab.name.clone(),
                editing: Some(tab.id),
                ports: tab.ports.clone(),
            });
        }
        if new_merged {
            self.merged_dialog = Some(MergedDialog {
                name: String::new(),
                editing: None,
                ports: self.connections.iter().map(|conn| conn.id).collect(),
            });
        }
        if to_close.is_none() {
            if let Some((id, boundary)) = reorder {
                let full = self.ordered_tabs();
                let boundary = tabs
                    .get(boundary)
                    .and_then(|next| full.iter().position(|tab| tab == next))
                    .or_else(|| {
                        tabs.last()
                            .and_then(|last| full.iter().position(|tab| tab == last))
                            .map(|i| i + 1)
                    })
                    .unwrap_or(full.len());
                self.drop_tab_in_header(id, boundary);
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
    }

    fn show_console_actions(&mut self, ui: &mut egui::Ui) {
        self.show_app_menu(ui);
        let export = ui
            .menu_button("     ", |ui| {
                super::chrome::menu_style(ui);
                for (label, csv) in [("Export text…", false), ("Export CSV…", true)] {
                    if super::chrome::menu_item(ui, label, if csv { "" } else { "Ctrl+Shift+S" })
                        .clicked()
                    {
                        if self.merged_selected {
                            self.export_merged_view(csv);
                        } else if let Some(active) = self.active_index() {
                            self.export_active_view(active, csv);
                        }
                        ui.close_menu();
                    }
                }
            })
            .response
            .on_hover_text("Export current view");
        super::chrome::paint_export(ui, export.rect);
        if super::chrome::action_button(
            ui,
            super::chrome::ActionIcon::Clear,
            false,
            "Clear console",
        )
        .clicked()
        {
            let port = if self.merged_selected {
                None
            } else {
                self.active_index().map(|i| self.connections[i].id)
            };
            self.clear_console(port);
        }
        if super::chrome::action_button(
            ui,
            super::chrome::ActionIcon::Search,
            self.show_search,
            "Search · Ctrl+Shift+F",
        )
        .clicked()
        {
            self.show_search = !self.show_search;
            if self.show_search {
                self.search_focus_request = true;
            }
        }
        ui.separator();
    }

    pub(crate) fn show_app_menu(&mut self, ui: &mut egui::Ui) {
        let macros_tooltip = macro_tooltip(&self.config.macros);
        let menu = ui
            .menu_button("     ", |ui| {
                super::chrome::menu_style(ui);
                self.show_console_options(ui);
                if ui
                    .button("Send file…")
                    .on_hover_text("Choose a file to send to the active console")
                    .clicked()
                {
                    self.choose_file_transfer();
                    ui.close_menu();
                }
                ui.separator();
                if super::chrome::menu_item(ui, "Macros", "Ctrl+Shift+M")
                    .on_hover_text(&macros_tooltip)
                    .clicked()
                {
                    self.show_macros_win = true;
                    ui.close_menu();
                }
                if ui.button("Show keyboard shortcuts").clicked() {
                    self.show_keyboard_shortcuts = true;
                    ui.close_menu();
                }
                if super::chrome::menu_item(ui, "Settings", "F2").clicked() {
                    self.show_settings = true;
                    ui.close_menu();
                }
                let updating = self.update_rx.is_some() || self.install_rx.is_some();
                let update_unavailable = if self.demo_mode {
                    "Updates are disabled in demo builds"
                } else {
                    "An update check or installation is in progress"
                };
                if ui
                    .add_enabled(
                        !self.demo_mode && !updating,
                        egui::Button::new("Check for updates"),
                    )
                    .on_disabled_hover_text(update_unavailable)
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
                    ui.ctx().open_url(egui::OpenUrl::new_tab(
                        "https://buymeacoffee.com/rustypig91g",
                    ));
                    ui.close_menu();
                }
                if super::chrome::menu_item(ui, "About", "F1").clicked() {
                    self.show_about = true;
                    ui.close_menu();
                }
            })
            .response
            .on_hover_text("More options");
        super::chrome::paint_overflow(ui, menu.rect);
    }

    /// A compact reference for the application-wide keyboard commands. Device
    /// input remains intentionally separate: unlisted keystrokes go to the
    /// active serial console.
    pub(crate) fn show_keyboard_shortcuts_window(&mut self, ctx: &egui::Context) {
        if !self.show_keyboard_shortcuts {
            return;
        }
        let mut open = true;
        let response = super::chrome::app_modal(ctx, "Keyboard shortcuts").show(ctx, |ui| {
            ui.set_width(540.0);
            super::chrome::dialog_style(ui);
            let scrollbar = &mut ui.spacing_mut().scroll;
            scrollbar.floating_allocated_width = scrollbar.bar_width + 8.0;
            super::chrome::modal_header(ui, "Keyboard shortcuts", &mut open);
            let height = (ctx.screen_rect().height() - 160.0).clamp(180.0, 600.0);
            egui::ScrollArea::vertical()
                .max_height(height)
                .min_scrolled_height(height)
                .show(ui, |ui| {
                    for (action, shortcut) in [
                        ("Previous / next tab", "Ctrl+Shift+Left / Right"),
                        ("Next tab in current pane", "Ctrl+Shift+Tab"),
                        ("About", "F1"),
                        ("Settings", "F2"),
                        ("Save current view as text", "Ctrl+Shift+S"),
                        ("Switch between split panes", "F6"),
                        ("Scroll console up / down one line", "Ctrl+Shift+Up / Down"),
                        (
                            "Scroll console up / down one page",
                            "Ctrl+Shift+Page Up / Down",
                        ),
                        ("Toggle plot (connection tabs)", "Ctrl+Shift+P"),
                        ("Pin console to bottom", "Ctrl+Shift+Space"),
                        (
                            "Log / Hex / ANSI view (connection tabs)",
                            "Ctrl+Shift+Q / W / E",
                        ),
                        ("Show or hide search", "Ctrl+Shift+F"),
                        ("Copy selected text / paste to console", "Ctrl+Shift+C / V"),
                        ("Change console text size", "Ctrl+mouse wheel"),
                        ("Open transmit macros", "Ctrl+Shift+M"),
                        ("Run the assigned macro", "Ctrl+Shift+0–9"),
                    ] {
                        ui.allocate_ui_with_layout(
                            egui::vec2(ui.available_width(), 32.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.label(action);
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.weak(shortcut);
                                    },
                                );
                            },
                        );
                    }
                });
            ui.separator();
            ui.weak("All other keystrokes are sent to the active serial console.");
        });
        self.show_keyboard_shortcuts = open && !response.should_close();
    }

    pub(crate) fn show_about_window(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        let logo_id = egui::Id::new("about_logo");
        let logo = ctx
            .data(|data| data.get_temp::<Option<egui::TextureHandle>>(logo_id))
            .unwrap_or_else(|| {
                let texture = match eframe::icon_data::from_png_bytes(include_bytes!("../icon.png"))
                {
                    Ok(icon) => Some(ctx.load_texture(
                        "Pigtail logo",
                        egui::ColorImage::from_rgba_unmultiplied(
                            [icon.width as usize, icon.height as usize],
                            &icon.rgba,
                        ),
                        egui::TextureOptions::LINEAR,
                    )),
                    Err(error) => {
                        tracing::warn!("loading About logo: {error}");
                        None
                    }
                };
                ctx.data_mut(|data| data.insert_temp(logo_id, texture.clone()));
                texture
            });
        let mut open = true;
        let response = super::chrome::app_modal(ctx, "About").show(ctx, |ui| {
            ui.set_width(420.0);
            ui.spacing_mut().button_padding = egui::vec2(10.0, 7.0);
            ui.spacing_mut().item_spacing = egui::vec2(12.0, 10.0);
            super::chrome::modal_header(ui, "About", &mut open);
            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                if let Some(logo) = &logo {
                    ui.add(egui::Image::new(logo).fit_to_exact_size(egui::vec2(104.0, 104.0)));
                    ui.add_space(8.0);
                }
                ui.heading("Rusty's Pigtail");
                ui.weak(concat!("Serial Terminal · v", env!("CARGO_PKG_VERSION")));
                ui.add_space(14.0);
                ui.label("A desktop serial terminal.");
                ui.add_space(14.0);
                ui.hyperlink_to(
                    "GitHub repository",
                    "https://github.com/rustypig91/pigtail-serial-console",
                );
            });
            ui.add_space(20.0);
        });
        self.show_about = open && !response.should_close();
    }

    /// The reference toolbar leaves the left side empty and groups the primary
    /// view switches and icon actions on the right.
    pub(crate) fn show_toolbar(&mut self, ctx: &egui::Context) {
        self.show_toolbar_in(ctx, None);
    }

    pub(crate) fn show_toolbar_in(&mut self, ctx: &egui::Context, parent: Option<&mut egui::Ui>) {
        let long_running_macros =
            self.long_running_macro_indicators(Instant::now(), self.macro_target_port());
        let mut stop_macro_run = None;
        let mut select_view = None;
        let mut toggle_plot = false;
        let toolbar_rect = super::workspace::show_panel(
            egui::TopBottomPanel::top(self.pane_widget_id("console_toolbar")).frame(
                egui::Frame::none()
                    .fill(super::chrome::header_fill(
                        ctx.style().visuals.dark_mode,
                        true,
                    ))
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        if ctx.style().visuals.dark_mode {
                            egui::Color32::from_rgb(39, 49, 61)
                        } else {
                            egui::Color32::from_rgb(211, 220, 232)
                        },
                    ))
                    .rounding(egui::Rounding {
                        nw: 6.0,
                        ne: 6.0,
                        sw: 0.0,
                        se: 0.0,
                    })
                    .inner_margin(egui::Margin::symmetric(8.0, 5.0)),
            ),
            ctx,
            parent,
            |ui| {
                ui.style_mut().interaction.selectable_labels = false;
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.spacing_mut().interact_size = egui::vec2(32.0, 32.0);
                ui.spacing_mut().button_padding = egui::vec2(8.0, 6.0);
                let mut search_in_header = false;
                ui.horizontal(|ui| {
                    show_macro_run_indicators(ui, &long_running_macros, &mut stop_macro_run);
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center).with_main_wrap(true),
                        |ui| {
                            self.show_console_actions(ui);
                            if self.merged_selected {
                                super::chrome::view_button(
                                    ui,
                                    super::chrome::ViewIcon::Log,
                                    "Log",
                                    true,
                                )
                                .on_hover_text("Merged chronological log");
                            } else if let Some(active) = self.active_index() {
                                let conn = &self.connections[active];
                                if super::chrome::view_button(
                                    ui,
                                    super::chrome::ViewIcon::Plot,
                                    "Plot",
                                    conn.show_plot,
                                )
                                .on_hover_text("Toggle plot · Ctrl+Shift+P")
                                .clicked()
                                {
                                    toggle_plot = true;
                                }
                                if super::chrome::view_button(
                                    ui,
                                    super::chrome::ViewIcon::Terminal,
                                    "ANSI/VT",
                                    conn.screen_view,
                                )
                                .on_hover_text("Terminal screen · Ctrl+Shift+E")
                                .clicked()
                                {
                                    select_view = Some((true, false));
                                }
                                if super::chrome::view_button(
                                    ui,
                                    super::chrome::ViewIcon::Hex,
                                    "Hex",
                                    conn.hex_view && !conn.screen_view,
                                )
                                .on_hover_text("Raw bytes · Ctrl+Shift+W")
                                .clicked()
                                {
                                    select_view = Some((false, true));
                                }
                                if super::chrome::view_button(
                                    ui,
                                    super::chrome::ViewIcon::Log,
                                    "Log",
                                    !conn.hex_view && !conn.screen_view,
                                )
                                .on_hover_text("Chronological log · Ctrl+Shift+Q")
                                .clicked()
                                {
                                    select_view = Some((false, false));
                                }
                            }
                            if self.show_search && ui.available_size_before_wrap().x >= 280.0 {
                                let width = ui.available_size_before_wrap().x;
                                ui.allocate_ui_with_layout(
                                    egui::vec2(width, 32.0),
                                    egui::Layout::left_to_right(egui::Align::Center),
                                    |ui| self.show_header_search(ui),
                                );
                                search_in_header = true;
                            }
                        },
                    );
                });
                // Narrow split panes keep the search controls in the header,
                // on a second row when the view buttons use the first row.
                if self.show_search && !search_in_header {
                    self.show_header_search(ui);
                }
            },
        );
        let backdrop_id = self.pane_widget_id("header_backdrop");
        if let Some(mut backdrop) = super::chrome::HeaderBackdrop::current(ctx, backdrop_id) {
            backdrop.rect = backdrop.rect.union(toolbar_rect);
            ctx.data_mut(|data| data.insert_temp(backdrop_id, backdrop));
        }
        if let Some(run_index) = stop_macro_run {
            self.stop_macro_run(run_index);
        }
        if let Some(active) = self.active_index() {
            if toggle_plot {
                self.connections[active].show_plot = !self.connections[active].show_plot;
            }
            if let Some((screen, hex)) = select_view {
                self.connections[active].screen_view = screen;
                self.connections[active].hex_view = hex;
                self.save_session();
            }
        }
    }

    /// A slim status strip with a compact pin-to-bottom toggle.
    pub(crate) fn show_status_footer(&mut self, ctx: &egui::Context) {
        self.show_status_footer_in(ctx, None);
    }

    pub(crate) fn show_status_footer_in(
        &mut self,
        ctx: &egui::Context,
        parent: Option<&mut egui::Ui>,
    ) {
        let dark = ctx.style().visuals.dark_mode;
        let mut toggle_pin = false;
        super::workspace::show_panel(
            egui::TopBottomPanel::bottom(self.pane_widget_id("status_footer")).frame(
                egui::Frame::none()
                    .fill(if dark {
                        egui::Color32::from_rgb(18, 24, 31)
                    } else {
                        egui::Color32::from_rgb(239, 243, 248)
                    })
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        if dark {
                            egui::Color32::from_rgb(39, 49, 61)
                        } else {
                            egui::Color32::from_rgb(211, 220, 232)
                        },
                    ))
                    .inner_margin(egui::Margin::symmetric(10.0, 3.0)),
            ),
            ctx,
            parent,
            |ui| {
                ui.style_mut().interaction.selectable_labels = false;
                ui.style_mut().override_text_style = Some(egui::TextStyle::Small);
                ui.style_mut()
                    .text_styles
                    .insert(egui::TextStyle::Small, egui::FontId::proportional(11.0));
                ui.spacing_mut().interact_size.y = 14.0;
                ui.spacing_mut().item_spacing.x = 10.0;
                ui.horizontal(|ui| {
                    let width = ui.available_width();
                    let (follow, unread) = if self.merged_selected {
                        ui.weak("Merged");
                        ui.weak(format!("{} lines", self.merged.len()));
                        if width > 400.0 && self.merged_filter_active() {
                            ui.weak(format!("{} shown", self.merged_view().len()));
                        }
                        if width > 500.0 && !self.merged_search_matches.is_empty() {
                            ui.weak(format!(
                                "match {}/{}",
                                self.merged_search_pos.map_or(0, |p| p + 1),
                                self.merged_search_matches.len()
                            ));
                        }
                        (self.merged_follow, self.merged_new_since_scroll)
                    } else if let Some(active) = self.active_index() {
                        let conn = &self.connections[active];
                        ui.colored_label(state_color(conn.state), conn.state.to_string());
                        ui.weak(format!("{} lines", conn.store.next_abs_index()));
                        if width > 450.0 {
                            ui.weak(conn.port_config.summary());
                        }
                        if let Some(err) = &conn.last_error {
                            ui.colored_label(egui::Color32::from_rgb(255, 95, 95), "Error")
                                .on_hover_text(&err.msg);
                        }
                        if width > 550.0 && conn.filter_index_active() {
                            ui.weak(format!("{} shown", conn.filter_index.len()));
                        }
                        if width > 650.0 && !conn.screen_view && !conn.search_matches.is_empty() {
                            ui.weak(format!(
                                "match {}/{}",
                                conn.search_pos.map_or(0, |p| p + 1),
                                conn.search_matches.len()
                            ));
                        }
                        if width > 750.0
                            && (conn.store.evicted_any()
                                || conn.raw_evicted_any
                                || conn.series_evicted_any)
                        {
                            ui.weak("History limited")
                                .on_hover_text("Older history evicted; full capture on disk");
                        }
                        (conn.follow, conn.new_since_scroll)
                    } else {
                        ui.weak("No connection");
                        return;
                    };
                    if ui.available_width() > 28.0 {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if super::chrome::pin_button(ui, follow).clicked() {
                                toggle_pin = true;
                            }
                            if !follow && unread > 0 && ui.available_width() > 80.0 {
                                ui.weak(format!("{unread} new"));
                            }
                        });
                    }
                });
            },
        );
        if toggle_pin {
            if self.merged_selected {
                self.merged_follow = !self.merged_follow;
                if self.merged_follow {
                    self.merged_new_since_scroll = 0;
                }
            } else if let Some(active) = self.active_index() {
                let conn = &mut self.connections[active];
                conn.follow = !conn.follow;
                if conn.follow {
                    conn.new_since_scroll = 0;
                }
            }
        }
    }

    /// Secondary controls stay in the overflow menu rather than crowding the
    /// primary Log / Hex / ANSI/VT / Plot group.
    fn show_console_options(&mut self, ui: &mut egui::Ui) {
        let mut has_options = false;
        if !self.merged_selected {
            if let Some(active) = self.active_index() {
                if let Some(err) = &self.connections[active].last_error {
                    has_options = true;
                    if ui
                        .button("Show error details…")
                        .on_hover_text(&err.msg)
                        .clicked()
                    {
                        self.show_error_win = Some(self.connections[active].id);
                        ui.close_menu();
                    }
                }
            }
        }
        if self.merged_selected {
            has_options = true;
            let selected_label = self
                .merged_tx_port
                .and_then(|id| self.connections.iter().find(|conn| conn.id == id))
                .map(|conn| short_label(conn.display_label()))
                .unwrap_or_else(|| "Select device".into());
            let mut selected = self.merged_tx_port;
            egui::ComboBox::from_id_salt(self.pane_widget_id("merged_tx_device"))
                .selected_text(format!("Send to: {selected_label}"))
                .show_ui(ui, |ui| {
                    super::chrome::popup_style(ui);
                    for conn in self
                        .connections
                        .iter()
                        .filter(|conn| self.merged_contains(conn.id))
                    {
                        if ui
                            .add_enabled(
                                conn.state != ConnState::Closed,
                                egui::SelectableLabel::new(
                                    selected == Some(conn.id),
                                    short_label(conn.display_label()),
                                ),
                            )
                            .clicked()
                        {
                            selected = Some(conn.id);
                        }
                    }
                });
            self.merged_tx_port = selected;
        }
        if self.config.highlight.iter().any(|rule| rule.enabled) {
            has_options = true;
            ui.checkbox(&mut self.highlights_visible, "Highlights");
        }
        if has_options {
            ui.separator();
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
                .frame(super::chrome::dialog_frame(ctx))
                .show(ctx, |ui| {
                    super::chrome::dialog_style(ui);
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
                            super::chrome::popup_style(ui);
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
                                super::chrome::popup_style(ui);
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
            .frame(super::chrome::dialog_frame(ctx))
            .show(ctx, |ui| {
                super::chrome::dialog_style(ui);
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
                        super::chrome::popup_style(ui);
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
                    super::chrome::popup_style(ui);
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
                    super::chrome::popup_style(ui);
                    for p in [Parity::None, Parity::Odd, Parity::Even] {
                        ui.selectable_value(&mut cfg.parity, p, parity_label(p));
                    }
                });
            ui.end_row();

            ui.label("Stop bits");
            egui::ComboBox::from_id_salt("stopbits")
                .selected_text(format!("{}", u8::from(cfg.stop_bits)))
                .show_ui(ui, |ui| {
                    super::chrome::popup_style(ui);
                    for s in [StopBits::One, StopBits::Two] {
                        ui.selectable_value(&mut cfg.stop_bits, s, format!("{}", u8::from(s)));
                    }
                });
            ui.end_row();

            ui.label("Flow control");
            egui::ComboBox::from_id_salt("flow")
                .selected_text(flow_label(cfg.flow_control))
                .show_ui(ui, |ui| {
                    super::chrome::popup_style(ui);
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
                    super::chrome::popup_style(ui);
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
                    super::chrome::popup_style(ui);
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
    fn about_and_settings_accelerators_are_exclusive_and_consume_the_key() {
        let (mut app, _enum_tx) = test_app("dialog-accelerators");
        let ctx = egui::Context::default();
        for key in [Key::F1, Key::F2] {
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    ..Default::default()
                },
                |ctx| {
                    app.consume_app_shortcuts(ctx);
                    assert_eq!(app.show_about, key == Key::F1);
                    assert_eq!(app.show_settings, key == Key::F2);
                    assert!(ctx.input(|input| input.events.is_empty()));
                },
            );
        }
    }

    #[test]
    fn about_and_settings_modals_block_background_clicks() {
        for settings in [false, true] {
            let (mut app, _enum_tx) = test_app(if settings {
                "settings-modal"
            } else {
                "about-modal"
            });
            app.show_settings = settings;
            app.show_about = !settings;
            let ctx = egui::Context::default();
            let mut clicked = false;
            let mut render = |events| {
                ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1000.0, 900.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            clicked |= ui.button("Background action").clicked();
                        });
                        app.show_settings_window(ctx);
                        app.show_about_window(ctx);
                    },
                )
            };
            for _ in 0..3 {
                render(vec![]);
            }
            let pos = egui::pos2(50.0, 18.0);
            for pressed in [true, false] {
                render(vec![
                    Event::PointerMoved(pos),
                    Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ]);
            }
            assert!(!clicked, "Modal must block background controls");
        }
    }

    #[test]
    fn macros_accelerator_opens_without_connection_and_consumes_only_ctrl_shift_m() {
        let (mut app, _enum_tx) = test_app("macros-accelerator");
        let ctx = egui::Context::default();
        for modifiers in [
            egui::Modifiers::CTRL,
            egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
        ] {
            let _ = ctx.run(
                egui::RawInput {
                    events: vec![Event::Key {
                        key: Key::M,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers,
                    }],
                    ..Default::default()
                },
                |ctx| {
                    ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("text-field")));
                    app.consume_app_shortcuts(ctx);
                    let is_accelerator = modifiers.shift;
                    assert_eq!(app.show_macros_win, is_accelerator);
                    assert_eq!(ctx.input(|input| input.events.is_empty()), is_accelerator);
                },
            );
        }
    }

    #[test]
    fn plus_opens_a_connection_or_the_merged_view_picker() {
        let (mut app, _enum_tx) = test_app("new-merged-menu");
        let ctx = egui::Context::default();
        let frame = |app: &mut crate::app::App, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 700.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    app.show_header(ctx);
                    app.show_merged_dialog(ctx);
                    app.show_tab_close_confirmation(ctx);
                },
            )
        };
        let click_button = |app: &mut crate::app::App, label: &str, button| {
            frame(app, vec![]);
            let output = frame(app, vec![]);
            let center = output
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape {
                        (text.galley.text() == label).then_some(text.pos + text.galley.size() / 2.0)
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| panic!("missing control: {label}"));
            for pressed in [true, false] {
                frame(
                    app,
                    vec![
                        Event::PointerMoved(center),
                        Event::PointerButton {
                            pos: center,
                            button,
                            pressed,
                            modifiers: Default::default(),
                        },
                    ],
                );
            }
        };
        let click = |app: &mut crate::app::App, label: &str| {
            click_button(app, label, egui::PointerButton::Primary);
        };
        click(&mut app, "+");
        click(&mut app, "New merged view");
        click(&mut app, "Create merged view");
        assert!(app.merged_tabs[0].ports.is_empty());
        app.close_merged_tab(0);
        click(&mut app, "+");
        click(&mut app, "New connection");
        assert!(app.config_dialog.is_some());
        app.config_dialog = None;
        for id in 1..=3 {
            app.connections.push(app.make_connection(
                PortId(id),
                format!("Tab {id}"),
                Default::default(),
                Default::default(),
                inert_handle(PortId(id)),
            ));
        }
        assert!(app.merged_tabs.is_empty());
        click(&mut app, "+");
        assert!(app.config_dialog.is_none());
        click(&mut app, "New connection");
        assert!(app.config_dialog.is_some());
        app.config_dialog = None;
        click(&mut app, "+");
        click(&mut app, "New merged view");
        assert!(app.merged_dialog.is_some());
        assert!(app.floating_window_open());
        click(&mut app, "Cancel");
        assert!(app.merged_tabs.is_empty());
        click(&mut app, "+");
        click(&mut app, "New merged view");
        // The tab and checkbox share a label; select the subset directly here.
        app.merged_dialog.as_mut().unwrap().ports = vec![PortId(1), PortId(3)];
        click(&mut app, "Create merged view");
        assert!(app.merged_dialog.is_none());
        assert_eq!(app.merged_tabs[0].ports, vec![PortId(1), PortId(3)]);
        click(&mut app, "+");
        click(&mut app, "New merged view");
        click(&mut app, "Create merged view");
        assert_eq!(app.merged_tabs.len(), 2);
        assert_eq!(app.loaded_merged_tab, Some(1));
        let edited_id = app.merged_tabs[0].id;
        let label = app.merged_tabs[0].name.clone();
        click_button(&mut app, &label, egui::PointerButton::Secondary);
        click(&mut app, "Options");
        assert_eq!(app.merged_dialog.as_ref().unwrap().editing, Some(edited_id));
        assert_eq!(
            app.merged_dialog.as_ref().unwrap().ports,
            vec![PortId(1), PortId(3)]
        );
        app.merged_dialog.as_mut().unwrap().ports.clear();
        app.merged_dialog.as_mut().unwrap().name = "Cancelled name".into();
        click(&mut app, "Cancel");
        assert_eq!(app.merged_tabs[0].name, label);
        assert_eq!(app.merged_tabs[0].ports, vec![PortId(1), PortId(3)]);
        click_button(&mut app, &label, egui::PointerButton::Secondary);
        click(&mut app, "Options");
        app.merged_dialog.as_mut().unwrap().ports = vec![PortId(2)];
        app.merged_dialog.as_mut().unwrap().name = "  My merged view  ".into();
        click(&mut app, "Save");
        assert_eq!(app.merged_tabs[0].name, "My merged view");
        assert_eq!(app.merged_tabs.len(), 2);
        assert_eq!(app.merged_tabs[0].id, edited_id);
        assert_eq!(app.merged_tabs[0].ports, vec![PortId(2)]);
        assert_eq!(
            app.merged_tabs[1].ports,
            vec![PortId(1), PortId(2), PortId(3)]
        );
        assert_eq!(app.loaded_merged_tab, Some(0));
        click_button(&mut app, "My merged view", egui::PointerButton::Middle);
        assert!(app.tab_close_confirmation.is_some());
        assert_eq!(app.merged_tabs.len(), 2);
        click(&mut app, "Cancel");
        assert!(app.tab_close_confirmation.is_none());
        assert_eq!(app.merged_tabs.len(), 2);
        click_button(&mut app, "My merged view", egui::PointerButton::Secondary);
        click(&mut app, "Close tab");
        assert!(app.tab_close_confirmation.is_some());
        click(&mut app, "Close tab");
        assert!(app.tab_close_confirmation.is_none());
        assert_eq!(app.merged_tabs.len(), 1);
        assert_eq!(app.connections.len(), 3);
    }

    #[test]
    fn close_confirmation_blocks_tab_actions_until_dismissed() {
        let (mut app, _enum_tx) = test_app("close-confirmation-header");
        for id in [PortId(1), PortId(2)] {
            app.connections.push(app.make_connection(
                id,
                format!("Tab {}", id.0),
                Default::default(),
                Default::default(),
                inert_handle(id),
            ));
        }
        app.request_close_connection(0);
        let ctx = egui::Context::default();
        let frame = |app: &mut crate::app::App, events| {
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
        let output = frame(&mut app, vec![]);
        let tab = output
            .shapes
            .iter()
            .find_map(|shape| {
                if let egui::Shape::Text(text) = &shape.shape {
                    (text.galley.text() == "Tab 2").then_some(text.pos + text.galley.size() / 2.0)
                } else {
                    None
                }
            })
            .unwrap();
        let click = |app: &mut crate::app::App, button| {
            for pressed in [true, false] {
                frame(
                    app,
                    vec![
                        Event::PointerMoved(tab),
                        Event::PointerButton {
                            pos: tab,
                            button,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
        };
        click(&mut app, egui::PointerButton::Secondary);
        assert!(
            !ctx.is_context_menu_open(),
            "port options must stay inaccessible"
        );
        click(&mut app, egui::PointerButton::Middle);
        assert_eq!(
            app.tab_close_confirmation,
            Some((crate::app::TabId::Connection(PortId(1)), false))
        );
        click(&mut app, egui::PointerButton::Primary);
        assert_eq!(app.active, 0);
        app.tab_close_confirmation = None;
        frame(&mut app, vec![]);
        click(&mut app, egui::PointerButton::Primary);
        assert_eq!(app.active, 1, "dismissing restores tab interaction");
    }

    #[test]
    fn mouse_drag_reorders_tabs_without_changing_selection() {
        check_mouse_tab_drag(egui::PointerButton::Primary, true, false);
    }

    #[test]
    fn secondary_and_middle_drags_do_not_reorder_tabs() {
        check_mouse_tab_drag(egui::PointerButton::Secondary, false, false);
        check_mouse_tab_drag(egui::PointerButton::Middle, false, false);
    }

    #[test]
    fn merged_tabs_drag_between_connections_without_changing_selection() {
        check_mouse_tab_drag(egui::PointerButton::Primary, true, true);
        check_mouse_tab_drag(egui::PointerButton::Secondary, false, true);
        check_mouse_tab_drag(egui::PointerButton::Middle, false, true);
    }

    fn check_mouse_tab_drag(drag_button: egui::PointerButton, should_reorder: bool, merged: bool) {
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
        if merged {
            app.create_merged_tab(vec![PortId(1), PortId(2)]);
            app.merged_selected = false;
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
        let start = tab_center(if merged { "Merged 1" } else { "Tab 1" });
        let end = tab_center(if merged { "Tab 1" } else { "Tab 3" }) + egui::vec2(25.0, 0.0);
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
            if should_reorder && !merged {
                vec![PortId(2), PortId(3), PortId(1)]
            } else {
                vec![PortId(1), PortId(2), PortId(3)]
            }
        );
        assert_eq!(app.connections[app.active].id, PortId(2));
        if merged {
            use crate::app::TabId::{Connection, Merged};
            assert_eq!(
                app.ordered_tabs(),
                if should_reorder {
                    vec![
                        Connection(PortId(1)),
                        Merged(1),
                        Connection(PortId(2)),
                        Connection(PortId(3)),
                    ]
                } else {
                    vec![
                        Connection(PortId(1)),
                        Connection(PortId(2)),
                        Connection(PortId(3)),
                        Merged(1),
                    ]
                }
            );
        }
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
    fn ctrl_shift_arrows_and_tab_cycle_tabs_and_consume_the_terminal_input() {
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

        app.create_merged_tab(vec![PortId(1), PortId(2)]);
        app.merged_selected = false;
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

        let ctx = egui::Context::default();
        app.active = 0;
        ctx.begin_pass(egui::RawInput {
            events: vec![tab_switch(Key::Tab)],
            ..Default::default()
        });
        app.consume_tab_switch_shortcut(&ctx);
        assert_eq!(app.active, 1, "Ctrl+Shift+Tab selects the next tab");
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
