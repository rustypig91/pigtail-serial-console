//! User-defined transmit macros and their non-blocking execution scheduler.

use crate::app::{App, MacroEditor, MacroRun};
use serialcore::config::{MacroStep, TransmitMacro};
use serialcore::reader::ConnState;
use serialcore::store::{IncomingLine, LineFlags, PortId};
use std::time::{Duration, Instant};

const MAX_DELAY_MS: u64 = 3_600_000;
const DEFAULT_DELAY_MS: u64 = 100;

impl App {
    /// The connection a macro started right now should keep targeting.
    ///
    /// In the merged view this is the explicit "Send to" device. A run stores
    /// its port id so switching tabs while it is delayed does not redirect the
    /// remaining commands to a different device.
    fn macro_target_port(&self) -> Option<PortId> {
        if self.merged_selected {
            self.merged_tx_port.filter(|id| {
                self.connections
                    .iter()
                    .any(|conn| conn.id == *id && conn.state != ConnState::Closed)
            })
        } else {
            self.active_index()
                .map(|index| &self.connections[index])
                .filter(|conn| conn.state != ConnState::Closed)
                .map(|conn| conn.id)
        }
    }

    /// Reserve an assigned Ctrl+Shift+digit chord before raw console input.
    pub(crate) fn consume_macro_shortcut(&mut self, ctx: &egui::Context) {
        if self.floating_window_open()
            || ctx.is_context_menu_open()
            || ctx.memory(|memory| memory.any_popup_open() || memory.focused().is_some())
            || self.macro_target_port().is_none()
        {
            return;
        }

        let keys = [
            egui::Key::Num0,
            egui::Key::Num1,
            egui::Key::Num2,
            egui::Key::Num3,
            egui::Key::Num4,
            egui::Key::Num5,
            egui::Key::Num6,
            egui::Key::Num7,
            egui::Key::Num8,
            egui::Key::Num9,
        ];
        let modifiers = egui::Modifiers::CTRL | egui::Modifiers::SHIFT;
        let mut selected = None;
        for (digit, key) in keys.into_iter().enumerate() {
            let Some(index) = self
                .config
                .macros
                .iter()
                .position(|macro_def| macro_def.shortcut == Some(digit as u8))
            else {
                continue;
            };
            if consume_macro_key(ctx, modifiers, key) {
                selected = Some(index);
                break;
            }
        }
        if let Some(index) = selected {
            self.start_macro(index, Instant::now());
        }
    }

    pub(crate) fn show_macros_window(&mut self, ctx: &egui::Context) {
        if self.show_macros_win {
            self.show_macro_catalog(ctx);
        }
        self.show_shortcut_conflict(ctx);
        self.show_macro_editor(ctx);
    }

