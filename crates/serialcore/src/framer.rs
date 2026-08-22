//! Byte chunks → lines, with partial-line handling.
//!
//! The framer holds a tail buffer for the in-progress line. It is fed
//! `(&[u8], Timestamp)` and emits completed lines. See spec §7.2–§7.4.
//!
//! Correctness rules exercised by the property tests:
//! - `\n`, `\r\n`, and bare `\r` all terminate a line.
//! - A `\r` at the very end of a chunk is ambiguous (possible `\r\n` split
//!   across the boundary); it is resolved on the next chunk.
//! - Terminators are stripped from stored content.
//! - Content is decoded UTF-8-lossily, invalid bytes → `·` (U+00B7), line
//!   flagged `INVALID_UTF8`.
//! - Lines are capped at [`MAX_LINE_LEN`]; overflow forces a break flagged
//!   `TRUNCATED`.
//! - Timestamp is that of the chunk carrying the line's *first* byte (§7.3).
//! - In [`TerminalMode::Vt100`], the CSI cursor-left/right and erase-in-line
//!   sequences (`ESC[nD`, `ESC[nC`, `ESC[K`) are executed against the tail —
//!   this is how a real terminal edits a line in place, and readline relies
//!   on it (e.g. history recall on the up arrow). Every other escape sequence
//!   (SGR color, window titles, ...) is left in the text untouched for
//!   [`crate::ansi::parse_line`] to interpret at display time.

use crate::clock::Timestamp;
use crate::config::TerminalMode;
use crate::store::LineFlags;
use memchr::{memchr, memchr2, memchr3};

/// Hard cap on a single line's byte length (spec §7.2). A device stuck emitting
/// bytes with no newline must not grow the tail without bound.
pub const MAX_LINE_LEN: usize = 64 * 1024;

/// A line produced by the framer, ready to become an [`crate::store::IncomingLine`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FramedLine {
    /// Sanitized UTF-8, terminator stripped.
    pub text: String,
    /// Arrival timestamp of the line's first byte.
    pub ts: Timestamp,
    pub flags: LineFlags,
    /// The live edit cursor's byte offset into `text`, for a still-open
    /// (`PROVISIONAL`) line only — e.g. where a shell would show its caret
    /// mid-prompt. `None` for a terminated line; the cursor stops mattering
    /// once a line is done.
    pub cursor: Option<usize>,
}

/// Parser state for a VT100/ANSI escape sequence in progress, carried across
/// chunk boundaries the same way `pending_cr` is. Only entered in [`TerminalMode::Vt100`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum EscState {
    /// Not inside an escape sequence.
    None,
    /// Saw ESC (0x1B); the next byte decides CSI (`[`), OSC (`]`), or a bare
    /// two-byte escape (anything else).
    Esc,
    /// Inside `ESC [ params... final`. `params` collects the parameter bytes
    /// (`0`-`9`, `;`, and friends) seen so far.
    Csi { params: Vec<u8> },
    /// Inside `ESC ] ... BEL` or `ESC ] ... ESC \` (OSC, e.g. a window-title
    /// sequence). Consumed and dropped; we don't act on it.
    Osc,
    /// Inside OSC, just saw ESC — one more byte (`\`) confirms the ST terminator.
    OscEsc,
}

/// Stateful line framer. One per port.
pub struct Framer {
    mode: TerminalMode,
    tail: Vec<u8>,
    tail_ts: Option<Timestamp>,
    /// Byte offset into `tail` where the next visible character is written.
    /// Equal to `tail.len()` except while a CSI cursor-movement escape (e.g.
    /// `ESC[nD`, sent by readline to reposition mid-line) has moved it back.
    cursor: usize,
    /// Previous chunk ended with a `\r` whose meaning depends on the next chunk:
    /// followed by `\n` it is a CRLF (swallow the LF), otherwise a bare CR.
    pending_cr: bool,
    /// An escape sequence begun in a previous chunk that hasn't seen its
    /// terminating byte yet.
    esc: EscState,
    /// Every byte of the in-progress escape sequence (including the leading
    /// ESC), in case it turns out not to be one of the cursor/erase commands
    /// we act on — then it's written to the tail verbatim. Colour (SGR) and
    /// other escapes are display concerns for [`crate::ansi::parse_line`]
    /// downstream, not the framer's; it must not consume bytes it doesn't
    /// itself need to interpret.
    esc_raw: Vec<u8>,
    /// We already emitted the current tail as PROVISIONAL, so the next emission
    /// for this line is a CONTINUATION that replaces it.
    emitted_provisional: bool,
    /// Arrival stamp of the most recent chunk. Stands in for `tail_ts` on a
    /// line with no content bytes of its own — a blank line, or one a bare
    /// CR wiped — whose only byte is the terminator, and which therefore
    /// never reached `append_to_tail` to be stamped.
    last_ts: Option<Timestamp>,
}

impl Default for Framer {
    fn default() -> Self {
        Framer::new()
    }
}

impl Framer {
    /// A framer with the legacy `Classic` CR handling (used by tests).
    pub fn new() -> Framer {
        Framer::with_mode(TerminalMode::Classic)
    }

    /// A framer with an explicit terminal mode (spec §7.2).
    pub fn with_mode(mode: TerminalMode) -> Framer {
        Framer {
            mode,
            tail: Vec::new(),
            tail_ts: None,
            cursor: 0,
            pending_cr: false,
            esc: EscState::None,
            esc_raw: Vec::new(),
            emitted_provisional: false,
            last_ts: None,
        }
    }

