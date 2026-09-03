//! User-defined transmit macros and their non-blocking execution scheduler.

use crate::app::{App, Connection, MacroEditor, MacroRun, MacroWait};
use regex::Regex;
use serialcore::config::{MacroStep, TransmitMacro};
use serialcore::reader::ConnState;
use serialcore::store::{IncomingLine, LineFlags, PortId};
use std::time::{Duration, Instant};

const MAX_DELAY_MS: u64 = 3_600_000;
const DEFAULT_DELAY_MS: u64 = 100;
const MACRO_INDICATOR_DELAY: Duration = Duration::from_millis(500);
const MAX_MACRO_REPEAT_COUNT: u32 = 1_000_000;

impl App {
    /// The connection a macro started right now should keep targeting.
    ///
    /// In the merged view this is the explicit "Send to" device. A run stores
    /// its port id so switching tabs while it is delayed does not redirect the
    /// remaining commands to a different device.
    pub(crate) fn macro_target_port(&self) -> Option<PortId> {
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
        let assignments: [Option<usize>; 10] = std::array::from_fn(|digit| {
            self.config
                .macros
                .iter()
                .position(|macro_def| macro_def.shortcut == Some(digit as u8))
        });
        let selected = consume_macro_keys(ctx, modifiers, &keys, &assignments);
        let now = Instant::now();
        for index in selected {
            self.start_macro(index, now);
        }
    }

    pub(crate) fn show_macros_window(&mut self, ctx: &egui::Context) {
        if self.show_macros_win {
            self.show_macro_catalog(ctx);
        }
        self.show_shortcut_conflict(ctx);
        self.show_running_macro_edit_confirmation(ctx);
        self.show_macro_editor(ctx);
    }

