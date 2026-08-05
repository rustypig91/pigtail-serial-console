//! The line store: a byte arena plus a metadata `Vec`, with front eviction.
//!
//! The store is owned exclusively by the UI thread (spec §5, rule 4). Data
//! arrives only through channels. Lines are stored as bytes in one arena rather
//! than one `String` per line — a million `String`s is a million allocations
//! plus ~48 bytes overhead each (spec §6).

use crate::clock::Timestamp;

/// Identifies an open port/connection within the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortId(pub u32);

/// Per-line status flags. Packed into a `u16`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineFlags(pub u16);

impl LineFlags {
    pub const PROVISIONAL: LineFlags = LineFlags(1 << 0);
    pub const RECONNECT_MARKER: LineFlags = LineFlags(1 << 1);
    pub const TX_ECHO: LineFlags = LineFlags(1 << 2);
    pub const INVALID_UTF8: LineFlags = LineFlags(1 << 3);
    pub const TRUNCATED: LineFlags = LineFlags(1 << 4);
    /// Not stored on a line; signals the store to extend the previous
    /// provisional line rather than append a new one (spec §7.4).
    pub const CONTINUATION: LineFlags = LineFlags(1 << 5);
    /// A user bookmark.
    pub const BOOKMARK: LineFlags = LineFlags(1 << 6);

    pub fn contains(self, other: LineFlags) -> bool {
        (self.0 & other.0) == other.0
    }
    pub fn insert(&mut self, other: LineFlags) {
        self.0 |= other.0;
    }
    pub fn remove(&mut self, other: LineFlags) {
        self.0 &= !other.0;
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for LineFlags {
    type Output = LineFlags;
    fn bitor(self, rhs: LineFlags) -> LineFlags {
        LineFlags(self.0 | rhs.0)
    }
}

/// A run of coloured characters within a line, produced by ANSI SGR parsing.
/// Offsets are byte offsets into the line's UTF-8 text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorSpan {
    pub start: u32,
    pub len: u32,
    /// Packed `0x00RRGGBB`, or `NO_COLOR` for "default foreground".
    pub rgb: u32,
    pub bold: bool,
}

impl ColorSpan {
    pub const NO_COLOR: u32 = 0xFF00_0000;
}

/// Metadata for one stored line. The bytes live in the arena.
#[derive(Clone, Debug)]
pub struct LineMeta {
    /// Offset into the arena (already translated for eviction).
    pub start: u32,
    pub len: u32,
    pub ts: Timestamp,
    pub port: PortId,
    pub flags: LineFlags,
    /// From ANSI SGR, usually empty.
    pub spans: smallvec::SmallVec<[ColorSpan; 2]>,
    /// The live edit cursor's byte offset into the line's text — set only
    /// while `flags` contains `PROVISIONAL` (see
    /// [`crate::framer::FramedLine::cursor`]); rendering uses it to draw a
    /// caret on the still-open line.
    pub cursor: Option<u32>,
}

/// A line ready to be appended to the store.
#[derive(Clone, Debug)]
pub struct IncomingLine {
    pub text: String,
    pub ts: Timestamp,
    pub port: PortId,
    pub flags: LineFlags,
    pub spans: smallvec::SmallVec<[ColorSpan; 2]>,
    pub cursor: Option<u32>,
}

/// A read-only reference to a stored line.
pub struct LineRef<'a> {
    pub text: &'a str,
    pub meta: &'a LineMeta,
}

/// Line arena with front eviction.
///
/// External references to lines use *absolute* indices (`line_base + local`),
/// so eviction never invalidates a bookmark — it just makes it resolve to
/// "evicted" (spec §7.7).
pub struct LineStore {
    arena: Vec<u8>,
    lines: Vec<LineMeta>,
    /// Bytes evicted from the front of the arena, for offset translation.
    arena_base: u64,
    /// Lines evicted from the front, for index translation.
    line_base: u64,
    max_lines: usize,
    /// Set once eviction has occurred, for the UI banner.
    evicted_any: bool,
}

