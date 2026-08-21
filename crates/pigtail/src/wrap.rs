//! Visual-row bookkeeping for the console.
//!
//! With wrapping on, one log line can cover several rows on screen, so the
//! scroll area can no longer treat "row N" as "line N". This index keeps a
//! running total of visual rows, which is what lets the view stay virtualized
//! and uniformly row-pitched (and so keeps the pin-to-bottom math exact): it
//! maps a row back to the line that owns it, and a line forward to the row it
//! starts on.
//!
//! Row counts are predicted from each line's *byte* length rather than from its
//! laid-out width. That is exact for the ASCII a serial console overwhelmingly
//! carries, and an over-estimate otherwise, since UTF-8 never spends fewer
//! bytes than characters. Over-estimating is the safe direction: a line's slot
//! comes out a row taller than it needs rather than shorter than the text
//! painted into it, so text is never clipped — at worst a rare non-ASCII line
//! leaves a blank row below itself.

use std::collections::VecDeque;

/// Rows one line of `len` bytes needs at `cols` columns. `cols == 0` means "do
/// not wrap", which is one row however long the line is.
pub fn rows_for(len: u32, cols: usize) -> u32 {
    if cols == 0 {
        return 1;
    }
    (len as usize).div_ceil(cols).max(1) as u32
}

/// Running visual-row totals for one console view.
pub struct WrapIndex {
    /// Cumulative row counts from an origin that never moves: `starts[i]` is
    /// entry `i`'s first row, counted from whenever this index was last built.
    /// Holds one extra element, so `starts[n]` is the end of the last entry and
    /// every lookup is a plain pair of neighbours. Row numbers handed out are
    /// relative to `starts[0]`, so evicting from the front stays a `pop_front`
    /// rather than a rebase of every element.
    starts: VecDeque<u64>,
    /// Absolute line index of entry 0, used to recognize front eviction.
    front_abs: u64,
    cols: usize,
    generation: u64,
}

impl WrapIndex {
    pub fn new() -> WrapIndex {
        WrapIndex {
            starts: VecDeque::from(vec![0]),
            front_abs: 0,
            cols: 0,
            generation: 0,
        }
    }

    /// Bring the index up to date for a view of `entries` lines.
    ///
    /// `abs_of` gives an entry's absolute line index — strictly increasing over
    /// the view, whether or not a filter is narrowing it — and `len_of` its
    /// byte length. Both are called only for the entries actually being
    /// (re)counted: appended lines in the common case, all of them when the
    /// column count or `generation` changed. `generation` is the caller's own
    /// counter for "the set of displayed lines was rebuilt wholesale", which a
    /// filter edit does.
    pub fn sync(
        &mut self,
        cols: usize,
        generation: u64,
        entries: usize,
        abs_of: impl Fn(usize) -> u64,
        len_of: impl Fn(usize) -> u32,
    ) {
        if cols != self.cols || generation != self.generation {
            self.rebuild(cols, generation, entries, &abs_of, &len_of);
            return;
        }

        // Front eviction: the store dropped lines off the start of the view.
        // Absolute indices only ever increase across the view, so the surviving
        // front's new position is a binary search away.
        if entries > 0 && abs_of(0) != self.front_abs {
            let dropped = partition_point(entries, |i| abs_of(i) < self.front_abs);
            if dropped >= self.len() {
                self.rebuild(cols, generation, entries, &abs_of, &len_of);
                return;
            }
            for _ in 0..dropped {
                self.starts.pop_front();
            }
            self.front_abs = abs_of(0);
        }

        if entries < self.len() {
            // Entries disappeared from somewhere other than the front (a
            // cleared console). Nothing incremental to salvage.
            self.rebuild(cols, generation, entries, &abs_of, &len_of);
            return;
        }

        // The newest line can grow in place while it is still open (a device
        // mid-prompt), so its count is always recomputed rather than trusted.
        if self.len() > 0 && self.len() == entries {
            self.starts.pop_back();
            self.push(len_of(entries - 1), cols);
        }
        for i in self.len()..entries {
            self.push(len_of(i), cols);
        }
    }

