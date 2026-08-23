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
/// so eviction never invalidates a held reference — it just makes it resolve
/// to "evicted" (spec §7.7).
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
    /// Set once a line you sent has been stored. Sticky, because those lines
    /// keep their ">" marker long after the setting that echoed them was turned
    /// off, and the column that marker sits in has to stay reserved for as long
    /// as any line here can carry one.
    tx_echo_any: bool,
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
            tx_echo_any: false,
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

    /// True if any line held here is one you sent (and so is drawn with a ">").
    pub fn tx_echo_any(&self) -> bool {
        self.tx_echo_any
    }

    /// Append a line. If `flags` contains `CONTINUATION`, the previous line
    /// (which must be `PROVISIONAL`) is replaced in place instead — its bytes
    /// are re-appended to the arena and the metadata updated, keeping the
    /// original absolute index. Returns the absolute index of the affected line.
    pub fn append(&mut self, line: IncomingLine) -> u64 {
        self.tx_echo_any |= line.flags.contains(LineFlags::TX_ECHO);
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

    /// Close off a still-open last line, if there is one: the stream feeding it
    /// has ended (the connection dropped), so it can never be continued. The
    /// live edit cursor goes with the `PROVISIONAL` flag, or a caret keeps being
    /// drawn on a line that is now history.
    ///
    /// The text is kept as it stands. The framer normally finalizes its tail
    /// itself on disconnect ([`crate::framer::Framer::flush_final`]); this is
    /// for the case where that tail is *empty* — a bare `\r` rewound the line
    /// after its provisional was already shown — where the last thing displayed
    /// is still the truest picture of what the device had on screen.
    pub fn finalize_last_provisional(&mut self) {
        if let Some(last) = self.lines.last_mut() {
            if last.flags.contains(LineFlags::PROVISIONAL) {
                last.flags.remove(LineFlags::PROVISIONAL);
                last.cursor = None;
            }
        }
    }

    /// Drop every resident line (the user clearing the console).
    ///
    /// Absolute indices keep advancing rather than restarting at zero, exactly
    /// as they do for eviction: anything holding an index (a search hit, a
    /// merged-view entry, a plot point) then resolves to "gone" instead
    /// of silently pointing at some unrelated later line.
    pub fn clear(&mut self) {
        self.arena_base += self.arena.len() as u64;
        self.arena.clear();
        self.line_base += self.lines.len() as u64;
        self.lines.clear();
        // No line is left to wear a ">", so the column it needed goes back to
        // the text.
        self.tx_echo_any = false;
        // Not `evicted_any`: nothing was dropped for want of capacity, so the
        // "lines evicted" notice in the header stays quiet.
    }

    /// Change the eviction cap, applying it immediately if the store is now
    /// over capacity. Cheap to call every frame: a no-op once the cap matches
    /// what's already set.
    pub fn set_max_lines(&mut self, max_lines: usize) {
        let max_lines = max_lines.max(1);
        if max_lines == self.max_lines {
            return;
        }
        self.max_lines = max_lines;
        // Evict the whole excess in one pass rather than via maybe_evict's
        // small fixed chunks: a cap dropped far below a large resident count
        // (e.g. 1,000,000 -> 10,000) would otherwise take hundreds of
        // O(remaining-length) drains, turning a single shrink into O(n^2)
        // work on the UI thread that owns this store.
        if self.lines.len() > self.max_lines {
            self.evict(self.lines.len() - self.max_lines);
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
        self.evict(evict_count);
    }

    /// Evict exactly `count` lines from the front (clamped to what's resident)
    /// in a single arena/metadata compaction.
    fn evict(&mut self, count: usize) {
        let count = count.min(self.lines.len());
        if count == 0 {
            return;
        }

        // Byte cutoff: start offset of the first line we keep.
        let byte_cutoff = self
            .lines
            .get(count)
            .map(|m| m.start as usize)
            .unwrap_or(self.arena.len());

        // Compact the arena.
        self.arena.drain(..byte_cutoff);
        self.arena_base += byte_cutoff as u64;

        // Drop metadata and shift offsets.
        self.lines.drain(..count);
        for m in &mut self.lines {
            m.start -= byte_cutoff as u32;
        }
        self.line_base += count as u64;
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
    fn finalize_last_provisional_keeps_the_text_but_drops_the_caret() {
        let clock = SessionClock::new();
        let mut s = LineStore::new(1000);
        let mut prov = incoming("50%", &clock);
        prov.flags = LineFlags::PROVISIONAL;
        prov.cursor = Some(3);
        let idx = s.append(prov);

        // Connection lost with the line still open.
        s.finalize_last_provisional();
        let line = s.get(idx).unwrap();
        assert_eq!(line.text, "50%", "what the device last showed is kept");
        assert!(!line.meta.flags.contains(LineFlags::PROVISIONAL));
        assert_eq!(line.meta.cursor, None, "no caret on a line that is history");

        // And it is no longer a target for continuation: output from the
        // reconnected stream starts its own line.
        let mut cont = incoming("later", &clock);
        cont.flags = LineFlags::CONTINUATION;
        let idx2 = s.append(cont);
        assert_eq!(idx2, idx + 1);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn finalize_last_provisional_is_a_no_op_on_a_settled_line() {
        let clock = SessionClock::new();
        let mut s = LineStore::new(1000);
        let idx = s.append(incoming("done", &clock));
        s.finalize_last_provisional();
        assert_eq!(s.get(idx).unwrap().text, "done");
        assert_eq!(s.len(), 1);
        // Empty store: nothing to finalize, and no panic.
        LineStore::new(10).finalize_last_provisional();
    }

    #[test]
    fn tracks_whether_any_line_was_sent() {
        // The console reserves a column for the ">" on sent lines from this,
        // and those lines outlive the local-echo setting that produced them —
        // an unreserved column would paint the marker over the text.
        let clock = SessionClock::new();
        let mut s = LineStore::new(1000);
        s.append(incoming("device output", &clock));
        assert!(!s.tx_echo_any());

        let mut sent = incoming("typed", &clock);
        sent.flags = LineFlags::TX_ECHO;
        s.append(sent);
        assert!(s.tx_echo_any());

        // Nothing is left to wear a marker after a clear.
        s.clear();
        assert!(!s.tx_echo_any());
    }

    #[test]
    fn clear_empties_store_without_reusing_indices() {
        let clock = SessionClock::new();
        let mut s = LineStore::new(1000);
        s.append(incoming("before", &clock));
        s.append(incoming("also before", &clock));
        s.clear();

        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert!(!s.evicted_any(), "clearing is not eviction");
        // Old indices read as gone, and the next line gets a fresh index rather
        // than inheriting index 0's identity (and its search hits).
        assert!(s.get(0).is_none());
        let idx = s.append(incoming("after", &clock));
        assert_eq!(idx, 2);
        assert_eq!(s.get(2).unwrap().text, "after");
        assert_eq!(s.first_abs_index(), 2);
    }

    #[test]
    fn set_max_lines_evicts_existing_lines_immediately() {
        // A cap lowered from Settings must apply to lines already resident,
        // not just to future appends (issue #13).
        let clock = SessionClock::new();
        let mut s = LineStore::new(1000);
        for n in 0..50 {
            s.append(incoming(&format!("line {n}"), &clock));
        }
        assert_eq!(s.len(), 50);

        s.set_max_lines(10);
        assert!(s.len() <= 10, "lowering the cap evicts without a new append");
        assert!(s.evicted_any());
        let last = s.next_abs_index() - 1;
        assert_eq!(s.get(last).unwrap().text, "line 49");
    }

    #[test]
    fn set_max_lines_is_a_no_op_when_unchanged() {
        let clock = SessionClock::new();
        let mut s = LineStore::new(1000);
        for n in 0..50 {
            s.append(incoming(&format!("line {n}"), &clock));
        }
        s.set_max_lines(1000);
        assert_eq!(s.len(), 50, "same cap shouldn't evict anything");
        assert!(!s.evicted_any());
    }

    #[test]
    fn set_max_lines_allows_growing_the_cap() {
        let clock = SessionClock::new();
        let mut s = LineStore::new(10);
        for n in 0..30 {
            s.append(incoming(&format!("line {n}"), &clock));
        }
        assert!(s.len() <= 10);

        // Raising the cap doesn't retroactively un-evict, but it does stop
        // further eviction until the new, larger cap is reached.
        s.set_max_lines(1000);
        let first_after_raise = s.first_abs_index();
        for n in 30..60 {
            s.append(incoming(&format!("line {n}"), &clock));
        }
        assert_eq!(
            s.first_abs_index(),
            first_after_raise,
            "no further eviction once the cap is well above the resident count"
        );
        assert_eq!(s.len(), 60 - first_after_raise as usize);
        assert_eq!(s.get(59).unwrap().text, "line 59");
        assert_eq!(
            s.get(first_after_raise).unwrap().text,
            format!("line {first_after_raise}")
        );
    }
}