impl LineStore {
    pub fn new(max_lines: usize) -> LineStore {
        LineStore {
            arena: Vec::new(),
            lines: Vec::new(),
            arena_base: 0,
            line_base: 0,
            max_lines: max_lines.max(1),
            evicted_any: false,
        }
    }

    /// Total lines ever appended (absolute index of the next line).
    pub fn next_abs_index(&self) -> u64 {
        self.line_base + self.lines.len() as u64
    }

    /// Number of lines currently resident.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Absolute index of the first resident line.
    pub fn first_abs_index(&self) -> u64 {
        self.line_base
    }

    pub fn evicted_any(&self) -> bool {
        self.evicted_any
    }

    /// Append a line. If `flags` contains `CONTINUATION`, the previous line
    /// (which must be `PROVISIONAL`) is replaced in place instead — its bytes
    /// are re-appended to the arena and the metadata updated, keeping the
    /// original absolute index. Returns the absolute index of the affected line.
    pub fn append(&mut self, line: IncomingLine) -> u64 {
        if line.flags.contains(LineFlags::CONTINUATION) {
            if let Some(last) = self.lines.last_mut() {
                if last.flags.contains(LineFlags::PROVISIONAL) {
                    // Re-append bytes; the old bytes become dead space reclaimed
                    // at the next eviction. Provisional continuations are rare
                    // relative to normal lines, so this waste is bounded.
                    let start = self.arena.len() as u64 + self.arena_base;
                    self.arena.extend_from_slice(line.text.as_bytes());
                    last.start = (start - self.arena_base) as u32;
                    last.len = line.text.len() as u32;
                    last.ts = line.ts;
                    let mut flags = line.flags;
                    flags.remove(LineFlags::CONTINUATION);
                    last.flags = flags;
                    last.spans = line.spans;
                    last.cursor = line.cursor;
                    return self.line_base + (self.lines.len() as u64 - 1);
                }
            }
            // No provisional predecessor: fall through and append as new.
        }

        let start = self.arena.len() as u32;
        self.arena.extend_from_slice(line.text.as_bytes());
        let mut flags = line.flags;
        flags.remove(LineFlags::CONTINUATION);
        self.lines.push(LineMeta {
            start,
            len: line.text.len() as u32,
            ts: line.ts,
            port: line.port,
            flags,
            spans: line.spans,
            cursor: line.cursor,
        });
        let abs = self.line_base + (self.lines.len() as u64 - 1);
        self.maybe_evict();
        abs
    }