    fn rebuild(
        &mut self,
        cols: usize,
        generation: u64,
        entries: usize,
        abs_of: &impl Fn(usize) -> u64,
        len_of: &impl Fn(usize) -> u32,
    ) {
        self.cols = cols;
        self.generation = generation;
        self.front_abs = if entries > 0 { abs_of(0) } else { 0 };
        self.starts.clear();
        self.starts.push_back(0);
        for i in 0..entries {
            self.push(len_of(i), cols);
        }
    }

    fn push(&mut self, len: u32, cols: usize) {
        let end = self.starts.back().copied().unwrap_or(0) + u64::from(rows_for(len, cols));
        self.starts.push_back(end);
    }

    /// Number of entries (lines) indexed.
    pub fn len(&self) -> usize {
        self.starts.len() - 1
    }

    fn origin(&self) -> u64 {
        self.starts.front().copied().unwrap_or(0)
    }

    /// Total visual rows across every entry.
    pub fn total_rows(&self) -> u64 {
        self.starts.back().copied().unwrap_or(0) - self.origin()
    }

    /// The row entry `i` starts on. Out-of-range entries report the total, so
    /// callers walking off the end get an empty slot rather than a panic.
    pub fn start_row(&self, entry: usize) -> u64 {
        match self.starts.get(entry) {
            Some(&row) => row - self.origin(),
            None => self.total_rows(),
        }
    }

    /// How many rows entry `i` covers.
    pub fn rows(&self, entry: usize) -> u32 {
        (self.start_row(entry + 1) - self.start_row(entry)) as u32
    }

    /// The entry that owns `row`, clamped to the last entry.
    pub fn entry_at_row(&self, row: u64) -> usize {
        let origin = self.origin();
        let target = row + origin;
        // `starts` is sorted, so this is the first entry starting after `row`.
        self.starts
            .partition_point(|&start| start <= target)
            .saturating_sub(1)
            .min(self.len().saturating_sub(1))
    }
}

