//! Bound OSC control strings before they reach vt100's unbounded std buffer.

const MAX_OSC_BYTES: usize = 4096;

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Escape,
    Osc(usize),
    DiscardOsc,
}

pub(crate) struct Terminal {
    parser: vt100::Parser,
    state: State,
}

impl Terminal {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, scrollback),
            state: State::Ground,
        }
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }
    pub fn screen_mut(&mut self) -> &mut vt100::Screen {
        self.parser.screen_mut()
    }

    pub fn process(&mut self, bytes: &[u8]) {
        // Plain output is by far the most common case; avoid another copy.
        if matches!(self.state, State::Ground) && !bytes.contains(&0x1b) {
            self.parser.process(bytes);
            return;
        }
        let mut filtered = Vec::with_capacity(bytes.len().min(MAX_OSC_BYTES));
        for chunk in bytes.chunks(MAX_OSC_BYTES) {
            filtered.clear();
            for &byte in chunk {
                let mut keep = true;
                self.state = match self.state {
                    State::Ground if byte == 0x1b => State::Escape,
                    State::Escape if byte == b']' => State::Osc(0),
                    State::Escape if byte == 0x1b => State::Escape,
                    State::Escape if matches!(byte, 0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f..=0xff) => {
                        State::Escape
                    }
                    State::Osc(_) if byte == 0x1b => State::Escape,
                    State::Osc(_) if matches!(byte, 0x07 | 0x18 | 0x1a) => State::Ground,
                    State::Osc(count) if count >= MAX_OSC_BYTES => {
                        // CAN closes the dependency's buffered OSC at the
                        // ceiling. Ignore the remainder until it terminates.
                        filtered.push(0x18);
                        keep = false;
                        State::DiscardOsc
                    }
                    State::Osc(count) => State::Osc(count + 1),
                    State::DiscardOsc => {
                        keep = false;
                        match byte {
                            0x1b => {
                                keep = true;
                                State::Escape
                            }
                            0x07 | 0x18 | 0x1a => State::Ground,
                            _ => State::DiscardOsc,
                        }
                    }
                    _ => State::Ground,
                };
                if keep {
                    filtered.push(byte);
                }
            }
            self.parser.process(&filtered);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_osc_is_discarded_and_screen_recovers_across_chunks() {
        for terminator in [b"\x07".as_slice(), b"\x1b\\"] {
            let mut terminal = Terminal::new(24, 80, 0);
            terminal.process(b"before\x1b");
            terminal.process(b"]0;");
            for _ in 0..256 {
                terminal.process(&vec![b'x'; MAX_OSC_BYTES]);
            }
            assert!(matches!(terminal.state, State::DiscardOsc));
            terminal.process(terminator);
            terminal.process(b"after\r\n\x1b[31mred");
            assert!(terminal.screen().contents().contains("beforeafter"));
            assert!(terminal.screen().contents().contains("red"));
            assert!(matches!(terminal.state, State::Ground));
        }
    }

    #[test]
    fn normal_controls_match_the_original_parser_for_split_reads() {
        let bytes = b"hello\r\n\x1b[31mred\x1b[0m\x1b]0;title\x07\x1b]2;other\x1b\\end\x1b\x00]0;nul\x07\x1b\x7f]2;del\x07\x1b\x18]plain\x1b Pignored\x1b\\\x1b[?1049halt\x1b[?1049l";
        let mut expected = vt100::Parser::new(24, 80, 0);
        expected.process(bytes);
        for split in 0..=bytes.len() {
            let mut terminal = Terminal::new(24, 80, 0);
            terminal.process(&bytes[..split]);
            terminal.process(&bytes[split..]);
            assert_eq!(
                terminal.screen().state_formatted(),
                expected.screen().state_formatted()
            );
        }
    }

    #[test]
    fn osc_after_ignored_escape_bytes_stays_bounded() {
        // VTE remains in Escape when executing C0 controls or ignoring DEL
        // and high bytes. The OSC guard must do the same, even across reads.
        for ignored in [0x00, 0x0a, 0x7f, 0xff] {
            let mut terminal = Terminal::new(24, 80, 0);
            terminal.process(&[0x1b, ignored]);
            terminal.process(b"]0;");
            terminal.process(&vec![b'x'; MAX_OSC_BYTES * 2]);
            assert!(matches!(terminal.state, State::DiscardOsc));
            terminal.process(b"\x07after");
            assert!(terminal.screen().contents().contains("after"));
        }
    }
}