    /// Read-only macro catalog. Definitions are changed only in the separate
    /// add/edit window so an accidental click cannot rewrite configuration.
    fn show_macro_catalog(&mut self, ctx: &egui::Context) {
        let mut open = self.show_macros_win;
        let can_run = self.macro_target_port().is_some();
        let editor_open = self.macro_editor.is_some();
        let mut run = None;
        let mut edit = None;
        let mut remove = None;
        let mut add = false;
        let mut stop_all = false;
        let mut shortcut_change = None;
        let mut shortcut_conflict = None;

        egui::Window::new("Transmit macros")
            .open(&mut open)
            .default_width(460.0)
            .default_height(360.0)
            .show(ctx, |ui| {
                ui.weak(
                    "Each command uses the selected device's line ending. Shortcuts run only \
                     while the console has keyboard focus.",
                );
                if !can_run {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        "Select a connection (and a Send to device in Merged) to run a macro.",
                    );
                }
                if !self.macro_runs.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} running", self.macro_runs.len()));
                        stop_all = ui.small_button("Stop all").clicked();
                    });
                }
                ui.separator();

                egui::ScrollArea::vertical().show(ui, |ui| {
                    if self.config.macros.is_empty() {
                        ui.weak("No macros configured.");
                    }
                    for (index, macro_def) in self.config.macros.iter().enumerate() {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                let name = if macro_def.name.trim().is_empty() {
                                    "(unnamed)"
                                } else {
                                    macro_def.name.trim()
                                };
                                ui.strong(name);
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add_enabled(!editor_open, egui::Button::new("Delete"))
                                            .clicked()
                                        {
                                            remove = Some(index);
                                        }
                                        if ui
                                            .add_enabled(
                                                !editor_open,
                                                egui::Button::new("Edit macro"),
                                            )
                                            .clicked()
                                        {
                                            edit = Some(index);
                                        }
                                        if ui
                                            .add_enabled(
                                                !editor_open
                                                    && can_run
                                                    && macro_has_command(macro_def),
                                                egui::Button::new("Run"),
                                            )
                                            .clicked()
                                        {
                                            run = Some(index);
                                        }
                                    },
                                );
                            });
                            let description = if macro_def.description.trim().is_empty() {
                                "—"
                            } else {
                                macro_def.description.trim()
                            };
                            egui::Grid::new(("macro-summary", index))
                                .num_columns(2)
                                .spacing([12.0, 4.0])
                                .show(ui, |ui| {
                                    ui.label("Description");
                                    ui.label(description);
                                    ui.end_row();
                                    ui.label("Shortcut");
                                    let current = macro_def.shortcut.filter(|digit| *digit <= 9);
                                    let mut selected = current;
                                    ui.add_enabled_ui(!editor_open, |ui| {
                                        egui::ComboBox::from_id_salt((
                                            "macro-catalog-shortcut",
                                            index,
                                        ))
                                        .selected_text(shortcut_label(current))
                                        .show_ui(
                                            ui,
                                            |ui| {
                                                ui.selectable_value(
                                                    &mut selected,
                                                    None,
                                                    "Unassigned",
                                                );
                                                for digit in 0..=9 {
                                                    let owner = shortcut_owner(
                                                        &self.config.macros,
                                                        digit,
                                                        Some(index),
                                                    );
                                                    let label = owner.map_or_else(
                                                        || shortcut_label(Some(digit)),
                                                        |owner| {
                                                            format!(
                                                                "{} — used by {}",
                                                                shortcut_label(Some(digit)),
                                                                macro_display_name(
                                                                    &self.config.macros[owner]
                                                                )
                                                            )
                                                        },
                                                    );
                                                    ui.selectable_value(
                                                        &mut selected,
                                                        Some(digit),
                                                        label,
                                                    );
                                                }
                                            },
                                        );
                                    });
                                    if selected != current {
                                        if let Some(digit) = selected {
                                            if let Some(owner) = shortcut_owner(
                                                &self.config.macros,
                                                digit,
                                                Some(index),
                                            ) {
                                                shortcut_conflict = Some((index, digit, owner));
                                            } else {
                                                shortcut_change = Some((index, Some(digit)));
                                            }
                                        } else {
                                            shortcut_change = Some((index, None));
                                        }
                                    }
                                    ui.end_row();
                                });
                        });
                        ui.add_space(6.0);
                    }
                });

                if ui
                    .add_enabled(!editor_open, egui::Button::new("+ Add macro"))
                    .clicked()
                {
                    add = true;
                }
            });

        self.show_macros_win = open;
        if stop_all {
            self.macro_runs.clear();
        }
        if let Some((index, shortcut)) = shortcut_change {
            if set_macro_shortcut(&mut self.config.macros, index, shortcut) {
                self.write_config();
            }
        }
        if let Some(conflict) = shortcut_conflict {
            self.macro_shortcut_conflict = Some(conflict);
        }
        if let Some(index) = remove {
            self.config.macros.remove(index);
            self.write_config();
            if run == Some(index) {
                run = None;
            } else if run.is_some_and(|run_index| run_index > index) {
                run = run.map(|run_index| run_index - 1);
            }
        }
        if let Some(index) = edit {
            self.open_macro_editor(Some(index));
        } else if add {
            self.open_macro_editor(None);
        }
        if let Some(index) = run {
            self.start_macro(index, Instant::now());
        }
    }

    fn open_macro_editor(&mut self, index: Option<usize>) {
        let draft = index
            .and_then(|index| self.config.macros.get(index).cloned())
            .unwrap_or_else(|| TransmitMacro {
                name: "New macro".into(),
                steps: vec![MacroStep::Command {
                    text: String::new(),
                }],
                ..Default::default()
            });
        self.macro_editor = Some(MacroEditor {
            index,
            step_selection: (!draft.steps.is_empty()).then_some(0),
            draft,
        });
    }

    fn show_macro_editor(&mut self, ctx: &egui::Context) {
        let Some(mut editor) = self.macro_editor.take() else {
            return;
        };
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        let title = if editor.index.is_some() {
            "Edit macro"
        } else {
            "Add macro"
        };

        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .default_width(520.0)
            .default_height(420.0)
            .show(ctx, |ui| {
                egui::Grid::new("macro-editor-fields")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.draft.name)
                                .hint_text("Macro name")
                                .desired_width(340.0),
                        );
                        ui.end_row();

                        ui.label("Description");
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.draft.description)
                                .hint_text("What this sequence does")
                                .desired_width(340.0),
                        );
                        ui.end_row();
                    });

                ui.separator();
                show_macro_steps(ui, &mut editor);
                ui.separator();
                ui.horizontal(|ui| {
                    save = ui.button("Save").clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });

        if save {
            save_macro(&mut self.config.macros, editor);
            self.write_config();
        } else if open && !cancel {
            self.macro_editor = Some(editor);
        }
    }

    fn show_shortcut_conflict(&mut self, ctx: &egui::Context) {
        let Some((target, digit, owner)) = self.macro_shortcut_conflict else {
            return;
        };
        let owner_name = self
            .config
            .macros
            .get(owner)
            .map(macro_display_name)
            .unwrap_or_else(|| "another macro".to_owned());
        let mut move_shortcut = false;
        let mut keep_shortcut = false;
        egui::Window::new("Shortcut already used")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(format!(
                    "{} is already used by \"{owner_name}\".",
                    shortcut_label(Some(digit))
                ));
                ui.label("Move this shortcut to the selected macro?");
                ui.horizontal(|ui| {
                    move_shortcut = ui.button("Yes, move it").clicked();
                    keep_shortcut = ui.button("No").clicked();
                });
            });
        if move_shortcut {
            if transfer_macro_shortcut(&mut self.config.macros, target, digit) {
                self.write_config();
            }
            self.macro_shortcut_conflict = None;
        } else if keep_shortcut {
            self.macro_shortcut_conflict = None;
        }
    }

    fn start_macro(&mut self, index: usize, now: Instant) -> bool {
        let Some(port) = self.macro_target_port() else {
            return false;
        };
        let Some(macro_def) = self.config.macros.get(index) else {
            return false;
        };
        if !macro_has_command(macro_def) {
            return false;
        }
        self.macro_runs.push(MacroRun {
            port,
            steps: macro_def.steps.clone(),
            next_step: 0,
            next_at: now,
        });
        true
    }

    /// Advance all runs without ever sleeping the UI thread.
    pub(crate) fn maintain_macro_runs(&mut self, ctx: &egui::Context) {
        self.maintain_macro_runs_at(Instant::now(), ctx);
    }

    fn maintain_macro_runs_at(&mut self, now: Instant, ctx: &egui::Context) {
        let mut pending = Vec::with_capacity(self.macro_runs.len());
        let mut advanced_any = false;
        for mut run in std::mem::take(&mut self.macro_runs) {
            // A run waiting in a long delay should disappear as soon as its
            // tab is closed (or a failed reconnect leaves it inert), rather
            // than lingering in the UI and scheduling a useless wake at the
            // old deadline. Lost/reconnecting connections remain valid
            // targets, matching interactive transmission behavior.
            if !self
                .connections
                .iter()
                .any(|conn| conn.id == run.port && conn.state != ConnState::Closed)
            {
                advanced_any = true;
                continue;
            }
            let mut target_exists = true;
            while run.next_step < run.steps.len() && run.next_at <= now {
                let step = run.steps[run.next_step].clone();
                run.next_step += 1;
                advanced_any = true;
                match step {
                    MacroStep::Command { text } => {
                        target_exists = self.send_macro_command(run.port, &text);
                        if !target_exists {
                            break;
                        }
                    }
                    MacroStep::Delay { delay_ms } => {
                        // A trailing delay has nothing to delay. Otherwise the
                        // next step is scheduled relative to when this delay
                        // actually begins, preserving the requested pause even
                        // if the UI woke a little late.
                        if run.next_step < run.steps.len() && delay_ms > 0 {
                            run.next_at = now
                                .checked_add(Duration::from_millis(delay_ms))
                                .unwrap_or(now);
                            break;
                        }
                    }
                }
            }
            if target_exists && run.next_step < run.steps.len() {
                pending.push(run);
            }
        }
        self.macro_runs = pending;

        // Commands are sent after the console has been drawn. A follow-up
        // frame makes local echo and the completed/running state visible even
        // when the device itself produces no output to wake the UI.
        if advanced_any {
            ctx.request_repaint();
        }
        if let Some(next_at) = self.macro_runs.iter().map(|run| run.next_at).min() {
            ctx.request_repaint_after(next_at.saturating_duration_since(Instant::now()));
        }
    }

    /// Send one macro command using the same per-port line ending, local echo,
    /// history, and follow behavior as an interactively entered command.
    ///
    /// Raw console text has already reached the device one character at a time.
    /// If a macro completes that partly typed line, its command is therefore a
    /// suffix of the same real device line rather than a separate line. Commit
    /// the combined text here so local echo, history, and `tx_input` continue to
    /// describe the bytes that were actually sent.
    fn send_macro_command(&mut self, port: PortId, command: &str) -> bool {
        let now = self.clock.now();
        let Some(conn) = self
            .connections
            .iter_mut()
            .find(|conn| conn.id == port && conn.state != ConnState::Closed)
        else {
            return false;
        };

        let mut bytes = command.as_bytes().to_vec();
        bytes.extend_from_slice(conn.port_config.line_ending.bytes());
        if !bytes.is_empty() {
            conn.handle.transmit(bytes);
        }
        let mut line = std::mem::take(&mut conn.tx_input);
        line.push_str(command);
        if !line.is_empty() && conn.tx_history.last().map(String::as_str) != Some(line.as_str()) {
            conn.tx_history.push(line.clone());
        }
        conn.tx_history_pos = None;
        if conn.port_config.local_echo && !line.is_empty() {
            conn.store.append(IncomingLine {
                text: line,
                ts: now,
                port: conn.id,
                flags: LineFlags::TX_ECHO,
                spans: Default::default(),
                cursor: None,
            });
            self.merged_dirty = true;
        }
        conn.follow = true;
        conn.new_since_scroll = 0;
        if self.merged_selected {
            self.merged_follow = true;
            self.merged_new_since_scroll = 0;
        }
        true
    }
}