    /// Forget the partially-framed line and any escape sequence in flight.
    /// Used when the console is cleared: the bytes of the open line are gone
    /// from the view and from the capture, so completing that line later would
    /// resurrect text the user just deleted.
    pub fn reset(&mut self) {
        *self = Framer::with_mode(self.mode);
    }

    /// Feed a chunk stamped at arrival. Completed lines are pushed onto `out`.
    pub fn push(&mut self, chunk: &[u8], ts: Timestamp, out: &mut Vec<FramedLine>) {
        let mut i = 0;
        self.last_ts = Some(ts);

        if self.pending_cr {
            self.pending_cr = false;
            let lf_next = !chunk.is_empty() && chunk[0] == b'\n';
            match self.mode {
                // The line was already emitted; swallow a trailing-CR's LF.
                TerminalMode::Classic => {
                    if lf_next {
                        i = 1;
                    }
                }
                // Emission was deferred: resolve the CR now.
                TerminalMode::Vt100 => {
                    if lf_next {
                        self.emit_line(out); // it was \r\n
                        i = 1;
                    } else {
                        self.rewind_tail(); // bare \r: overwrite the line
                    }
                }
                TerminalMode::LfOnly => {} // CR was already dropped
            }
        }

        // An escape sequence begun in a previous chunk (e.g. `ESC[12` split
        // across reads) picks up where it left off, before anything else.
        if self.esc != EscState::None {
            i = self.consume_escape(chunk, i, ts, out);
        }

        while i < chunk.len() {
            // Classic mode frames only on CR/LF. The terminal modes also honour a
            // backspace (0x08) as an erase within the current line. VT100 mode
            // additionally watches for ESC, so it can interpret the cursor-move
            // and erase-in-line sequences a real terminal (e.g. readline history
            // recall) relies on to edit the line in place.
            let found = match self.mode {
                TerminalMode::Classic => memchr2(b'\r', b'\n', &chunk[i..]),
                TerminalMode::LfOnly => memchr3(b'\r', b'\n', 0x08, &chunk[i..]),
                TerminalMode::Vt100 => {
                    let special = memchr3(b'\r', b'\n', 0x08, &chunk[i..]);
                    let esc = memchr(0x1b, &chunk[i..]);
                    match (special, esc) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (Some(a), None) => Some(a),
                        (None, Some(b)) => Some(b),
                        (None, None) => None,
                    }
                }
            };
            match found {
                Some(rel) => {
                    let pos = i + rel;
                    self.append_to_tail(&chunk[i..pos], ts, out);
                    match chunk[pos] {
                        b'\n' => {
                            self.emit_line(out);
                            i = pos + 1;
                        }
                        0x08 => {
                            // Only reached in the terminal modes: erase one char.
                            self.erase_last_char();
                            i = pos + 1;
                        }
                        0x1b => {
                            // Only reached in VT100 mode: start an escape sequence.
                            self.esc = EscState::Esc;
                            self.esc_raw.clear();
                            self.esc_raw.push(0x1b);
                            i = self.consume_escape(chunk, pos + 1, ts, out);
                        }
                        _ => {
                            // A carriage return; behavior depends on the mode.
                            i = self.handle_cr(chunk, pos, out);
                        }
                    }
                }
                None => {
                    self.append_to_tail(&chunk[i..], ts, out);
                    i = chunk.len();
                }
            }
        }
    }

    /// Consume bytes of an in-progress escape sequence starting at `chunk[i..]`.
    /// The line-editing commands readline actually relies on for e.g. history
    /// recall (cursor left/right, erase-in-line) are executed against the
    /// tail. Everything else — SGR colors, window titles, cursor positioning,
    /// etc. — isn't the framer's concern; it's written to the tail verbatim,
    /// unchanged, exactly as if this parser didn't exist, so it survives for
    /// [`crate::ansi::parse_line`] to interpret at display time. Returns the
    /// index just past the sequence, or `chunk.len()` if it isn't finished yet
    /// (state carries over to the next `push`).
    fn consume_escape(
        &mut self,
        chunk: &[u8],
        mut i: usize,
        ts: Timestamp,
        out: &mut Vec<FramedLine>,
    ) -> usize {
        while i < chunk.len() {
            let b = chunk[i];
            i += 1;
            self.esc_raw.push(b);
            match &mut self.esc {
                EscState::None => return i - 1,
                EscState::Esc => match b {
                    b'[' => self.esc = EscState::Csi { params: Vec::new() },
                    b']' => self.esc = EscState::Osc,
                    _ => self.esc = EscState::None, // simple two-byte escape, done
                },
                EscState::Csi { params } => {
                    if (0x30..=0x3F).contains(&b) {
                        params.push(b); // parameter byte (digits, ';', ...)
                    } else if (0x20..=0x2F).contains(&b) {
                        // intermediate byte; none of our recognized commands use one
                    } else if (0x40..=0x7E).contains(&b) {
                        let params = std::mem::take(params);
                        self.esc = EscState::None;
                        if self.apply_csi(b, &params) {
                            self.esc_raw.clear(); // handled: don't leak it as text
                        }
                    } else {
                        self.esc = EscState::None; // malformed; treat as opaque text
                    }
                }
                EscState::Osc => match b {
                    0x07 => self.esc = EscState::None, // BEL terminates OSC
                    0x1b => self.esc = EscState::OscEsc,
                    _ => {}
                },
                EscState::OscEsc => {
                    // `ESC \` (ST) terminates OSC; anything else, stay in OSC.
                    self.esc = if b == b'\\' {
                        EscState::None
                    } else {
                        EscState::Osc
                    };
                }
            }
            if self.esc == EscState::None {
                if !self.esc_raw.is_empty() {
                    let raw = std::mem::take(&mut self.esc_raw);
                    self.append_to_tail(&raw, ts, out);
                }
                return i;
            }
        }
        i
    }

    /// Act on a completed CSI sequence (`ESC [ params final`) if it's one of
    /// the line-editing commands readline actually uses: cursor left/right
    /// (`D`/`C`) and erase-in-line/erase-in-display (`K`/`J`). We only ever
    /// track one logical line, so "erase in display" collapses to the same
    /// thing as "erase in line". Returns whether it was handled — callers use
    /// that to decide whether the raw bytes still need to be preserved as
    /// text (everything not handled here, e.g. SGR colors, is display-only
    /// and left for [`crate::ansi::parse_line`]).
    fn apply_csi(&mut self, final_byte: u8, params: &[u8]) -> bool {
        let param = |idx: usize, default: usize| -> usize {
            params
                .split(|&b| b == b';')
                .nth(idx)
                .filter(|s| !s.is_empty())
                .and_then(|s| std::str::from_utf8(s).ok())
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(default)
        };
        match final_byte {
            b'D' => {
                self.move_cursor_left(param(0, 1).max(1));
                true
            }
            b'C' => {
                self.move_cursor_right(param(0, 1).max(1));
                true
            }
            b'K' | b'J' => {
                self.erase_in_line(param(0, 0));
                true
            }
            _ => false,
        }
    }

    /// Move the cursor left by `n` *visible* characters (CSI `D`), without
    /// deleting anything — a real terminal's cursor-back never erases. An
    /// escape sequence sitting in the tail (e.g. an SGR color code around a
    /// prompt) is zero-width on a real terminal and must not be counted as a
    /// character here, or a colored prompt throws the column count off and
    /// the device's `n` no longer lines up with our tail.
    fn move_cursor_left(&mut self, n: usize) {
        let spans = escape_spans(&self.tail);
        for _ in 0..n {
            skip_invisible_left(&mut self.cursor, &spans);
            if self.cursor == 0 {
                break;
            }
            self.cursor = prev_char_boundary(&self.tail, self.cursor);
        }
    }

    /// Move the cursor right by `n` visible characters (CSI `C`), clamped to
    /// the tail's end. See [`Framer::move_cursor_left`] on why escape
    /// sequences are skipped rather than counted.
    fn move_cursor_right(&mut self, n: usize) {
        let spans = escape_spans(&self.tail);
        for _ in 0..n {
            skip_invisible_right(&mut self.cursor, &spans);
            if self.cursor >= self.tail.len() {
                break;
            }
            self.cursor = next_char_boundary(&self.tail, self.cursor);
        }
    }

    /// Erase-in-line/display (CSI `K`/`J`): `0` cursor→end (default), `1`
    /// start→cursor, `2` the whole line. Mode `1` blanks with spaces rather
    /// than removing bytes, matching a real terminal (erasing doesn't shift
    /// what comes after); any escape sequence in that range is left alone
    /// rather than being blanked into garbage mid-sequence.
    fn erase_in_line(&mut self, mode: usize) {
        match mode {
            1 => {
                let spans = escape_spans(&self.tail);
                for (idx, b) in self.tail[..self.cursor].iter_mut().enumerate() {
                    if !spans.iter().any(|&(s, e)| idx >= s && idx < e) {
                        *b = b' ';
                    }
                }
            }
            2 => {
                self.tail.clear();
                self.cursor = 0;
            }
            _ => self.tail.truncate(self.cursor),
        }
    }

    /// Erase the last UTF-8 character from the tail (backspace). Handles the
    /// common `\b \b` erase pattern: the CR-overwrite of a space then a second
    /// backspace nets to removing one character.
    fn erase_last_char(&mut self) {
        while let Some(&b) = self.tail.last() {
            self.tail.pop();
            // Stop after removing a lead/ASCII byte (not a UTF-8 continuation).
            if b & 0xC0 != 0x80 {
                break;
            }
        }
        self.cursor = self.cursor.min(self.tail.len());
    }

    /// Handle a `\r` at `pos`, returning the index to resume at. A `\r` that is
    /// the final byte of the chunk is ambiguous (possible `\r\n` across the
    /// boundary) and defers to `pending_cr`.
    fn handle_cr(&mut self, chunk: &[u8], pos: usize, out: &mut Vec<FramedLine>) -> usize {
        let last = pos + 1 >= chunk.len();
        let lf_follows = !last && chunk[pos + 1] == b'\n';
        match self.mode {
            TerminalMode::Classic => {
                self.emit_line(out);
                if last {
                    self.pending_cr = true;
                    pos + 1
                } else if lf_follows {
                    pos + 2
                } else {
                    pos + 1
                }
            }
            TerminalMode::Vt100 => {
                if last {
                    // Defer: could be \r\n (terminate) or bare \r (overwrite).
                    self.pending_cr = true;
                    pos + 1
                } else if lf_follows {
                    self.emit_line(out); // \r\n terminates
                    pos + 2
                } else {
                    self.rewind_tail(); // bare \r overwrites the line
                    pos + 1
                }
            }
            TerminalMode::LfOnly => {
                // Drop the CR entirely; the tail before it is retained.
                pos + 1
            }
        }
    }

    /// Carriage-return overwrite: discard the current line's accumulated bytes so
    /// following text starts from column zero. A provisional already shown for
    /// this line stays "open" so its replacement carries CONTINUATION.
    fn rewind_tail(&mut self) {
        self.tail.clear();
        self.tail_ts = None;
        self.cursor = 0;
    }

    /// Emit the current tail as a PROVISIONAL line without terminating it
    /// (spec §7.4). Called after ~100ms of silence. Subsequent emissions for the
    /// same line carry CONTINUATION so the store replaces the provisional line.
    /// Returns `None` if there is nothing pending.
    pub fn flush_provisional(&mut self) -> Option<FramedLine> {
        if self.tail.is_empty() {
            return None;
        }
        let ts = self.tail_ts?;
        let (text, invalid, cursor) = sanitize_utf8_tracking(&self.tail, Some(self.cursor));
        let mut flags = LineFlags::PROVISIONAL;
        if invalid {
            flags.insert(LineFlags::INVALID_UTF8);
        }
        if self.emitted_provisional {
            flags.insert(LineFlags::CONTINUATION);
        }
        self.emitted_provisional = true;
        Some(FramedLine {
            text,
            ts,
            flags,
            cursor,
        })
    }

    /// Flush any pending tail as a final (terminated) line and reset the framer.
    /// Called whenever the byte stream ends — a one-shot source exhausted, or a
    /// connection lost — so the last unterminated line is neither lost nor left
    /// open. Closing it matters beyond the bytes: an open line is emitted
    /// `PROVISIONAL` and renders a caret, which would otherwise sit blinking on
    /// a line that can no longer grow. The rest of the per-line state goes with
    /// it (a half-parsed escape, an ambiguous trailing `\r`, the
    /// already-emitted-provisional bookkeeping), so a stream that starts again
    /// afterwards — a reconnect — begins a fresh line instead of continuing one
    /// that belonged to the stream that ended.
    pub fn flush_final(&mut self, out: &mut Vec<FramedLine>) {
        if !self.tail.is_empty() {
            self.emit_line(out);
        }
        self.reset();
    }

    fn append_to_tail(&mut self, bytes: &[u8], ts: Timestamp, out: &mut Vec<FramedLine>) {
        if bytes.is_empty() {
            return;
        }
        if self.tail_ts.is_none() {
            self.tail_ts = Some(ts);
        }
        let mut remaining = bytes;
        // A prior CSI cursor-back left us mid-line: new bytes overwrite the
        // existing content there (as a real terminal would) rather than being
        // appended past it.
        if self.cursor < self.tail.len() {
            let overwrite_len = remaining.len().min(self.tail.len() - self.cursor);
            self.tail[self.cursor..self.cursor + overwrite_len]
                .copy_from_slice(&remaining[..overwrite_len]);
            self.cursor += overwrite_len;
            remaining = &remaining[overwrite_len..];
        }
        // Enforce the length cap, breaking the line as many times as needed.
        while self.tail.len() + remaining.len() > MAX_LINE_LEN {
            let space = MAX_LINE_LEN - self.tail.len();
            self.tail.extend_from_slice(&remaining[..space]);
            remaining = &remaining[space..];
            self.emit_truncated(out);
        }
        self.tail.extend_from_slice(remaining);
        // From here on `remaining` only ever holds bytes that extend the tail
        // (the overwrite phase above already consumed any that landed inside
        // it), so the cursor simply advances past what was just appended —
        // it must not jump to `tail.len()` unconditionally, or a write that
        // stopped short of the tail's end (leaving trailing old content
        // beyond the cursor) would wrongly be treated as if it reached it.
        self.cursor += remaining.len();
    }

    /// The stamp for a line about to be emitted: the arrival of its first
    /// content byte, or — for a line that has none, a blank one or one a
    /// bare CR wiped — the arrival of the chunk carrying its terminator.
    /// Neither is only possible on a framer nothing was ever pushed to, which
    /// no emit path can reach. A zero `micros` is the wrong answer for a real
    /// line: it places it at the session start, so its delta and its distance
    /// from a mark read as wildly off while its wall clock reads correctly.
    fn stamp(&self) -> Timestamp {
        self.tail_ts.or(self.last_ts).unwrap_or(Timestamp {
            wall: chrono::Utc::now(),
            micros: 0,
        })
    }

    /// Emit the tail as a completed line and reset per-line state.
    fn emit_line(&mut self, out: &mut Vec<FramedLine>) {
        let ts = self.stamp();
        let (text, invalid) = sanitize_utf8(&self.tail);
        let mut flags = LineFlags::default();
        if invalid {
            flags.insert(LineFlags::INVALID_UTF8);
        }
        if self.emitted_provisional {
            flags.insert(LineFlags::CONTINUATION);
        }
        out.push(FramedLine {
            text,
            ts,
            flags,
            cursor: None,
        });
        self.reset_line();
    }

    /// Emit a forced break for an over-length line and keep accumulating the rest.
    fn emit_truncated(&mut self, out: &mut Vec<FramedLine>) {
        let ts = self.stamp();
        let (text, invalid) = sanitize_utf8(&self.tail);
        let mut flags = LineFlags::TRUNCATED;
        if invalid {
            flags.insert(LineFlags::INVALID_UTF8);
        }
        if self.emitted_provisional {
            flags.insert(LineFlags::CONTINUATION);
        }
        out.push(FramedLine {
            text,
            ts,
            flags,
            cursor: None,
        });
        // Keep the same tail_ts: the broken-up segments belong to one arrival.
        self.tail.clear();
        self.cursor = 0;
        self.emitted_provisional = false;
    }

    fn reset_line(&mut self) {
        self.tail.clear();
        self.tail_ts = None;
        self.cursor = 0;
        self.emitted_provisional = false;
    }

    /// Bytes currently buffered in the incomplete tail (for tests/metrics).
    pub fn tail_len(&self) -> usize {
        self.tail.len()
    }
}

