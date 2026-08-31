//! Minimal ANSI handling: parse SGR colour escapes into [`ColorSpan`]s and strip
//! all other escape sequences. No cursor addressing, no alternate screen — this
//! is a log viewer, not a terminal emulator (spec §2).

use crate::store::ColorSpan;
use smallvec::SmallVec;

/// Standard 8 ANSI colours + bright variants, as `0x00RRGGBB` (a common dark
/// palette). Index 0-7 normal, 8-15 bright.
const PALETTE: [u32; 16] = [
    0x000000, 0xCD3131, 0x0DBC79, 0xE5E510, 0x2472C8, 0xBC3FBC, 0x11A8CD, 0xE5E5E5, 0x666666,
    0xF14C4C, 0x23D18B, 0xF5F543, 0x3B8EEA, 0xD670D6, 0x29B8DB, 0xFFFFFF,
];

/// Result of stripping ANSI from a line: clean text plus colour spans over the
/// *clean* byte offsets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Styled {
    pub text: String,
    pub spans: SmallVec<[ColorSpan; 2]>,
    /// The `cursor` argument to [`parse_line`], translated from a raw offset
    /// into `input` to the corresponding offset in `text` — stripped escape
    /// sequences before it don't count, so the two rarely match.
    pub cursor: Option<usize>,
}

#[derive(Clone, Copy)]
struct SgrState {
    fg: u32,
    bg: u32,
    bold: bool,
}

impl SgrState {
    fn default_state() -> SgrState {
        SgrState {
            fg: ColorSpan::NO_COLOR,
            bg: ColorSpan::NO_COLOR,
            bold: false,
        }
    }
    fn is_default(&self) -> bool {
        self.fg == ColorSpan::NO_COLOR && self.bg == ColorSpan::NO_COLOR && !self.bold
    }
}

/// Parse a line's text, extracting SGR colours and removing every escape
/// sequence. The input is already-decoded UTF-8 (see the framer); this operates
/// on `&str`. `cursor`, if given, is a raw byte offset into `input` (e.g. a
/// live edit cursor from [`crate::framer::FramedLine::cursor`]) to translate
/// into the corresponding offset in the returned `Styled::text` — see
/// [`Styled::cursor`].
pub fn parse_line(input: &str, cursor: Option<usize>) -> Styled {
    // Fast path: no ESC at all — raw and clean offsets are identical.
    if !input.as_bytes().contains(&0x1B) {
        return Styled {
            text: input.to_string(),
            spans: SmallVec::new(),
            cursor: cursor.map(|c| c.min(input.len())),
        };
    }

    let mut text = String::with_capacity(input.len());
    let mut spans: SmallVec<[ColorSpan; 2]> = SmallVec::new();
    let mut state = SgrState::default_state();
    let mut span_start: u32 = 0;
    let mut resolved_cursor = None;

    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // An escape sequence is zero-width in the output, so a cursor sitting
        // right before one resolves here — it never appears inside one (see
        // `Framer::cursor`'s invariant), only ever at the start of whatever
        // this iteration is about to consume.
        if resolved_cursor.is_none() && cursor == Some(i) {
            resolved_cursor = Some(text.len());
        }
        if bytes[i] == 0x1B {
            // Escape. Handle CSI (`ESC [ ... final`); strip other escapes.
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                // Find the final byte in 0x40..=0x7E.
                let mut j = i + 2;
                while j < bytes.len() && !(0x40..=0x7E).contains(&bytes[j]) {
                    j += 1;
                }
                if j < bytes.len() {
                    let final_byte = bytes[j];
                    let params = &input[i + 2..j];
                    if final_byte == b'm' {
                        // SGR: a colour change closes the current span.
                        let cur_len = text.len() as u32 - span_start;
                        if cur_len > 0 && !state.is_default() {
                            spans.push(ColorSpan {
                                start: span_start,
                                len: cur_len,
                                rgb: state.fg,
                                bg: state.bg,
                                bold: state.bold,
                            });
                        }
                        apply_sgr(params, &mut state);
                        span_start = text.len() as u32;
                    }
                    // Any other CSI final byte: strip silently.
                    i = j + 1;
                    continue;
                } else {
                    // Unterminated CSI: drop the rest.
                    break;
                }
            } else {
                // Non-CSI escape (e.g. `ESC c`): skip ESC and the character
                // after it. A whole character, not a byte: the byte following
                // an ESC is not always ASCII — a device emitting garbage (the
                // wrong baud rate) has its invalid bytes sanitized to a
                // two-byte `·` by the framer — and stepping two bytes would
                // leave `i` inside that character, so the copy below would
                // slice `input` off a char boundary and panic.
                i += 1;
                if i < bytes.len() {
                    i += utf8_char_len(bytes[i]);
                }
                continue;
            }
        }
        // Regular byte: copy the whole UTF-8 char.
        let ch_len = utf8_char_len(bytes[i]);
        let end = (i + ch_len).min(bytes.len());
        text.push_str(&input[i..end]);
        i = end;
    }
    if resolved_cursor.is_none() {
        // At/after the end of `input` (or inside a dropped unterminated CSI).
        resolved_cursor = cursor.map(|_| text.len());
    }

    // Close a trailing coloured span.
    let cur_len = text.len() as u32 - span_start;
    if cur_len > 0 && !state.is_default() {
        spans.push(ColorSpan {
            start: span_start,
            len: cur_len,
            rgb: state.fg,
            bg: state.bg,
            bold: state.bold,
        });
    }

    Styled {
        text,
        spans,
        cursor: resolved_cursor,
    }
}