fn macro_has_command(macro_def: &TransmitMacro) -> bool {
    macro_def
        .steps
        .iter()
        .any(|step| matches!(step, MacroStep::Command { .. }))
}

fn show_macro_steps(ui: &mut egui::Ui, editor: &mut MacroEditor) {
    ui.horizontal(|ui| {
        ui.label("Steps");
        ui.weak("Select a step to insert after it; use ↑/↓ to rearrange.");
    });

    let mut remove_step = None;
    let mut move_step_to = None;
    let step_count = editor.draft.steps.len();
    let row_height = ui.spacing().interact_size.y;
    egui::ScrollArea::vertical()
        .max_height(250.0)
        .show(ui, |ui| {
            for (step_index, step) in editor.draft.steps.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    let selected = editor.step_selection == Some(step_index);
                    let kind = match step {
                        MacroStep::Command { .. } => "Command",
                        MacroStep::Delay { .. } => "Delay",
                    };
                    if ui
                        .add_sized(
                            [72.0, row_height],
                            egui::SelectableLabel::new(selected, kind),
                        )
                        .clicked()
                    {
                        editor.step_selection = Some(step_index);
                    }
                    match step {
                        MacroStep::Command { text } => {
                            let edit = ui.add_sized(
                                [300.0, row_height],
                                egui::TextEdit::singleline(text)
                                    .hint_text("command")
                                    .font(egui::TextStyle::Monospace),
                            );
                            if edit.clicked() {
                                editor.step_selection = Some(step_index);
                            }
                        }
                        MacroStep::Delay { delay_ms } => {
                            let edit = ui
                                .add_sized(
                                    [300.0, row_height],
                                    egui::DragValue::new(delay_ms)
                                        .range(0..=MAX_DELAY_MS)
                                        .suffix(" ms"),
                                )
                                .on_hover_text("Wait before advancing to the next step");
                            if edit.clicked() {
                                editor.step_selection = Some(step_index);
                            }
                        }
                    }
                    if ui
                        .add_enabled(step_index > 0, egui::Button::new("↑").small())
                        .on_hover_text("Move this step up")
                        .clicked()
                    {
                        move_step_to = Some((step_index, step_index - 1));
                    }
                    if ui
                        .add_enabled(step_index + 1 < step_count, egui::Button::new("↓").small())
                        .on_hover_text("Move this step down")
                        .clicked()
                    {
                        move_step_to = Some((step_index, step_index + 1));
                    }
                    if ui.small_button("−").clicked() {
                        remove_step = Some(step_index);
                    }
                });
            }
        });

    if let Some(step_index) = remove_step {
        editor.draft.steps.remove(step_index);
        editor.step_selection = selection_after_remove(editor.step_selection, step_index);
    } else if let Some((from, requested_destination)) = move_step_to {
        let destination = move_step(&mut editor.draft.steps, from, requested_destination);
        editor.step_selection = selection_after_move(editor.step_selection, from, destination);
    }

    let selected_step = editor
        .step_selection
        .filter(|step_index| editor.draft.steps.get(*step_index).is_some());
    ui.horizontal(|ui| {
        if ui.small_button("+ Add command").clicked() {
            let insert_at =
                selected_step.map_or(editor.draft.steps.len(), |step_index| step_index + 1);
            editor.draft.steps.insert(
                insert_at,
                MacroStep::Command {
                    text: String::new(),
                },
            );
            editor.step_selection = Some(insert_at);
        }
        let add_delay = ui
            .add(egui::Button::new("+ Add delay").small())
            .on_hover_text("Insert after the selected step, or append when none is selected");
        if add_delay.clicked() {
            let insert_at =
                selected_step.map_or(editor.draft.steps.len(), |step_index| step_index + 1);
            editor.draft.steps.insert(
                insert_at,
                MacroStep::Delay {
                    delay_ms: DEFAULT_DELAY_MS,
                },
            );
            editor.step_selection = Some(insert_at);
        }
    });
}

