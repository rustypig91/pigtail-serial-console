//! Raw console input. The live console behaves like a real terminal: every
//! keystroke is sent straight to the device as it is typed, and the device's
//! echo (its prompt, the characters it reflects back) is what appears in the
//! log. There is no editable prompt field — the console captures the keyboard
//! whenever nothing else (search box, a dialog) holds focus. Almost no
//! keyboard shortcuts are reserved — every key reaches the device — except
//! that clipboard copy/paste have moved to Ctrl+Shift+C/V, freeing plain
//! Ctrl+C and Ctrl+V to always reach the device (interrupt / SYN) regardless
//! of whether text happens to be selected in the log, and Ctrl+Shift+F opens
//! search (see `log::show_console`), freeing plain Ctrl+F to reach the device.
//!
//! `PortConfig` still governs the send line ending, optional local echo (for
//! devices that don't echo), and whether Up/Down recall local history instead
//! of being sent to the device.

use crate::app::App;
use egui::{Event, Key};
use serialcore::reader::ConnState;
use serialcore::store::{IncomingLine, LineFlags};

impl App {
    /// Translate this frame's keyboard input into bytes for the device. Called
    /// only when a live device is selected (directly or as the merged view's
    /// send target) and no widget holds focus. Returns whether input was sent.
    pub(crate) fn console_key_input(&mut self, ctx: &egui::Context, active: usize) -> bool {
        let events = ctx.input(|i| i.events.clone());
        if events.is_empty() {
            return false;
        }
        // egui-winit maps Ctrl+C/X/V to Copy/Cut/Paste events regardless of
        // Shift, so Shift has to be read separately to tell plain Ctrl+C/V
        // (send the control byte) apart from Ctrl+Shift+C/V (clipboard).
        let shift = ctx.input(|i| i.modifiers.shift);
        let now = self.clock.now();
        let Some(conn) = self.connections.get_mut(active) else {
            return false;
        };
        // A `Closed` tab (a dead reader left by a failed reconnect, see
        // `App::reconnect_with_config`) has no channel on the other end:
        // typing into it would be silently dropped while local echo still
        // showed it as sent, so it's excluded here rather than left to look
        // like it worked.
        if conn.state == ConnState::Closed {
            return false;
        }
        let ending = conn.port_config.line_ending;
        let local_echo = conn.port_config.local_echo;
        let local_history = conn.port_config.local_history;

        let mut out: Vec<u8> = Vec::new();
        // Lines committed to the log this frame when local echo is on.
        let mut echo_lines: Vec<String> = Vec::new();

        for ev in &events {
            match ev {
                // Printable text (respects layout/shift). Enter/Tab are not Text.
                Event::Text(t) => {
                    out.extend_from_slice(t.as_bytes());
                    conn.tx_input.push_str(t);
                }
                // egui-winit turns Ctrl+C/X/V into Copy/Cut/Paste events (no Key
                // event), so we handle them here. Plain Ctrl+C/X/V reach the
                // device (interrupt / cancel / SYN); the Shift variants are the
                // clipboard actions instead, and are left for egui's own label
                // selection handling to act on (it also reacts to these events).
                Event::Copy => {
                    if !shift {
                        out.push(0x03);
                    }
                }
                Event::Cut => {
                    if !shift {
                        out.push(0x18);
                    }
                }
                Event::Paste(t) => {
                    if shift {
                        out.extend_from_slice(t.as_bytes());
                        conn.tx_input.push_str(t);
                    } else {
                        out.push(0x16); // SYN (Ctrl+V)
                    }
                }
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    match key {
                        Key::Enter => {
                            out.extend_from_slice(ending.bytes());
                            let line = std::mem::take(&mut conn.tx_input);
                            if !line.is_empty()
                                && conn.tx_history.last().map(|s| s.as_str()) != Some(line.as_str())
                            {
                                conn.tx_history.push(line.clone());
                            }
                            conn.tx_history_pos = None;
                            if local_echo && !line.is_empty() {
                                echo_lines.push(line);
                            }
                        }
                        Key::Backspace => {
                            out.push(0x7f); // DEL
                            conn.tx_input.pop();
                        }
                        Key::Tab => out.push(b'\t'),
                        Key::Escape => out.push(0x1b),
                        Key::ArrowUp => {
                            if local_history {
                                recall(conn, -1, &mut out);
                            } else {
                                out.extend_from_slice(b"\x1b[A");
                            }
                        }
                        Key::ArrowDown => {
                            if local_history {
                                recall(conn, 1, &mut out);
                            } else {
                                out.extend_from_slice(b"\x1b[B");
                            }
                        }
                        Key::ArrowRight => out.extend_from_slice(b"\x1b[C"),
                        Key::ArrowLeft => out.extend_from_slice(b"\x1b[D"),
                        Key::Home => out.extend_from_slice(b"\x1b[H"),
                        Key::End => out.extend_from_slice(b"\x1b[F"),
                        Key::Delete => out.extend_from_slice(b"\x1b[3~"),
                        Key::Insert => out.extend_from_slice(b"\x1b[2~"),
                        Key::PageUp => out.extend_from_slice(b"\x1b[5~"),
                        Key::PageDown => out.extend_from_slice(b"\x1b[6~"),
                        // Ctrl+<letter> → control code. (Ctrl+C/X/V arrive as
                        // Copy/Cut/Paste events, handled above, not here.)
                        _ => {
                            if modifiers.ctrl {
                                if let Some(b) = ctrl_code(*key) {
                                    out.push(b);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Nothing typed at all. Checked against *both*, because a frame can
        // produce a line to echo and no bytes to send: pressing Enter with the
        // line ending set to "none" contributes an empty `ending.bytes()`, and
        // the characters themselves went out in the frames they were typed in.
        // Returning on `out` alone dropped that echo line on the floor — after
        // `tx_input` had already been taken and pushed to history, so the input
        // was consumed and only its echo went missing (issue #43).
        if out.is_empty() && echo_lines.is_empty() {
            return false;
        }
        if !out.is_empty() {
            conn.handle.transmit(out);
        }
        for line in echo_lines {
            conn.store.append(IncomingLine {
                text: line,
                ts: now,
                port: conn.id,
                flags: LineFlags::TX_ECHO,
                spans: Default::default(),
                cursor: None,
            });
        }
        // Typing re-engages autoscroll so the cursor stays in view.
        conn.follow = true;
        conn.new_since_scroll = 0;
        true
    }
}

/// Recall a previous (`dir = -1`) or next (`dir = 1`) history entry: clear what
/// the device currently shows on the line (a DEL per character) and replay the
/// recalled text, so the device's line editor ends up holding it.
fn recall(conn: &mut crate::app::Connection, dir: i32, out: &mut Vec<u8>) {
    if conn.tx_history.is_empty() {
        return;
    }
    let len = conn.tx_history.len();
    let new_pos = match (conn.tx_history_pos, dir) {
        (None, -1) => Some(len - 1),
        (Some(p), -1) => Some(p.saturating_sub(1)),
        (Some(p), 1) if p + 1 < len => Some(p + 1),
        (Some(_), 1) => None,
        (None, 1) => None,
        _ => conn.tx_history_pos,
    };
    conn.tx_history_pos = new_pos;
    let target = new_pos
        .map(|p| conn.tx_history[p].clone())
        .unwrap_or_default();
    for _ in 0..conn.tx_input.chars().count() {
        out.push(0x7f);
    }
    out.extend_from_slice(target.as_bytes());
    conn.tx_input = target;
}

/// Map `Ctrl` + a letter to its ASCII control code (`Ctrl+A` = 1 … `Ctrl+Z` = 26).
fn ctrl_code(key: Key) -> Option<u8> {
    let letter = match key {
        Key::A => b'a',
        Key::B => b'b',
        Key::C => b'c',
        Key::D => b'd',
        Key::E => b'e',
        Key::F => b'f',
        Key::G => b'g',
        Key::H => b'h',
        Key::I => b'i',
        Key::J => b'j',
        Key::K => b'k',
        Key::L => b'l',
        Key::M => b'm',
        Key::N => b'n',
        Key::O => b'o',
        Key::P => b'p',
        Key::Q => b'q',
        Key::R => b'r',
        Key::S => b's',
        Key::T => b't',
        Key::U => b'u',
        Key::V => b'v',
        Key::W => b'w',
        Key::X => b'x',
        Key::Y => b'y',
        Key::Z => b'z',
        _ => return None,
    };
    Some(letter & 0x1f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{inert_handle, test_app};
    use crate::app::{App, Connection};
    use serialcore::config::{LineEnding, PortConfig};
    use serialcore::store::PortId;

    /// Feed one frame's worth of keyboard events to the console.
    fn press(app: &mut App, events: Vec<Event>) {
        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput {
            events,
            ..Default::default()
        });
        app.console_key_input(&ctx, 0);
        let _ = ctx.end_pass();
    }

    fn enter() -> Event {
        Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn tab() -> Event {
        Event::Key {
            key: Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn ctrl_shift_f() -> Event {
        Event::Key {
            key: Key::F,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                ctrl: true,
                shift: true,
                ..Default::default()
            },
        }
    }

    /// Draw the focusable UI and process one complete input frame.
    fn frame(app: &mut App, ctx: &egui::Context, events: Vec<Event>) {
        let _ = ctx.run(
            egui::RawInput {
                events,
                ..Default::default()
            },
            |ctx| {
                let console_tab_claimed = app.claim_console_tab_before_layout(ctx);
                app.show_header(ctx);
                app.show_console(ctx, console_tab_claimed);
                app.release_console_tab_after_layout(ctx, console_tab_claimed);
            },
        );
    }

    fn echoed(conn: &Connection) -> Vec<String> {
        (conn.store.first_abs_index()..conn.store.next_abs_index())
            .filter_map(|i| conn.store.get(i))
            .filter(|l| l.meta.flags.contains(LineFlags::TX_ECHO))
            .map(|l| l.text.to_string())
            .collect()
    }

    /// Issue #43: with the line ending set to "none", pressing Enter puts no
    /// bytes in `out` — the typed characters went out in the frames they were
    /// typed in, and `LineEnding::None` adds nothing of its own. The early
    /// return on `out.is_empty()` then bailed *after* `tx_input` had been taken
    /// and pushed to history, so the input was consumed and only its local echo
    /// went missing.
    #[test]
    fn a_line_with_no_line_ending_is_still_echoed() {
        for ending in [LineEnding::None, LineEnding::Lf] {
            let (mut app, _enum_tx) = test_app("echo-no-ending");
            let id = PortId(0);
            let conn = app.make_connection(
                id,
                "probe".into(),
                Default::default(),
                PortConfig {
                    line_ending: ending,
                    local_echo: true,
                    ..Default::default()
                },
                inert_handle(id),
            );
            app.connections.push(conn);

            press(&mut app, vec![Event::Text("status".into())]);
            assert_eq!(app.connections[0].tx_input, "status");

            press(&mut app, vec![enter()]);
            let conn = &app.connections[0];
            assert_eq!(
                echoed(conn),
                vec!["status".to_string()],
                "local echo has to commit the line whatever the line ending is; \
                 with {ending:?} it was silently dropped"
            );
            assert_eq!(conn.tx_input, "", "and the input line is consumed");
            assert_eq!(conn.tx_history, vec!["status".to_string()]);
            assert!(conn.follow, "sending re-engages autoscroll");
        }
    }

    /// The early return still has to fire on a frame that produced nothing at
    /// all, or an idle frame would keep re-engaging follow under a user who has
    /// deliberately scrolled back.
    #[test]
    fn a_frame_with_no_input_changes_nothing() {
        let (mut app, _enum_tx) = test_app("echo-idle");
        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            Default::default(),
            PortConfig::default(),
            inert_handle(id),
        );
        conn.follow = false;
        app.connections.push(conn);

        press(&mut app, Vec::new());
        assert!(
            !app.connections[0].follow,
            "an idle frame must not re-engage follow"
        );
        assert!(app.connections[0].store.is_empty());
    }

    /// A bare console has no focused widget. egui normally interprets Tab in
    /// that state as a request to focus the first header control, which used to
    /// make the post-layout console-input gate reject the same key event.
    #[test]
    fn tab_is_kept_by_an_unfocused_console_instead_of_ui_focus_navigation() {
        let (mut app, _enum_tx) = test_app("console-tab");
        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            Default::default(),
            PortConfig::default(),
            inert_handle(id),
        );
        conn.follow = false;
        app.connections.push(conn);

        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput {
            events: vec![tab()],
            ..Default::default()
        });
        let console_tab_claimed = app.claim_console_tab_before_layout(&ctx);
        app.show_header(&ctx);
        assert!(
            ctx.memory(|m| m.focused().is_some()),
            "the console's temporary guard must own focus during layout"
        );

        app.show_console(&ctx, console_tab_claimed);
        app.release_console_tab_after_layout(&ctx, console_tab_claimed);

        assert!(
            app.connections[0].follow,
            "processing the Tab byte should re-engage console follow mode"
        );
        assert!(
            ctx.memory(|m| m.focused().is_none()),
            "Tab must not leave a UI control focused"
        );
        let _ = ctx.end_pass();
    }

    #[test]
    fn tab_navigates_the_ui_when_there_is_no_console_to_receive_it() {
        let (mut app, _enum_tx) = test_app("no-console-tab");
        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput {
            events: vec![tab()],
            ..Default::default()
        });
        let console_tab_claimed = app.claim_console_tab_before_layout(&ctx);

        app.show_header(&ctx);
        app.show_console(&ctx, console_tab_claimed);
        app.release_console_tab_after_layout(&ctx, console_tab_claimed);

        assert!(
            ctx.memory(|m| m.focused().is_some()),
            "without a connection, Tab must keep the UI focus egui assigned"
        );
        let _ = ctx.end_pass();
    }

    #[test]
    fn tab_navigates_the_ui_when_the_active_console_is_closed() {
        let (mut app, _enum_tx) = test_app("closed-console-tab");
        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            Default::default(),
            PortConfig::default(),
            inert_handle(id),
        );
        conn.state = ConnState::Closed;
        app.connections.push(conn);

        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput {
            events: vec![tab()],
            ..Default::default()
        });
        let console_tab_claimed = app.claim_console_tab_before_layout(&ctx);

        app.show_header(&ctx);
        app.show_console(&ctx, console_tab_claimed);
        app.release_console_tab_after_layout(&ctx, console_tab_claimed);

        assert!(
            ctx.memory(|m| m.focused().is_some()),
            "a Closed tab cannot receive input, so Tab must retain UI focus"
        );
        let _ = ctx.end_pass();
    }

    #[test]
    fn tab_stays_in_the_ui_while_a_context_menu_is_open() {
        let (mut app, _enum_tx) = test_app("context-menu-tab");
        let id = PortId(0);
        let conn = app.make_connection(
            id,
            "probe".into(),
            Default::default(),
            PortConfig::default(),
            inert_handle(id),
        );
        app.connections.push(conn);

        let ctx = egui::Context::default();
        let mut menu_anchor = egui::Rect::NOTHING;
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                menu_anchor = ui.button("menu anchor").rect;
            });
        });
        let pos = menu_anchor.center();
        let _ = ctx.run(
            egui::RawInput {
                events: vec![
                    Event::PointerMoved(pos),
                    Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Secondary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                    Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Secondary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.button("menu anchor").context_menu(|ui| {
                        let _ = ui.button("menu action");
                    });
                });
            },
        );
        assert!(ctx.is_context_menu_open());

        ctx.begin_pass(egui::RawInput {
            events: vec![tab()],
            ..Default::default()
        });
        let console_tab_claimed = app.claim_console_tab_before_layout(&ctx);

        assert!(
            !console_tab_claimed,
            "an open context menu, not the console, owns Tab"
        );
        app.show_header(&ctx);
        assert!(
            ctx.memory(|memory| memory.focused().is_some()),
            "Tab must remain available to UI focus navigation"
        );
        let _ = ctx.end_pass();
    }

    #[test]
    fn tab_is_not_sent_when_an_overlay_opens_in_the_same_frame() {
        let (mut app, _enum_tx) = test_app("same-frame-overlay-tab");
        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            Default::default(),
            PortConfig::default(),
            inert_handle(id),
        );
        conn.follow = false;
        app.connections.push(conn);

        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput {
            events: vec![tab()],
            ..Default::default()
        });
        let console_tab_claimed = app.claim_console_tab_before_layout(&ctx);
        assert!(
            console_tab_claimed,
            "the overlay is not open yet when pre-layout ownership is decided"
        );

        ctx.memory_mut(|memory| memory.open_popup(egui::Id::new("same-frame popup")));

        app.show_console(&ctx, console_tab_claimed);
        app.release_console_tab_after_layout(&ctx, console_tab_claimed);
        assert!(
            !app.connections[0].follow,
            "the post-layout gate must not transmit Tab after a menu opens"
        );
        let _ = ctx.end_pass();
    }

    #[test]
    fn tab_stays_in_the_ui_while_an_egui_popup_is_open() {
        let (mut app, _enum_tx) = test_app("popup-tab");
        let id = PortId(0);
        let conn = app.make_connection(
            id,
            "probe".into(),
            Default::default(),
            PortConfig::default(),
            inert_handle(id),
        );
        app.connections.push(conn);

        let ctx = egui::Context::default();
        ctx.memory_mut(|memory| memory.open_popup(egui::Id::new("test popup")));
        ctx.begin_pass(egui::RawInput {
            events: vec![tab()],
            ..Default::default()
        });
        let console_tab_claimed = app.claim_console_tab_before_layout(&ctx);

        assert!(
            !console_tab_claimed,
            "an open popup, not the console, owns Tab"
        );
        app.show_header(&ctx);
        assert!(
            ctx.memory(|memory| memory.focused().is_some()),
            "Tab must remain available to UI focus navigation"
        );
        let _ = ctx.end_pass();
    }

    /// A single egui frame can contain multiple keyboard events. The Tab guard
    /// must stop a following Enter from activating the first header tab and
    /// redirecting the whole batch away from the console that was active when
    /// the user typed it.
    #[test]
    fn tab_and_enter_in_one_frame_stay_on_the_active_console() {
        let (mut app, _enum_tx) = test_app("console-tab-enter");
        for id in [PortId(0), PortId(1)] {
            let mut conn = app.make_connection(
                id,
                format!("probe-{}", id.0),
                Default::default(),
                PortConfig::default(),
                inert_handle(id),
            );
            conn.follow = false;
            app.connections.push(conn);
        }
        app.active = 1;

        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput {
            events: vec![tab(), enter()],
            ..Default::default()
        });
        let console_tab_claimed = app.claim_console_tab_before_layout(&ctx);

        app.show_header(&ctx);
        assert_eq!(
            app.active, 1,
            "Enter in the Tab frame must not activate the first header tab"
        );
        app.show_console(&ctx, console_tab_claimed);
        app.release_console_tab_after_layout(&ctx, console_tab_claimed);

        assert!(
            !app.connections[0].follow,
            "the inactive connection must receive none of the input batch"
        );
        assert!(
            app.connections[1].follow,
            "Tab and Enter must be processed by the originally active console"
        );
        assert!(ctx.memory(|m| m.focused().is_none()));
        let _ = ctx.end_pass();
    }

    #[test]
    fn merged_console_sends_keyboard_input_to_its_selected_device() {
        let (mut app, _enum_tx) = test_app("merged-send-target");
        for id in [PortId(0), PortId(1)] {
            let mut conn = app.make_connection(
                id,
                format!("probe-{}", id.0),
                Default::default(),
                PortConfig::default(),
                inert_handle(id),
            );
            conn.follow = false;
            app.connections.push(conn);
        }
        app.merged_selected = true;
        app.merged_tx_port = Some(PortId(1));
        app.merged_follow = false;

        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![Event::Text("x".into())]);

        assert!(app.connections[0].tx_input.is_empty());
        assert_eq!(app.connections[1].tx_input, "x");
        assert!(!app.connections[0].follow);
        assert!(app.connections[1].follow);
        assert!(
            app.merged_follow,
            "sending should re-pin the visible console"
        );
    }

    #[test]
    fn tab_is_not_sent_when_search_opens_in_the_same_input_batch() {
        let (mut app, _enum_tx) = test_app("same-frame-search-tab");
        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            Default::default(),
            PortConfig::default(),
            inert_handle(id),
        );
        conn.follow = false;
        app.connections.push(conn);

        let ctx = egui::Context::default();
        frame(&mut app, &ctx, vec![ctrl_shift_f(), tab()]);

        assert!(app.show_search, "the shortcut must still open search");
        assert!(
            !app.connections[0].follow,
            "the pending search field, not the console, owns Tab"
        );
    }

    #[test]
    fn keyboard_close_of_search_releases_the_next_tab_to_the_console() {
        let (mut app, _enum_tx) = test_app("keyboard-close-search");
        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            Default::default(),
            PortConfig::default(),
            inert_handle(id),
        );
        conn.follow = false;
        app.connections.push(conn);
        app.show_search = true;
        app.search_focus_request = true;

        let ctx = egui::Context::default();
        frame(&mut app, &ctx, Vec::new());
        // Text field -> case -> Prev -> Next -> Close.
        for _ in 0..4 {
            frame(&mut app, &ctx, vec![tab()]);
        }
        frame(&mut app, &ctx, vec![enter()]);

        assert!(!app.show_search, "Enter on Close must hide search");
        assert!(
            ctx.memory(|memory| memory.focused().is_none()),
            "the removed Close button must not retain keyboard focus"
        );

        frame(&mut app, &ctx, vec![tab()]);
        assert!(
            app.connections[0].follow,
            "the first Tab after closing search must reach the console"
        );
    }
}