/// `slice::partition_point` over an index range, for a view whose entries are
/// reached through a closure rather than laid out in memory.
fn partition_point(len: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0usize, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_for_counts_whole_and_partial_rows() {
        assert_eq!(rows_for(0, 80), 1);
        assert_eq!(rows_for(80, 80), 1);
        assert_eq!(rows_for(81, 80), 2);
        assert_eq!(rows_for(160, 80), 2);
        // No wrapping: one row however long.
        assert_eq!(rows_for(10_000, 0), 1);
    }

    /// A view of `lens`, with absolute indices starting at `first`.
    fn sync(index: &mut WrapIndex, cols: usize, gen: u64, first: u64, lens: &[u32]) {
        index.sync(cols, gen, lens.len(), |i| first + i as u64, |i| lens[i]);
    }

    #[test]
    fn maps_rows_to_entries_both_ways() {
        let mut idx = WrapIndex::new();
        // 1 row, 3 rows, 1 row.
        sync(&mut idx, 10, 0, 0, &[5, 25, 10]);
        assert_eq!(idx.total_rows(), 5);
        assert_eq!(idx.start_row(0), 0);
        assert_eq!(idx.start_row(1), 1);
        assert_eq!(idx.rows(1), 3);
        assert_eq!(idx.start_row(2), 4);
        for (row, entry) in [(0, 0), (1, 1), (2, 1), (3, 1), (4, 2)] {
            assert_eq!(idx.entry_at_row(row), entry, "row {row}");
        }
        // Past the end clamps rather than panicking.
        assert_eq!(idx.entry_at_row(99), 2);
    }

    #[test]
    fn appending_extends_without_disturbing_earlier_rows() {
        let mut idx = WrapIndex::new();
        sync(&mut idx, 10, 0, 0, &[5, 25]);
        let before = idx.start_row(1);
        sync(&mut idx, 10, 0, 0, &[5, 25, 5, 5]);
        assert_eq!(idx.start_row(1), before);
        assert_eq!(idx.total_rows(), 6);
    }

    #[test]
    fn last_entry_is_recounted_as_it_grows() {
        // The newest line is still open and gets longer in place; its row count
        // has to follow it.
        let mut idx = WrapIndex::new();
        sync(&mut idx, 10, 0, 0, &[5, 5]);
        assert_eq!(idx.total_rows(), 2);
        sync(&mut idx, 10, 0, 0, &[5, 35]);
        assert_eq!(idx.total_rows(), 5);
    }

    #[test]
    fn front_eviction_drops_only_the_evicted_rows() {
        let mut idx = WrapIndex::new();
        sync(&mut idx, 10, 0, 0, &[25, 5, 5]);
        assert_eq!(idx.total_rows(), 5);
        // The first two lines were evicted; the view now starts at abs 2.
        sync(&mut idx, 10, 0, 2, &[5, 15]);
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.start_row(0), 0);
        assert_eq!(idx.total_rows(), 3);
    }

    #[test]
    fn changing_columns_or_generation_rebuilds() {
        let mut idx = WrapIndex::new();
        sync(&mut idx, 10, 0, 0, &[25]);
        assert_eq!(idx.total_rows(), 3);
        // Wider window, same lines.
        sync(&mut idx, 30, 0, 0, &[25]);
        assert_eq!(idx.total_rows(), 1);
        // A filter rebuild replaces the entry set wholesale.
        sync(&mut idx, 30, 1, 7, &[25, 25]);
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.total_rows(), 2);
    }

    /// The whole design rests on predicted row counts agreeing with what the
    /// text layout actually produces: a slot shorter than its text would let a
    /// line spill over the one below it, and every scroll offset is computed
    /// from the prediction rather than from measurement. So check the two
    /// against each other for the ASCII a console carries, at the same widths
    /// and break rule the console lays rows out with — including the pixel
    /// rounding, which is why this is checked across display scalings and not
    /// just at 1×.
    #[test]
    fn predicted_rows_match_what_egui_lays_out() {
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            for size in [6.0f32, 12.0, 17.0, 40.0] {
                let ctx = egui::Context::default();
                ctx.set_pixels_per_point(ppp);
                let _ = ctx.run(Default::default(), |ctx| {
                    let font = egui::FontId::monospace(size);
                    // Mirrors `panes::log::Metrics`: the advance a row is laid
                    // out with, and a wrap width holding exactly `cols` of them.
                    let advance = ctx.fonts(|f| f.glyph_width(&font, '0'));
                    let char_w = ((advance * ppp).round() / ppp).max(1.0);
                    for cols in [8usize, 40, 80, 133] {
                        for len in [0usize, 1, 7, 39, 79, 80, 81, 160, 161, 999] {
                            let mut job = egui::text::LayoutJob::single_section(
                                "x".repeat(len),
                                egui::TextFormat {
                                    font_id: font.clone(),
                                    ..Default::default()
                                },
                            );
                            job.wrap.max_width = cols as f32 * char_w + char_w * 0.25;
                            job.wrap.break_anywhere = true;
                            let galley = ctx.fonts(|f| f.layout_job(job));
                            assert_eq!(
                                galley.rows.len(),
                                rows_for(len as u32, cols) as usize,
                                "{len} chars at {cols} columns, {size}pt, {ppp}× scaling"
                            );
                        }
                    }
                });
            }
        }
    }

    #[test]
    fn a_cleared_console_rebuilds_from_empty() {
        let mut idx = WrapIndex::new();
        sync(&mut idx, 10, 0, 0, &[25, 25]);
        sync(&mut idx, 10, 0, 0, &[]);
        assert_eq!(idx.len(), 0);
        assert_eq!(idx.total_rows(), 0);
        // And an empty view answers lookups instead of panicking.
        assert_eq!(idx.entry_at_row(0), 0);
        assert_eq!(idx.start_row(0), 0);
    }
}
