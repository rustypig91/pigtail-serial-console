//! User-defined transmit macros and their non-blocking execution scheduler.

use crate::app::{App, MacroRun};
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
            let pressed = ctx.input(|input| {
                input.events.iter().any(|event| {
                    matches!(
                        event,
                        egui::Event::Key {
                            key: event_key,
                            pressed: true,
                            repeat: false,
                            modifiers: event_modifiers,
                            ..
                        } if *event_key == key && event_modifiers.matches_logically(modifiers)
                    )
                })
            });
            if pressed && ctx.input_mut(|input| input.consume_key(modifiers, key)) {
                selected = Some(index);
                break;
            }
        }
        if let Some(index) = selected {
            self.start_macro(index, Instant::now());
        }
    }

    pub(crate) fn show_macros_window(&mut self, ctx: &egui::Context) {
        if !self.show_macros_win {
            return;
        }

        let mut open = self.show_macros_win;
        let can_run = self.macro_target_port().is_some();
        let mut changed = false;
        let mut run = None;
        let mut remove = None;
        let mut shortcut_change = None;
        let mut add_macro = false;
        let mut stop_all = false;
        let mut step_selection = self.macro_step_selection;
        let shortcut_owners = shortcut_owners(&self.config.macros);

        egui::Window::new("Transmit macros")
            .open(&mut open)
            .default_width(520.0)
            .default_height(420.0)
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
                    for (index, macro_def) in self.config.macros.iter_mut().enumerate() {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                changed |= ui
                                    .add(
                                        egui::TextEdit::singleline(&mut macro_def.name)
                                            .hint_text("Macro name")
                                            .desired_width(260.0),
                                    )
                                    .changed();
                                if ui
                                    .add_enabled(
                                        can_run && macro_has_command(macro_def),
                                        egui::Button::new("Run"),
                                    )
                                    .clicked()
                                {
                                    run = Some(index);
                                }
                                if ui.small_button("Delete").clicked() {
                                    remove = Some(index);
                                }
                            });

                            egui::Grid::new(("macro-fields", index))
                                .num_columns(2)
                                .spacing([12.0, 6.0])
                                .show(ui, |ui| {
                                    ui.label("Description");
                                    changed |= ui
                                        .add(
                                            egui::TextEdit::singleline(&mut macro_def.description)
                                                .hint_text("What this sequence does")
                                                .desired_width(340.0),
                                        )
                                        .changed();
                                    ui.end_row();

                                    ui.label("Shortcut");
                                    let mut shortcut =
                                        macro_def.shortcut.filter(|digit| *digit <= 9);
                                    egui::ComboBox::from_id_salt(("macro-shortcut", index))
                                        .selected_text(shortcut_label(shortcut))
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(&mut shortcut, None, "Unassigned");
                                            for digit in 0..=9 {
                                                let available = shortcut_owners[digit as usize]
                                                    .is_none_or(|owner| owner == index);
                                                let response = ui.add_enabled(
                                                    available,
                                                    egui::SelectableLabel::new(
                                                        shortcut == Some(digit),
                                                        shortcut_label(Some(digit)),
                                                    ),
                                                );
                                                if response
                                                    .on_disabled_hover_text(
                                                        "Already assigned to another macro",
                                                    )
                                                    .clicked()
                                                {
                                                    shortcut = Some(digit);
                                                }
                                            }
                                        });
                                    if shortcut != macro_def.shortcut {
                                        shortcut_change = Some((index, shortcut));
                                    }
                                    ui.end_row();
                                });

                            ui.horizontal(|ui| {
                                ui.label("Steps");
                                ui.weak(
                                    "Select a step to insert after it; use ↑/↓ to rearrange.",
                                );
                            });
                            let mut remove_step = None;
                            let mut move_step_to = None;
                            let step_count = macro_def.steps.len();
                            let row_height = ui.spacing().interact_size.y;
                            for (step_index, step) in macro_def.steps.iter_mut().enumerate() {
                                ui.horizontal(|ui| {
                                    let selected = step_selection == Some((index, step_index));
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
                                        step_selection = Some((index, step_index));
                                    }
                                    match step {
                                        MacroStep::Command { text } => {
                                            let edit = ui.add_sized(
                                                [300.0, row_height],
                                                egui::TextEdit::singleline(text)
                                                    .hint_text("command")
                                                    .font(egui::TextStyle::Monospace),
                                            );
                                            changed |= edit.changed();
                                            if edit.clicked() {
                                                step_selection = Some((index, step_index));
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
                                                .on_hover_text(
                                                    "Wait before advancing to the next step",
                                                );
                                            changed |= edit.changed();
                                            if edit.clicked() {
                                                step_selection = Some((index, step_index));
                                            }
                                        }
                                    }
                                    if ui
                                        .add_enabled(
                                            step_index > 0,
                                            egui::Button::new("↑").small(),
                                        )
                                        .on_hover_text("Move this step up")
                                        .clicked()
                                    {
                                        move_step_to = Some((step_index, step_index - 1));
                                    }
                                    if ui
                                        .add_enabled(
                                            step_index + 1 < step_count,
                                            egui::Button::new("↓").small(),
                                        )
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

                            if let Some(step_index) = remove_step {
                                macro_def.steps.remove(step_index);
                                step_selection =
                                    selection_after_remove(step_selection, index, step_index);
                                changed = true;
                            } else if let Some((from, requested_destination)) = move_step_to {
                                let destination =
                                    move_step(&mut macro_def.steps, from, requested_destination);
                                step_selection =
                                    selection_after_move(step_selection, index, from, destination);
                                changed |= destination != from;
                            }

                            let selected_step =
                                step_selection.and_then(|(macro_index, step_index)| {
                                    (macro_index == index
                                        && macro_def.steps.get(step_index).is_some())
                                    .then_some(step_index)
                            });
                            ui.horizontal(|ui| {
                                if ui.small_button("+ Add command").clicked() {
                                    let insert_at = selected_step
                                        .map_or(macro_def.steps.len(), |step_index| step_index + 1);
                                    macro_def.steps.insert(
                                        insert_at,
                                        MacroStep::Command {
                                            text: String::new(),
                                        },
                                    );
                                    step_selection = Some((index, insert_at));
                                    changed = true;
                                }
                                let add_delay = ui
                                    .add(egui::Button::new("+ Add delay").small())
                                    .on_hover_text(
                                        "Insert after the selected step, or append when none is selected",
                                    );
                                if add_delay.clicked() {
                                    let insert_at = selected_step
                                        .map_or(macro_def.steps.len(), |step_index| step_index + 1);
                                    macro_def.steps.insert(
                                        insert_at,
                                        MacroStep::Delay {
                                            delay_ms: DEFAULT_DELAY_MS,
                                        },
                                    );
                                    step_selection = Some((index, insert_at));
                                    changed = true;
                                }
                            });
                        });
                        ui.add_space(6.0);
                    }
                });

                if ui.button("+ New macro").clicked() {
                    add_macro = true;
                }
            });

        self.show_macros_win = open;
        self.macro_step_selection = step_selection;
        if stop_all {
            self.macro_runs.clear();
        }
        if let Some((index, shortcut)) = shortcut_change {
            changed |= assign_shortcut(&mut self.config.macros, index, shortcut);
        }
        if let Some(index) = remove {
            self.config.macros.remove(index);
            self.macro_step_selection =
                self.macro_step_selection
                    .and_then(|(macro_index, step_index)| match macro_index.cmp(&index) {
                        std::cmp::Ordering::Less => Some((macro_index, step_index)),
                        std::cmp::Ordering::Equal => None,
                        std::cmp::Ordering::Greater => Some((macro_index - 1, step_index)),
                    });
            changed = true;
            // `run` can only name the same card if Run and Delete somehow land
            // in one input frame. Deletion wins rather than shifting the index
            // onto the next macro.
            if run == Some(index) {
                run = None;
            } else if run.is_some_and(|run_index| run_index > index) {
                run = run.map(|run_index| run_index - 1);
            }
        }
        if add_macro {
            let index = self.config.macros.len();
            self.config.macros.push(TransmitMacro {
                name: "New macro".into(),
                steps: vec![MacroStep::Command {
                    text: String::new(),
                }],
                ..Default::default()
            });
            self.macro_step_selection = Some((index, 0));
            changed = true;
        }
        if changed {
            self.write_config();
        }
        if let Some(index) = run {
            self.start_macro(index, Instant::now());
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

fn selection_after_remove(
    selection: Option<(usize, usize)>,
    macro_index: usize,
    removed: usize,
) -> Option<(usize, usize)> {
    selection.and_then(|(selected_macro, selected_step)| {
        if selected_macro != macro_index || selected_step < removed {
            Some((selected_macro, selected_step))
        } else if selected_step == removed {
            None
        } else {
            Some((selected_macro, selected_step - 1))
        }
    })
}

fn selection_after_move(
    selection: Option<(usize, usize)>,
    macro_index: usize,
    from: usize,
    destination: usize,
) -> Option<(usize, usize)> {
    selection.map(|(selected_macro, selected_step)| {
        if selected_macro != macro_index {
            return (selected_macro, selected_step);
        }
        let selected_step = if selected_step == from {
            destination
        } else if from < destination && selected_step > from && selected_step <= destination {
            selected_step - 1
        } else if destination < from && selected_step >= destination && selected_step < from {
            selected_step + 1
        } else {
            selected_step
        };
        (selected_macro, selected_step)
    })
}

fn shortcut_label(shortcut: Option<u8>) -> String {
    shortcut.map_or_else(
        || "Unassigned".to_owned(),
        |digit| format!("Ctrl+Shift+{digit}"),
    )
}

fn shortcut_owners(macros: &[TransmitMacro]) -> [Option<usize>; 10] {
    let mut owners = [None; 10];
    for (index, macro_def) in macros.iter().enumerate() {
        if let Some(digit) = macro_def.shortcut.filter(|digit| *digit <= 9) {
            owners[digit as usize].get_or_insert(index);
        }
    }
    owners
}

/// Assign a shortcut without taking it away from another macro.
fn assign_shortcut(macros: &mut [TransmitMacro], index: usize, shortcut: Option<u8>) -> bool {
    let Some(macro_def) = macros.get(index) else {
        return false;
    };
    if macro_def.shortcut == shortcut
        || shortcut.is_some_and(|digit| {
            digit > 9
                || macros.iter().enumerate().any(|(other_index, other)| {
                    other_index != index && other.shortcut == Some(digit)
                })
        })
    {
        return false;
    }
    macros[index].shortcut = shortcut;
    true
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
        assert_eq!(selection_after_move(Some((0, 2)), 0, 2, 1), Some((0, 1)));
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
    fn shortcut_cannot_be_assigned_to_two_macros() {
        let mut app = app_with_macro(100);
        app.config.macros.push(TransmitMacro {
            name: "Second".into(),
            ..Default::default()
        });

        assert!(!assign_shortcut(&mut app.config.macros, 1, Some(7)));
        assert_eq!(app.config.macros[0].shortcut, Some(7));
        assert_eq!(app.config.macros[1].shortcut, None);

        assert!(assign_shortcut(&mut app.config.macros, 1, Some(6)));
        assert_eq!(app.config.macros[0].shortcut, Some(7));
        assert_eq!(app.config.macros[1].shortcut, Some(6));
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
}