/// The byte offset of the UTF-8 character boundary immediately before `idx`.
fn prev_char_boundary(bytes: &[u8], idx: usize) -> usize {
    let mut i = idx;
    while i > 0 {
        i -= 1;
        if bytes[i] & 0xC0 != 0x80 {
            break;
        }
    }
    i
}

/// The byte offset of the UTF-8 character boundary immediately after `idx`.
fn next_char_boundary(bytes: &[u8], idx: usize) -> usize {
    let mut i = idx;
    if i < bytes.len() {
        i += 1;
        while i < bytes.len() && bytes[i] & 0xC0 == 0x80 {
            i += 1;
        }
    }
    i
}

/// The `[start, end)` ranges in `tail` occupied by escape sequences that were
/// passed through as opaque text (see [`Framer::consume_escape`]). Used to
/// make cursor movement count real terminal columns instead of raw bytes.
/// Every escape blob written to the tail is complete (never a partial one
/// left mid-parse), so re-parsing it here is safe and exact.
fn escape_spans(tail: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < tail.len() {
        if tail[i] == 0x1b {
            let end = skip_one_escape(tail, i);
            spans.push((i, end));
            i = end;
        } else {
            i += 1;
        }
    }
    spans
}

/// Given `tail[i] == ESC`, the offset just past the complete escape sequence
/// starting there (mirrors the grammar in [`Framer::consume_escape`]).
fn skip_one_escape(tail: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    if j >= tail.len() {
        return tail.len();
    }
    match tail[j] {
        b'[' => {
            j += 1;
            while j < tail.len() && !(0x40..=0x7E).contains(&tail[j]) {
                j += 1;
            }
            (j + 1).min(tail.len())
        }
        b']' => {
            j += 1;
            while j < tail.len() {
                if tail[j] == 0x07 {
                    return j + 1;
                }
                if tail[j] == 0x1b && j + 1 < tail.len() && tail[j + 1] == b'\\' {
                    return j + 2;
                }
                j += 1;
            }
            tail.len()
        }
        _ => (j + 1).min(tail.len()),
    }
}

