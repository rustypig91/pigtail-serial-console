//! File-transfer preparation shared by the GUI and the reader thread.
//!
//! Preparing a file may involve reading and validating the whole input, so the
//! GUI runs [`prepare`] on a worker thread.  The resulting flat byte buffer is
//! handed to the serial reader, which applies pacing without blocking either
//! the UI or receive capture.

use crate::config::LineEnding;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How a dropped file is interpreted before it is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferMode {
    Raw,
    Text,
    Hex,
}

impl TransferMode {
    pub fn label(self) -> &'static str {
        match self {
            TransferMode::Raw => "Raw bytes",
            TransferMode::Text => "Text lines",
            TransferMode::Hex => "Hex decoded",
        }
    }
}

/// Decoding policy for text input.  All decoded text is transmitted as UTF-8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextDecoding {
    Utf8Strict,
    Utf8Lossy,
    Latin1,
}

impl TextDecoding {
    pub fn label(self) -> &'static str {
        match self {
            TextDecoding::Utf8Strict => "UTF-8 (stop on error)",
            TextDecoding::Utf8Lossy => "UTF-8 (replace errors)",
            TextDecoding::Latin1 => "Latin-1",
        }
    }
}

/// User-selected conversion and pacing settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferOptions {
    pub mode: TransferMode,
    pub line_ending: LineEnding,
    pub text_decoding: TextDecoding,
    pub line_delay: Duration,
    pub char_delay: Duration,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            mode: TransferMode::Raw,
            line_ending: LineEnding::Lf,
            text_decoding: TextDecoding::Utf8Strict,
            line_delay: Duration::from_millis(10),
            char_delay: Duration::ZERO,
        }
    }
}

/// A fully validated transfer ready for paced writes.
#[derive(Clone, Debug)]
pub struct PreparedTransfer {
    pub path: PathBuf,
    pub data: Vec<u8>,
    /// Byte offsets immediately after each converted text line.  The reader
    /// pauses at these boundaries when line pacing is enabled.
    pub line_ends: Vec<usize>,
    pub line_delay: Duration,
    pub char_delay: Duration,
}

impl PreparedTransfer {
    pub fn total_bytes(&self) -> usize {
        self.data.len()
    }

    /// Wire-time estimate due to configured pacing.  Serial framing time is
    /// intentionally omitted because flow control and driver buffering make it
    /// less predictable than the explicit delays the user selected.
    pub fn estimated_duration(&self) -> Duration {
        duration_mul(self.char_delay, self.data.len().saturating_sub(1)).saturating_add(
            duration_mul(self.line_delay, self.line_ends.len().saturating_sub(1)),
        )
    }
}

fn duration_mul(duration: Duration, factor: usize) -> Duration {
    let nanos = duration
        .as_nanos()
        .saturating_mul(factor as u128)
        .min(Duration::MAX.as_nanos());
    Duration::new(
        (nanos / 1_000_000_000) as u64,
        (nanos % 1_000_000_000) as u32,
    )
}

/// Read, decode, and validate `path` according to `options`.
pub fn prepare(path: &Path, options: &TransferOptions) -> Result<PreparedTransfer, String> {
    let input = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let (data, line_ends) = match options.mode {
        TransferMode::Raw => (input, Vec::new()),
        TransferMode::Text => prepare_text(&input, options)?,
        TransferMode::Hex => prepare_hex(&input)?,
    };
    Ok(PreparedTransfer {
        path: path.to_owned(),
        data,
        line_ends,
        line_delay: options.line_delay,
        char_delay: options.char_delay,
    })
}