    /// Look up a line by absolute index. Returns `None` if evicted or not yet
    /// present.
    pub fn get(&self, abs_index: u64) -> Option<LineRef<'_>> {
        if abs_index < self.line_base {
            return None;
        }
        let local = (abs_index - self.line_base) as usize;
        let meta = self.lines.get(local)?;
        let start = meta.start as usize;
        let end = start + meta.len as usize;
        let bytes = self.arena.get(start..end)?;
        // The arena only ever contains valid UTF-8 (framer sanitizes).
        let text = std::str::from_utf8(bytes).unwrap_or("");
        Some(LineRef { text, meta })
    }

    /// Iterate resident lines by absolute index range (clamped to what exists).
    pub fn range(&self, start_abs: u64, end_abs: u64) -> impl Iterator<Item = LineRef<'_>> {
        let lo = start_abs.max(self.line_base);
        let hi = end_abs.min(self.next_abs_index());
        (lo..hi).filter_map(move |i| self.get(i))
    }

    /// Set or clear a flag on a line by absolute index (e.g. bookmarks).
    pub fn set_flag(&mut self, abs_index: u64, flag: LineFlags, on: bool) {
        if abs_index < self.line_base {
            return;
        }
        let local = (abs_index - self.line_base) as usize;
        if let Some(meta) = self.lines.get_mut(local) {
            if on {
                meta.flags.insert(flag);
            } else {
                meta.flags.remove(flag);
            }
        }
    }

    /// Evict from the front in a ~10% chunk when over capacity. Never one line
    /// at a time (that would be O(n) per line, spec §7.7).
    fn maybe_evict(&mut self) {
        if self.lines.len() <= self.max_lines {
            return;
        }
        let evict_count = (self.max_lines / 10).max(1);
        let evict_count = evict_count.min(self.lines.len());

        // Byte cutoff: start offset of the first line we keep.
        let byte_cutoff = self
            .lines
            .get(evict_count)
            .map(|m| m.start as usize)
            .unwrap_or(self.arena.len());

        // Compact the arena.
        self.arena.drain(..byte_cutoff);
        self.arena_base += byte_cutoff as u64;

        // Drop metadata and shift offsets.
        self.lines.drain(..evict_count);
        for m in &mut self.lines {
            m.start -= byte_cutoff as u32;
        }
        self.line_base += evict_count as u64;
        self.evicted_any = true;
    }

    /// Approximate resident memory footprint in bytes.
    pub fn approx_bytes(&self) -> usize {
        self.arena.capacity() + self.lines.capacity() * std::mem::size_of::<LineMeta>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SessionClock;

    fn incoming(text: &str, clock: &SessionClock) -> IncomingLine {
        IncomingLine {
            text: text.to_string(),
            ts: clock.now(),
            port: PortId(0),
            flags: LineFlags::default(),
            spans: Default::default(),
            cursor: None,
        }
    }

    #[test]
    fn append_and_get() {
        let clock = SessionClock::new();
        let mut s = LineStore::new(1000);
        let i0 = s.append(incoming("hello", &clock));
        let i1 = s.append(incoming("world", &clock));
        assert_eq!(i0, 0);
        assert_eq!(i1, 1);
        assert_eq!(s.get(0).unwrap().text, "hello");
        assert_eq!(s.get(1).unwrap().text, "world");
        assert!(s.get(2).is_none());
    }

    #[test]
    fn eviction_preserves_absolute_indices() {
        let clock = SessionClock::new();
        let mut s = LineStore::new(100);
        for n in 0..250 {
            s.append(incoming(&format!("line {n}"), &clock));
        }
        assert!(s.evicted_any());
        assert!(s.len() <= 100);
        // Early lines evicted -> resolve to None, not to the wrong line.
        assert!(s.get(0).is_none());
        // The most recent line is still index 249 and reads correctly.
        assert_eq!(s.get(249).unwrap().text, "line 249");
        // First resident index matches line_base.
        let first = s.first_abs_index();
        assert_eq!(s.get(first).unwrap().text, format!("line {first}"));
    }

    #[test]
    fn provisional_continuation_replaces_in_place() {
        let clock = SessionClock::new();
        let mut s = LineStore::new(1000);
        let mut prov = incoming("> ", &clock);
        prov.flags = LineFlags::PROVISIONAL;
        let idx = s.append(prov);

        let mut cont = incoming("> ready", &clock);
        cont.flags = LineFlags::CONTINUATION;
        let idx2 = s.append(cont);

        assert_eq!(idx, idx2, "continuation keeps the same absolute index");
        assert_eq!(s.len(), 1);
        let line = s.get(idx).unwrap();
        assert_eq!(line.text, "> ready");
        assert!(!line.meta.flags.contains(LineFlags::PROVISIONAL));
        assert!(!line.meta.flags.contains(LineFlags::CONTINUATION));
    }

    #[test]
    fn bookmark_flag_roundtrip() {
        let clock = SessionClock::new();
        let mut s = LineStore::new(1000);
        let idx = s.append(incoming("mark me", &clock));
        s.set_flag(idx, LineFlags::BOOKMARK, true);
        assert!(s.get(idx).unwrap().meta.flags.contains(LineFlags::BOOKMARK));
        s.set_flag(idx, LineFlags::BOOKMARK, false);
        assert!(!s.get(idx).unwrap().meta.flags.contains(LineFlags::BOOKMARK));
    }
}