/// If `cursor` sits exactly at the end of an escape span, jump to its start
/// (repeatedly, in case of adjacent spans) so it doesn't get counted as a
/// visible character to step over.
fn skip_invisible_left(cursor: &mut usize, spans: &[(usize, usize)]) {
    while let Some(&(start, _)) = spans.iter().find(|&&(_, end)| end == *cursor) {
        *cursor = start;
    }
}

/// If `cursor` sits exactly at the start of an escape span, jump to its end
/// (repeatedly, in case of adjacent spans).
fn skip_invisible_right(cursor: &mut usize, spans: &[(usize, usize)]) {
    while let Some(&(_, end)) = spans.iter().find(|&&(start, _)| start == *cursor) {
        *cursor = end;
    }
}

/// Decode `bytes` as UTF-8, replacing each invalid byte with `·` (U+00B7).
/// Returns the string and whether any replacement occurred.
pub fn sanitize_utf8(bytes: &[u8]) -> (String, bool) {
    let (text, invalid, _) = sanitize_utf8_tracking(bytes, None);
    (text, invalid)
}

/// [`sanitize_utf8`], but also translates a raw byte offset into `bytes`
/// (e.g. the live edit cursor) into the corresponding offset in the
/// sanitized output. A straight copy would do for valid UTF-8, but an
/// invalid run is replaced 1:N with `·` (U+00B7, 2 bytes each) — this walks
/// the same substitution once and reports where `track` landed, rather than
/// assuming byte offsets carry over unchanged. `track` is clamped to
/// `bytes.len()`; once resolved it is always `Some` (when `track` was).
fn sanitize_utf8_tracking(bytes: &[u8], track: Option<usize>) -> (String, bool, Option<usize>) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), false, track.map(|t| t.min(s.len()))),
        Err(_) => {
            let mut out = String::with_capacity(bytes.len());
            let mut invalid = false;
            let mut i = 0;
            let mut resolved = None;
            while i < bytes.len() {
                match std::str::from_utf8(&bytes[i..]) {
                    Ok(s) => {
                        if resolved.is_none() {
                            if let Some(t) = track {
                                if t <= i + s.len() {
                                    resolved = Some(out.len() + (t - i));
                                }
                            }
                        }
                        out.push_str(s);
                        break;
                    }
                    Err(e) => {
                        let valid = e.valid_up_to();
                        if resolved.is_none() {
                            if let Some(t) = track {
                                if t <= i + valid {
                                    resolved = Some(out.len() + (t - i));
                                }
                            }
                        }
                        if valid > 0 {
                            // valid_up_to is a guaranteed UTF-8 boundary.
                            out.push_str(std::str::from_utf8(&bytes[i..i + valid]).unwrap_or(""));
                        }
                        i += valid;
                        let bad = e.error_len().unwrap_or(bytes.len() - i);
                        if resolved.is_none() {
                            if let Some(t) = track {
                                if t < i + bad {
                                    // Cursor pointed at one of the bad bytes: land it
                                    // right after that byte's replacement glyph.
                                    let dot = '\u{00B7}'.len_utf8();
                                    resolved = Some(out.len() + (t - i + 1) * dot);
                                }
                            }
                        }
                        for _ in 0..bad {
                            out.push('\u{00B7}');
                        }
                        invalid = true;
                        i += bad;
                    }
                }
            }
            if resolved.is_none() {
                // Offset was at/after the very end of `bytes`.
                resolved = track.map(|_| out.len());
            }
            (out, invalid, resolved)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SessionClock;

    fn ts(micros: i64) -> Timestamp {
        Timestamp {
            wall: chrono::Utc::now(),
            micros,
        }
    }

    fn texts(lines: &[FramedLine]) -> Vec<String> {
        lines.iter().map(|l| l.text.clone()).collect()
    }

    /// Feed the whole input as a single chunk.
    fn frame_whole(input: &[u8]) -> Vec<FramedLine> {
        let mut f = Framer::new();
        let mut out = Vec::new();
        f.push(input, ts(0), &mut out);
        f.flush_final(&mut out);
        out
    }

    #[test]
    fn basic_lf() {
        let out = frame_whole(b"a\nb\nc\n");
        assert_eq!(texts(&out), vec!["a", "b", "c"]);
    }

    #[test]
    fn crlf_and_bare_cr() {
        assert_eq!(texts(&frame_whole(b"a\r\nb\rc\n")), vec!["a", "b", "c"]);
    }

    fn frame_whole_mode(input: &[u8], mode: TerminalMode) -> Vec<FramedLine> {
        let mut f = Framer::with_mode(mode);
        let mut out = Vec::new();
        f.push(input, ts(0), &mut out);
        f.flush_final(&mut out);
        out
    }

    #[test]
    fn vt100_bare_cr_overwrites() {
        // Progress-bar style: each \r rewinds; only \n ends the line.
        let out = frame_whole_mode(b"10%\r50%\r100%\ndone\n", TerminalMode::Vt100);
        assert_eq!(texts(&out), vec!["100%", "done"]);
    }

    #[test]
    fn vt100_crlf_still_terminates() {
        assert_eq!(
            texts(&frame_whole_mode(b"a\r\nb\r\n", TerminalMode::Vt100)),
            vec!["a", "b"]
        );
    }

    #[test]
    fn vt100_cr_overwrite_across_chunks() {
        let mut f = Framer::with_mode(TerminalMode::Vt100);
        let mut out = Vec::new();
        f.push(b"abc\r", ts(0), &mut out); // trailing \r is ambiguous
        f.push(b"def\n", ts(1), &mut out); // not \n → bare \r overwrote "abc"
        assert_eq!(texts(&out), vec!["def"]);

        let mut out2 = Vec::new();
        let mut f2 = Framer::with_mode(TerminalMode::Vt100);
        f2.push(b"abc\r", ts(0), &mut out2);
        f2.push(b"\ndef\n", ts(1), &mut out2); // \n → it was \r\n
        assert_eq!(texts(&out2), vec!["abc", "def"]);
    }

    #[test]
    fn vt100_backspace_erases() {
        // A bare backspace erases the previous char.
        assert_eq!(
            texts(&frame_whole_mode(b"abc\x08d\n", TerminalMode::Vt100)),
            vec!["abd"]
        );
        // The shell's "\b \b" erase pattern nets to one removed char.
        assert_eq!(
            texts(&frame_whole_mode(b"abc\x08 \x08\n", TerminalMode::Vt100)),
            vec!["ab"]
        );
    }

    #[test]
    fn vt100_csi_cursor_left_overwrites_in_place() {
        // Readline-style history recall: the shell backs the cursor up over
        // the typed text with `ESC[nD` and writes the recalled command over
        // it, rather than resending `\r` + a full redraw.
        let out = frame_whole_mode(b"foo\x1b[3Dls -la\n", TerminalMode::Vt100);
        assert_eq!(texts(&out), vec!["ls -la"]);
    }

    #[test]
    fn vt100_csi_erase_to_eol_trims_leftover() {
        // Recalling a *shorter* history entry: cursor backs up over the old
        // text, the new (shorter) text overwrites the front of it, and
        // `ESC[K` erases what's left of the old text instead of it lingering
        // as trailing garbage appended after the new command.
        let out = frame_whole_mode(b"hello world\x1b[11Dhi\x1b[K\n", TerminalMode::Vt100);
        assert_eq!(texts(&out), vec!["hi"]);
    }

    #[test]
    fn vt100_csi_cursor_left_across_chunks() {
        let mut f = Framer::with_mode(TerminalMode::Vt100);
        let mut out = Vec::new();
        f.push(b"abc\x1b[2", ts(0), &mut out); // split mid-parameter
        f.push(b"DX\n", ts(1), &mut out);
        assert_eq!(texts(&out), vec!["aXc"]);
    }

    #[test]
    fn vt100_unrecognized_escapes_pass_through_unchanged() {
        // SGR color codes are the concern of `ansi::parse_line`, which runs
        // on the framer's output at display time — the framer must leave
        // them in the text untouched rather than stripping them itself, or
        // colored Linux prompts would lose their color.
        assert_eq!(
            texts(&frame_whole_mode(
                b"\x1b[31mred\x1b[0m\n",
                TerminalMode::Vt100
            )),
            vec!["\x1b[31mred\x1b[0m"]
        );
        // Likewise an OSC window-title sequence: not our concern, passed through.
        assert_eq!(
            texts(&frame_whole_mode(
                b"\x1b]0;title\x07after\n",
                TerminalMode::Vt100
            )),
            vec!["\x1b]0;title\x07after"]
        );
    }

    #[test]
    fn vt100_csi_cursor_move_still_intercepted_amid_sgr_color() {
        // A colored prompt combined with readline's cursor-back-and-overwrite
        // history recall: the color codes pass through untouched while the
        // cursor move and overwrite are still applied correctly.
        let out = frame_whole_mode(b"\x1b[32mfoo\x1b[3Dbar\x1b[0m\n", TerminalMode::Vt100);
        assert_eq!(texts(&out), vec!["\x1b[32mbar\x1b[0m"]);
    }

    #[test]
    fn vt100_colored_prompt_redraw_survives_async_log() {
        // Reproduces a real device trace (Zephyr's shell): a colored,
        // unterminated prompt ("usr:~$ " in bold green) gets redrawn with
        // `ESC[nD` (cursor back over the *visible* prompt) + `ESC[J` (erase
        // to end) + a fresh colored reprint, not a bare `\r`. Before this
        // fix, `ESC[J` was unhandled (leaked into the text) and cursor math
        // counted the color codes' own bytes as columns, so `ESC[7D` landed
        // mid-prompt instead of at its start — corrupting it down to "usr"
        // garbage smeared across every log line.
        let mut f = Framer::with_mode(TerminalMode::Vt100);
        let mut out = Vec::new();
        f.push(b"\x1b[1;32musr:~$ \x1b[m", ts(0), &mut out);
        f.push(b"\x1b[7D\x1b[J\x1b[1;32musr:~$ \x1b[m", ts(1), &mut out);
        let prov = f.flush_provisional().unwrap();
        let styled = crate::ansi::parse_line(&prov.text, prov.cursor);
        assert_eq!(styled.text, "usr:~$ ");
        assert_eq!(styled.spans.len(), 1);
        assert_eq!(styled.spans[0].rgb, 0x0DBC79);
        assert!(styled.spans[0].bold);
        assert_eq!(
            styled.cursor,
            Some(7),
            "cursor at the end of the reprinted prompt"
        );
    }

    #[test]
    fn lf_only_strips_cr() {
        assert_eq!(
            texts(&frame_whole_mode(b"a\rb\r\nc\n", TerminalMode::LfOnly)),
            vec!["ab", "c"]
        );
    }

    #[test]
    fn blank_lines_preserved() {
        assert_eq!(texts(&frame_whole(b"a\n\nb\n")), vec!["a", "", "b"]);
    }

    #[test]
    fn crlf_split_across_chunks() {
        let mut f = Framer::new();
        let mut out = Vec::new();
        f.push(b"hello\r", ts(0), &mut out);
        // No line yet emitted beyond "hello" (the \r terminated it), and the LF
        // must be swallowed, not treated as a blank line.
        f.push(b"\nworld\n", ts(1), &mut out);
        assert_eq!(texts(&out), vec!["hello", "world"]);
    }

    #[test]
    fn invalid_utf8_becomes_middot_and_flags() {
        let out = frame_whole(&[b'a', 0xFF, 0xFE, b'b', b'\n']);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "a··b");
        assert!(out[0].flags.contains(LineFlags::INVALID_UTF8));
    }

    #[test]
    fn timestamp_is_first_byte_arrival() {
        let mut f = Framer::new();
        let mut out = Vec::new();
        f.push(b"par", ts(100), &mut out); // first bytes arrive at 100
        f.push(b"tial\n", ts(200), &mut out); // completes at 200
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "partial");
        assert_eq!(out[0].ts.micros, 100, "carries first-byte arrival time");
    }

    #[test]
    fn blank_line_carries_terminator_arrival() {
        // A line with no content bytes never reaches `append_to_tail`, so it
        // has no first-byte stamp to carry. It must still be stamped from the
        // chunk that terminated it: a zero `micros` puts it at session start,
        // which shows up as a wild negative in the from-mark column and a wild
        // positive in the delta one, while its wall clock reads correctly.
        let mut f = Framer::new();
        let mut out = Vec::new();
        f.push(b"a\n", ts(100), &mut out);
        f.push(b"\n", ts(200), &mut out);
        f.push(b"\r\n", ts(300), &mut out);
        assert_eq!(texts(&out), vec!["a", "", ""]);
        assert_eq!(out[1].ts.micros, 200, "blank line stamped on its LF");
        assert_eq!(out[2].ts.micros, 300, "blank CRLF line stamped on its CR");
    }

    #[test]
    fn cr_wiped_line_carries_terminator_arrival() {
        // A bare CR discards the content bytes framed so far, and with them
        // their stamp; what the CR leaves behind is stamped like a blank line.
        let mut f = Framer::with_mode(TerminalMode::Vt100);
        let mut out = Vec::new();
        f.push(b"typed\r", ts(100), &mut out);
        f.push(b"\r\n", ts(200), &mut out);
        assert_eq!(texts(&out), vec![""]);
        assert_eq!(out[0].ts.micros, 200);
    }

    #[test]
    fn provisional_then_continuation() {
        let mut f = Framer::new();
        let mut out = Vec::new();
        f.push(b"> ", ts(0), &mut out);
        assert!(out.is_empty(), "no terminator yet");
        let prov = f.flush_provisional().unwrap();
        assert_eq!(prov.text, "> ");
        assert_eq!(prov.cursor, Some(2), "cursor sits at the end of the prompt");
        assert!(prov.flags.contains(LineFlags::PROVISIONAL));
        assert!(!prov.flags.contains(LineFlags::CONTINUATION));

        f.push(b"ready\n", ts(1), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "> ready");
        assert_eq!(out[0].cursor, None, "a terminated line has no live cursor");
        assert!(out[0].flags.contains(LineFlags::CONTINUATION));
    }

    #[test]
    fn flush_provisional_cursor_after_csi_move() {
        // The cursor a caller would want to render as a caret must reflect
        // where CSI cursor-movement actually left it, not just "end of text".
        let mut f = Framer::with_mode(TerminalMode::Vt100);
        let mut out = Vec::new();
        f.push(b"foo\x1b[2D", ts(0), &mut out);
        let prov = f.flush_provisional().unwrap();
        assert_eq!(prov.text, "foo");
        assert_eq!(prov.cursor, Some(1), "moved back 2 from the end of \"foo\"");
    }

    #[test]
    fn flush_provisional_cursor_survives_invalid_utf8_before_it() {
        // An invalid byte before the cursor is replaced with `·` (2 bytes in
        // UTF-8, vs. the original 1 raw byte), so a naive raw-offset carried
        // straight into `text` would land short of the real cursor. Move the
        // cursor back 1 (to land right before 'c') so it's not just sitting
        // at "end of text" either way, which wouldn't exercise the mapping.
        let mut f = Framer::with_mode(TerminalMode::Vt100);
        let mut out = Vec::new();
        f.push(&[b'a', 0xFF, b'b', b'c'], ts(0), &mut out);
        f.push(b"\x1b[1D", ts(1), &mut out);
        let prov = f.flush_provisional().unwrap();
        assert_eq!(prov.text, "a·bc");
        assert_eq!(prov.cursor, Some(4));
        assert_eq!(&prov.text[prov.cursor.unwrap()..], "c");
    }

    #[test]
    fn long_line_is_truncated() {
        let mut input = vec![b'x'; MAX_LINE_LEN + 100];
        input.push(b'\n');
        let out = frame_whole(&input);
        assert_eq!(out.len(), 2);
        assert!(out[0].flags.contains(LineFlags::TRUNCATED));
        assert_eq!(out[0].text.len(), MAX_LINE_LEN);
        assert_eq!(out[1].text.len(), 100);
        assert!(!out[1].flags.contains(LineFlags::TRUNCATED));
    }

    #[test]
    fn unterminated_tail_not_emitted_until_flush() {
        let mut f = Framer::new();
        let mut out = Vec::new();
        f.push(b"no newline", ts(0), &mut out);
        assert!(out.is_empty());
        f.flush_final(&mut out);
        assert_eq!(texts(&out), vec!["no newline"]);
    }

    #[test]
    fn flush_final_closes_the_open_line_and_starts_the_next_one_fresh() {
        // A connection dropping mid-prompt. The prompt was already shown as a
        // provisional line (caret and all), so its final form must arrive as a
        // CONTINUATION that replaces it in place — not as a second copy — and
        // must carry no cursor, since nothing more can be typed into a line
        // whose connection is gone.
        let mut f = Framer::with_mode(TerminalMode::Vt100);
        let mut out = Vec::new();
        f.push(b"usr:~$ ", ts(0), &mut out);
        let prov = f.flush_provisional().unwrap();
        assert!(prov.flags.contains(LineFlags::PROVISIONAL));
        assert_eq!(prov.cursor, Some(7));

        f.flush_final(&mut out);
        assert_eq!(texts(&out), vec!["usr:~$ "]);
        assert!(out[0].flags.contains(LineFlags::CONTINUATION));
        assert!(!out[0].flags.contains(LineFlags::PROVISIONAL));
        assert_eq!(out[0].cursor, None);

        // Reconnected: the fresh prompt is its own line, not a continuation of
        // the one that ended with the previous connection.
        out.clear();
        f.push(b"usr:~$ \n", ts(1), &mut out);
        assert_eq!(texts(&out), vec!["usr:~$ "]);
        assert!(!out[0].flags.contains(LineFlags::CONTINUATION));
    }

    #[test]
    fn flush_final_drops_a_half_parsed_escape_and_pending_cr() {
        // Bytes cut off mid-sequence belong to the stream that ended; the next
        // connection's first byte must not be read as their continuation.
        let mut f = Framer::with_mode(TerminalMode::Vt100);
        let mut out = Vec::new();
        f.push(b"a\x1b[2", ts(0), &mut out); // truncated CSI
        f.flush_final(&mut out);
        assert_eq!(texts(&out), vec!["a"]);

        out.clear();
        f.push(b"Db\n", ts(1), &mut out); // no longer a cursor-left command
        assert_eq!(texts(&out), vec!["Db"]);

        // Same for an ambiguous trailing CR: it must not swallow the LF that
        // opens the reconnected stream.
        let mut f = Framer::with_mode(TerminalMode::Vt100);
        let mut out = Vec::new();
        f.push(b"x\r", ts(0), &mut out);
        f.flush_final(&mut out);
        assert_eq!(texts(&out), vec!["x"]);
        out.clear();
        f.push(b"\ny\n", ts(1), &mut out);
        assert_eq!(texts(&out), vec!["", "y"]);
    }

    #[test]
    fn utf8_split_across_chunk_boundary_ok() {
        // '€' is E2 82 AC. Split it across two pushes; it must reassemble.
        let mut f = Framer::new();
        let mut out = Vec::new();
        f.push(&[0xE2, 0x82], ts(0), &mut out);
        f.push(&[0xAC, b'\n'], ts(1), &mut out);
        assert_eq!(texts(&out), vec!["€"]);
        assert!(!out[0].flags.contains(LineFlags::INVALID_UTF8));
    }

    /// The core property (spec §10): splitting the same input at *any* byte
    /// boundary produces identical lines.
    #[test]
    fn split_invariance_exhaustive() {
        let clock = SessionClock::new();
        let inputs: &[&[u8]] = &[
            b"a\nb\nc\n",
            b"a\r\nb\rc\n",
            b"\r\n\r\n\r\n",
            b"no terminators here",
            b"trailing\r",
            b"mixed\r\n\rline\nend",
            &[b'x', 0xFF, b'y', b'\n', 0xC3, 0x28, b'\n'],
        ];
        for input in inputs {
            let reference = {
                let mut f = Framer::new();
                let mut out = Vec::new();
                f.push(input, clock.now(), &mut out);
                f.flush_final(&mut out);
                texts(&out)
            };
            for split in 0..=input.len() {
                let mut f = Framer::new();
                let mut out = Vec::new();
                f.push(&input[..split], clock.now(), &mut out);
                f.push(&input[split..], clock.now(), &mut out);
                f.flush_final(&mut out);
                assert_eq!(
                    texts(&out),
                    reference,
                    "input {input:?} split at {split} diverged"
                );
            }
        }
    }
}