fn prepare_text(input: &[u8], options: &TransferOptions) -> Result<(Vec<u8>, Vec<usize>), String> {
    let decoded = match options.text_decoding {
        TextDecoding::Utf8Strict => std::str::from_utf8(input)
            .map_err(|e| format!("invalid UTF-8 at byte {}", e.valid_up_to()))?
            .to_owned(),
        TextDecoding::Utf8Lossy => String::from_utf8_lossy(input).into_owned(),
        TextDecoding::Latin1 => input.iter().map(|&byte| char::from(byte)).collect(),
    };

    let ending = options.line_ending.bytes();
    let mut data = Vec::with_capacity(decoded.len());
    let mut line_ends = Vec::new();
    // Accept LF, CRLF, and old-style bare CR input. An unterminated final line
    // is still a line and receives the selected outgoing ending.
    let mut chars = decoded.char_indices().peekable();
    let mut start = 0;
    while let Some((offset, ch)) = chars.next() {
        if !matches!(ch, '\r' | '\n') {
            continue;
        }
        data.extend_from_slice(&decoded.as_bytes()[start..offset]);
        data.extend_from_slice(ending);
        line_ends.push(data.len());
        start = offset + ch.len_utf8();
        if ch == '\r' && chars.peek().is_some_and(|(_, next)| *next == '\n') {
            let (lf_offset, lf) = chars.next().expect("peeked at LF");
            start = lf_offset + lf.len_utf8();
        }
    }
    if start < decoded.len() {
        data.extend_from_slice(&decoded.as_bytes()[start..]);
        data.extend_from_slice(ending);
        line_ends.push(data.len());
    }
    Ok((data, line_ends))
}

fn prepare_hex(input: &[u8]) -> Result<(Vec<u8>, Vec<usize>), String> {
    let text = std::str::from_utf8(input).map_err(|e| {
        format!(
            "hex input is not UTF-8 text (invalid byte at {})",
            e.valid_up_to()
        )
    })?;
    let mut data = Vec::new();
    let mut line_ends = Vec::new();
    for (line_index, source_line) in text.lines().enumerate() {
        let without_hash = source_line.split('#').next().unwrap_or_default();
        let line = without_hash.split("//").next().unwrap_or_default();
        let before = data.len();
        for token in line.split(|c: char| c.is_whitespace() || ",:;_".contains(c)) {
            let digits = token
                .strip_prefix("0x")
                .or_else(|| token.strip_prefix("0X"))
                .unwrap_or(token);
            if digits.is_empty() {
                continue;
            }
            if digits.len() % 2 != 0 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!(
                    "invalid hex token {token:?} on line {}",
                    line_index + 1
                ));
            }
            let (pairs, remainder) = digits.as_bytes().as_chunks::<2>();
            debug_assert!(remainder.is_empty());
            for pair in pairs {
                let pair = std::str::from_utf8(pair).expect("hex digits are ASCII");
                data.push(u8::from_str_radix(pair, 16).expect("validated hex pair"));
            }
        }
        if data.len() != before {
            line_ends.push(data.len());
        }
    }
    Ok((data, line_ends))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_normalizes_mixed_line_endings() {
        let options = TransferOptions {
            mode: TransferMode::Text,
            line_ending: LineEnding::CrLf,
            ..Default::default()
        };
        let (data, ends) = prepare_text(b"one\r\ntwo\nthree\rfour", &options).unwrap();
        assert_eq!(data, b"one\r\ntwo\r\nthree\r\nfour\r\n");
        assert_eq!(ends, vec![5, 10, 17, 23]);
    }

    #[test]
    fn strict_and_lossy_utf8_are_distinct() {
        let mut options = TransferOptions {
            mode: TransferMode::Text,
            ..Default::default()
        };
        assert!(prepare_text(b"bad \xff", &options).is_err());
        options.text_decoding = TextDecoding::Utf8Lossy;
        assert_eq!(
            prepare_text(b"bad \xff", &options).unwrap().0,
            "bad �\n".as_bytes()
        );
    }

    #[test]
    fn hex_accepts_common_separators_and_comments() {
        let (data, ends) = prepare_hex(b"0x01, 02:03 # first\ndead beef // second\n").unwrap();
        assert_eq!(data, [1, 2, 3, 0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(ends, vec![3, 7]);
        assert!(prepare_hex(b"0x123\n").is_err());
    }
}