/// Consume a shortcut by its digit key even when Shift changes its logical key.
///
/// Winit reports, for example, Shift+2 as `Quote` on several keyboard layouts,
/// while retaining `Num2` as the physical key. `InputState::consume_key` checks
/// only the logical key, so it would neither recognize nor consume that event.
fn consume_macro_key(
    ctx: &egui::Context,
    modifiers: egui::Modifiers,
    digit_key: egui::Key,
) -> bool {
    ctx.input_mut(|input| {
        let matches = |event: &egui::Event, allow_repeat: bool| {
            matches!(
                event,
                egui::Event::Key {
                    key,
                    physical_key,
                    pressed: true,
                    repeat,
                    modifiers: event_modifiers,
                } if (allow_repeat || !repeat)
                    && (*key == digit_key || *physical_key == Some(digit_key))
                    && event_modifiers.matches_logically(modifiers)
            )
        };
        let pressed = input.events.iter().any(|event| matches(event, false));
        if pressed {
            // Remove repeats from the same input batch too, matching
            // `InputState::consume_key`'s behavior.
            input.events.retain(|event| !matches(event, true));
        }
        pressed
    })
}

/// Move `from` to the requested final index, returning the clamped index used.
fn move_step(steps: &mut Vec<MacroStep>, from: usize, destination: usize) -> usize {
    if from >= steps.len() {
        return from;
    }
    let step = steps.remove(from);
    let destination = destination.min(steps.len());
    steps.insert(destination, step);
    destination
}