fn utf8_char_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else if first >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

fn apply_sgr(params: &str, state: &mut SgrState) {
    if params.is_empty() {
        *state = SgrState::default_state();
        return;
    }
    let mut it = params.split(';').peekable();
    while let Some(tok) = it.next() {
        let code: i32 = tok.parse().unwrap_or(0);
        match code {
            0 => *state = SgrState::default_state(),
            1 => state.bold = true,
            22 => state.bold = false,
            30..=37 => state.fg = PALETTE[(code - 30) as usize],
            90..=97 => state.fg = PALETTE[(code - 90 + 8) as usize],
            39 => state.fg = ColorSpan::NO_COLOR,
            40..=47 => state.bg = PALETTE[(code - 40) as usize],
            100..=107 => state.bg = PALETTE[(code - 100 + 8) as usize],
            49 => state.bg = ColorSpan::NO_COLOR,
            38 => {
                // Extended colour: `38;5;n` or `38;2;r;g;b`.
                match it.next().and_then(|s| s.parse::<i32>().ok()) {
                    Some(5) => {
                        if let Some(n) = it.next().and_then(|s| s.parse::<u32>().ok()) {
                            state.fg = xterm256(n);
                        }
                    }
                    Some(2) => {
                        let r = it.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
                        let g = it.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
                        let b = it.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
                        state.fg = (r << 16) | (g << 8) | b;
                    }
                    _ => {}
                }
            }
            48 => {
                // Extended background: `48;5;n` or `48;2;r;g;b`.
                match it.next().and_then(|s| s.parse::<i32>().ok()) {
                    Some(5) => {
                        if let Some(n) = it.next().and_then(|s| s.parse::<u32>().ok()) {
                            state.bg = xterm256(n);
                        }
                    }
                    Some(2) => {
                        let r = it.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
                        let g = it.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
                        let b = it.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
                        state.bg = (r << 16) | (g << 8) | b;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn xterm256(n: u32) -> u32 {
    match n {
        0..=15 => PALETTE[n as usize],
        16..=231 => {
            let n = n - 16;
            let r = n / 36;
            let g = (n % 36) / 6;
            let b = n % 6;
            let conv = |c: u32| if c == 0 { 0 } else { 55 + c * 40 };
            (conv(r) << 16) | (conv(g) << 8) | conv(b)
        }
        232..=255 => {
            let v = 8 + (n - 232) * 10;
            (v << 16) | (v << 8) | v
        }
        _ => ColorSpan::NO_COLOR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_untouched() {
        let s = parse_line("hello world", None);
        assert_eq!(s.text, "hello world");
        assert!(s.spans.is_empty());
    }

    #[test]
    fn strips_and_colours_sgr() {
        // red "ERR" then reset then "ok"
        let s = parse_line("\x1b[31mERR\x1b[0mok", None);
        assert_eq!(s.text, "ERRok");
        assert_eq!(s.spans.len(), 1);
        assert_eq!(s.spans[0].start, 0);
        assert_eq!(s.spans[0].len, 3);
        assert_eq!(s.spans[0].rgb, 0xCD3131);
    }

    #[test]
    fn strips_non_sgr_csi() {
        // Clear-line and cursor moves must vanish without leaving residue.
        let s = parse_line("a\x1b[2Kb\x1b[Hc", None);
        assert_eq!(s.text, "abc");
        assert!(s.spans.is_empty());
    }

    #[test]
    fn bold_and_truecolor() {
        let s = parse_line("\x1b[1;38;2;255;0;0mX", None);
        assert_eq!(s.text, "X");
        assert_eq!(s.spans.len(), 1);
        assert!(s.spans[0].bold);
        assert_eq!(s.spans[0].rgb, 0xFF0000);
    }

    #[test]
    fn standard_and_bright_backgrounds_reset_independently() {
        let s = parse_line("\x1b[41mred\x1b[104mblue\x1b[49mplain", None);
        assert_eq!(s.text, "redblueplain");
        assert_eq!(s.spans.len(), 2);
        assert_eq!(s.spans[0].bg, 0xCD3131);
        assert_eq!(s.spans[0].rgb, ColorSpan::NO_COLOR);
        assert_eq!(s.spans[1].bg, 0x3B8EEA);
        assert_eq!(s.spans[1].rgb, ColorSpan::NO_COLOR);
    }

    #[test]
    fn extended_backgrounds_support_xterm_and_truecolor() {
        let s = parse_line("\x1b[48;5;202mX\x1b[48;2;1;2;3mY", None);
        assert_eq!(s.text, "XY");
        assert_eq!(s.spans.len(), 2);
        assert_eq!(s.spans[0].bg, xterm256(202));
        assert_eq!(s.spans[1].bg, 0x010203);
    }

    #[test]
    fn unterminated_escape_does_not_panic() {
        let s = parse_line("text\x1b[", None);
        assert_eq!(s.text, "text");
    }

    #[test]
    fn cursor_passes_through_when_no_escapes() {
        let s = parse_line("hello", Some(3));
        assert_eq!(s.cursor, Some(3));
    }

    #[test]
    fn cursor_before_escape_lands_at_its_stripped_position() {
        // Cursor sits right after "abc" (raw offset 8: 5 bytes of "\x1b[32m"
        // + 3 of "abc") — right before the color reset that follows it in the
        // raw text. That reset is zero-width once stripped, so the cursor
        // must land at the same clean-text offset either way.
        let s = parse_line("\x1b[32mabc\x1b[0m", Some(8));
        assert_eq!(s.text, "abc");
        assert_eq!(s.cursor, Some(3));
    }

    #[test]
    fn cursor_after_escape_shifts_back_with_stripped_bytes() {
        // Raw offset 8 is the very end of "\x1b[32mabc" (5 raw bytes of
        // "\x1b[32m" + 3 of "abc"); in the clean 3-byte "abc" that's offset 3.
        let s = parse_line("\x1b[32mabc", Some(8));
        assert_eq!(s.text, "abc");
        assert_eq!(s.cursor, Some(3));
    }

    #[test]
    fn cursor_mid_word_lands_between_the_right_characters() {
        let s = parse_line("\x1b[32mabc", Some(6)); // 5 (prefix) + 1 ("a")
        assert_eq!(s.text, "abc");
        assert_eq!(s.cursor, Some(1));
    }

    #[test]
    fn cursor_past_end_clamps_to_stripped_text_len() {
        let s = parse_line("\x1b[32mabc\x1b[0m", Some(999));
        assert_eq!(s.text, "abc");
        assert_eq!(s.cursor, Some(3));
    }

    /// A two-byte escape whose second byte belongs to a multi-byte character —
    /// which is what a line of garbage looks like once the framer has replaced
    /// its invalid bytes with `·` — used to step the parser into the middle of
    /// that character, and the next slice of `input` then panicked on a
    /// non-boundary index, taking the whole app down with it.
    #[test]
    fn a_two_byte_escape_over_a_multibyte_char_does_not_split_it() {
        let s = parse_line("a\x1b\u{b7}b", None);
        assert_eq!(s.text, "ab", "the escape swallows the whole character");

        // The same shape at the very end of the line, and with the widest
        // character UTF-8 has, in case the step runs off the end instead.
        assert_eq!(parse_line("a\x1b\u{b7}", None).text, "a");
        assert_eq!(parse_line("a\x1b\u{1f600}b", None).text, "ab");

        // A whole line of garbage, framed the way the reader frames it: every
        // invalid byte became `·`, and the ESCs among them land against one.
        let garbage: Vec<u8> = (0u8..=255).rev().collect();
        let (text, _) = crate::framer::sanitize_utf8(&garbage);
        let _ = parse_line(&text, Some(text.len() / 2));
    }
}