    /// Read-only macro catalog. Definitions are changed only in the separate
    /// add/edit window so an accidental click cannot rewrite configuration.
    fn show_macro_catalog(&mut self, ctx: &egui::Context) {
        let mut open = self.show_macros_win;
        let target_port = self.macro_target_port();
        let can_run = target_port.is_some();
        // Confirmation windows retain catalog indices. Prevent edits behind
        // them from moving those indices before the user answers.
        let definition_dialog_open = self.macro_editor.is_some()
            || self.macro_running_edit_confirmation.is_some()
            || self.macro_shortcut_conflict.is_some();
        let mut run = None;
        let mut stop = None;
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
                ui.separator();

                egui::ScrollArea::vertical().show(ui, |ui| {
                    if self.config.macros.is_empty() {
                        ui.weak("No macros configured.");
                    }
                    for (index, macro_def) in self.config.macros.iter().enumerate() {
                        let running = self.macro_runs.iter().any(|run| {
                            run.macro_index == Some(index) && Some(run.port) == target_port
                        });
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
                                            .add_enabled(
                                                !definition_dialog_open,
                                                egui::Button::new("Delete"),
                                            )
                                            .clicked()
                                        {
                                            remove = Some(index);
                                        }
                                        if ui
                                            .add_enabled(
                                                !definition_dialog_open,
                                                egui::Button::new("Edit macro"),
                                            )
                                            .clicked()
                                        {
                                            edit = Some(index);
                                        }
                                        let can_start = !definition_dialog_open
                                            && can_run
                                            && macro_has_command(macro_def);
                                        if ui
                                            .add_enabled(
                                                running || can_start,
                                                egui::Button::new(if running {
                                                    "Stop"
                                                } else {
                                                    "Run"
                                                }),
                                            )
                                            .clicked()
                                        {
                                            if running {
                                                if let Some(port) = target_port {
                                                    stop = Some((index, port));
                                                }
                                            } else {
                                                run = Some(index);
                                            }
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
                                    ui.add_enabled_ui(!definition_dialog_open, |ui| {
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
                                    ui.label("Runs");
                                    ui.label(macro_runs_label(macro_def));
                                    ui.end_row();
                                });
                        });
                        ui.add_space(6.0);
                    }
                });

                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!definition_dialog_open, egui::Button::new("+ Add macro"))
                        .clicked()
                    {
                        add = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !self.macro_runs.is_empty() {
                            stop_all = ui.button("Stop all").clicked();
                            ui.label(format!("{} running", self.macro_runs.len()));
                        }
                    });
                });
            });

        self.show_macros_win = open;
        if stop_all {
            self.macro_runs.clear();
        }
        if let Some((index, port)) = stop {
            self.stop_macro_on_port(index, port);
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
            for running in &mut self.macro_runs {
                match running.macro_index {
                    Some(running_index) if running_index == index => running.macro_index = None,
                    Some(running_index) if running_index > index => {
                        running.macro_index = Some(running_index - 1);
                    }
                    _ => {}
                }
            }
            self.write_config();
            if run == Some(index) {
                run = None;
            } else if run.is_some_and(|run_index| run_index > index) {
                run = run.map(|run_index| run_index - 1);
            }
        }
        if let Some(index) = edit {
            if self
                .macro_runs
                .iter()
                .any(|run| run.macro_index == Some(index))
            {
                self.macro_running_edit_confirmation = Some(index);
            } else {
                self.open_macro_editor(Some(index));
            }
        } else if add {
            self.open_macro_editor(None);
        }
        if let Some(index) = run {
            self.start_macro(index, Instant::now());
        }
    }

    fn show_running_macro_edit_confirmation(&mut self, ctx: &egui::Context) {
        let Some(index) = self.macro_running_edit_confirmation else {
            return;
        };
        let Some(macro_def) = self.config.macros.get(index) else {
            self.macro_running_edit_confirmation = None;
            return;
        };
        let name = macro_display_name(macro_def);
        let running_count = self
            .macro_runs
            .iter()
            .filter(|run| run.macro_index == Some(index))
            .count();
        if running_count == 0 {
            self.macro_running_edit_confirmation = None;
            self.open_macro_editor(Some(index));
            return;
        }

        let mut open = true;
        let mut continue_editing = false;
        let mut cancel = false;
        egui::Window::new("Macro is running")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                let terminals = if running_count == 1 {
                    "terminal"
                } else {
                    "terminals"
                };
                ui.label(format!(
                    "\"{name}\" is running on {running_count} {terminals}."
                ));
                ui.label("Continuing will stop it on every terminal before editing.");
                ui.horizontal(|ui| {
                    continue_editing = ui.button("Stop and edit").clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });

        if continue_editing {
            self.stop_macro(index);
            self.macro_running_edit_confirmation = None;
            self.open_macro_editor(Some(index));
        } else if cancel || !open {
            self.macro_running_edit_confirmation = None;
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

                        ui.label("Runs");
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut editor.draft.repeat_indefinitely, false, "Run");
                            ui.add_enabled(
                                !editor.draft.repeat_indefinitely,
                                egui::DragValue::new(&mut editor.draft.repeat_count)
                                    .range(1..=MAX_MACRO_REPEAT_COUNT)
                                    .suffix(" times"),
                            );
                            ui.radio_value(
                                &mut editor.draft.repeat_indefinitely,
                                true,
                                "Indefinitely",
                            );
                        });
                        ui.end_row();
                    });

                ui.separator();
                show_macro_steps(ui, &mut editor);
                ui.separator();
                let wait_patterns_valid = editor.draft.steps.iter().all(|step| match step {
                    MacroStep::WaitFor { pattern } => Regex::new(pattern).is_ok(),
                    _ => true,
                });
                ui.horizontal(|ui| {
                    save = ui
                        .add_enabled(wait_patterns_valid, egui::Button::new("Save"))
                        .clicked();
                    cancel = ui.button("Cancel").clicked();
                    if !wait_patterns_valid {
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            "Fix invalid wait-for expressions before saving.",
                        );
                    }
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
        if self
            .macro_runs
            .iter()
            .any(|run| run.macro_index == Some(index) && run.port == port)
        {
            return false;
        }
        let Some(macro_def) = self.config.macros.get(index) else {
            return false;
        };
        if !macro_has_command(macro_def) {
            return false;
        }
        self.macro_runs.push(MacroRun {
            macro_index: Some(index),
            name: macro_display_name(macro_def),
            started_at: now,
            repetitions_remaining: (!macro_def.repeat_indefinitely)
                .then_some(macro_def.repeat_count.max(1) - 1),
            port,
            steps: macro_def.steps.clone(),
            next_step: 0,
            next_at: now,
            wait_for: None,
        });
        true
    }

    pub(crate) fn long_running_macro_indicators(
        &self,
        now: Instant,
        port: Option<PortId>,
    ) -> Vec<(usize, String)> {
        self.macro_runs
            .iter()
            .enumerate()
            .filter(|(_, run)| {
                Some(run.port) == port
                    && now.saturating_duration_since(run.started_at) >= MACRO_INDICATOR_DELAY
            })
            .map(|(run_index, run)| (run_index, run.name.clone()))
            .collect()
    }

    pub(crate) fn stop_macro_run(&mut self, run_index: usize) {
        if run_index < self.macro_runs.len() {
            self.macro_runs.remove(run_index);
        }
    }

    fn stop_macro(&mut self, macro_index: usize) {
        self.macro_runs
            .retain(|run| run.macro_index != Some(macro_index));
    }

    fn stop_macro_on_port(&mut self, macro_index: usize, port: PortId) {
        self.macro_runs
            .retain(|run| run.macro_index != Some(macro_index) || run.port != port);
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

            if let Some(wait) = &run.wait_for {
                let matched = self
                    .connections
                    .iter()
                    .find(|conn| conn.id == run.port)
                    .is_some_and(|conn| macro_wait_matches(conn, wait));
                if !matched {
                    pending.push(run);
                    continue;
                }
                run.wait_for = None;
                run.next_at = now;
                advanced_any = true;
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
                        // A trailing delay applies between loop iterations;
                        // without a loop it has nothing to delay. The next step
                        // is scheduled relative to when this delay actually
                        // begins, preserving the requested pause even if the UI
                        // woke a little late.
                        if (run.next_step < run.steps.len() || macro_run_will_repeat(&run))
                            && delay_ms > 0
                        {
                            run.next_at = now
                                .checked_add(Duration::from_millis(delay_ms))
                                .unwrap_or(now);
                            break;
                        }
                    }
                    MacroStep::WaitFor { pattern } => {
                        let Some(conn) = self.connections.iter().find(|conn| conn.id == run.port)
                        else {
                            target_exists = false;
                            break;
                        };
                        let Ok(regex) = Regex::new(&pattern) else {
                            // The editor prevents invalid expressions from
                            // being saved. A hand-edited config should stop the
                            // run instead of leaving it stuck forever.
                            target_exists = false;
                            break;
                        };
                        let wait = MacroWait {
                            raw_start: conn.raw_next(),
                            regex,
                        };
                        if wait.regex.is_match("") {
                            continue;
                        }
                        run.wait_for = Some(wait);
                        break;
                    }
                }
            }
            if target_exists {
                if run.wait_for.is_some() {
                    // A wait remains part of the current execution even when it
                    // is the final step. Do not complete (or begin the next
                    // repetition) until its receive condition has matched.
                    pending.push(run);
                } else if run.next_step >= run.steps.len() && macro_run_will_repeat(&run) {
                    if let Some(remaining) = &mut run.repetitions_remaining {
                        *remaining -= 1;
                    }
                    run.next_step = 0;
                    // Continue on a fresh frame. This prevents an indefinitely
                    // looping macro with no delay or wait step from locking the
                    // UI in this scheduler call.
                    pending.push(run);
                } else if run.next_step < run.steps.len() {
                    pending.push(run);
                }
            }
        }
        self.macro_runs = pending;

        // Commands are sent after the console has been drawn. A follow-up
        // frame makes local echo and the completed/running state visible even
        // when the device itself produces no output to wake the UI.
        if advanced_any {
            ctx.request_repaint();
        }
        if let Some(next_at) = self
            .macro_runs
            .iter()
            .filter(|run| run.wait_for.is_none())
            .map(|run| run.next_at)
            .min()
        {
            ctx.request_repaint_after(next_at.saturating_duration_since(Instant::now()));
        }
        // A receive wait has no timer or animation of its own, but its footer
        // indicator must still appear once the run reaches its threshold on an
        // otherwise silent connection.
        for indicator_at in self.macro_runs.iter().filter_map(|run| {
            (now.saturating_duration_since(run.started_at) < MACRO_INDICATOR_DELAY)
                .then(|| run.started_at.checked_add(MACRO_INDICATOR_DELAY))
                .flatten()
        }) {
            ctx.request_repaint_after(indicator_at.saturating_duration_since(Instant::now()));
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

/// Match only bytes received since this wait began. The raw ring is bounded;
/// if a very long wait outlives retained history, the surviving suffix is used.
/// Converting the full suffix at once also preserves UTF-8 characters split
/// across reader batches.
fn macro_wait_matches(conn: &Connection, wait: &MacroWait) -> bool {
    let start = wait.raw_start.max(conn.raw_base);
    let skip = usize::try_from(start.saturating_sub(conn.raw_base)).unwrap_or(usize::MAX);
    let bytes: Vec<u8> = conn.raw_ring.iter().skip(skip).copied().collect();
    wait.regex.is_match(&String::from_utf8_lossy(&bytes))
}

fn macro_has_command(macro_def: &TransmitMacro) -> bool {
    macro_def
        .steps
        .iter()
        .any(|step| matches!(step, MacroStep::Command { .. }))
}

fn macro_run_will_repeat(run: &MacroRun) -> bool {
    run.repetitions_remaining != Some(0)
}

fn macro_runs_label(macro_def: &TransmitMacro) -> String {
    if macro_def.repeat_indefinitely {
        "Indefinitely".to_owned()
    } else {
        match macro_def.repeat_count.max(1) {
            1 => "Once".to_owned(),
            count => format!("{count} times"),
        }
    }
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
                        MacroStep::WaitFor { .. } => "Wait for",
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
                        MacroStep::WaitFor { pattern } => {
                            let error = Regex::new(pattern).err().map(|error| error.to_string());
                            let edit = ui.add_sized(
                                [300.0, row_height],
                                egui::TextEdit::singleline(pattern)
                                    .hint_text("regular expression")
                                    .font(egui::TextStyle::Monospace),
                            );
                            if edit.clicked() {
                                editor.step_selection = Some(step_index);
                            }
                            if let Some(error) = error {
                                ui.colored_label(ui.visuals().error_fg_color, "⚠")
                                    .on_hover_text(format!("Invalid regular expression: {error}"));
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
                    if ui
                        .small_button("−")
                        .on_hover_text("Remove this step")
                        .clicked()
                    {
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
        let add_wait = ui
            .add(egui::Button::new("+ Add wait for").small())
            .on_hover_text("Wait for newly received serial data to match a regular expression");
        if add_wait.clicked() {
            let insert_at =
                selected_step.map_or(editor.draft.steps.len(), |step_index| step_index + 1);
            editor.draft.steps.insert(
                insert_at,
                MacroStep::WaitFor {
                    pattern: String::new(),
                },
            );
            editor.step_selection = Some(insert_at);
        }
    });
}

/// Consume assigned shortcuts in event order even when Shift changes their
/// logical keys. Repeat events are consumed without starting another run.
///
/// Winit reports, for example, Shift+2 as `Quote` on several keyboard layouts,
/// while retaining `Num2` as the physical key. `InputState::consume_key` checks
/// only the logical key, so it would neither recognize nor consume that event.
fn consume_macro_keys(
    ctx: &egui::Context,
    modifiers: egui::Modifiers,
    digit_keys: &[egui::Key; 10],
    assignments: &[Option<usize>; 10],
) -> Vec<usize> {
    ctx.input_mut(|input| {
        let mut selected = Vec::new();
        input.events.retain(|event| {
            let egui::Event::Key {
                key,
                physical_key,
                pressed: true,
                repeat,
                modifiers: event_modifiers,
            } = event
            else {
                return true;
            };
            if !event_modifiers.matches_logically(modifiers) {
                return true;
            }
            let digit = physical_key
                .as_ref()
                .and_then(|key| digit_keys.iter().position(|candidate| candidate == key))
                .or_else(|| digit_keys.iter().position(|candidate| candidate == key));
            let Some(index) = digit.and_then(|digit| assignments[digit]) else {
                return true;
            };
            if !repeat {
                selected.push(index);
            }
            false
        });
        selected
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
            ..Default::default()
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
    fn a_macro_cannot_be_started_twice_and_can_be_stopped_by_its_run() {
        let mut app = app_with_macro(60_000);
        let started = Instant::now();
        assert!(app.start_macro(0, started));
        assert!(!app.start_macro(0, started));
        assert_eq!(app.macro_runs.len(), 1);

        assert!(app
            .long_running_macro_indicators(started + Duration::from_millis(499), Some(PortId(0)),)
            .is_empty());
        let indicators = app
            .long_running_macro_indicators(started + Duration::from_millis(500), Some(PortId(0)));
        assert_eq!(indicators, [(0, "Setup".to_owned())]);

        app.stop_macro_run(indicators[0].0);
        assert!(app.macro_runs.is_empty());
    }

    #[test]
    fn the_same_macro_can_run_once_on_each_terminal() {
        let mut app = app_with_macro(60_000);
        let second_id = PortId(1);
        let second = app.make_connection(
            second_id,
            "other".into(),
            Default::default(),
            PortConfig::default(),
            inert_handle(second_id),
        );
        app.connections.push(second);
        let started = Instant::now();

        assert!(app.start_macro(0, started));
        app.active = 1;
        assert!(app.start_macro(0, started));
        assert!(!app.start_macro(0, started));
        assert_eq!(app.macro_runs.len(), 2);

        let shown_at = started + MACRO_INDICATOR_DELAY;
        assert_eq!(
            app.long_running_macro_indicators(shown_at, Some(PortId(0)))
                .len(),
            1
        );
        assert_eq!(
            app.long_running_macro_indicators(shown_at, Some(second_id))
                .len(),
            1
        );

        app.stop_macro_on_port(0, second_id);
        assert_eq!(app.macro_runs.len(), 1);
        assert_eq!(app.macro_runs[0].port, PortId(0));
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
    fn a_finite_loop_runs_the_macro_the_requested_number_of_times() {
        let mut app = app_with_macro(100);
        app.config.macros[0].steps = vec![MacroStep::Command {
            text: "again".into(),
        }];
        app.config.macros[0].repeat_count = 3;
        let started = Instant::now();
        app.start_macro(0, started);

        app.maintain_macro_runs_at(started, &egui::Context::default());
        assert_eq!(echoed(&app), ["again"]);
        app.maintain_macro_runs_at(started, &egui::Context::default());
        assert_eq!(echoed(&app), ["again", "again"]);
        app.maintain_macro_runs_at(started, &egui::Context::default());
        assert_eq!(echoed(&app), ["again", "again", "again"]);
        assert!(app.macro_runs.is_empty());
    }

    #[test]
    fn an_indefinite_loop_runs_until_stopped() {
        let mut app = app_with_macro(100);
        app.config.macros[0].steps = vec![MacroStep::Command {
            text: "again".into(),
        }];
        app.config.macros[0].repeat_indefinitely = true;
        let started = Instant::now();
        app.start_macro(0, started);

        for _ in 0..3 {
            app.maintain_macro_runs_at(started, &egui::Context::default());
        }
        assert_eq!(echoed(&app), ["again", "again", "again"]);
        assert_eq!(app.macro_runs.len(), 1);

        app.stop_macro(0);
        assert!(app.macro_runs.is_empty());
    }

    #[test]
    fn a_trailing_delay_is_preserved_between_loop_iterations() {
        let mut app = app_with_macro(100);
        app.config.macros[0].steps = vec![
            MacroStep::Command {
                text: "again".into(),
            },
            MacroStep::Delay { delay_ms: 100 },
        ];
        app.config.macros[0].repeat_count = 2;
        let started = Instant::now();
        app.start_macro(0, started);

        app.maintain_macro_runs_at(started, &egui::Context::default());
        app.maintain_macro_runs_at(
            started + Duration::from_millis(99),
            &egui::Context::default(),
        );
        assert_eq!(echoed(&app), ["again"]);
        app.maintain_macro_runs_at(
            started + Duration::from_millis(100),
            &egui::Context::default(),
        );
        assert_eq!(echoed(&app), ["again", "again"]);
        assert!(app.macro_runs.is_empty());
    }

    #[test]
    fn wait_for_matches_only_new_bus_data_and_can_span_batches() {
        let mut app = app_with_macro(100);
        app.config.macros[0].steps = vec![
            MacroStep::Command {
                text: "reboot".into(),
            },
            MacroStep::WaitFor {
                pattern: r"BOOT\s+OK".into(),
            },
            MacroStep::Command {
                text: "status".into(),
            },
        ];
        // Identical output from before the command must not release the wait.
        app.connections[0].push_raw_bytes(b"BOOT OK\r\n");
        let started = Instant::now();
        app.start_macro(0, started);
        app.maintain_macro_runs_at(started, &egui::Context::default());
        assert_eq!(echoed(&app), ["reboot"]);
        assert!(app.macro_runs[0].wait_for.is_some());

        app.connections[0].push_raw_bytes(b"BO");
        app.maintain_macro_runs_at(started, &egui::Context::default());
        assert_eq!(echoed(&app), ["reboot"]);

        app.connections[0].push_raw_bytes(b"OT OK\r\n");
        app.maintain_macro_runs_at(started, &egui::Context::default());
        assert_eq!(echoed(&app), ["reboot", "status"]);
        assert!(app.macro_runs.is_empty());
    }

    #[test]
    fn a_final_wait_for_keeps_the_macro_running_until_it_matches() {
        let mut app = app_with_macro(100);
        app.config.macros[0].steps = vec![
            MacroStep::Command {
                text: "reboot".into(),
            },
            MacroStep::WaitFor {
                pattern: "READY".into(),
            },
        ];
        let started = Instant::now();
        app.start_macro(0, started);

        app.maintain_macro_runs_at(started, &egui::Context::default());

        assert_eq!(echoed(&app), ["reboot"]);
        assert_eq!(app.macro_runs.len(), 1);
        assert!(app.macro_runs[0].wait_for.is_some());

        app.connections[0].push_raw_bytes(b"READY\r\n");
        app.maintain_macro_runs_at(started, &egui::Context::default());

        assert!(app.macro_runs.is_empty());
    }

    #[test]
    fn invalid_wait_for_expression_stops_a_hand_edited_macro() {
        let mut app = app_with_macro(100);
        app.config.macros[0].steps = vec![
            MacroStep::Command {
                text: "first".into(),
            },
            MacroStep::WaitFor {
                pattern: "(".into(),
            },
            MacroStep::Command {
                text: "second".into(),
            },
        ];
        let started = Instant::now();
        app.start_macro(0, started);

        app.maintain_macro_runs_at(started, &egui::Context::default());

        assert_eq!(echoed(&app), ["first"]);
        assert!(app.macro_runs.is_empty());
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
    fn every_assigned_shortcut_in_one_input_batch_is_consumed_and_started() {
        let mut app = app_with_macro(100);
        app.config.macros.push(TransmitMacro {
            name: "Second".into(),
            steps: vec![MacroStep::Command {
                text: "other".into(),
            }],
            shortcut: Some(2),
            ..Default::default()
        });
        let ctx = egui::Context::default();
        let key_event = |key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
        };
        ctx.begin_pass(egui::RawInput {
            events: vec![key_event(egui::Key::Num7), key_event(egui::Key::Num2)],
            ..Default::default()
        });

        app.consume_macro_shortcut(&ctx);

        assert_eq!(app.macro_runs.len(), 2);
        assert_eq!(
            app.macro_runs
                .iter()
                .map(|run| run.macro_index)
                .collect::<Vec<_>>(),
            [Some(0), Some(1)]
        );
        assert!(ctx.input(|input| input.events.is_empty()));
        let _ = ctx.end_pass();
    }

    #[test]
    fn shortcut_key_repeats_are_consumed_without_restarting_the_macro() {
        let mut app = app_with_macro(100);
        let ctx = egui::Context::default();
        let shortcut = egui::Event::Key {
            key: egui::Key::Num7,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
        };
        ctx.begin_pass(egui::RawInput {
            events: vec![shortcut.clone()],
            ..Default::default()
        });
        app.consume_macro_shortcut(&ctx);
        assert_eq!(app.macro_runs.len(), 1);
        app.stop_macro(0);
        let _ = ctx.end_pass();

        // egui determines repeat state from the key-down state retained
        // across passes, regardless of the repeat value supplied in RawInput.
        ctx.begin_pass(egui::RawInput {
            events: vec![shortcut],
            ..Default::default()
        });

        app.consume_macro_shortcut(&ctx);

        assert!(app.macro_runs.is_empty());
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