fn selection_after_remove(selection: Option<usize>, removed: usize) -> Option<usize> {
    selection.and_then(|selected_step| {
        if selected_step < removed {
            Some(selected_step)
        } else if selected_step == removed {
            None
        } else {
            Some(selected_step - 1)
        }
    })
}

fn selection_after_move(
    selection: Option<usize>,
    from: usize,
    destination: usize,
) -> Option<usize> {
    selection.map(|selected_step| {
        if selected_step == from {
            destination
        } else if from < destination && selected_step > from && selected_step <= destination {
            selected_step - 1
        } else if destination < from && selected_step >= destination && selected_step < from {
            selected_step + 1
        } else {
            selected_step
        }
    })
}

fn shortcut_label(shortcut: Option<u8>) -> String {
    shortcut.map_or_else(
        || "Unassigned".to_owned(),
        |digit| format!("Ctrl+Shift+{digit}"),
    )
}

fn macro_display_name(macro_def: &TransmitMacro) -> String {
    let name = macro_def.name.trim();
    if name.is_empty() {
        "(unnamed)".to_owned()
    } else {
        name.to_owned()
    }
}

fn shortcut_owner(
    macros: &[TransmitMacro],
    digit: u8,
    edited_index: Option<usize>,
) -> Option<usize> {
    macros
        .iter()
        .enumerate()
        .find(|(index, macro_def)| {
            Some(*index) != edited_index && macro_def.shortcut == Some(digit)
        })
        .map(|(index, _)| index)
}

