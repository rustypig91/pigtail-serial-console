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
    /// only when a live console tab is active and no widget holds focus.
    pub(crate) fn console_key_input(&mut self, ctx: &egui::Context, active: usize) {
        let events = ctx.input(|i| i.events.clone());
        if events.is_empty() {
            return;
        }
        // egui-winit maps Ctrl+C/X/V to Copy/Cut/Paste events regardless of
        // Shift, so Shift has to be read separately to tell plain Ctrl+C/V
        // (send the control byte) apart from Ctrl+Shift+C/V (clipboard).
        let shift = ctx.input(|i| i.modifiers.shift);
        let now = self.clock.now();
        let Some(conn) = self.connections.get_mut(active) else {
            return;
        };
        // A `Closed` tab (a dead reader left by a failed reconnect, see
        // `App::reconnect_with_config`) has no channel on the other end:
        // typing into it would be silently dropped while local echo still
        // showed it as sent, so it's excluded here rather than left to look
        // like it worked.
        if conn.state == ConnState::Closed {
            return;
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

        if out.is_empty() {
            return;
        }
        conn.handle.transmit(out);
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