fn save_macro(macros: &mut Vec<TransmitMacro>, mut editor: MacroEditor) -> usize {
    let target = editor.index.filter(|index| *index < macros.len());
    // Shortcut ownership belongs to the catalog, never the definition editor.
    editor.draft.shortcut = target.and_then(|index| macros[index].shortcut);
    if let Some(index) = target {
        macros[index] = editor.draft;
        index
    } else {
        macros.push(editor.draft);
        macros.len() - 1
    }
}

fn set_macro_shortcut(macros: &mut [TransmitMacro], index: usize, shortcut: Option<u8>) -> bool {
    let Some(macro_def) = macros.get(index) else {
        return false;
    };
    if macro_def.shortcut == shortcut
        || shortcut
            .is_some_and(|digit| digit > 9 || shortcut_owner(macros, digit, Some(index)).is_some())
    {
        return false;
    }
    macros[index].shortcut = shortcut;
    true
}

fn transfer_macro_shortcut(macros: &mut [TransmitMacro], target: usize, digit: u8) -> bool {
    if target >= macros.len() || digit > 9 {
        return false;
    }
    let changed = macros[target].shortcut != Some(digit)
        || macros
            .iter()
            .enumerate()
            .any(|(index, macro_def)| index != target && macro_def.shortcut == Some(digit));
    for (index, macro_def) in macros.iter_mut().enumerate() {
        if index != target && macro_def.shortcut == Some(digit) {
            macro_def.shortcut = None;
        }
    }
    macros[target].shortcut = Some(digit);
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{inert_handle, test_app};
    use serialcore::config::{LineEnding, PortConfig};

    fn app_with_macro(delay_ms: u64) -> App {
        let (mut app, _enum_tx) = test_app("transmit-macro");
        app.config.macros.push(TransmitMacro {
            name: "Setup".into(),
            description: "Prepare target".into(),
            steps: vec![
                MacroStep::Command {
                    text: "first".into(),
                },
                MacroStep::Delay { delay_ms },
                MacroStep::Command {
                    text: "second".into(),
                },
            ],
            shortcut: Some(7),
        });
        let id = PortId(0);
        let conn = app.make_connection(
            id,
            "probe".into(),
            Default::default(),
            PortConfig {
                line_ending: LineEnding::CrLf,
                local_echo: true,
                ..Default::default()
            },
            inert_handle(id),
        );
        app.connections.push(conn);
        app
    }

    fn echoed(app: &App) -> Vec<String> {
        let conn = &app.connections[0];
        (conn.store.first_abs_index()..conn.store.next_abs_index())
            .filter_map(|index| conn.store.get(index))
            .filter(|line| line.meta.flags.contains(LineFlags::TX_ECHO))
            .map(|line| line.text.to_string())
            .collect()
    }

    #[test]
    fn macro_commands_wait_for_the_configured_delay() {
        let mut app = app_with_macro(100);
        let ctx = egui::Context::default();
        let started = Instant::now();
        assert!(app.start_macro(0, started));

        app.maintain_macro_runs_at(started, &ctx);
        assert_eq!(echoed(&app), ["first"]);
        assert_eq!(app.macro_runs.len(), 1);

        app.maintain_macro_runs_at(started + Duration::from_millis(99), &ctx);
        assert_eq!(echoed(&app), ["first"]);

        app.maintain_macro_runs_at(started + Duration::from_millis(100), &ctx);
        assert_eq!(echoed(&app), ["first", "second"]);
        assert!(app.macro_runs.is_empty());
        assert_eq!(app.connections[0].tx_history, ["first", "second"]);
    }

    #[test]
    fn each_delay_step_controls_only_what_follows_it() {
        let mut app = app_with_macro(100);
        app.config.macros[0].steps.extend([
            MacroStep::Delay { delay_ms: 250 },
            MacroStep::Command {
                text: "third".into(),
            },
        ]);
        let ctx = egui::Context::default();
        let started = Instant::now();
        app.start_macro(0, started);

        app.maintain_macro_runs_at(started, &ctx);
        app.maintain_macro_runs_at(started + Duration::from_millis(100), &ctx);
        assert_eq!(echoed(&app), ["first", "second"]);

        app.maintain_macro_runs_at(started + Duration::from_millis(349), &ctx);
        assert_eq!(echoed(&app), ["first", "second"]);
        app.maintain_macro_runs_at(started + Duration::from_millis(350), &ctx);
        assert_eq!(echoed(&app), ["first", "second", "third"]);
    }

    #[test]
    fn macro_command_commits_a_partly_typed_console_line_consistently() {
        let mut app = app_with_macro(100);
        // Raw console input is transmitted as it is typed, so this prefix is
        // already present on the device before the macro starts.
        app.connections[0].tx_input = "prefix-".into();
        let started = Instant::now();
        app.start_macro(0, started);

        app.maintain_macro_runs_at(started, &egui::Context::default());

        assert_eq!(echoed(&app), ["prefix-first"]);
        assert_eq!(app.connections[0].tx_history, ["prefix-first"]);
        assert!(app.connections[0].tx_input.is_empty());
    }

    #[test]
    fn command_and_delay_steps_can_be_rearranged() {
        let mut steps = vec![
            MacroStep::Command { text: "a".into() },
            MacroStep::Delay { delay_ms: 50 },
            MacroStep::Command { text: "b".into() },
        ];

        let destination = move_step(&mut steps, 2, 1);

        assert_eq!(destination, 1);
        assert_eq!(
            steps,
            [
                MacroStep::Command { text: "a".into() },
                MacroStep::Command { text: "b".into() },
                MacroStep::Delay { delay_ms: 50 },
            ]
        );
        assert_eq!(selection_after_move(Some(2), 2, 1), Some(1));
    }

    #[test]
    fn assigned_ctrl_shift_digit_starts_and_consumes_a_macro() {
        let mut app = app_with_macro(100);
        let ctx = egui::Context::default();
        let event = egui::Event::Key {
            key: egui::Key::Num7,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
        };
        ctx.begin_pass(egui::RawInput {
            events: vec![event],
            ..Default::default()
        });

        app.consume_macro_shortcut(&ctx);

        assert_eq!(app.macro_runs.len(), 1);
        assert!(ctx.input(|input| input.events.is_empty()));
        let _ = ctx.end_pass();
    }

    #[test]
    fn shortcut_uses_the_physical_digit_when_shift_changes_the_logical_key() {
        let mut app = app_with_macro(100);
        app.config.macros[0].shortcut = Some(2);
        let ctx = egui::Context::default();
        // On layouts including Swedish and German, Shift+2 is a quote. Winit
        // exposes that as the logical key while preserving the digit as the
        // physical key.
        let event = egui::Event::Key {
            key: egui::Key::Quote,
            physical_key: Some(egui::Key::Num2),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
        };
        ctx.begin_pass(egui::RawInput {
            events: vec![event],
            ..Default::default()
        });

        app.consume_macro_shortcut(&ctx);

        assert_eq!(app.macro_runs.len(), 1);
        assert!(ctx.input(|input| input.events.is_empty()));
        let _ = ctx.end_pass();
    }

    #[test]
    fn confirming_a_conflict_moves_the_occupied_shortcut() {
        let mut app = app_with_macro(100);
        app.config.macros.push(TransmitMacro {
            name: "Second".into(),
            ..Default::default()
        });

        assert_eq!(shortcut_owner(&app.config.macros, 7, Some(1)), Some(0));
        assert!(transfer_macro_shortcut(&mut app.config.macros, 1, 7));

        assert_eq!(app.config.macros[0].shortcut, None);
        assert_eq!(app.config.macros[1].shortcut, Some(7));
    }

    #[test]
    fn editing_uses_a_draft_until_save() {
        let mut app = app_with_macro(100);
        app.open_macro_editor(Some(0));
        app.macro_editor.as_mut().unwrap().draft.name = "Changed".into();

        assert_eq!(app.config.macros[0].name, "Setup");

        let editor = app.macro_editor.take().unwrap();
        save_macro(&mut app.config.macros, editor);
        assert_eq!(app.config.macros[0].name, "Changed");
    }

    #[test]
    fn definition_editor_cannot_change_a_shortcut() {
        let mut app = app_with_macro(100);
        let editor = MacroEditor {
            index: Some(0),
            draft: TransmitMacro {
                name: "Changed".into(),
                shortcut: Some(2),
                ..Default::default()
            },
            step_selection: None,
        };

        assert_eq!(save_macro(&mut app.config.macros, editor), 0);
        assert_eq!(app.config.macros[0].name, "Changed");
        assert_eq!(app.config.macros[0].shortcut, Some(7));
    }

    #[test]
    fn a_running_macro_keeps_its_original_target() {
        let mut app = app_with_macro(100);
        let second_id = PortId(1);
        let second = app.make_connection(
            second_id,
            "other".into(),
            Default::default(),
            PortConfig {
                local_echo: true,
                ..Default::default()
            },
            inert_handle(second_id),
        );
        app.connections.push(second);
        let started = Instant::now();
        app.start_macro(0, started);
        app.maintain_macro_runs_at(started, &egui::Context::default());

        app.active = 1;
        app.maintain_macro_runs_at(
            started + Duration::from_millis(100),
            &egui::Context::default(),
        );

        assert_eq!(echoed(&app), ["first", "second"]);
        assert!(app.connections[1].store.is_empty());
    }

    #[test]
    fn a_delayed_macro_stops_as_soon_as_its_target_closes() {
        let mut app = app_with_macro(60_000);
        let started = Instant::now();
        app.start_macro(0, started);
        app.maintain_macro_runs_at(started, &egui::Context::default());
        assert_eq!(app.macro_runs.len(), 1);

        app.connections[0].state = ConnState::Closed;
        app.maintain_macro_runs_at(
            started + Duration::from_millis(1),
            &egui::Context::default(),
        );

        assert!(app.macro_runs.is_empty());
        assert_eq!(echoed(&app), ["first"]);
    }
}
