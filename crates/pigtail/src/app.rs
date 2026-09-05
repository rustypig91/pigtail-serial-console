//! Application state and the `update()` dispatch. Draws by delegating to the
//! `panes` modules; holds no IO — data arrives from reader threads via channels.

use crate::paths::AppPaths;
use crate::wrap::WrapIndex;
use crossbeam_channel::Receiver;
use serialcore::clock::{SessionClock, Timestamp};
use serialcore::config::{
    Config, ExtractRule, PortConfig, PortIdentity, SavedConnection, TransmitMacro,
};
use serialcore::enumerate::{
    match_identity, spawn_enumerator, DiscoveredPort, EnumEvent, MatchResult,
};
use serialcore::extract::CompiledExtract;
use serialcore::filter::{Combine, FilterIndex, FilterRule, FilterSet};
use serialcore::framer::Framer;
use serialcore::reader::{self, ConnState, ErrorScope, ReaderEvent, SourceSpec};
use serialcore::series::Series;
use serialcore::session::{self, SessionMeta};
use serialcore::store::{IncomingLine, LineFlags, LineStore, PortId};
use serialcore::transfer::{PreparedTransfer, TransferOptions};
use serialcore::update;
use serialcore::wake::Wake;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const CONFIG_WRITE_DELAY: Duration = Duration::from_secs(1);

/// Retention limits derived from the single user-facing memory setting.
///
/// The ratios preserve the old preload and plot defaults at one million lines,
/// while giving raw bytes enough room to represent ordinary console lines. The
/// byte caps keep a manually edited, extreme `max_lines` value from turning one
/// connection into an unbounded allocation; every value offered by Settings
/// still scales through the full range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HistoryLimits {
    pub max_lines: usize,
    pub raw_bytes: usize,
    pub preload_bytes: usize,
    pub series_points: usize,
}

const MIN_RAW_BYTES: usize = 1 << 20;
const MAX_RAW_BYTES: usize = 640 * 1024 * 1024;
const MIN_PRELOAD_BYTES: usize = 1 << 20;
const MAX_PRELOAD_BYTES: usize = 80 * 1024 * 1024;
const DEFAULT_HISTORY_LINES: usize = 1_000_000;
const DEFAULT_RAW_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_PRELOAD_BYTES: usize = 8 * 1024 * 1024;

fn scale_from_default(max_lines: usize, default: usize) -> usize {
    ((max_lines as u128 * default as u128) / DEFAULT_HISTORY_LINES as u128).min(usize::MAX as u128)
        as usize
}

pub(crate) fn history_limits(max_lines: usize) -> HistoryLimits {
    let max_lines = max_lines.max(1);
    HistoryLimits {
        max_lines,
        raw_bytes: scale_from_default(max_lines, DEFAULT_RAW_BYTES)
            .clamp(MIN_RAW_BYTES, MAX_RAW_BYTES),
        preload_bytes: scale_from_default(max_lines, DEFAULT_PRELOAD_BYTES)
            .clamp(MIN_PRELOAD_BYTES, MAX_PRELOAD_BYTES),
        series_points: (max_lines.saturating_add(9) / 10).max(2),
    }
}

/// A plotted series plus its display state.
pub struct SeriesEntry {
    pub series: Series,
    pub color: egui::Color32,
    pub visible: bool,
    pub own_axis: bool,
}

/// Palette for auto-assigned series colours.
pub const SERIES_PALETTE: [egui::Color32; 8] = [
    egui::Color32::from_rgb(0x6c, 0xb6, 0xff),
    egui::Color32::from_rgb(0x8d, 0xdb, 0x8c),
    egui::Color32::from_rgb(0xf2, 0xc5, 0x5c),
    egui::Color32::from_rgb(0xe8, 0x7d, 0xba),
    egui::Color32::from_rgb(0xc3, 0x9d, 0xf5),
    egui::Color32::from_rgb(0x5f, 0xd6, 0xcf),
    egui::Color32::from_rgb(0xff, 0x9d, 0x5c),
    egui::Color32::from_rgb(0xb5, 0xd0, 0x6a),
];

/// A compiled highlight rule cached for render-time use.
///
/// Colour only. egui has no synthetic bold and no bold monospace face is
/// bundled, so there is nothing to draw a bold run *with* — hence
/// [`serialcore::config::HighlightRule::bold`] is not represented here, and is
/// no longer offered in the Highlight rules window either (issue #45).
pub struct CompiledHighlight {
    pub re: regex::Regex,
    pub color: egui::Color32,
}

/// Compile a search exactly as both indexing and row highlighting understand
/// it. An invalid regex is treated as literal text, preserving the search
/// bar's existing forgiving behaviour.
pub(crate) fn compile_search(query: &str, case_sensitive: bool) -> Option<regex::Regex> {
    if query.is_empty() {
        return None;
    }
    regex::RegexBuilder::new(query)
        .case_insensitive(!case_sensitive)
        .build()
        .or_else(|_| {
            regex::RegexBuilder::new(&regex::escape(query))
                .case_insensitive(!case_sensitive)
                .build()
        })
        .ok()
}

/// One entry in the timestamp-interleaved merged view.
#[derive(Clone, Copy)]
pub struct MergedEntry {
    pub micros: i64,
    pub port: PortId,
    pub abs: u64,
    /// Position in the view, as a number that only ever increases along it.
    ///
    /// This is what the row index keys entries by, and it needs a key that is
    /// *strictly* increasing to tell an eviction from a reshuffle. `micros`
    /// looks like one and is not: every line framed out of a single read
    /// carries that read's timestamp, so a burst of output shares one to the
    /// microsecond. Renumbered whenever the view is reordered rather than
    /// appended to — which already forces the index to rebuild.
    pub seq: u64,
}

/// Load config from disk, falling back to defaults on any error.
pub fn load_config(paths: &AppPaths) -> Config {
    match std::fs::read_to_string(&paths.config_file) {
        Ok(s) => match Config::from_toml(&s) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("config parse error, using defaults: {e}");
                Config::default()
            }
        },
        Err(_) => Config::default(),
    }
}

/// egui already embeds Hack for monospace text, and it covers common UI
/// symbols that its proportional Ubuntu font does not (including ↑ and ↓).
/// Reuse it as the final proportional fallback instead of bundling a second
/// copy of another font solely for those glyphs.
fn app_font_definitions() -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    if fonts.font_data.contains_key("Hack") {
        let proportional = fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default();
        proportional.retain(|font| font != "Hack");
        proportional.push("Hack".to_owned());
    }
    fonts
}

/// List the session captures already on disk, paired with their metadata, so a
/// reopened tab can be preloaded from the most recent one for its device.
fn snapshot_captures(sessions_dir: &std::path::Path) -> Vec<(PathBuf, SessionMeta)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.to_string_lossy().ends_with(".session.bin") {
            continue;
        }
        if let Ok(meta) = session::read_meta(&path) {
            out.push((path, meta));
        }
    }
    out
}

/// Order this device's captures oldest-first.
///
/// Sorting on `start_wall` alone is not enough: it is the app's clock anchor,
/// so every capture written in one run of the app carries the same value. The
/// micros stamp on the first record breaks those ties, being measured from that
/// same anchor.
///
/// A capture holding no records at all has no such stamp, and sorts *last*
/// within its run rather than first. It is either the one still being written
/// to or one just emptied by a clear, and both are the newest thing in the run;
/// placing an emptied capture first would let history restore walk past it into
/// the older captures whose output the clear discarded.
fn captures_for<'a>(
    captures: &'a [(PathBuf, SessionMeta)],
    identity: &PortIdentity,
) -> Vec<(&'a PathBuf, &'a SessionMeta)> {
    let mut mine: Vec<(&PathBuf, &SessionMeta, u64)> = captures
        .iter()
        .filter(|(_, m)| &m.identity == identity)
        .map(|(path, m)| {
            let first = session::read_first_micros(path)
                .ok()
                .flatten()
                .unwrap_or(u64::MAX);
            (path, m, first)
        })
        .collect();
    mine.sort_by_key(|(_, m, first)| (m.start_wall, *first));
    mine.into_iter().map(|(p, m, _)| (p, m)).collect()
}

/// Stored history ready to replay: one entry per capture, oldest first, each
/// pairing the capture's metadata with the tail of its raw records.
type RestoredHistory<'a> = Vec<(&'a SessionMeta, Vec<(u64, Vec<u8>)>)>;

/// Collect this device's stored history, walking back from the newest capture
/// until `budget` bytes of raw records are covered. Returns the captures
/// oldest-first, each with the tail of its records, ready to replay.
fn gather_history<'a>(
    captures: &'a [(PathBuf, SessionMeta)],
    identity: &PortIdentity,
    mut budget: usize,
) -> RestoredHistory<'a> {
    let mine = captures_for(captures, identity);
    let mut restored: RestoredHistory<'a> = Vec::new();
    for (path, meta) in mine.iter().rev() {
        if budget == 0 {
            break;
        }
        if let Ok(records) = session::read_tail_records(path, budget) {
            let used: usize = records.iter().map(|(_, b)| b.len()).sum();
            budget = budget.saturating_sub(used);
            if !records.is_empty() {
                restored.push((*meta, records));
            }
        }
        // A capture the user cleared mid-session holds only what arrived after
        // the clear, and everything older was discarded on purpose: stop here
        // rather than putting it back.
        if meta.cleared {
            break;
        }
    }
    restored.reverse();
    restored
}

/// Preload the prior captures for `identity` into `conn.store`, each closed by
/// a boundary marker, so a reopened tab shows the output it had before the app
/// was last closed with new live output continuing below the last marker.
///
/// History is gathered from the newest capture backwards until `preload_bytes`
/// is filled, rather than from the newest one alone. A
/// single run can leave several captures behind — applying new port options
/// respawns the reader onto a fresh one — and what the console showed at exit
/// was itself part restored, part live, so stopping at the newest capture
/// throws away output that was on screen when the app closed.
fn preload_last_session(
    conn: &mut Connection,
    captures: &[(PathBuf, SessionMeta)],
    identity: &PortIdentity,
    clock: &SessionClock,
    preload_bytes: usize,
) {
    let restored = gather_history(captures, identity, preload_bytes);

    // Re-frame the raw bytes exactly as the reader did, stamping each line with
    // the *original* wall-clock time (start of that capture plus its offset) so
    // absolute timestamps read as when the data actually arrived. Each capture
    // is framed on its own: they are separate streams, and the last line of one
    // must not swallow the first line of the next.
    for (meta, records) in &restored {
        let mut framer = Framer::with_mode(conn.port_config.terminal);
        let mut framed = Vec::new();
        for (micros, bytes) in records {
            // Checked: the offset comes off disk, and a capture torn by a
            // crash (or an older one damaged since) can name one that runs
            // the date past what chrono represents — which `+` answers with
            // a panic, before the window has even opened.
            let wall = meta
                .start_wall
                .checked_add_signed(chrono::Duration::microseconds(*micros as i64))
                .unwrap_or(meta.start_wall);
            // Not the offset off disk: that counts from *that* run's start, and
            // this line is about to share a store — and every interval drawn
            // from it — with output stamped against this run's clock. Projected
            // through the wall clock instead, the only reference the two runs
            // share, which puts it on the negative side of the axis where a
            // line recorded before this run began belongs.
            let ts = Timestamp {
                wall,
                micros: clock.micros_at(wall),
            };
            framer.push(bytes, ts, &mut framed);
        }
        framer.flush_final(&mut framed);
        // A capture that framed to nothing gets no marker: a boundary with no
        // output above it is just noise.
        let Some(last_ts) = framed.last().map(|line| line.ts) else {
            continue;
        };
        // The console's marker text, shared with the hex view's boundary so the
        // two views name the same capture the same way.
        let label = format!(
            "previous session · {}",
            // Local time, like every other wall-clock stamp on screen; only
            // storage is UTC.
            meta.start_wall
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
        );
        // The same records, as the bytes they were on the wire. Pushed as one
        // run so the hex view numbers them from this capture's start.
        let raw_start = conn.raw_next();
        for (_, bytes) in records {
            conn.push_raw_bytes(bytes);
        }
        conn.raw_sessions.push(RawSession {
            start: raw_start,
            label: Some(label.clone()),
        });
        for line in framed {
            let styled = serialcore::ansi::parse_line(&line.text, line.cursor);
            conn.store.append(IncomingLine {
                text: styled.text,
                ts: line.ts,
                port: conn.id,
                flags: line.flags,
                spans: styled.spans,
                cursor: styled.cursor.map(|c| c as u32),
            });
        }
        // Boundary closing this capture. Below it is either the next restored
        // session or, after the last one, the live output.
        conn.store.append(IncomingLine {
            text: label,
            ts: last_ts,
            port: conn.id,
            flags: LineFlags::RECONNECT_MARKER,
            spans: Default::default(),
            cursor: None,
        });
    }
}

/// Append `bytes` to a raw ring holding at most `cap`, evicting from the front,
/// and advance `base` by everything that did not stay.
///
/// `base + ring.len()` is the absolute index of the next byte, and every
/// [`RawSession::start`] is measured on that scale — so a byte dropped here must
/// be counted here, including the head of a push larger than the ring itself.
/// Get that wrong and the hex view addresses the wrong bytes.
/// Returns whether any bytes were evicted.
///
/// Evicting in bulk rather than a byte at a time is what makes preloading
/// worthwhile: a session restore can hand this several megabytes to pour
/// through a one-megabyte ring.
fn push_raw(ring: &mut VecDeque<u8>, base: &mut u64, bytes: &[u8], cap: usize) -> bool {
    // Only the tail of an oversized push can survive it.
    let keep = bytes.len().min(cap);
    let dropped_head = bytes.len() - keep;
    *base += dropped_head as u64;
    let bytes = &bytes[bytes.len() - keep..];

    let overflow = (ring.len() + keep).saturating_sub(cap);
    if overflow > 0 {
        ring.drain(..overflow);
        *base += overflow as u64;
    }
    ring.extend(bytes.iter().copied());
    dropped_head > 0 || overflow > 0
}

/// One run's worth of bytes inside a connection's raw ring.
///
/// The ring holds this run's output *and* whatever was restored from earlier
/// captures, one after the other, exactly as the console holds their lines. The
/// hex view counts each run's offsets from its own first byte, so a dump always
/// starts at `00000000` however much history sits above it.
pub struct RawSession {
    /// Absolute index — counting every byte ever pushed to this connection — of
    /// this run's first byte. Offsets shown in the hex view are measured from
    /// here, and eviction never shifts it.
    pub start: u64,
    /// Boundary drawn under this run's bytes, carrying the same text as the
    /// console's marker. `None` for the run in progress, which nothing closes.
    pub label: Option<String>,
}

/// An error shown on a connection's tab, paired with what it is about.
///
/// The scope is what keeps a reconnect from erasing errors it doesn't fix:
/// only the link's own failures are retired by the link coming back.
#[derive(Clone, Debug)]
pub struct TabError {
    pub scope: ErrorScope,
    pub msg: String,
}

/// File transfer currently owned by a connection's reader.
pub struct TransferProgress {
    pub file_name: String,
    pub sent: usize,
    pub total: usize,
}

impl TabError {
    /// An error about the link itself, raised by the UI rather than the reader
    /// (a spawn that failed before any reader existed to report it).
    pub fn connection(msg: impl Into<String>) -> Self {
        TabError {
            scope: ErrorScope::Connection,
            msg: msg.into(),
        }
    }
}

/// One open connection (a tab).
pub struct Connection {
    pub id: PortId,
    /// Automatically detected device/path label. Kept even when `name` is set
    /// so clearing a custom name can restore it without re-enumerating.
    pub label: String,
    /// User-assigned display name, if any.
    pub name: Option<String>,
    pub identity: PortIdentity,
    pub port_config: PortConfig,
    pub handle: reader::ReaderHandle,
    pub store: LineStore,
    pub state: ConnState,
    /// Advances whenever a connected port leaves `Connected`. Macro runs copy
    /// this value so even a disconnect/reconnect completed within one UI frame
    /// still cancels work that belonged to the old connection.
    pub(crate) disconnect_generation: u64,
    /// "Pinned": follow the tail and autoscroll. While set, the console offset is
    /// forced to the bottom every frame (not via egui's `stick_to_bottom`, which
    /// stops following under fast input). Toggled from the footer, and
    /// auto-enabled whenever the user sends.
    pub follow: bool,
    /// Lines received while follow is disengaged.
    pub new_since_scroll: u64,
    /// Viewport height egui measured for the pinned scroll area last frame. The
    /// "scroll to bottom" offset is `n_rows * row_height - viewport_height`;
    /// rows are exactly `row_height` with zero inter-row spacing, so the content
    /// height is computed directly (no measured-content feedback, which would
    /// chase noise and jitter). Stable frame-to-frame; a stale value is harmless.
    pub pin_view_h: f32,
    /// Running visual-row totals for the console, which stop matching line
    /// counts as soon as one long line wraps onto several rows.
    pub wrap_index: WrapIndex,
    /// Bumped whenever the set of displayed lines is rebuilt wholesale (a
    /// filter edit), which is the one change `wrap_index` cannot follow
    /// incrementally.
    pub filter_generation: u64,
    /// Absolute index of the line at the top of the view last frame. Re-pinning
    /// to it is what keeps the reader's place when the rows underneath change
    /// height — a text-size change, a window resize.
    pub top_line: Option<u64>,
    /// Console columns and text size the view was last laid out with; `None`
    /// until it has been drawn once. A change means every row moved.
    pub console_layout: Option<(usize, u8)>,
    /// Bounded ring of raw bytes for hex view.
    pub raw_ring: VecDeque<u8>,
    /// Current cap for `raw_ring`, derived from the global history setting.
    raw_capacity: usize,
    /// Current console-line cap, used to detect a settings-driven decrease.
    history_max_lines: usize,
    /// A decrease trimmed resident history; release the old backing allocations
    /// once the interactive edit finishes rather than reallocating every drag frame.
    history_allocation_shrink_pending: bool,
    /// Set once the hex history has discarded raw bytes at its capacity.
    pub(crate) raw_evicted_any: bool,
    /// Bytes dropped from the front of `raw_ring`, for translating a
    /// [`RawSession`]'s absolute start into a position in the ring.
    pub raw_base: u64,
    /// First byte in the latest contiguous receive region. Disconnects and
    /// dropped-output notices advance this without discarding retained hex
    /// history, so consumers cannot mistake bytes across a gap for neighbors.
    pub(crate) raw_contiguous_start: u64,
    /// The runs whose bytes the ring holds, oldest first.
    pub raw_sessions: Vec<RawSession>,
    /// The most recent error on this connection, kept with its scope: a
    /// successful (re)connect retires a [`ErrorScope::Connection`] error but
    /// says nothing about a [`ErrorScope::Session`] one, which outlives it.
    pub last_error: Option<TabError>,
    /// Present only while this reader is sending a dropped file. Completion,
    /// cancellation, disconnect, and failure all remove it silently.
    pub transfer_progress: Option<TransferProgress>,
    /// User-set time mark for delta-from-mark display, on the session axis, so
    /// a mark set on a restored line is negative. Deliberately not persisted:
    /// a mark is a "measure from here" gesture on the console in front of you,
    /// and the next run counts from its own start until you set one again.
    pub mark_micros: Option<i64>,
    /// Show the raw-byte hex view instead of decoded lines (spec §7.11).
    pub hex_view: bool,
    // Filtering (spec §7.8).
    pub filter_rules: Vec<FilterRule>,
    pub filter_combine: Combine,
    pub filter_index: FilterIndex,
    pub filter_dirty: bool,
    pub filter_errors: Vec<(usize, String)>,
    // Search (spec §7.8).
    pub search_query: String,
    pub search_case_sensitive: bool,
    pub search_matches: Vec<u64>,
    pub search_pos: Option<usize>,
    pub search_dirty: bool,
    pub search_tested_upto: u64,
    /// The line last jumped to — a search hit, or the line behind a plot point
    /// that was clicked. Not drawn in the log itself; it is what puts the
    /// log→plot marker on the plot.
    pub selected: Option<u64>,
    /// Scroll request: centre this absolute line on the next frame.
    pub scroll_to: Option<u64>,
    /// How far this port has been folded into the merged view.
    pub merged_upto: u64,
    // Plotting (spec §7.13).
    pub extract_rules: Vec<ExtractRule>,
    pub extract_compiled: Vec<CompiledExtract>,
    pub extract_dirty: bool,
    pub extract_errors: Vec<String>,
    pub series: Vec<SeriesEntry>,
    pub series_index: HashMap<String, usize>,
    /// Current per-series point cap, derived from the global history setting.
    series_capacity: usize,
    /// Largest capacity whose recoverable console history has been replayed.
    /// Kept separate from `series_capacity` so interactive increases can be
    /// applied cheaply and coalesced into one rebuild when editing finishes.
    series_backfilled_capacity: usize,
    /// Set once at least one plotted series has discarded a point.
    pub(crate) series_evicted_any: bool,
    pub plot_follow: bool,
    /// Set by the plot's Fit button, consumed by the next frame's draw: the
    /// bounds can only be set from inside the plot's own closure.
    pub plot_fit: bool,
    pub show_plot: bool,
    // Transmit state. Line ending / echo / history settings live in
    // `port_config`. `tx_input` accumulates the current input line locally (for
    // history recall and, when enabled, local echo).
    pub tx_input: String,
    pub tx_history: Vec<String>,
    pub tx_history_pos: Option<usize>,
    pub dtr: bool,
    pub rts: bool,
}

fn normalized_tab_name(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

impl Connection {
    /// Label presented to the user, preferring their custom name over the
    /// automatically detected device/path description.
    pub fn display_label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.label)
    }

    /// Compact source name used before each row in the merged console.
    /// Automatic labels include a path in parentheses, for which the first
    /// token remains the useful compact identifier; custom names are kept
    /// intact so a name such as "left sensor" remains recognizable.
    pub fn merged_label(&self) -> &str {
        self.name
            .as_deref()
            .unwrap_or_else(|| self.label.split_whitespace().next().unwrap_or(&self.label))
    }

    pub(crate) fn apply_history_limits(&mut self, limits: HistoryLimits) {
        self.history_allocation_shrink_pending |= limits.max_lines < self.history_max_lines
            || limits.raw_bytes < self.raw_capacity
            || limits.series_points < self.series_capacity;
        self.history_max_lines = limits.max_lines;
        self.store.set_max_lines(limits.max_lines);
        self.raw_capacity = limits.raw_bytes;
        // An empty append applies a lower cap to bytes already resident while
        // leaving the absolute-index accounting in one well-tested place.
        self.raw_evicted_any |= push_raw(
            &mut self.raw_ring,
            &mut self.raw_base,
            &[],
            self.raw_capacity,
        );
        let shrank = limits.series_points < self.series_capacity;
        self.series_capacity = limits.series_points;
        for entry in &mut self.series {
            self.series_evicted_any |= entry.series.set_capacity(self.series_capacity);
        }
        if shrank {
            // A lower cap invalidates backfill above it, but an intermediate
            // drag value must not claim that history missing since the last
            // settled capacity has already been replayed.
            self.series_backfilled_capacity =
                self.series_backfilled_capacity.min(self.series_capacity);
        }
    }

    /// Settle one or more history-capacity edits. This replays recoverable plot
    /// history after increases and releases excess backing allocations after
    /// decreases. Settings calls it only once its DragValue is no longer being
    /// dragged, avoiding a scan or reallocation for every intermediate value.
    /// Returns whether a decrease was settled, so app-level caches derived from
    /// console history can be invalidated once instead of on every drag frame.
    pub(crate) fn finish_history_capacity_change(&mut self) -> bool {
        if self.series_capacity > self.series_backfilled_capacity {
            self.grow_series_history();
            self.series_backfilled_capacity = self.series_capacity;
        }
        let history_shrank = self.history_allocation_shrink_pending;
        if history_shrank {
            self.store.shrink_to_fit();
            self.raw_ring.shrink_to_fit();
            for entry in &mut self.series {
                entry.series.shrink_to_fit();
            }
            self.history_allocation_shrink_pending = false;
        }
        history_shrank
    }

    fn drain_events(&mut self, max_lines: usize) -> bool {
        self.apply_history_limits(history_limits(max_lines));
        let mut changed = false;
        // Non-blocking drain of all pending reader events (spec §5).
        while let Ok(ev) = self.handle.events.try_recv() {
            changed = true;
            match ev {
                ReaderEvent::State(s) => {
                    // Leaving Connected ends whatever line was still open. The
                    // reader finalizes it for us when it had bytes in hand; this
                    // catches the line it had already rewound (a bare `\r`), for
                    // which it has nothing left to send and whose caret would
                    // otherwise stay lit across the outage and beyond it.
                    if s != ConnState::Connected {
                        self.store.finalize_last_provisional();
                        self.mark_raw_discontinuity();
                    } else if matches!(
                        self.last_error,
                        Some(TabError {
                            scope: ErrorScope::Connection,
                            ..
                        })
                    ) {
                        // A successful (re)connect means whatever was wrong
                        // with the *link* no longer applies; don't leave a
                        // stale error showing once the port is open again.
                        // Session-scoped errors (a capture file that couldn't
                        // be opened, say) are untouched by reconnecting, so
                        // they survive it.
                        self.last_error = None;
                    }
                    self.set_state(s);
                }
                ReaderEvent::Error { scope, msg } => {
                    tracing::warn!(port = self.id.0, "{msg}");
                    // A dropped-command notice (session-scoped) while the link
                    // itself is down would otherwise clobber the connection
                    // error explaining *why* — and unlike a connection error,
                    // nothing later clears it, so it would keep hiding the
                    // real cause for the rest of the outage. The link being
                    // down already implies commands can't get through, so
                    // just keep showing that.
                    let hides_connection_error = scope == ErrorScope::Session
                        && matches!(
                            self.last_error,
                            Some(TabError {
                                scope: ErrorScope::Connection,
                                ..
                            })
                        );
                    if !hides_connection_error {
                        self.last_error = Some(TabError { scope, msg });
                    }
                }
                ReaderEvent::TransferProgress { sent, total } => {
                    if let Some(progress) = &mut self.transfer_progress {
                        progress.sent = sent;
                        progress.total = total;
                    }
                }
                ReaderEvent::TransferEnded => self.transfer_progress = None,
                ReaderEvent::OutputDropped {
                    raw_bytes,
                    line_updates,
                    at,
                } => {
                    self.mark_raw_discontinuity();
                    let label = format!(
                        "output dropped · {raw_bytes} bytes, {line_updates} line updates · display was busy"
                    );

                    // A missing completion may have belonged to the open line
                    // already on screen. Close it before the boundary so a
                    // retained continuation cannot rewrite text from before
                    // the gap.
                    self.store.finalize_last_provisional();

                    // Close the current hex run with the same visible gap
                    // marker. The next batch opens a fresh run whose offsets
                    // restart at zero rather than pretending the retained raw
                    // bytes are contiguous with bytes we discarded.
                    if let Some(session) = self.raw_sessions.last_mut() {
                        if session.label.is_none() {
                            session.label = Some(label.clone());
                        }
                    }

                    let next = self.store.next_abs_index();
                    let abs = self.store.append(IncomingLine {
                        text: label,
                        ts: at,
                        port: self.id,
                        flags: LineFlags::RECONNECT_MARKER,
                        spans: Default::default(),
                        cursor: None,
                    });
                    if !self.follow && abs >= next {
                        self.new_since_scroll += 1;
                    }
                }
                ReaderEvent::Batch(batch) => {
                    self.open_live_raw_session();
                    self.push_raw_bytes(&batch.raw);
                    let mut pairs: Vec<(String, f64)> = Vec::new();
                    for line in batch.lines {
                        // Parse SGR colours and strip other escapes (spec §2, §7.9).
                        let styled = serialcore::ansi::parse_line(&line.text, line.cursor);
                        let is_data = feeds_plot(line.flags);
                        let text = styled.text;
                        // Where a genuinely new line would land. A
                        // `CONTINUATION` instead replaces the open provisional
                        // line in place and hands back *its* index, which is
                        // below this — see below.
                        let next = self.store.next_abs_index();
                        let abs = self.store.append(IncomingLine {
                            text: text.clone(),
                            ts: line.ts,
                            port: self.id,
                            flags: line.flags,
                            spans: styled.spans,
                            cursor: styled.cursor.map(|c| c as u32),
                        });
                        // Counted per *line*, not per event: a line the device
                        // is still writing is re-sent every ~20ms as it grows,
                        // and counting those would have the footer's "N new"
                        // climb by ~50/s while the console gained nothing at
                        // all (issue #46). Keyed off the index rather than the
                        // flag, so a continuation with no provisional
                        // predecessor — which `append` correctly treats as a
                        // new line — still counts as one.
                        if !self.follow && abs >= next {
                            self.new_since_scroll += 1;
                        }
                        // Run extraction and push points (spec §7.13).
                        if is_data && !self.extract_compiled.is_empty() {
                            pairs.clear();
                            for rule in &self.extract_compiled {
                                rule.extract(&text, &mut pairs);
                            }
                            if !pairs.is_empty() {
                                let t = line.ts.micros as f64 / 1_000_000.0;
                                for (name, value) in pairs.drain(..) {
                                    self.push_series_point(&name, t, value, abs);
                                }
                            }
                        }
                    }
                }
            }
        }
        changed
    }

    /// Absolute index of the next byte to enter the raw ring.
    pub fn raw_next(&self) -> u64 {
        self.raw_base + self.raw_ring.len() as u64
    }

    /// Append bytes to the hex view's ring, evicting from the front at capacity.
    pub fn push_raw_bytes(&mut self, bytes: &[u8]) {
        self.raw_evicted_any |= push_raw(
            &mut self.raw_ring,
            &mut self.raw_base,
            bytes,
            self.raw_capacity,
        );
    }

    /// Begin a new contiguous receive region at the current raw position.
    pub(crate) fn mark_raw_discontinuity(&mut self) {
        self.raw_contiguous_start = self.raw_next();
    }

    pub(crate) fn set_state(&mut self, state: ConnState) {
        if self.state == ConnState::Connected && state != ConnState::Connected {
            self.disconnect_generation = self.disconnect_generation.wrapping_add(1);
        }
        self.state = state;
    }

    /// Open the run of bytes this session is producing, unless it is open
    /// already. Called on arrival rather than at connect: restored history is
    /// pushed first, and this run's bytes begin after it.
    fn open_live_raw_session(&mut self) {
        if self.raw_sessions.last().is_none_or(|s| s.label.is_some()) {
            self.raw_sessions.push(RawSession {
                start: self.raw_next(),
                label: None,
            });
        }
    }

    /// Bring the console's visual-row index up to date for a view `cols`
    /// characters wide (0 when wrapping is off). Cheap on a normal frame: only
    /// lines appended since the last call are counted.
    pub fn sync_wrap(&mut self, cols: usize) {
        // Destructured so the index can be updated while the store it reads
        // stays borrowed — they are disjoint fields, but only field access
        // proves that to the borrow checker.
        let Connection {
            store,
            filter_index,
            filter_rules,
            filter_generation,
            wrap_index,
            ..
        } = self;
        let filter_active = !filter_rules
            .iter()
            .all(|r| !r.enabled || r.pattern.is_empty());
        let matching = filter_index.matching();
        let first_abs = store.first_abs_index();
        let entries = if filter_active {
            matching.len()
        } else {
            store.len()
        };
        let abs_of = |i: usize| {
            if filter_active {
                matching[i]
            } else {
                first_abs + i as u64
            }
        };
        wrap_index.sync(cols, *filter_generation, entries, abs_of, |i| {
            crate::panes::wrap_len(store, abs_of(i))
        });
    }

    /// True if at least one filter rule is enabled and non-empty.
    pub fn filter_index_active(&self) -> bool {
        !self
            .filter_rules
            .iter()
            .all(|r| !r.enabled || r.pattern.is_empty())
    }

    /// Compile any changed extraction rules and, when they have changed,
    /// re-derive every series from the lines already in the console.
    ///
    /// Re-reading the session is the whole point: a rule is written *after*
    /// seeing the output that suggested it, and a plot that began at the moment
    /// you finished typing would omit exactly the lines that prompted the rule.
    fn maintain_extract(&mut self) {
        if !self.extract_dirty {
            return;
        }
        self.extract_dirty = false;
        self.extract_compiled.clear();
        self.extract_errors.clear();
        for rule in &self.extract_rules {
            match CompiledExtract::compile(rule) {
                Ok(c) => self.extract_compiled.push(c),
                Err(e) => self.extract_errors.push(e),
            }
        }
        self.rebuild_series();
    }

    /// Re-run the compiled rules over the whole store, replacing the series.
    fn rebuild_series(&mut self) {
        // Colour, visibility and Y2 belong to the series *name*, not to the rule
        // text that happened to produce it: keep them across a re-read so
        // touching a rule does not reshuffle a plot you have just set up.
        let remembered: HashMap<String, SeriesStyle> = self
            .series
            .iter()
            .map(|e| {
                (
                    e.series.name().to_string(),
                    SeriesStyle {
                        color: e.color,
                        visible: e.visible,
                        own_axis: e.own_axis,
                    },
                )
            })
            .collect();
        self.series.clear();
        self.series_index.clear();
        self.series_backfilled_capacity = self.series_capacity;
        if self.extract_compiled.is_empty() {
            return;
        }
        // Destructured so the store stays borrowed for reading while the series
        // it feeds are written — disjoint fields, but only field access proves
        // that to the borrow checker.
        let Connection {
            store,
            series,
            series_index,
            extract_compiled,
            ..
        } = self;
        self.series_evicted_any |= extract_all(
            store,
            extract_compiled,
            series,
            series_index,
            &remembered,
            self.series_capacity,
        );
    }

    /// Grow each series and backfill it from the resident console without
    /// throwing away samples whose source lines have already been evicted.
    fn grow_series_history(&mut self) {
        let first_resident = self.store.first_abs_index();
        for entry in &mut self.series {
            entry.series.set_capacity(self.series_capacity);
            // The suffix is replayed below. The prefix cannot be reconstructed
            // from the line store, so it must survive the rebuild in place.
            entry.series.retain_before_line(first_resident);
        }
        if self.extract_compiled.is_empty() {
            return;
        }
        let Connection {
            store,
            series,
            series_index,
            extract_compiled,
            ..
        } = self;
        self.series_evicted_any |= extract_all(
            store,
            extract_compiled,
            series,
            series_index,
            &HashMap::new(),
            self.series_capacity,
        );
    }

    fn push_series_point(&mut self, name: &str, t: f64, value: f64, line: u64) {
        let idx = series_slot(
            &mut self.series,
            &mut self.series_index,
            None,
            name,
            self.series_capacity,
        );
        self.series_evicted_any |= self.series[idx].series.push(t, value, line);
    }
}

/// How a series is drawn, remembered by name across a rebuild.
#[derive(Clone, Copy)]
struct SeriesStyle {
    color: egui::Color32,
    visible: bool,
    own_axis: bool,
}

/// Index of the series called `name`, appending it when it is new. `remembered`
/// gives back the look a series of that name had before a rebuild.
fn series_slot(
    series: &mut Vec<SeriesEntry>,
    index: &mut HashMap<String, usize>,
    remembered: Option<&HashMap<String, SeriesStyle>>,
    name: &str,
    capacity: usize,
) -> usize {
    if let Some(&i) = index.get(name) {
        return i;
    }
    let style = remembered
        .and_then(|m| m.get(name).copied())
        .unwrap_or_else(|| SeriesStyle {
            color: SERIES_PALETTE[series.len() % SERIES_PALETTE.len()],
            visible: true,
            own_axis: false,
        });
    series.push(SeriesEntry {
        series: Series::new(name.to_string(), capacity),
        color: style.color,
        visible: style.visible,
        own_axis: style.own_axis,
    });
    index.insert(name.to_string(), series.len() - 1);
    series.len() - 1
}

/// Whether a line's text is something the extraction rules should read.
///
/// Shared by live ingest ([`Connection::drain_events`]) and the re-read
/// ([`extract_all`]) so the plot cannot depend on which of the two produced a
/// point. Three kinds of line are excluded:
///
/// - a reconnect/session marker, which is the app talking, not the device;
/// - your own echoed input ([`LineFlags::TX_ECHO`]), for the same reason;
/// - a still-open [`LineFlags::PROVISIONAL`] line, which is device output but
///   only *part* of it. The framer shows an unterminated line after ~20ms of
///   silence and replaces it in place as it grows, so extracting one plots a
///   number that is still being typed: `temp:23.4` arriving in two reads
///   deposits `temp = 23` off the provisional and `temp = 23.4` off the
///   completed line, and the first is a spike that never existed (issue #38).
///   A line is plottable once it is terminated, which is also when its number
///   is known to be whole.
fn feeds_plot(flags: LineFlags) -> bool {
    !flags.contains(LineFlags::RECONNECT_MARKER)
        && !flags.contains(LineFlags::TX_ECHO)
        && !flags.contains(LineFlags::PROVISIONAL)
}

/// Fill `series` by running `rules` over every line of `store` that this run
/// recorded.
///
/// Restored history is skipped. It sits at a negative stamp (see
/// [`SessionClock::micros_at`]), often hours or days below zero, and folding it
/// into the same axis would stretch the plot across the gap between two runs to
/// show a handful of stale points. The plot covers the session in front of you.
fn extract_all(
    store: &LineStore,
    rules: &[CompiledExtract],
    series: &mut Vec<SeriesEntry>,
    index: &mut HashMap<String, usize>,
    remembered: &HashMap<String, SeriesStyle>,
    capacity: usize,
) -> bool {
    let mut pairs: Vec<(String, f64)> = Vec::new();
    let mut evicted = false;
    for abs in store.first_abs_index()..store.next_abs_index() {
        let Some(line) = store.get(abs) else {
            continue;
        };
        // The gate live ingest applies (see `feeds_plot`), plus the session
        // cut-off.
        if !feeds_plot(line.meta.flags) || line.meta.ts.micros < 0 {
            continue;
        }
        pairs.clear();
        for rule in rules {
            rule.extract(line.text, &mut pairs);
        }
        if pairs.is_empty() {
            continue;
        }
        let t = line.meta.ts.micros as f64 / 1_000_000.0;
        for (name, value) in pairs.drain(..) {
            let idx = series_slot(series, index, Some(remembered), &name, capacity);
            evicted |= series[idx].series.push(t, value, abs);
        }
    }
    evicted
}

/// Working state of the modal new-connection dialog (spec-redesign: opening a
/// tab first shows a serial-port configuration window).
#[derive(Default)]
pub struct ConfigDialog {
    pub selected_path: Option<String>,
    pub config: PortConfig,
    /// Name field for saving the current params as a preset.
    pub preset_name: String,
    /// When set, the dialog is editing an existing tab's port options rather
    /// than creating a new connection; applying reconnects that tab.
    pub editing: Option<PortId>,
}

/// Working state of the modal used to name an existing connection tab.
pub struct RenameDialog {
    pub port: PortId,
    /// Empty means "use the detected device label" when saved.
    pub name: String,
}

/// The update notice: what it says, and what its buttons do.
/// A background failure with nowhere else to show itself (see
/// `App::connect_errors`): a title naming what was being attempted, and the
/// formatted error.
#[derive(PartialEq)]
pub struct ConnectError {
    pub title: &'static str,
    pub message: String,
}

/// Whether one enumerated port is already represented by an open tab other
/// than `except`.
///
/// A serial-numbered device can be recognized after its OS path changes. When
/// several indistinguishable serial-less devices are present, identity
/// matching is deliberately ambiguous, so their current paths disambiguate
/// them instead of adding one device disabling every identical sibling.
pub(crate) fn available_port_is_added(
    index: usize,
    available: &[DiscoveredPort],
    connections: &[Connection],
    except: Option<PortId>,
) -> bool {
    connections
        .iter()
        .filter(|conn| Some(conn.id) != except)
        .any(|conn| match match_identity(&conn.identity, available) {
            MatchResult::Definite(found) => found == index,
            MatchResult::Ambiguous(candidates) => {
                candidates.contains(&index) && available[index].path == conn.identity.path_fallback
            }
            MatchResult::None => false,
        })
}

fn detected_connection_label(identity: &PortIdentity, path: &str) -> String {
    let device = identity.label();
    if device == path {
        path.to_owned()
    } else {
        format!("{device} ({path})")
    }
}

fn resolved_port<'a>(
    identity: &PortIdentity,
    available: &'a [DiscoveredPort],
) -> Option<&'a DiscoveredPort> {
    match match_identity(identity, available) {
        MatchResult::Definite(index) => available.get(index),
        MatchResult::Ambiguous(candidates) => candidates
            .into_iter()
            .filter_map(|index| available.get(index))
            .find(|port| port.path == identity.path_fallback),
        MatchResult::None => None,
    }
}

pub struct UpdateDialog {
    pub title: String,
    pub message: String,
    /// Release tag to download and install. `None` hides the Update button.
    pub update_version: Option<String>,
    /// Release page for manual downloads, also retained after update failures.
    pub download_url: Option<String>,
    /// Version the "Skip this version" button records. `None` hides that button.
    pub skip_version: Option<String>,
}

/// Confirmation and preparation state for one dropped file.
pub struct FileTransferDialog {
    pub port: PortId,
    pub path: PathBuf,
    pub file_name: String,
    pub source_size: u64,
    pub preview: Vec<u8>,
    pub options: TransferOptions,
    pub prepared: Option<PreparedTransfer>,
    pub prepare_rx: Option<Receiver<Result<PreparedTransfer, String>>>,
    pub prepare_error: Option<String>,
}

/// One in-flight macro execution. The definition is copied when it starts so
/// editing or deleting a macro cannot change a sequence halfway through.
pub(crate) struct MacroRun {
    /// Catalog position of the definition that started this run. It becomes
    /// `None` if that definition is deleted while its copied steps continue.
    pub(crate) macro_index: Option<usize>,
    pub(crate) name: String,
    pub(crate) started_at: Instant,
    /// Further full executions after the current one; `None` means forever.
    pub(crate) repetitions_remaining: Option<u32>,
    pub(crate) port: PortId,
    pub(crate) disconnect_generation: u64,
    pub(crate) steps: Vec<serialcore::config::MacroStep>,
    pub(crate) next_step: usize,
    pub(crate) next_at: Instant,
    pub(crate) wait_for: Option<MacroWait>,
}

/// A receive condition starts at an absolute raw-byte position, so output
/// already in the console before the wait step cannot satisfy it.
pub(crate) struct MacroWait {
    pub(crate) regex: regex::Regex,
    pub(crate) raw_start: u64,
}

/// Draft owned by the separate add/edit window. Config is changed only when
/// the user saves, so closing the editor can discard every in-progress change.
pub(crate) struct MacroEditor {
    pub(crate) index: Option<usize>,
    pub(crate) draft: TransmitMacro,
    pub(crate) step_selection: Option<usize>,
}

/// A retention edit that would immediately discard existing captures.
pub(crate) struct RetentionCleanupConfirmation {
    pub(crate) days: u32,
    pub(crate) paths: Vec<PathBuf>,
}

pub struct App {
    pub clock: SessionClock,
    pub config: Config,
    pub paths: AppPaths,
    /// When the first not-yet-persisted config change was made. Keeping the
    /// first timestamp coalesces per-frame and per-keystroke edits into at most
    /// one write per second without postponing persistence indefinitely during
    /// a long edit.
    config_dirty_since: Option<Instant>,
    /// Handed to every background thread so it can pull the UI out of idle when
    /// it has something to show. See `update()`.
    pub wake: Wake,
    pub enum_rx: Receiver<EnumEvent>,
    pub available: Vec<DiscoveredPort>,
    pub connections: Vec<Connection>,
    /// Active tab index; `connections.len()` selects the merged view.
    pub active: usize,
    pub next_port_id: u32,
    /// `Some` while the modal new-connection dialog is open.
    pub config_dialog: Option<ConfigDialog>,
    /// `Some` while the modal tab-name dialog is open.
    pub rename_dialog: Option<RenameDialog>,
    /// Compiled global highlight rules; rebuilt when `highlight_dirty`.
    pub highlight_cache: Vec<CompiledHighlight>,
    pub highlight_dirty: bool,
    /// Runtime master switch for rendering highlights. Individual rule states
    /// stay untouched so they can be restored with one click from the footer.
    pub highlights_visible: bool,
    /// Timestamp-interleaved merged view across all ports (spec §7.12).
    pub merged: Vec<MergedEntry>,
    pub merged_dirty: bool,
    /// Visual-row totals for the merged view, as `Connection::wrap_index` is
    /// for a single tab.
    pub merged_wrap: WrapIndex,
    /// Hands out `MergedEntry::seq`. Only ever counts up while the view is
    /// appended to; a reorder renumbers the whole view and resets it.
    pub merged_seq: u64,
    /// Bumped whenever `merged` is rebuilt or reordered rather than appended
    /// to, which is the one change `merged_wrap` cannot follow incrementally.
    pub merged_generation: u64,
    /// Per-port eviction boundary last applied to `merged`. This makes the
    /// common append-only path a constant-time check per connection while still
    /// letting `prune_merged` remove dead entries from inside the interleaving.
    pub merged_pruned_before: HashMap<PortId, u64>,
    /// Filter state owned by the merged pseudo-tab. A merged filter is kept
    /// separate from every port's filter so its meaning does not depend on
    /// whichever real tab happened to be active last.
    pub merged_filter_rules: Vec<FilterRule>,
    pub merged_filter_combine: Combine,
    pub merged_filter_errors: Vec<(usize, String)>,
    pub merged_filter_dirty: bool,
    /// Cached subset of `merged` that passes the merged filter. This is only
    /// consulted while a rule is active; the ordinary unfiltered path reads
    /// `merged` directly and pays no extra per-frame scan.
    pub merged_filtered: Vec<MergedEntry>,
    pub merged_filter_generation: u64,
    pub merged_filter_source_generation: u64,
    pub merged_filter_upto_seq: u64,
    /// Search state owned by the merged pseudo-tab. Matches keep the complete
    /// merged key so navigation can identify both the port and its line.
    pub merged_search_query: String,
    pub merged_search_case_sensitive: bool,
    pub merged_search_matches: Vec<MergedEntry>,
    pub merged_search_pos: Option<usize>,
    pub merged_search_dirty: bool,
    pub merged_search_source_generation: u64,
    pub merged_search_upto_seq: u64,
    pub merged_scroll_to: Option<MergedEntry>,
    /// Follow the tail of the merged console. Kept separately from each
    /// connection's pin so browsing the aggregate does not disturb any tab's
    /// own scroll position.
    pub merged_follow: bool,
    /// Entries added while the merged console is not following its tail.
    pub merged_new_since_scroll: u64,
    /// Last measured viewport height, used by the merged view's explicit
    /// bottom-pin calculation.
    pub merged_pin_view_h: f32,
    /// Device that receives raw keyboard input while the merged tab is open.
    pub merged_tx_port: Option<PortId>,
    /// True when the merged pseudo-tab is active.
    pub merged_selected: bool,
    // Floating tool windows, toggled from the console right-click menu, so the
    // main window stays uncluttered.
    pub show_settings: bool,
    pub show_macros_win: bool,
    /// Global reference for the keyboard commands Pigtail reserves.
    pub show_keyboard_shortcuts: bool,
    pub(crate) macro_editor: Option<MacroEditor>,
    /// Macro definition awaiting confirmation because it is currently running.
    pub(crate) macro_running_edit_confirmation: Option<usize>,
    /// Target macro, requested digit, and the macro currently owning it.
    pub(crate) macro_shortcut_conflict: Option<(usize, u8, usize)>,
    /// Retention edit waiting for the user to approve deletion of old captures.
    pub(crate) retention_cleanup_confirmation: Option<RetentionCleanupConfirmation>,
    /// Value being edited in Settings. It is kept separate from the saved
    /// configuration until the number box loses focus.
    pub(crate) session_retention_draft: Option<u32>,
    pub show_filters_win: bool,
    pub show_highlight_win: bool,
    pub show_extract_win: bool,
    /// `Some(port_id)` while the popup showing that connection's `last_error`
    /// in full is open (clicked from the footer's error indicator). Keyed by
    /// the connection it was opened for, not "whichever tab is active", so
    /// switching tabs while it's open doesn't silently swap the message.
    pub show_error_win: Option<PortId>,
    pub show_search: bool,
    /// Set when search should grab keyboard focus next frame (e.g. after Ctrl+F).
    pub search_focus_request: bool,
    /// `Some` while an update check is in flight.
    pub update_rx: Option<Receiver<update::CheckResult>>,
    pub install_rx: Option<Receiver<update::InstallEvent>>,
    pub update_progress: Option<f32>,
    /// True when the in-flight check came from the menu's "Check for updates".
    /// A manual check always reports a result and ignores a previous skip; the
    /// startup check stays silent unless there is a new version to announce.
    pub update_manual: bool,
    /// `Some` while the update notice is showing.
    pub update_dialog: Option<UpdateDialog>,
    /// Confirmation dialog opened by dropping a file onto the active console.
    pub file_transfer_dialog: Option<FileTransferDialog>,
    /// Most recent file-transfer choices. They deliberately live only in the
    /// running app, so opening another file is convenient without turning a
    /// one-off transfer setting into a persistent preference.
    pub file_transfer_options: Option<TransferOptions>,
    /// Console text size to flash over the middle of the window, and the time
    /// it was set. Ctrl+wheel has nothing else to show for itself: the change
    /// it makes is legible only if you already know what you are looking for.
    pub font_toast: Option<(u8, f64)>,
    /// Command sequences waiting for their next non-blocking delayed send.
    pub(crate) macro_runs: Vec<MacroRun>,
    /// Background operations that failed outright — opening or reconnecting a
    /// port, starting the enumerator — rather than through the normal
    /// per-connection error path (e.g. the OS refused to spawn a thread).
    /// A queue, not a single slot: two such failures can land in the same
    /// tick (e.g. two restored connections both losing a thread-exhaustion
    /// race), and neither should silently erase the other before either is
    /// shown.
    pub connect_errors: VecDeque<ConnectError>,
}

impl App {
    /// Build an `App` from its already-resolved dependencies. Shared by
    /// `App::new` (a live `eframe::CreationContext`) and the test harness (a
    /// hand-rolled `Wake`/channel), so a new field is added to `App` in one
    /// place instead of two struct literals kept in sync by hand.
    fn assemble(config: Config, paths: AppPaths, wake: Wake, enum_rx: Receiver<EnumEvent>) -> App {
        App {
            clock: SessionClock::new(),
            config,
            paths,
            config_dirty_since: None,
            wake,
            enum_rx,
            available: Vec::new(),
            connections: Vec::new(),
            active: 0,
            next_port_id: 0,
            config_dialog: None,
            rename_dialog: None,
            highlight_cache: Vec::new(),
            highlight_dirty: true,
            highlights_visible: true,
            merged: Vec::new(),
            merged_seq: 0,
            merged_dirty: false,
            merged_wrap: WrapIndex::new(),
            merged_generation: 0,
            merged_pruned_before: HashMap::new(),
            merged_filter_rules: Vec::new(),
            merged_filter_combine: Combine::And,
            merged_filter_errors: Vec::new(),
            merged_filter_dirty: true,
            merged_filtered: Vec::new(),
            merged_filter_generation: 0,
            merged_filter_source_generation: 0,
            merged_filter_upto_seq: 0,
            merged_search_query: String::new(),
            merged_search_case_sensitive: false,
            merged_search_matches: Vec::new(),
            merged_search_pos: None,
            merged_search_dirty: true,
            merged_search_source_generation: 0,
            merged_search_upto_seq: 0,
            merged_scroll_to: None,
            merged_follow: true,
            merged_new_since_scroll: 0,
            merged_pin_view_h: 0.0,
            merged_tx_port: None,
            merged_selected: false,
            show_settings: false,
            show_macros_win: false,
            show_keyboard_shortcuts: false,
            macro_editor: None,
            macro_running_edit_confirmation: None,
            macro_shortcut_conflict: None,
            retention_cleanup_confirmation: None,
            session_retention_draft: None,
            show_filters_win: false,
            show_highlight_win: false,
            show_extract_win: false,
            show_error_win: None,
            show_search: false,
            search_focus_request: false,
            update_rx: None,
            install_rx: None,
            update_progress: None,
            update_manual: false,
            update_dialog: None,
            file_transfer_dialog: None,
            file_transfer_options: None,
            font_toast: None,
            macro_runs: Vec::new(),
            connect_errors: VecDeque::new(),
        }
    }

    pub fn new(cc: &eframe::CreationContext<'_>, paths: AppPaths, config: Config) -> App {
        cc.egui_ctx.set_fonts(app_font_definitions());

        // Theme from settings.
        let dark = config.settings.theme != "light";
        cc.egui_ctx.set_visuals(if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });

        // The wake every background thread gets: an idle UI schedules no frames
        // of its own, so a repaint request is the only thing that brings it back.
        let wake = Wake::new({
            let ctx = cc.egui_ctx.clone();
            move || ctx.request_repaint()
        });

        let (tx, rx) = crossbeam_channel::unbounded();
        // A failure here means the OS refused to create the thread (resource
        // exhaustion) — rare, and not fatal: the app still runs, it just won't
        // discover new ports until restarted with more headroom.
        let enum_spawn_err = spawn_enumerator(tx, Duration::from_millis(500), wake.clone()).err();

        let mut app = App::assemble(config, paths, wake, rx);
        if let Some(e) = enum_spawn_err {
            app.record_connect_error("Couldn't start port detection", e.to_string());
        }

        // Silent startup check for a newer release. Debug builds are skipped: a
        // working copy is routinely at the same version as — or ahead of — the
        // published tag, so there is nothing useful to say about it.
        if !cfg!(debug_assertions) && app.config.settings.check_updates {
            app.start_update_check(false);
        }

        // Snapshot the captures already on disk *before* opening anything, so a
        // tab is preloaded from its previous run, not the empty file its own
        // reopen is about to create.
        let prior_captures = snapshot_captures(&app.paths.sessions);

        // Reopen the connections that were open at last exit (remembered
        // session). Paths are resolved by identity in the reader, so this works
        // before enumeration has produced its first snapshot; a device that is
        // not present simply sits reconnecting until it appears. Each reopened
        // tab is preloaded with the output it had before the app was closed.
        // Cloning is an `Arc` bump, and it keeps the loop below from holding a
        // shared borrow of `app` across the `connections.last_mut()`.
        let clock = app.clock.clone();
        for saved in app.config.last_open.clone() {
            // The non-saving inner call, not `open_connection`: `last_open` is
            // already exactly this list, and saving after each entry would
            // rewrite it from `self.connections` mid-restore, permanently
            // dropping any earlier entry that failed transiently before a
            // later one succeeds and triggers the save.
            if app.open_connection_inner(saved.identity.clone(), None, saved.config, saved.name) {
                let conn = app.connections.last_mut().unwrap();
                preload_last_session(
                    conn,
                    &prior_captures,
                    &saved.identity,
                    &clock,
                    history_limits(app.config.settings.max_lines).preload_bytes,
                );
            }
        }
        app.active = 0;
        app.merged_selected = false;

        app
    }

    /// Open the modal new-connection dialog, seeded with sensible defaults.
    ///
    /// A no-op while a dialog is already open: replacing it would throw away
    /// whatever the user has filled in so far (issue #16). `show_header`
    /// disables the controls that lead here for the same reason, but the
    /// guard lives here so every entry point — including the empty-console
    /// "+ New connection" button — is covered by construction.
    pub fn open_config_dialog(&mut self) {
        if self.config_dialog.is_some() || self.rename_dialog.is_some() {
            return;
        }
        let selected_path = self
            .available
            .iter()
            .enumerate()
            .find(|(index, _)| {
                !available_port_is_added(*index, &self.available, &self.connections, None)
            })
            .map(|(_, port)| port)
            .map(|p| p.path.clone());
        self.config_dialog = Some(ConfigDialog {
            selected_path,
            config: PortConfig::default(),
            preset_name: String::new(),
            editing: None,
        });
    }

    /// Open the config dialog to edit an existing tab's port options. Applying
    /// it reconnects that tab with the new settings.
    /// Like `open_config_dialog`, this leaves an already-open dialog alone
    /// rather than discarding its in-progress edits.
    pub fn open_port_options(&mut self, index: usize) {
        if self.config_dialog.is_some() || self.rename_dialog.is_some() {
            return;
        }
        let Some(conn) = self.connections.get(index) else {
            return;
        };
        let config = conn.port_config.clone();
        // Pre-select the device's current path if it's present right now.
        let selected_path = resolved_port(&conn.identity, &self.available).map(|p| p.path.clone());
        self.config_dialog = Some(ConfigDialog {
            selected_path,
            config,
            preset_name: String::new(),
            editing: Some(conn.id),
        });
    }

    /// Open the modal for assigning a display name to an existing tab.
    pub fn open_rename_dialog(&mut self, index: usize) {
        if self.config_dialog.is_some() || self.rename_dialog.is_some() {
            return;
        }
        let Some(conn) = self.connections.get(index) else {
            return;
        };
        self.rename_dialog = Some(RenameDialog {
            port: conn.id,
            name: conn.name.clone().unwrap_or_default(),
        });
    }

    /// Apply a custom tab name. Whitespace-only input restores the detected
    /// device label, and the remembered session is updated immediately.
    pub fn rename_connection(&mut self, port: PortId, name: &str) {
        let Some(conn) = self.connections.iter_mut().find(|conn| conn.id == port) else {
            return;
        };
        conn.name = normalized_tab_name(name);
        self.save_session();
    }

    /// Spawn a reader for a live serial device (shared by open and reconnect).
    fn spawn_serial_reader(
        &self,
        id: PortId,
        identity: &PortIdentity,
        config: &PortConfig,
        initial_path: Option<String>,
    ) -> std::io::Result<reader::ReaderHandle> {
        let meta = SessionMeta {
            identity: identity.clone(),
            config: config.clone(),
            start_wall: self.clock.start_wall(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            port_label: identity.label(),
            cleared: false,
        };
        let reader_config = reader::ReaderConfig {
            port_id: id,
            clock: self.clock.clone(),
            session_dir: Some(self.paths.sessions.clone()),
            meta,
            terminal: config.terminal,
            wake: self.wake.clone(),
        };
        let spec = SourceSpec::Serial {
            identity: identity.clone(),
            config: config.clone(),
            initial_path,
        };
        reader::spawn(reader_config, spec)
    }

    /// Log `title: message` and queue it as a user-visible background-operation
    /// error (spec: a resource-exhaustion thread-spawn failure is recoverable,
    /// not fatal). Shared by every such failure site so they can't drift apart.
    ///
    /// Skips a repeat of any message already queued (scanning the whole
    /// queue, not just the tail, catches two distinct devices failing in
    /// alternation) and caps the
    /// queue so a sustained flap can't grow it without bound. Once at the
    /// cap, new arrivals are dropped rather than the front entry, which is
    /// either on screen right now or the oldest one the user hasn't
    /// acknowledged yet.
    pub(crate) fn record_connect_error(&mut self, title: &'static str, message: String) {
        tracing::error!("{title}: {message}");
        let err = ConnectError { title, message };
        if self.connect_errors.contains(&err) {
            return;
        }
        const MAX_CONNECT_ERRORS: usize = 20;
        if self.connect_errors.len() >= MAX_CONNECT_ERRORS {
            return;
        }
        self.connect_errors.push_back(err);
    }

    /// Whether a modal dialog anchored at CENTER_CENTER (see `show_connect_error`)
    /// should wait its turn behind the connect-error queue instead of
    /// stacking on top of it.
    pub(crate) fn defer_to_connect_error(&self) -> bool {
        !self.connect_errors.is_empty()
    }

    /// Record `err` from a failed [`App::spawn_serial_reader`] as a
    /// user-visible error instead of letting it crash the app.
    /// Returns the formatted message so callers that also need it (e.g. to
    /// annotate a tab's `last_error`) don't have to reformat `err` themselves.
    fn report_connect_error(&mut self, identity: &PortIdentity, err: std::io::Error) -> String {
        let msg = format!("couldn't open {}: {err}", identity.label());
        self.record_connect_error("Couldn't connect", msg.clone());
        msg
    }

    /// Reconnect the tab identified by `port_id` with new port settings, in
    /// place (same tab position). The device identity follows the chosen path if
    /// one is selected and present, otherwise the tab keeps its current device.
    /// The existing console output is preserved — the new session's lines
    /// continue below a marker — so a reconnect never clears the log.
    pub fn reconnect_with_config(
        &mut self,
        port_id: PortId,
        initial_path: Option<String>,
        config: PortConfig,
    ) {
        let Some(index) = self.connections.iter().position(|c| c.id == port_id) else {
            // The tab the dialog was editing is gone. `show_header` disables
            // closing a tab while the dialog is up, so this should not be
            // reachable from the UI — but returning silently would close the
            // dialog as if "Apply & reconnect" had worked (issue #16), so say
            // so instead of pretending.
            self.record_connect_error(
                "Couldn't reconnect",
                "the tab these port options belong to is no longer open, so there \
                 is nothing to reconnect."
                    .to_string(),
            );
            return;
        };
        let old_identity = self.connections[index].identity.clone();
        let identity = initial_path
            .as_ref()
            .and_then(|p| self.available.iter().find(|d| &d.path == p))
            .map(|d| d.identity.clone())
            .unwrap_or_else(|| old_identity.clone());

        // Close the old reader *before* opening the new one. Both address the
        // same device, and a serial port is opened exclusively — with the old
        // reader still holding it the new one's first open fails, and it goes
        // away into its reconnect backoff for up to two seconds before trying
        // again. The wait is no new cost: this join already happened, just
        // after the spawn rather than before it.
        self.connections[index].handle.shutdown_in_place();
        // `shutdown_in_place` drains and discards the old reader's final
        // events, including `TransferEnded`, so retire its UI state here.
        self.connections[index].transfer_progress = None;

        // Keep the same port id so the preserved lines still map to this tab in
        // the merged view; only the reader (and its capture file) is replaced.
        let handle =
            match self.spawn_serial_reader(port_id, &identity, &config, initial_path.clone()) {
                Ok(handle) => handle,
                Err(e) => {
                    // The old reader is already shut down and inert (its thread is
                    // gone, so it needs no further handling), and there is no new
                    // one to replace it with. Leave the tab in place — its console
                    // history is worth keeping — marked closed with the error, but
                    // do not change its identity/config/label since the switch to
                    // them never actually took effect.
                    //
                    // The message names the tab's own (unchanged) identity, not
                    // `identity`: when a path switch is what's being attempted,
                    // `identity` is the *new*, not-yet-adopted device, and a
                    // message naming it would be talking about a device the tab
                    // never shows.
                    let msg = self.report_connect_error(&old_identity, e);
                    let conn = &mut self.connections[index];
                    // The reader that would have completed the last open line is
                    // gone for good (no replacement was spawned), so finalize it
                    // now the same way the normal state-transition path
                    // (`drain_events`) and the success path below do — otherwise
                    // its caret stays lit forever on a tab that will never
                    // reconnect.
                    conn.store.finalize_last_provisional();
                    conn.set_state(ConnState::Closed);
                    conn.last_error = Some(TabError::connection(msg));
                    self.merged_dirty = true;
                    return;
                }
            };

        let path_label = initial_path.unwrap_or_else(|| identity.label());
        let label = detected_connection_label(&identity, &path_label);
        let dtr = config.dtr_on_open;
        let rts = config.rts_on_open;
        // Marker delineating the settings change in the preserved log.
        let marker_text = format!("reconnected · {}", config.summary());
        let marker_ts = self.clock.now();

        {
            let conn = &mut self.connections[index];
            // Drops the spent handle, whose thread is already joined.
            conn.handle = handle;
            conn.identity = identity;
            conn.port_config = config;
            conn.label = label;
            conn.set_state(ConnState::Connecting);
            conn.dtr = dtr;
            conn.rts = rts;
            conn.last_error = None;
            // Console (store, raw_ring, filters, search, plot series, marks,
            // selection, scroll position) is intentionally left untouched —
            // except for closing any line the old reader left open, since the
            // reader that would have completed it is being replaced.
            conn.store.finalize_last_provisional();
            conn.store.append(IncomingLine {
                text: marker_text,
                ts: marker_ts,
                port: conn.id,
                flags: LineFlags::RECONNECT_MARKER,
                spans: Default::default(),
                cursor: None,
            });
        }

        self.active = index;
        self.merged_selected = false;
        self.merged_dirty = true;
        self.save_session();
    }

    /// Connect using the current dialog state, then close it.
    ///
    /// Every path below has already taken the dialog, so a silent return
    /// would close it as if "Connect" had worked (issue #16) — the same
    /// pretend-success `reconnect_with_config` reports on rather than hides.
    pub fn connect_from_dialog(&mut self) {
        let Some(dialog) = self.config_dialog.take() else {
            return;
        };
        let Some(path) = dialog.selected_path else {
            // "Connect" is only enabled with a port selected, so this is not
            // reachable by hand.
            self.record_connect_error(
                "Couldn't connect",
                "no port was selected, so there is nothing to connect to.".to_string(),
            );
            return;
        };
        let Some(port) = self.available.iter().find(|p| p.path == path).cloned() else {
            // Unlike the arm above this one *is* reachable: the port list is
            // refreshed in the background, so a device that unplugs or
            // re-enumerates between filling the dialog in and pressing
            // Connect leaves the selected path behind, naming nothing.
            self.record_connect_error(
                "Couldn't connect",
                format!(
                    "{path} is no longer among the detected ports — it may have been \
                     unplugged or renamed. Open a new connection to pick from the \
                     ports present now."
                ),
            );
            return;
        };
        self.open_connection(port.identity, Some(port.path), dialog.config);
    }

    /// Lower-level open used by manual connect. Saves the session afterward
    /// so a new tab reopens next launch.
    pub(crate) fn open_connection(
        &mut self,
        identity: PortIdentity,
        initial_path: Option<String>,
        port_config: PortConfig,
    ) {
        if self.open_connection_inner(identity, initial_path, port_config, None) {
            self.save_session();
        }
    }

    /// As `open_connection`, but does not persist. Used for the startup
    /// restore loop, where `config.last_open` already holds the desired state
    /// and saving after each entry would rewrite it from whatever has been
    /// restored so far. Returns whether a tab was pushed.
    fn open_connection_inner(
        &mut self,
        identity: PortIdentity,
        initial_path: Option<String>,
        port_config: PortConfig,
        name: Option<String>,
    ) -> bool {
        let id = PortId(self.next_port_id);
        self.next_port_id += 1;
        let path_label = initial_path.clone().unwrap_or_else(|| identity.label());
        let label = detected_connection_label(&identity, &path_label);

        let handle = match self.spawn_serial_reader(id, &identity, &port_config, initial_path) {
            Ok(handle) => handle,
            Err(e) => {
                // No tab exists yet to attach the message to, so there is no
                // use for `report_connect_error`'s returned copy here — queue
                // it directly instead of formatting-then-cloning-then-discarding.
                let msg = format!("couldn't open {}: {e}", identity.label());
                self.record_connect_error("Couldn't connect", msg);
                return false;
            }
        };
        let mut conn = self.make_connection(id, label, identity, port_config, handle);
        conn.name = name.and_then(|name| normalized_tab_name(&name));
        self.connections.push(conn);
        self.active = self.connections.len() - 1;
        self.merged_selected = false;
        self.merged_dirty = true;
        true
    }

    /// Persist the set of currently-open connections so they reopen next
    /// launch.
    fn save_session(&mut self) {
        self.config.last_open = self
            .connections
            .iter()
            // A `Closed` tab is a dead reader kept only for its console
            // history (see `reconnect_with_config`), not something to reopen
            // next launch.
            .filter(|c| c.state != ConnState::Closed)
            .map(|c| SavedConnection {
                identity: c.identity.clone(),
                name: c.name.clone(),
                config: c.port_config.clone(),
            })
            .collect();
        self.write_config();
    }

    pub(crate) fn make_connection(
        &self,
        id: PortId,
        label: String,
        identity: PortIdentity,
        port_config: PortConfig,
        handle: reader::ReaderHandle,
    ) -> Connection {
        let dtr = port_config.dtr_on_open;
        let rts = port_config.rts_on_open;
        let limits = history_limits(self.config.settings.max_lines);
        Connection {
            id,
            label,
            name: None,
            identity,
            port_config,
            handle,
            store: LineStore::new(limits.max_lines),
            state: ConnState::Connecting,
            disconnect_generation: 0,
            follow: true,
            new_since_scroll: 0,
            pin_view_h: 0.0,
            wrap_index: WrapIndex::new(),
            filter_generation: 0,
            top_line: None,
            console_layout: None,
            raw_ring: VecDeque::new(),
            raw_capacity: limits.raw_bytes,
            history_max_lines: limits.max_lines,
            history_allocation_shrink_pending: false,
            raw_evicted_any: false,
            raw_base: 0,
            raw_contiguous_start: 0,
            raw_sessions: Vec::new(),
            last_error: None,
            transfer_progress: None,
            mark_micros: None,
            hex_view: false,
            filter_rules: Vec::new(),
            filter_combine: Combine::And,
            filter_index: FilterIndex::new(),
            filter_dirty: false,
            filter_errors: Vec::new(),
            search_query: String::new(),
            search_case_sensitive: false,
            search_matches: Vec::new(),
            search_pos: None,
            search_dirty: false,
            search_tested_upto: 0,
            selected: None,
            scroll_to: None,
            merged_upto: 0,
            extract_rules: Vec::new(),
            extract_compiled: Vec::new(),
            extract_dirty: false,
            extract_errors: Vec::new(),
            series: Vec::new(),
            series_index: HashMap::new(),
            series_capacity: limits.series_points,
            series_backfilled_capacity: limits.series_points,
            series_evicted_any: false,
            plot_follow: true,
            plot_fit: false,
            show_plot: false,
            tx_input: String::new(),
            tx_history: Vec::new(),
            tx_history_pos: None,
            dtr,
            rts,
        }
    }

    /// Mark the config for a debounced write from [`eframe::App::update`].
    pub fn write_config(&mut self) {
        self.config_dirty_since.get_or_insert_with(Instant::now);
    }

    /// Serialize and atomically replace the platform config file now.
    ///
    /// Failed writes stay dirty so a later update or shutdown can retry them.
    fn flush_config(&mut self) -> bool {
        if self.config_dirty_since.is_none() {
            return true;
        }
        let path = &self.paths.config_file;
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!("creating config directory: {e}");
                self.config_dirty_since = Some(Instant::now());
                return false;
            }
        }
        let result = self
            .config
            .to_toml()
            .map_err(|e| e.to_string())
            .and_then(|toml| serialcore::fs::atomic_write(path, toml).map_err(|e| e.to_string()));
        match result {
            Ok(()) => {
                self.config_dirty_since = None;
                true
            }
            Err(e) => {
                tracing::warn!("writing config: {e}");
                self.config_dirty_since = Some(Instant::now());
                false
            }
        }
    }

    /// Flush a due config change, and make sure an idle UI wakes for it.
    fn maintain_config(&mut self, ctx: &egui::Context) {
        let Some(dirty_since) = self.config_dirty_since else {
            return;
        };
        let elapsed = dirty_since.elapsed();
        if elapsed >= CONFIG_WRITE_DELAY {
            if !self.flush_config() {
                ctx.request_repaint_after(CONFIG_WRITE_DELAY);
            }
        } else {
            ctx.request_repaint_after(CONFIG_WRITE_DELAY - elapsed);
        }
    }

    /// Start a background check for a newer release. `manual` marks the explicit
    /// Menu → "Check for updates" action, which reports a result either way;
    /// the startup check only speaks up when there is a new version.
    pub fn start_update_check(&mut self, manual: bool) {
        if self.update_rx.is_some() || self.install_rx.is_some() {
            return; // one already in flight
        }
        self.update_manual = manual;
        match update::spawn_check(self.wake.clone()) {
            Ok(rx) => self.update_rx = Some(rx),
            Err(e) => self.record_connect_error("Couldn't check for updates", e.to_string()),
        }
    }

    /// Turn a finished update check into a dialog — or into silence. What to say
    /// is decided in `serialcore::update`; this only words it.
    fn poll_update_check(&mut self) {
        // `try_recv` inside the `let` so the borrow of `update_rx` ends before we
        // clear it below.
        let Some(result) = self.update_rx.as_ref().and_then(|rx| rx.try_recv().ok()) else {
            return;
        };
        self.update_rx = None;
        if let Err(e) = &result {
            tracing::warn!("update check: {e}");
        }

        let current = env!("CARGO_PKG_VERSION");
        let notice = update::notice_for(
            result,
            current,
            self.config.settings.skipped_version.as_deref(),
            self.update_manual,
        );

        self.update_dialog = notice.map(|notice| match notice {
            update::Notice::Available { version, url } => UpdateDialog {
                title: "Update available".into(),
                message: format!(
                    "v{} is available. You are on v{current}.\nUpdate will download, install, and restart Pigtail. Active connections will close.",
                    version.trim_start_matches('v')
                ),
                update_version: Some(version.clone()),
                download_url: Some(url),
                skip_version: Some(version),
            },
            update::Notice::UpToDate => UpdateDialog {
                title: "Up to date".into(),
                message: format!("You're running the latest version (v{current})."),
                update_version: None,
                download_url: None,
                skip_version: None,
            },
            update::Notice::Failed(why) => UpdateDialog {
                title: "Update check failed".into(),
                message: why,
                update_version: None,
                download_url: None,
                skip_version: None,
            },
        });
    }

    pub(crate) fn start_update_download(&mut self, version: String) {
        if self.install_rx.is_some() || self.update_rx.is_some() {
            return;
        }
        match update::spawn_download(version, self.wake.clone()) {
            Ok(rx) => {
                self.install_rx = Some(rx);
                self.update_progress = Some(0.0);
                if let Some(dialog) = &mut self.update_dialog {
                    dialog.title = "Downloading update".into();
                    dialog.message = "Downloading Pigtail...".into();
                }
            }
            Err(e) => self.update_install_failed(e.to_string()),
        }
    }

    fn update_install_failed(&mut self, message: String) {
        self.install_rx = None;
        self.update_progress = None;
        if let Some(dialog) = &mut self.update_dialog {
            dialog.title = "Update failed".into();
            dialog.message = message;
        }
    }

    fn poll_update_install(&mut self, ctx: &egui::Context) {
        loop {
            let event = match self.install_rx.as_ref().map(|rx| rx.try_recv()) {
                Some(Ok(event)) => event,
                Some(Err(crossbeam_channel::TryRecvError::Disconnected)) => {
                    self.update_install_failed(
                        "The updater stopped unexpectedly. Please try again.".into(),
                    );
                    break;
                }
                _ => break,
            };
            match event {
                update::InstallEvent::Progress { downloaded, total } => {
                    self.update_progress = Some(downloaded as f32 / total as f32);
                }
                update::InstallEvent::Downloaded(Ok(prepared)) => {
                    // Do not close or replace the application if its settings
                    // could not be saved. Dropping prepared removes the download.
                    if !self.flush_config() {
                        self.update_install_failed("Could not save settings. Resolve the save error, then try updating again.".into());
                        break;
                    }
                    match update::spawn_install(prepared, self.wake.clone()) {
                        Ok(rx) => {
                            self.install_rx = Some(rx);
                            self.update_progress = None;
                            if let Some(dialog) = &mut self.update_dialog {
                                dialog.title = "Installing update".into();
                                dialog.message = "Installing Pigtail...".into();
                            }
                        }
                        Err(e) => self.update_install_failed(e.to_string()),
                    }
                    break;
                }
                update::InstallEvent::Installed(Ok(outcome)) => {
                    self.install_rx = None;
                    if let update::InstallOutcome::Restart(path) = outcome {
                        *crate::RESTART_PATH.lock().expect("restart path lock") = Some(path);
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    break;
                }
                update::InstallEvent::Downloaded(Err(e))
                | update::InstallEvent::Installed(Err(e)) => {
                    self.update_install_failed(e);
                    break;
                }
            }
        }
    }

    /// Disconnect and close the active connection tab.
    pub fn close_connection(&mut self, index: usize) {
        if index >= self.connections.len() {
            return;
        }
        let conn = self.connections.remove(index);
        if self.merged_tx_port == Some(conn.id) {
            self.merged_tx_port = None;
        }
        conn.handle.shutdown();
        if self.active >= self.connections.len() {
            self.active = self.connections.len().saturating_sub(1);
        }
        self.merged_dirty = true;
        self.save_session();
    }

    fn poll_enumerator(&mut self) {
        while let Ok(ev) = self.enum_rx.try_recv() {
            if let EnumEvent::Snapshot(snap) = ev {
                self.available = snap;
                // Restored tabs are opened before the enumerator's first
                // snapshot, so they initially know only their saved identity.
                // Once the current device is resolved, include its live OS path
                // in the detected label used by the tab tooltip.
                for conn in &mut self.connections {
                    if let Some(port) = resolved_port(&conn.identity, &self.available) {
                        conn.label = detected_connection_label(&conn.identity, &port.path);
                    }
                }
            }
        }
    }

    /// Recompile global highlight rules when they change.
    fn rebuild_highlight_if_dirty(&mut self) {
        if !self.highlight_dirty {
            return;
        }
        self.highlight_dirty = false;
        self.highlight_cache.clear();
        for rule in &self.config.highlight {
            if !rule.enabled || rule.pattern.is_empty() {
                continue;
            }
            let Ok(re) = regex::RegexBuilder::new(&rule.pattern)
                .case_insensitive(!rule.case_sensitive)
                .build()
            else {
                continue;
            };
            self.highlight_cache.push(CompiledHighlight {
                re,
                color: parse_hex_color(&rule.color)
                    .unwrap_or(egui::Color32::from_rgb(0xff, 0x55, 0x55)),
            });
        }
    }

    /// Extend/rebuild each connection's filter index and prune evicted entries.
    fn maintain_filters(&mut self) {
        for conn in &mut self.connections {
            let (set, errors) = FilterSet::compile(&conn.filter_rules, conn.filter_combine);
            conn.filter_errors = errors;
            if conn.filter_dirty {
                conn.filter_dirty = false;
                conn.filter_index.rebuild(&conn.store, &set);
                // The displayed set just changed out from under the row index.
                conn.filter_generation += 1;
            } else {
                conn.filter_index.prune_evicted(&conn.store);
                conn.filter_index.extend(&conn.store, &set);
            }
        }
    }

    /// Recompute search matches (incremental, per active connection only).
    fn maintain_search(&mut self) {
        let Some(conn) = self.connections.get_mut(self.active) else {
            return;
        };
        if conn.search_query.is_empty() {
            conn.search_matches.clear();
            conn.search_pos = None;
            conn.search_tested_upto = conn.store.next_abs_index();
            return;
        }
        let Some(re) = compile_search(&conn.search_query, conn.search_case_sensitive) else {
            return;
        };

        if conn.search_dirty {
            conn.search_dirty = false;
            conn.search_matches.clear();
            conn.search_tested_upto = conn.store.first_abs_index();
        }
        // Drop evicted matches. `search_pos` is an index *into* this vec, so it
        // has to come down by however many were dropped off the front, or the
        // hit the user is standing on silently becomes a different one. If the
        // cursor's own match was among those dropped, it's invalidated rather
        // than clamped, so it doesn't silently jump to an unrelated match.
        let first = conn.store.first_abs_index();
        if let Some(p) = conn.search_matches.iter().position(|&i| i >= first) {
            if p > 0 {
                conn.search_matches.drain(..p);
                conn.search_pos =
                    conn.search_pos
                        .and_then(|cur| if cur < p { None } else { Some(cur - p) });
            }
        } else if !conn.search_matches.is_empty() {
            // Every recorded match was evicted.
            conn.search_matches.clear();
            conn.search_pos = None;
        }

        let end = conn.store.next_abs_index();
        // The newest line is re-tested rather than trusted, exactly as
        // `FilterIndex::extend` does and for the same reason: a line the device
        // is still writing is shown provisionally and then *replaced in place*
        // when it grows, keeping its absolute index — so "tested already" is not
        // the same as "settled", and a half-written line that didn't match yet
        // would otherwise stay unfindable for good (issue #39).
        let newest = end.saturating_sub(1).max(first);
        let start = conn.search_tested_upto.max(first).min(newest);
        // Its verdict from last time goes with it: it may have matched then and
        // not now. `search_matches` is ascending, so that is a suffix trim.
        while conn.search_matches.last().is_some_and(|&abs| abs >= start) {
            conn.search_matches.pop();
        }
        for abs in start..end {
            if let Some(line) = conn.store.get(abs) {
                if re.is_match(line.text) {
                    conn.search_matches.push(abs);
                }
            }
        }
        conn.search_tested_upto = end;

        // A trim can leave the cursor past the end of what survived.
        if conn
            .search_pos
            .is_some_and(|p| p >= conn.search_matches.len())
        {
            conn.search_pos = None;
        }
        if conn.search_pos.is_none() && !conn.search_matches.is_empty() {
            conn.search_pos = Some(conn.search_matches.len() - 1);
        }
    }

    /// Finish a coalesced history-capacity edit across every tab. A decrease can
    /// evict console lines without a reader event, so it must also invalidate
    /// the merged view: otherwise quiet and closed tabs leave dead blank rows in
    /// that cache until another connection happens to produce output.
    pub(crate) fn finish_history_capacity_changes(&mut self) -> bool {
        let mut settled_change = false;
        let mut history_shrank = false;
        for conn in &mut self.connections {
            settled_change |= conn.series_capacity > conn.series_backfilled_capacity
                || conn.history_allocation_shrink_pending;
            history_shrank |= conn.finish_history_capacity_change();
        }
        self.merged_dirty |= history_shrank;
        settled_change
    }

    /// Maintain the timestamp-interleaved merged view (spec §7.12). Rebuilds on
    /// connect/close; otherwise a fast append of each port's new tail.
    fn maintain_merged(&mut self) {
        let rebuilding = self.merged_dirty;
        if self.merged_dirty {
            self.merged_dirty = false;
            self.merged.clear();
            // A rebuild can remove entries (for example when a tab closes or
            // history is trimmed), so the old aggregate unread count no longer
            // describes the rebuilt view.
            self.merged_new_since_scroll = 0;
            self.merged_seq = 0;
            self.merged_generation += 1;
            self.merged_pruned_before.clear();
            for conn in &mut self.connections {
                let first = conn.store.first_abs_index();
                conn.merged_upto = first;
                self.merged_pruned_before.insert(conn.id, first);
            }
        }
        // Collect new entries from every port.
        let mut fresh: Vec<MergedEntry> = Vec::new();
        for conn in &mut self.connections {
            let start = conn.merged_upto.max(conn.store.first_abs_index());
            let end = conn.store.next_abs_index();
            for abs in start..end {
                if let Some(line) = conn.store.get(abs) {
                    fresh.push(MergedEntry {
                        micros: line.meta.ts.micros,
                        port: conn.id,
                        abs,
                        // Stamped below, once the batch is in view order.
                        seq: 0,
                    });
                }
            }
            conn.merged_upto = end;
        }
        self.prune_merged();
        if fresh.is_empty() {
            return;
        }
        if !rebuilding && !self.merged_follow {
            self.merged_new_since_scroll = self
                .merged_new_since_scroll
                .saturating_add(fresh.len() as u64);
        }
        fresh.sort_by_key(|e| e.micros);
        for e in &mut fresh {
            e.seq = self.merged_seq;
            self.merged_seq += 1;
        }
        // Fast path: the new tail is entirely after what we already have. An
        // empty view always qualifies — hence `MIN` and not `0`, which restored
        // history (stamped before this run's start, so negative) would fail.
        let last_micros = self.merged.last().map(|e| e.micros).unwrap_or(i64::MIN);
        if fresh[0].micros >= last_micros {
            self.merged.extend(fresh);
        } else {
            // Rare: a slow port produced an earlier timestamp. Merge properly —
            // and since that reorders entries the row index already counted,
            // make it count them again.
            self.merged.extend(fresh);
            self.merged.sort_by_key(|e| e.micros);
            // The sort interleaved the new entries among the old, so the
            // numbering no longer runs along the view. Lay it down again — the
            // index is rebuilding this frame regardless.
            for (i, e) in self.merged.iter_mut().enumerate() {
                e.seq = i as u64;
            }
            self.merged_seq = self.merged.len() as u64;
            self.merged_generation += 1;
        }
    }

    /// True when the merged pseudo-tab has at least one enabled, non-empty
    /// filter rule. This deliberately follows `Connection::filter_index_active`:
    /// even an invalid active rule keeps the filtered view selected while its
    /// compile error is shown in the filter window.
    pub(crate) fn merged_filter_active(&self) -> bool {
        !self
            .merged_filter_rules
            .iter()
            .all(|r| !r.enabled || r.pattern.is_empty())
    }

    /// The entries currently displayed by the merged pseudo-tab.
    pub(crate) fn merged_view(&self) -> &[MergedEntry] {
        if self.merged_filter_active() {
            &self.merged_filtered
        } else {
            &self.merged
        }
    }

    pub(crate) fn merged_view_generation(&self) -> u64 {
        if self.merged_filter_active() {
            // The low bit identifies which backing view the generation belongs
            // to. The two counters advance independently and often hold the
            // same value, so returning either one raw would let a filter toggle
            // look unchanged to the wrap and search caches.
            self.merged_filter_generation
                .wrapping_mul(2)
                .wrapping_add(1)
        } else {
            self.merged_generation.wrapping_mul(2)
        }
    }

    /// Maintain the filtered merged subset without rescanning the entire log
    /// for every paint. Filter edits and merged reorders rebuild it; ordinary
    /// output appends new matches and only re-tests each port's mutable newest
    /// line (the same provisional-line rule used by `FilterIndex`).
    fn maintain_merged_filter(&mut self, any_data: bool) {
        let (set, errors) =
            FilterSet::compile(&self.merged_filter_rules, self.merged_filter_combine);
        self.merged_filter_errors = errors;

        if !self.merged_filter_active() {
            if !self.merged_filtered.is_empty() {
                self.merged_filtered.clear();
                self.merged_filter_generation += 1;
            }
            self.merged_filter_dirty = false;
            self.merged_filter_source_generation = self.merged_generation;
            self.merged_filter_upto_seq = self.merged_seq;
            return;
        }

        if self.merged_filter_dirty
            || self.merged_filter_source_generation != self.merged_generation
        {
            self.merged_filter_dirty = false;
            self.merged_filtered = self
                .merged
                .iter()
                .copied()
                .filter(|entry| merged_entry_matches(&self.connections, *entry, &set))
                .collect();
            self.merged_filter_generation += 1;
            self.merged_filter_source_generation = self.merged_generation;
            self.merged_filter_upto_seq = self.merged_seq;
            return;
        }

        // A store eviction only removes a prefix of `merged`, so the filtered
        // cache can lose the same prefix without forcing the wrap index to
        // rebuild: its strictly increasing `seq` keys identify the shift.
        if let Some(first) = self.merged.first().map(|entry| entry.seq) {
            let keep = self
                .merged_filtered
                .partition_point(|entry| entry.seq < first);
            if keep > 0 {
                self.merged_filtered.drain(..keep);
            }
        } else {
            self.merged_filtered.clear();
        }

        if !any_data {
            return;
        }

        let old_upto = self.merged_filter_upto_seq;
        let mut candidates: Vec<(MergedEntry, bool)> = self
            .merged
            .iter()
            .copied()
            .filter(|entry| entry.seq >= old_upto)
            .map(|entry| {
                let matches = merged_entry_matches(&self.connections, entry, &set);
                (entry, matches)
            })
            .collect();

        // The newest line on every port may have been replaced in place rather
        // than appended. Re-test its old merged entry as well.
        for conn in &self.connections {
            let Some(abs) = conn.store.next_abs_index().checked_sub(1) else {
                continue;
            };
            let Some(entry) = self
                .merged
                .iter()
                .rev()
                .find(|entry| entry.port == conn.id && entry.abs == abs)
                .copied()
            else {
                continue;
            };
            if entry.seq < old_upto {
                candidates.push((entry, merged_entry_matches(&self.connections, entry, &set)));
            }
        }

        let mut changed_old_entry = false;
        for (entry, matches) in candidates {
            match self
                .merged_filtered
                .binary_search_by_key(&entry.seq, |candidate| candidate.seq)
            {
                Ok(pos) if !matches => {
                    self.merged_filtered.remove(pos);
                    changed_old_entry |= entry.seq < old_upto;
                }
                Err(pos) if matches => {
                    self.merged_filtered.insert(pos, entry);
                    changed_old_entry |= entry.seq < old_upto;
                }
                _ => {}
            }
        }
        if changed_old_entry {
            // Insertion/removal in the middle changes every following visual
            // row. Appends are handled incrementally by `WrapIndex` instead.
            self.merged_filter_generation += 1;
        }
        self.merged_filter_upto_seq = self.merged_seq;
    }

    /// Maintain search matches over exactly what the merged view displays.
    /// Like the per-port search, regex errors fall back to a literal search.
    fn maintain_merged_search(&mut self, any_data: bool) {
        let source_generation = self.merged_view_generation();
        if self.merged_search_query.is_empty() {
            self.merged_search_matches.clear();
            self.merged_search_pos = None;
            self.merged_search_dirty = false;
            self.merged_search_source_generation = source_generation;
            self.merged_search_upto_seq = self.merged_seq;
            return;
        }
        let Some(re) = compile_search(&self.merged_search_query, self.merged_search_case_sensitive)
        else {
            return;
        };

        if self.merged_search_dirty || self.merged_search_source_generation != source_generation {
            self.merged_search_dirty = false;
            self.merged_search_matches = self
                .merged_view()
                .iter()
                .copied()
                .filter(|entry| merged_entry_searches(&self.connections, *entry, &re))
                .collect();
            self.merged_search_pos = (!self.merged_search_matches.is_empty())
                .then(|| self.merged_search_matches.len() - 1);
            self.merged_search_source_generation = source_generation;
            self.merged_search_upto_seq = self.merged_seq;
            return;
        }

        let current = self
            .merged_search_pos
            .and_then(|pos| self.merged_search_matches.get(pos))
            .map(|entry| entry.seq);
        let view = self.merged_view();
        if let Some(first) = view.first().map(|entry| entry.seq) {
            let keep = self
                .merged_search_matches
                .partition_point(|entry| entry.seq < first);
            if keep > 0 {
                self.merged_search_matches.drain(..keep);
            }
        } else {
            self.merged_search_matches.clear();
        }

        if any_data {
            let old_upto = self.merged_search_upto_seq;
            let mut candidates: Vec<(MergedEntry, bool)> = self
                .merged_view()
                .iter()
                .copied()
                .filter(|entry| entry.seq >= old_upto)
                .map(|entry| {
                    let matches = merged_entry_searches(&self.connections, entry, &re);
                    (entry, matches)
                })
                .collect();
            for conn in &self.connections {
                let Some(abs) = conn.store.next_abs_index().checked_sub(1) else {
                    continue;
                };
                let Some(entry) = self
                    .merged_view()
                    .iter()
                    .rev()
                    .find(|entry| entry.port == conn.id && entry.abs == abs)
                    .copied()
                else {
                    continue;
                };
                if entry.seq < old_upto {
                    candidates.push((entry, merged_entry_searches(&self.connections, entry, &re)));
                }
            }
            for (entry, matches) in candidates {
                match self
                    .merged_search_matches
                    .binary_search_by_key(&entry.seq, |candidate| candidate.seq)
                {
                    Ok(pos) if !matches => {
                        self.merged_search_matches.remove(pos);
                    }
                    Err(pos) if matches => self.merged_search_matches.insert(pos, entry),
                    _ => {}
                }
            }
            self.merged_search_upto_seq = self.merged_seq;
        }

        self.merged_search_pos = current
            .and_then(|seq| {
                self.merged_search_matches
                    .binary_search_by_key(&seq, |entry| entry.seq)
                    .ok()
            })
            .or_else(|| {
                (!self.merged_search_matches.is_empty())
                    .then(|| self.merged_search_matches.len() - 1)
            });
    }

    /// Drop merged entries whose line has been evicted from its port's store.
    ///
    /// Without this the merged view is the one place in the app that grows
    /// without bound: every port's store evicts at `max_lines`, but an entry
    /// here outlives the line it points at, and is then a row that can only
    /// ever draw as blank space. It is also built on every frame that carries
    /// data, whether or not the merged tab is the one on screen, so a long
    /// session at speed leaks it steadily.
    ///
    /// Each port evicts independently, so a quiet port can keep the first merged
    /// entry alive while a busy port leaves dead entries later in the
    /// interleaving. Remove those interior entries too. A prefix-only removal
    /// remains incremental; an interior removal bumps the generation because it
    /// changes the position of every following row.
    fn prune_merged(&mut self) {
        let first_by_port: HashMap<PortId, u64> = self
            .connections
            .iter()
            .map(|conn| (conn.id, conn.store.first_abs_index()))
            .collect();
        let eviction_advanced = first_by_port.len() != self.merged_pruned_before.len()
            || first_by_port
                .iter()
                .any(|(port, first)| self.merged_pruned_before.get(port) != Some(first));
        if !eviction_advanced {
            return;
        }
        let mut saw_live = false;
        let mut removed_after_live = false;
        self.merged.retain(|entry| {
            let live = first_by_port
                .get(&entry.port)
                .is_some_and(|&first| entry.abs >= first);
            if live {
                saw_live = true;
            } else if saw_live {
                removed_after_live = true;
            }
            live
        });
        if removed_after_live {
            self.merged_generation += 1;
        }
        self.merged_pruned_before = first_by_port;
    }

    /// Clear the console: drop every line on screen *and* the capture on disk.
    ///
    /// Deliberately destructive — the point is that cleared output is gone, not
    /// merely scrolled away, so it also can't come back as preloaded history on
    /// the next launch. `port` names a single connection to clear, which is
    /// what a merged *row*'s menu means; without one, the merged view clears
    /// every port (that is what the window is showing) and a single tab clears
    /// itself.
    ///
    /// Bytes already in flight (read but not yet drained from the reader
    /// channel) still land afterwards. That's a line or two at most, and they
    /// are output that arrived after the click.
    pub fn clear_console(&mut self, port: Option<PortId>) {
        let targets: Vec<usize> = match port {
            Some(id) => self
                .connections
                .iter()
                .position(|c| c.id == id)
                .into_iter()
                .collect(),
            None if self.merged_selected => (0..self.connections.len()).collect(),
            None => self.active_index().into_iter().collect(),
        };
        for i in targets {
            let conn = &mut self.connections[i];
            // A Closed tab's reader thread is gone, so this command would
            // silently vanish into a dropped channel — the on-disk capture
            // wouldn't actually be truncated even though the UI below is
            // cleared regardless. Skip it, matching the DTR/RTS/break/
            // transmit guards elsewhere against a zombie tab.
            if conn.state != ConnState::Closed {
                conn.handle.clear_log();
            }
            conn.store.clear();
            // The offsets restart at zero with the next byte: nothing is left
            // above for them to be counted from.
            conn.raw_base = conn.raw_next();
            conn.raw_ring.clear();
            conn.raw_contiguous_start = conn.raw_base;
            conn.raw_sessions.clear();
            // Everything derived from the lines that just went away. The dirty
            // flags make the next frame rebuild both indices against the now
            // empty store rather than leaving stale absolute indices behind.
            conn.filter_dirty = true;
            conn.search_dirty = true;
            conn.search_matches.clear();
            conn.search_pos = None;
            conn.search_tested_upto = conn.store.next_abs_index();
            conn.series.clear();
            conn.series_index.clear();
            conn.selected = None;
            conn.scroll_to = None;
            conn.mark_micros = None;
            conn.new_since_scroll = 0;
            // An empty console has nothing to scroll back to, so resume live.
            conn.follow = true;
        }
        self.merged_dirty = true;
    }

    /// True while any modal dialog or floating tool window is open. Their
    /// controls (buttons, checkboxes, ComboBoxes) never take egui focus, so
    /// `memory().focused().is_none()` alone wouldn't catch them; callers that
    /// need to know whether such UI is soaking up clicks/keys (e.g. gating
    /// raw console input) should check this instead of the individual
    /// fields, so a newly added window only needs to be listed here once.
    pub fn floating_window_open(&self) -> bool {
        self.config_dialog.is_some()
            || self.rename_dialog.is_some()
            || self.file_transfer_dialog.is_some()
            || !self.connect_errors.is_empty()
            || self.update_dialog.is_some()
            || self.show_settings
            || self.show_macros_win
            || self.show_keyboard_shortcuts
            || self.macro_editor.is_some()
            || self.macro_running_edit_confirmation.is_some()
            || self.macro_shortcut_conflict.is_some()
            || self.retention_cleanup_confirmation.is_some()
            || self.show_filters_win
            || self.show_highlight_win
            || self.show_extract_win
            || self.show_error_win.is_some()
    }

    /// Index of the active connection, clamped, or `None` if there are none.
    pub fn active_index(&self) -> Option<usize> {
        if self.connections.is_empty() {
            None
        } else {
            Some(self.active.min(self.connections.len() - 1))
        }
    }

    /// Jump to the next/previous search match.
    pub fn search_step(&mut self, dir: i64) {
        if self.merged_selected {
            if self.merged_search_matches.is_empty() {
                return;
            }
            let len = self.merged_search_matches.len() as i64;
            let cur = self.merged_search_pos.unwrap_or(0) as i64;
            let next = (cur + dir).rem_euclid(len) as usize;
            self.merged_search_pos = Some(next);
            self.merged_scroll_to = Some(self.merged_search_matches[next]);
            return;
        }
        let Some(conn) = self.connections.get_mut(self.active) else {
            return;
        };
        if conn.search_matches.is_empty() {
            return;
        }
        let len = conn.search_matches.len() as i64;
        let cur = conn.search_pos.unwrap_or(0) as i64;
        let next = (cur + dir).rem_euclid(len) as usize;
        conn.search_pos = Some(next);
        let abs = conn.search_matches[next];
        conn.selected = Some(abs);
        conn.scroll_to = Some(abs);
    }
}

fn merged_entry_matches(connections: &[Connection], entry: MergedEntry, set: &FilterSet) -> bool {
    connections
        .iter()
        .find(|conn| conn.id == entry.port)
        .and_then(|conn| conn.store.get(entry.abs))
        .is_some_and(|line| set.matches(line.text))
}

fn merged_entry_searches(
    connections: &[Connection],
    entry: MergedEntry,
    re: &regex::Regex,
) -> bool {
    connections
        .iter()
        .find(|conn| conn.id == entry.port)
        .and_then(|conn| conn.store.get(entry.abs))
        .is_some_and(|line| re.is_match(line.text))
}

/// Parse `#rrggbb` into a colour.
pub fn parse_hex_color(s: &str) -> Option<egui::Color32> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(egui::Color32::from_rgb(r, g, b))
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_enumerator();
        self.poll_update_check();
        self.poll_update_install(ctx);
        if self.install_rx.is_some()
            && self.update_progress.is_none()
            && ctx.input(|i| i.viewport().close_requested())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        self.poll_file_drop(ctx);
        self.close_window_on_escape(ctx);

        // Macro shortcuts belong to the unfocused console, just like raw
        // keyboard input. Consume an assigned chord before it can become bytes
        // for the device or activate a widget during layout.
        self.consume_macro_shortcut(ctx);
        self.consume_tab_switch_shortcut(ctx);

        let max_lines = self.config.settings.max_lines;
        let mut any_data = false;
        for conn in &mut self.connections {
            // Before the drain: a rule edited last frame re-reads the console's
            // history here, and the lines arriving below then extend it once.
            conn.maintain_extract();
            if conn.drain_events(max_lines) {
                any_data = true;
            }
        }

        // Maintain derived indices.
        self.rebuild_highlight_if_dirty();
        self.maintain_filters();
        if any_data || self.merged_dirty {
            self.maintain_merged();
        }
        self.maintain_merged_filter(any_data);
        self.maintain_search();
        self.maintain_merged_search(any_data);

        // Minimal chrome: a header of tabs on top, a status footer at the
        // bottom, and the console filling everything in between. Tool panels are
        // floating windows toggled from the console's right-click menu.
        // Claim a console-owned Tab before any focusable widgets see the frame.
        // Otherwise egui can focus a header control, and a batched Enter/Space
        // event can activate that control before `show_console` gives Tab back
        // to the device.
        let console_tab_claimed = self.claim_console_tab_before_layout(ctx);
        self.show_header(ctx);
        self.show_footer(ctx);
        self.show_plot(ctx); // bottom panel, only when enabled for the tab
        self.show_console(ctx, console_tab_claimed);
        self.show_file_drop_overlay(ctx);

        // Floating windows.
        self.show_config_dialog(ctx);
        self.show_rename_dialog(ctx);
        self.show_tool_windows(ctx);
        self.show_macros_window(ctx);
        self.show_settings_window(ctx);
        self.show_keyboard_shortcuts_window(ctx);
        self.show_update_dialog(ctx);
        self.show_file_transfer(ctx);
        self.show_font_toast(ctx);
        self.show_connect_error(ctx);
        // Keep the guard focused until every widget has been drawn, so none of
        // the later floating windows can claim this Tab either.
        self.release_console_tab_after_layout(ctx, console_tab_claimed);

        // Sends every command whose deadline has arrived and schedules only
        // the next deadline, keeping an otherwise idle console asleep between
        // macro steps.
        self.maintain_macro_runs(ctx);

        // A close-request frame may be the last update the app receives. Save
        // immediately in that case; otherwise coalesce rapid edits and wake the
        // on-demand UI when their one-second deadline arrives.
        if ctx.input(|i| i.viewport().close_requested()) {
            self.flush_config();
        } else {
            self.maintain_config(ctx);
        }

        // egui only draws when something asks it to, and nothing here animates on
        // its own clock, so an *open but silent* connection must not schedule
        // frames — doing so cost ~7.5% of a core redrawing an unchanged frame at
        // 60fps, on an empty console, and more with rows on screen. What brings
        // us back is `self.wake`: reader and enumerator threads request a repaint
        // when they actually produce something.
        //
        // The one case a wake can't cover is a reader whose event channel filled
        // while we slept: it has nothing it can send, so it can't wake us, and we
        // are the only one who can drain it. So whenever a frame *did* see data,
        // schedule the next one — that keeps a backlog draining, and keeps
        // follow-tail smooth through a burst.
        if any_data {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.flush_config();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serialcore::session::SessionWriter;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pigtail-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    fn identity(serial: &str) -> PortIdentity {
        PortIdentity {
            vid: Some(0x0483),
            pid: Some(0x374B),
            serial_number: Some(serial.to_string()),
            ..Default::default()
        }
    }

    /// Write a capture holding `records`, returning what `snapshot_captures`
    /// would have produced for it.
    fn capture(
        dir: &std::path::Path,
        identity: &PortIdentity,
        start: chrono::DateTime<chrono::Utc>,
        records: &[(u64, &[u8])],
    ) -> (PathBuf, SessionMeta) {
        let meta = SessionMeta {
            identity: identity.clone(),
            config: PortConfig::default(),
            start_wall: start,
            app_version: "test".into(),
            port_label: identity.label(),
            cleared: false,
        };
        let mut w = SessionWriter::create(dir, &meta).unwrap();
        for (micros, bytes) in records {
            w.write_record(*micros, bytes).unwrap();
        }
        w.flush().unwrap();
        (w.bin_path().to_path_buf(), meta)
    }

    fn texts(restored: &RestoredHistory<'_>) -> Vec<String> {
        restored
            .iter()
            .flat_map(|(_, records)| records.iter())
            .map(|(_, bytes)| String::from_utf8_lossy(bytes).to_string())
            .collect()
    }

    #[test]
    fn history_spans_every_capture_of_the_device() {
        let dir = scratch("history");
        let dev = identity("A1");
        // One run of the app leaves several captures behind — applying new port
        // options respawns the reader onto a fresh one — and they all carry the
        // app clock's `start_wall`, so only the record stamps order them.
        let run = chrono::Utc::now();
        let older = capture(&dir, &dev, run, &[(1_000, b"before the reconnect\n")]);
        let newer = capture(&dir, &dev, run, &[(9_000, b"after it\n")]);
        // A second device's capture must not leak into this one's history.
        let other = capture(&dir, &identity("B2"), run, &[(2_000, b"other device\n")]);

        let captures = vec![newer, older, other];
        let restored = gather_history(&captures, &dev, 1 << 20);
        assert_eq!(
            texts(&restored),
            vec!["before the reconnect\n", "after it\n"],
            "both captures restore, oldest first"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn history_stops_at_a_cleared_capture() {
        let dir = scratch("cleared");
        let dev = identity("A1");
        let run = chrono::Utc::now();
        let older = capture(&dir, &dev, run, &[(1_000, b"cleared away\n")]);
        let mut newer = capture(&dir, &dev, run, &[(9_000, b"kept\n")]);
        newer.1.cleared = true;

        let captures = vec![older, newer];
        let restored = gather_history(&captures, &dev, 1 << 20);
        assert_eq!(
            texts(&restored),
            vec!["kept\n"],
            "output discarded by Clear console does not come back"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn history_stops_at_a_capture_cleared_with_nothing_after_it() {
        // Clear console empties the capture, so until new output arrives it has
        // no records — and so no first stamp to order it by. It is still the
        // newest capture of the run, and the walk back has to stop at it rather
        // than treat it as the oldest and reach past it.
        let dir = scratch("cleared-empty");
        let dev = identity("A1");
        let run = chrono::Utc::now();
        let older = capture(&dir, &dev, run, &[(1_000, b"cleared away\n")]);
        let mut newer = capture(&dir, &dev, run, &[]);
        newer.1.cleared = true;

        let captures = vec![older, newer];
        let restored = gather_history(&captures, &dev, 1 << 20);
        assert!(
            texts(&restored).is_empty(),
            "a clear with no output after it still holds back the older captures"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn history_is_bounded_by_the_byte_budget() {
        let dir = scratch("budget");
        let dev = identity("A1");
        let run = chrono::Utc::now();
        let older = capture(&dir, &dev, run, &[(1_000, b"0123456789")]);
        let newer = capture(&dir, &dev, run, &[(9_000, b"0123456789")]);

        let captures = vec![older, newer];
        let restored = gather_history(&captures, &dev, 10);
        assert_eq!(
            texts(&restored).len(),
            1,
            "the budget stops the walk at the newest capture"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
    fn stored(lines: &[(&str, i64, LineFlags)]) -> LineStore {
        let mut store = LineStore::new(1000);
        for (text, micros, flags) in lines {
            store.append(IncomingLine {
                text: (*text).to_string(),
                ts: Timestamp {
                    wall: chrono::Utc::now(),
                    micros: *micros,
                },
                port: PortId(0),
                flags: *flags,
                spans: Default::default(),
                cursor: None,
            });
        }
        store
    }

    /// The default rule: every `name:value` / `name=value` pair on a line.
    fn kv_rules() -> Vec<CompiledExtract> {
        vec![CompiledExtract::compile(&ExtractRule {
            mode: serialcore::config::ExtractMode::Kv,
            prefix: None,
            pattern: None,
            kv_separators: None,
        })
        .unwrap()]
    }

    fn extracted(store: &LineStore, remembered: &HashMap<String, SeriesStyle>) -> Vec<SeriesEntry> {
        let mut series = Vec::new();
        let mut index = HashMap::new();
        extract_all(
            store,
            &kv_rules(),
            &mut series,
            &mut index,
            remembered,
            history_limits(1_000_000).series_points,
        );
        series
    }

    /// The point of re-reading: a rule is written after seeing the output that
    /// suggested it, and used to plot nothing until the *next* line arrived.
    #[test]
    fn a_rule_reads_the_lines_already_in_the_console() {
        let store = stored(&[
            ("temp:1", 1_000_000, LineFlags::default()),
            ("nothing here", 2_000_000, LineFlags::default()),
            ("temp:3", 3_000_000, LineFlags::default()),
        ]);
        let series = extracted(&store, &HashMap::new());
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].series.name(), "temp");
        assert_eq!(series[0].series.len(), 2);
        let last = series[0].series.last().unwrap();
        assert_eq!((last.t, last.value), (3.0, 3.0));
        assert_eq!(last.line, 2, "the point still points back at its line");
    }

    /// Everything the live path skips, the re-read skips too — plus the
    /// previous run's output, which sits at a negative stamp.
    #[test]
    fn a_re_read_takes_only_this_session_s_device_output() {
        let store = stored(&[
            ("temp:9", -60_000_000, LineFlags::default()),
            ("temp:2", 0, LineFlags::RECONNECT_MARKER),
            ("temp:5", 1_000_000, LineFlags::TX_ECHO),
            ("temp:7", 2_000_000, LineFlags::default()),
        ]);
        let series = extracted(&store, &HashMap::new());
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].series.len(), 1);
        assert_eq!(series[0].series.last().unwrap().value, 7.0);
    }

    /// Editing a rule must not reshuffle a plot you have just set up.
    #[test]
    fn a_re_read_keeps_the_look_of_a_series_it_finds_again() {
        let store = stored(&[("temp:1 rpm:2", 1_000_000, LineFlags::default())]);
        let mut remembered = HashMap::new();
        remembered.insert(
            "rpm".to_string(),
            SeriesStyle {
                color: egui::Color32::RED,
                visible: false,
                own_axis: true,
            },
        );
        let series = extracted(&store, &remembered);
        let rpm = series.iter().find(|e| e.series.name() == "rpm").unwrap();
        assert_eq!(rpm.color, egui::Color32::RED);
        assert!(!rpm.visible);
        assert!(rpm.own_axis, "the Y2 toggle survives the re-read");
        let temp = series.iter().find(|e| e.series.name() == "temp").unwrap();
        assert_eq!(
            temp.color, SERIES_PALETTE[0],
            "a series seen for the first time still takes a palette colour"
        );
    }

    /// A line the device is still writing is shown provisionally and replaced
    /// in place as it grows, so extracting one plots a number that is still
    /// being typed. The re-read has always skipped it by construction (it walks
    /// the settled store); this pins the rule down explicitly.
    #[test]
    fn a_re_read_skips_a_line_the_device_is_still_writing() {
        let store = stored(&[
            ("temp:23", 1_000_000, LineFlags::PROVISIONAL),
            ("rpm:1200", 2_000_000, LineFlags::default()),
        ]);
        let series = extracted(&store, &HashMap::new());
        assert_eq!(
            series
                .iter()
                .map(|e| e.series.name().to_string())
                .collect::<Vec<_>>(),
            vec!["rpm".to_string()],
            "a half-written line is not a sample"
        );
    }
    fn ring(cap: usize, pushes: &[&[u8]]) -> (VecDeque<u8>, u64, bool) {
        let mut ring = VecDeque::new();
        let mut base = 0;
        let mut evicted = false;
        for bytes in pushes {
            evicted |= push_raw(&mut ring, &mut base, bytes, cap);
        }
        (ring, base, evicted)
    }

    /// `base + len` has to stay the count of every byte ever pushed: the hex
    /// view resolves a run's absolute start through it, and a byte lost without
    /// being counted would slide every offset in the view.
    #[test]
    fn the_raw_ring_counts_every_byte_it_drops() {
        let (ring, base, evicted) = ring(4, &[b"ab", b"cdef"]);
        assert_eq!(base + ring.len() as u64, 6);
        assert_eq!(ring.iter().copied().collect::<Vec<u8>>(), b"cdef");
        assert_eq!(base, 2);
        assert!(evicted);
    }

    /// A push bigger than the ring keeps its tail — and still counts the head it
    /// threw away.
    #[test]
    fn a_push_larger_than_the_ring_keeps_its_tail() {
        let (ring, base, evicted) = ring(4, &[b"abcdefghij"]);
        assert_eq!(ring.iter().copied().collect::<Vec<u8>>(), b"ghij");
        assert_eq!(base, 6);
        assert_eq!(base + ring.len() as u64, 10);
        assert!(evicted);
    }

    #[test]
    fn a_ring_under_capacity_drops_nothing() {
        let (ring, base, evicted) = ring(8, &[b"ab", b"cd"]);
        assert_eq!(ring.iter().copied().collect::<Vec<u8>>(), b"abcd");
        assert_eq!(base, 0);
        assert!(!evicted);
    }

    #[test]
    fn memory_setting_scales_every_history_limit() {
        assert_eq!(
            history_limits(DEFAULT_HISTORY_LINES),
            HistoryLimits {
                max_lines: 1_000_000,
                raw_bytes: 64 * 1024 * 1024,
                preload_bytes: 8 * 1024 * 1024,
                series_points: 100_000,
            },
            "the default retains the established preload and plot limits"
        );
        assert_eq!(
            history_limits(10_000),
            HistoryLimits {
                max_lines: 10_000,
                raw_bytes: MIN_RAW_BYTES,
                preload_bytes: MIN_PRELOAD_BYTES,
                series_points: 1_000,
            },
        );
        assert_eq!(
            history_limits(10_000_000),
            HistoryLimits {
                max_lines: 10_000_000,
                raw_bytes: MAX_RAW_BYTES,
                preload_bytes: MAX_PRELOAD_BYTES,
                series_points: 1_000_000,
            },
            "the full Settings range scales all three derived limits"
        );
    }

    #[test]
    fn lowering_memory_setting_trims_open_hex_and_plot_history() {
        let (app, _enum_tx) = test_app("lower-history-limits");
        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            identity("A1"),
            PortConfig::default(),
            inert_handle(id),
        );
        conn.push_raw_bytes(&vec![0x55; 2 * 1024 * 1024]);
        let raw_allocation_before = conn.raw_ring.capacity();
        for i in 0..2_000 {
            conn.push_series_point("temp", i as f64, i as f64, i);
        }

        conn.apply_history_limits(history_limits(10_000));
        assert_eq!(
            conn.raw_ring.capacity(),
            raw_allocation_before,
            "an interactive resize defers reallocating until the edit settles"
        );
        assert!(conn.finish_history_capacity_change());

        assert_eq!(conn.raw_ring.len(), MIN_RAW_BYTES);
        assert_eq!(conn.raw_ring.capacity(), conn.raw_ring.len());
        assert!(conn.raw_ring.capacity() < raw_allocation_before);
        assert_eq!(conn.raw_base, MIN_RAW_BYTES as u64);
        assert!(conn.raw_evicted_any);
        assert_eq!(conn.series[0].series.len(), 1_000);
        assert_eq!(conn.series[0].series.t_range(), Some((1_000.0, 1_999.0)));
        assert!(conn.series_evicted_any);
    }

    #[test]
    fn raising_plot_limit_rebuilds_points_still_in_the_console() {
        let (app, _enum_tx) = test_app("raise-history-limits");
        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            identity("A1"),
            PortConfig::default(),
            inert_handle(id),
        );
        conn.apply_history_limits(history_limits(10_000));
        assert!(conn.finish_history_capacity_change());
        conn.extract_compiled = kv_rules();
        let wall = chrono::Utc::now();
        for i in 0..2_000 {
            conn.store.append(IncomingLine {
                text: format!("temp:{i}"),
                ts: Timestamp {
                    wall,
                    micros: i as i64,
                },
                port: id,
                flags: LineFlags::default(),
                spans: Default::default(),
                cursor: None,
            });
        }
        conn.rebuild_series();
        assert_eq!(conn.series[0].series.len(), 1_000);

        // Model a drag that overshoots and settles lower. Neither intermediate
        // update may rebuild, and shrinking from the overshoot must not forget
        // that history above the original 1,000-point cap is still missing.
        conn.apply_history_limits(history_limits(30_000));
        conn.apply_history_limits(history_limits(20_000));
        assert_eq!(
            conn.series[0].series.len(),
            1_000,
            "capacity growth stays cheap until the edit is committed"
        );
        assert!(conn.finish_history_capacity_change());

        assert_eq!(conn.series[0].series.len(), 2_000);
        assert_eq!(conn.series[0].series.t_range(), Some((0.0, 0.001999)));
    }

    #[test]
    fn raising_plot_limit_preserves_sparse_points_older_than_the_console() {
        let (app, _enum_tx) = test_app("raise-sparse-history-limit");
        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            identity("A1"),
            PortConfig::default(),
            inert_handle(id),
        );
        conn.apply_history_limits(history_limits(10_000));
        assert!(conn.finish_history_capacity_change());
        conn.extract_compiled = kv_rules();
        let wall = chrono::Utc::now();
        for i in 0..20_000 {
            let is_sample = i % 100 == 0;
            let text = if is_sample {
                format!("temp:{i}")
            } else {
                "noise".to_string()
            };
            let abs = conn.store.append(IncomingLine {
                text,
                ts: Timestamp {
                    wall,
                    micros: i as i64,
                },
                port: id,
                flags: LineFlags::default(),
                spans: Default::default(),
                cursor: None,
            });
            if is_sample {
                conn.push_series_point("temp", i as f64 / 1_000_000.0, i as f64, abs);
            }
        }
        assert!(conn.store.first_abs_index() > 0);
        assert_eq!(conn.series[0].series.len(), 200);

        conn.apply_history_limits(history_limits(DEFAULT_HISTORY_LINES));
        assert!(!conn.finish_history_capacity_change());

        assert_eq!(conn.series[0].series.len(), 200);
        assert_eq!(conn.series[0].series.t_range(), Some((0.0, 0.0199)));
    }

    /// A minimal `App`, built by hand rather than through `App::new` (which
    /// needs a live `eframe::CreationContext`). Its own enumerator channel is
    /// swapped for one this test controls.
    pub(crate) fn test_app(name: &str) -> (App, crossbeam_channel::Sender<EnumEvent>) {
        test_app_with_config(name, Config::default())
    }

    #[test]
    fn bundled_hack_font_is_the_final_proportional_fallback() {
        let fonts = app_font_definitions();
        assert!(fonts.font_data.contains_key("Hack"));
        assert_eq!(
            fonts
                .families
                .get(&egui::FontFamily::Proportional)
                .and_then(|family| family.last())
                .map(String::as_str),
            Some("Hack")
        );

        let ctx = egui::Context::default();
        ctx.set_fonts(fonts);
        ctx.begin_pass(Default::default());
        assert!(ctx.fonts(|fonts| {
            let ui_font = egui::FontId::proportional(12.0);
            fonts.has_glyph(&ui_font, '↑') && fonts.has_glyph(&ui_font, '↓')
        }));
        let _ = ctx.end_pass();
    }

    /// Like `test_app`, but seeded with a config the test provides.
    fn test_app_with_config(
        name: &str,
        config: Config,
    ) -> (App, crossbeam_channel::Sender<EnumEvent>) {
        let dir = scratch(name);
        let (tx, rx) = crossbeam_channel::unbounded();
        let app = App::assemble(
            config,
            AppPaths {
                config_file: dir.join("pigtail.toml"),
                sessions: dir.join("sessions"),
                crash_log: dir.join("crash.log"),
            },
            Wake::new(|| {}),
            rx,
        );
        (app, tx)
    }

    fn update_test_app(name: &str) -> (App, crossbeam_channel::Sender<update::InstallEvent>) {
        let (mut app, _) = test_app(name);
        app.update_dialog = Some(UpdateDialog {
            title: "Downloading update".into(),
            message: String::new(),
            update_version: Some("v1.2.3".into()),
            download_url: Some(
                "https://github.com/rustypig91/pigtail-serial-console/releases/tag/v1.2.3".into(),
            ),
            skip_version: Some("v1.2.3".into()),
        });
        let (tx, rx) = crossbeam_channel::unbounded();
        app.install_rx = Some(rx);
        (app, tx)
    }

    #[test]
    fn update_progress_and_failure_keep_the_release_available_for_retry() {
        let (mut app, tx) = update_test_app("update-retry");
        let ctx = egui::Context::default();
        tx.send(update::InstallEvent::Progress {
            downloaded: 25,
            total: 100,
        })
        .unwrap();
        app.poll_update_install(&ctx);
        assert_eq!(app.update_progress, Some(0.25));
        tx.send(update::InstallEvent::Downloaded(Err(
            "Connection lost".into()
        )))
        .unwrap();
        app.poll_update_install(&ctx);
        assert!(app.install_rx.is_none());
        assert!(app.update_progress.is_none());
        let dialog = app.update_dialog.unwrap();
        assert_eq!(dialog.title, "Update failed");
        assert_eq!(dialog.message, "Connection lost");
        assert_eq!(dialog.update_version.as_deref(), Some("v1.2.3"));
    }

    #[test]
    fn disconnected_updater_reports_failure_instead_of_spinning_forever() {
        let (mut app, tx) = update_test_app("update-disconnected");
        drop(tx);
        app.poll_update_install(&egui::Context::default());
        assert!(app.install_rx.is_none());
        assert_eq!(app.update_dialog.unwrap().title, "Update failed");
    }

    #[test]
    fn an_active_update_prevents_duplicate_downloads_and_checks() {
        let (mut app, _tx) = update_test_app("update-duplicate");
        let original = app.install_rx.as_ref().unwrap().clone();
        app.start_update_download("v9.9.9".into());
        app.start_update_check(true);
        assert!(app.install_rx.as_ref().unwrap().same_channel(&original));
        assert!(app.update_rx.is_none());
    }

    #[test]
    fn config_changes_are_coalesced_until_the_debounce_deadline() {
        let (mut app, _enum_tx) = test_app("config-debounce");
        app.config.settings.max_lines = 12_345;
        app.write_config();
        let first_change = app.config_dirty_since.unwrap();

        app.config.settings.max_lines = 54_321;
        app.write_config();

        assert_eq!(app.config_dirty_since, Some(first_change));
        assert!(
            !app.paths.config_file.exists(),
            "marking more changes must not write inline"
        );

        app.config_dirty_since = Some(Instant::now() - CONFIG_WRITE_DELAY);
        app.maintain_config(&egui::Context::default());

        assert!(app.config_dirty_since.is_none());
        assert_eq!(load_config(&app.paths).settings.max_lines, 54_321);
        assert!(!app.paths.config_file.with_extension("toml.tmp").exists());
        std::fs::remove_dir_all(app.paths.config_file.parent().unwrap()).ok();
    }

    /// A reader handle backed by no real device, for tests that only care
    /// about connection bookkeeping. Its thread exits as soon as it is asked
    /// to shut down.
    pub(crate) fn inert_handle(id: PortId) -> reader::ReaderHandle {
        let config = reader::ReaderConfig {
            port_id: id,
            clock: SessionClock::new(),
            session_dir: None,
            meta: SessionMeta {
                identity: PortIdentity::default(),
                config: PortConfig::default(),
                start_wall: chrono::Utc::now(),
                app_version: "test".into(),
                port_label: String::new(),
                cleared: false,
            },
            terminal: Default::default(),
            wake: Wake::new(|| {}),
        };
        reader::spawn(
            config,
            SourceSpec::OneShot(Box::new(
                serialcore::source::ScriptedSource::new(Vec::new()),
            )),
        )
        .unwrap()
    }

    fn add_merged_test_connection(
        app: &mut App,
        id: PortId,
        label: &str,
        lines: &[(&str, i64, LineFlags)],
    ) {
        let mut conn = app.make_connection(
            id,
            label.to_owned(),
            identity(label),
            PortConfig::default(),
            inert_handle(id),
        );
        for (text, micros, flags) in lines {
            conn.store.append(IncomingLine {
                text: (*text).to_owned(),
                ts: Timestamp {
                    wall: chrono::Utc::now(),
                    micros: *micros,
                },
                port: id,
                flags: *flags,
                spans: Default::default(),
                cursor: None,
            });
        }
        app.connections.push(conn);
        app.merged_dirty = true;
    }

    #[test]
    fn search_case_sensitivity_applies_to_single_and_merged_views() {
        let (mut app, _enum_tx) = test_app("search-case-sensitive");
        add_merged_test_connection(
            &mut app,
            PortId(1),
            "probe",
            &[
                ("ERROR exact", 1, LineFlags::default()),
                ("error folded", 2, LineFlags::default()),
            ],
        );
        app.maintain_merged();

        app.connections[0].search_query = "ERROR".into();
        app.connections[0].search_dirty = true;
        app.maintain_search();
        assert_eq!(app.connections[0].search_matches, [0, 1]);

        app.connections[0].search_case_sensitive = true;
        app.connections[0].search_dirty = true;
        app.maintain_search();
        assert_eq!(app.connections[0].search_matches, [0]);

        app.merged_search_query = "ERROR".into();
        app.merged_search_dirty = true;
        app.maintain_merged_search(false);
        assert_eq!(app.merged_search_matches.len(), 2);

        app.merged_search_case_sensitive = true;
        app.merged_search_dirty = true;
        app.maintain_merged_search(false);
        assert_eq!(app.merged_search_matches.len(), 1);
        assert_eq!(app.merged_search_matches[0].abs, 0);
    }

    #[test]
    fn search_case_sensitivity_also_applies_to_literal_fallback() {
        let insensitive = compile_search("[A", false).unwrap();
        assert!(insensitive.is_match("prefix [a suffix"));

        let sensitive = compile_search("[A", true).unwrap();
        assert!(sensitive.is_match("prefix [A suffix"));
        assert!(!sensitive.is_match("prefix [a suffix"));
    }

    #[test]
    fn highlight_rules_honor_case_sensitivity() {
        let (mut app, _enum_tx) = test_app("highlight-case-sensitive");
        let rule = &mut app.config.highlight[0];
        rule.enabled = true;
        rule.case_sensitive = true;
        app.rebuild_highlight_if_dirty();
        assert!(app.highlight_cache[0].re.is_match("ERROR"));
        assert!(!app.highlight_cache[0].re.is_match("error"));

        app.config.highlight[0].case_sensitive = false;
        app.highlight_dirty = true;
        app.rebuild_highlight_if_dirty();
        assert!(app.highlight_cache[0].re.is_match("error"));
    }

    #[test]
    fn merged_filter_and_search_use_the_interleaved_view() {
        let (mut app, _enum_tx) = test_app("merged-filter-search");
        add_merged_test_connection(
            &mut app,
            PortId(1),
            "alpha",
            &[
                ("alpha ready", 1, LineFlags::default()),
                ("ERROR alpha", 3, LineFlags::default()),
            ],
        );
        add_merged_test_connection(
            &mut app,
            PortId(2),
            "beta",
            &[
                ("ERROR beta target", 2, LineFlags::default()),
                ("beta ready", 4, LineFlags::default()),
            ],
        );
        app.maintain_merged();
        app.merged_filter_rules.push(FilterRule {
            pattern: "ERROR".into(),
            ..FilterRule::default()
        });
        app.merged_filter_dirty = true;
        app.maintain_merged_filter(false);

        let shown: Vec<_> = app
            .merged_view()
            .iter()
            .map(|entry| (entry.port, entry.abs))
            .collect();
        assert_eq!(shown, [(PortId(2), 0), (PortId(1), 1)]);

        app.merged_search_query = "target".into();
        app.merged_search_dirty = true;
        app.maintain_merged_search(false);
        assert_eq!(app.merged_search_matches.len(), 1);
        assert_eq!(app.merged_search_matches[0].port, PortId(2));

        app.merged_selected = true;
        app.search_step(1);
        assert_eq!(
            app.merged_scroll_to.map(|entry| (entry.port, entry.abs)),
            Some((PortId(2), 0))
        );
    }

    #[test]
    fn merged_filter_and_search_retest_a_growing_line() {
        let (mut app, _enum_tx) = test_app("merged-growing-line");
        add_merged_test_connection(
            &mut app,
            PortId(1),
            "probe",
            &[("booting", 1, LineFlags::PROVISIONAL)],
        );
        app.maintain_merged();
        app.merged_filter_rules.push(FilterRule {
            pattern: "ERROR".into(),
            ..FilterRule::default()
        });
        app.merged_filter_dirty = true;
        app.maintain_merged_filter(false);
        assert!(app.merged_view().is_empty());

        app.connections[0].store.append(IncomingLine {
            text: "booting: ERROR target".into(),
            ts: Timestamp {
                wall: chrono::Utc::now(),
                micros: 2,
            },
            port: PortId(1),
            flags: LineFlags::PROVISIONAL | LineFlags::CONTINUATION,
            spans: Default::default(),
            cursor: None,
        });
        app.maintain_merged_filter(true);
        assert_eq!(app.merged_view().len(), 1);

        app.merged_search_query = "target".into();
        app.merged_search_dirty = true;
        app.maintain_merged_search(true);
        assert_eq!(app.merged_search_matches.len(), 1);
    }

    #[test]
    fn merged_search_rebuilds_when_filter_mode_changes() {
        let (mut app, _enum_tx) = test_app("merged-filter-generation");
        add_merged_test_connection(
            &mut app,
            PortId(1),
            "probe",
            &[
                ("keep target", 1, LineFlags::default()),
                ("drop target", 2, LineFlags::default()),
            ],
        );
        app.maintain_merged();
        app.merged_search_query = "target".into();
        app.merged_search_dirty = true;
        app.maintain_merged_search(false);
        assert_eq!(app.merged_search_matches.len(), 2);
        let unfiltered_generation = app.merged_view_generation();

        app.merged_filter_rules.push(FilterRule {
            pattern: "keep".into(),
            ..FilterRule::default()
        });
        app.merged_filter_dirty = true;
        app.maintain_merged_filter(false);
        assert_ne!(app.merged_view_generation(), unfiltered_generation);
        app.maintain_merged_search(false);
        assert_eq!(app.merged_search_matches.len(), 1);
        assert_eq!(app.merged_search_matches[0].abs, 0);

        app.merged_filter_rules[0].enabled = false;
        app.merged_filter_dirty = true;
        app.maintain_merged_filter(false);
        assert_eq!(app.merged_view_generation(), unfiltered_generation);
        app.maintain_merged_search(false);
        assert_eq!(app.merged_search_matches.len(), 2);
    }

    #[test]
    fn merged_caches_drop_a_busy_ports_interior_evictions() {
        let (mut app, _enum_tx) = test_app("merged-interior-eviction");
        add_merged_test_connection(
            &mut app,
            PortId(1),
            "quiet",
            &[("quiet", 0, LineFlags::default())],
        );
        add_merged_test_connection(
            &mut app,
            PortId(2),
            "busy",
            &[
                ("hit one", 1, LineFlags::default()),
                ("hit two", 2, LineFlags::default()),
                ("hit three", 3, LineFlags::default()),
            ],
        );
        app.maintain_merged();
        app.merged_filter_rules.push(FilterRule {
            pattern: "hit".into(),
            ..FilterRule::default()
        });
        app.merged_filter_dirty = true;
        app.maintain_merged_filter(false);
        app.merged_search_query = "hit".into();
        app.merged_search_dirty = true;
        app.maintain_merged_search(false);
        assert_eq!(app.merged_filtered.len(), 3);
        assert_eq!(app.merged_search_matches.len(), 3);

        // The quiet row remains the first merged entry while the busy port
        // evicts two entries from the middle of the interleaving.
        app.connections[1].store.set_max_lines(1);
        app.maintain_merged();
        app.maintain_merged_filter(false);
        app.maintain_merged_search(false);

        let remaining: Vec<_> = app
            .merged
            .iter()
            .map(|entry| (entry.port, entry.abs))
            .collect();
        assert_eq!(remaining, [(PortId(1), 0), (PortId(2), 2)]);
        assert_eq!(app.merged_filtered.len(), 1);
        assert_eq!(app.merged_filtered[0].abs, 2);
        assert_eq!(app.merged_search_matches.len(), 1);
        assert_eq!(app.merged_search_matches[0].abs, 2);
    }

    #[test]
    fn lowering_history_limit_rebuilds_a_silent_merged_view() {
        let (mut app, _enum_tx) = test_app("merged-settings-eviction");
        let id = PortId(1);
        let mut conn = app.make_connection(
            id,
            "quiet".into(),
            identity("quiet"),
            PortConfig::default(),
            inert_handle(id),
        );
        let wall = chrono::Utc::now();
        for micros in 0..10_001 {
            conn.store.append(IncomingLine {
                text: String::new(),
                ts: Timestamp { wall, micros },
                port: id,
                flags: LineFlags::default(),
                spans: Default::default(),
                cursor: None,
            });
        }
        app.connections.push(conn);
        app.merged_dirty = true;
        app.maintain_merged();
        assert_eq!(app.merged.len(), 10_001);

        // No reader event accompanies a Settings edit. Settling the edit must
        // therefore schedule the merged-cache maintenance itself.
        app.connections[0].apply_history_limits(history_limits(10_000));
        assert_eq!(app.connections[0].store.first_abs_index(), 1);
        assert_eq!(
            app.merged.len(),
            10_001,
            "the cache is stale before settling"
        );
        assert!(app.finish_history_capacity_changes());
        assert!(app.merged_dirty);
        assert!(
            !app.finish_history_capacity_changes(),
            "settling an unchanged capacity must not schedule another frame"
        );
        app.maintain_merged();

        assert_eq!(app.merged.len(), 10_000);
        assert!(app
            .merged
            .iter()
            .all(|entry| app.connections[0].store.get(entry.abs).is_some()));
    }

    #[test]
    fn a_tab_name_is_trimmed_persisted_and_can_be_cleared() {
        let (mut app, _enum_tx) = test_app("tab-name");
        let id = PortId(1);
        let conn = app.make_connection(
            id,
            "Detected device (/dev/ttyUSB0)".into(),
            identity("named"),
            PortConfig::default(),
            inert_handle(id),
        );
        app.connections.push(conn);

        app.rename_connection(id, "  left sensor  ");
        assert_eq!(app.connections[0].display_label(), "left sensor");
        assert_eq!(app.connections[0].merged_label(), "left sensor");
        assert_eq!(app.config.last_open[0].name.as_deref(), Some("left sensor"));

        app.rename_connection(id, "   ");
        assert_eq!(
            app.connections[0].display_label(),
            "Detected device (/dev/ttyUSB0)"
        );
        assert_eq!(app.connections[0].merged_label(), "Detected");
        assert_eq!(app.config.last_open[0].name, None);
    }

    /// `save_session` must not persist a `Closed` zombie into `last_open`: if
    /// a live tab for the same device also exists (e.g. it was reopened
    /// manually once the zombie stopped counting as "already open"), both
    /// would be restored next launch and fight over the same exclusive port.
    #[test]
    fn save_session_excludes_closed_tabs_from_last_open() {
        let (mut app, _enum_tx) = test_app("save-session-excludes-closed");

        let dead_id = PortId(0);
        let mut dead = app.make_connection(
            dead_id,
            "probe (COM3)".into(),
            identity("A1"),
            PortConfig::default(),
            inert_handle(dead_id),
        );
        dead.state = ConnState::Closed;
        app.connections.push(dead);

        let live_id = PortId(1);
        let live = app.make_connection(
            live_id,
            "probe (COM3)".into(),
            identity("A2"),
            PortConfig::default(),
            inert_handle(live_id),
        );
        app.connections.push(live);

        app.save_session();

        assert_eq!(
            app.config.last_open.len(),
            1,
            "a Closed tab must not be persisted, whether or not a live tab \
             for the same or a different device also exists"
        );
        assert_eq!(app.config.last_open[0].identity, identity("A2"));
    }

    /// The config dialog is modal: while one is open, the paths that would
    /// open another (the header's "+", the empty console's "+ New
    /// connection") must leave the in-progress form alone rather than
    /// replacing it with a fresh one (issue #16).
    #[test]
    fn open_config_dialog_keeps_an_in_progress_dialog() {
        let (mut app, _enum_tx) = test_app("dialog-clobber-new");

        app.open_config_dialog();
        app.config_dialog.as_mut().unwrap().preset_name = "half typed".into();
        app.config_dialog.as_mut().unwrap().config.baud = 4800;

        app.open_config_dialog();

        let dialog = app.config_dialog.as_ref().expect("dialog still open");
        assert_eq!(dialog.preset_name, "half typed");
        assert_eq!(dialog.config.baud, 4800, "edits survive a second open");
    }

    #[test]
    fn new_connection_dialog_preselects_only_a_device_not_already_added() {
        let (mut app, _enum_tx) = test_app("dialog-unused-device");
        let added_identity = identity("A1");
        let unused_identity = identity("B2");
        app.available = vec![
            DiscoveredPort {
                path: "/dev/ttyUSB0".into(),
                identity: added_identity.clone(),
            },
            DiscoveredPort {
                path: "/dev/ttyUSB1".into(),
                identity: unused_identity,
            },
        ];
        let id = PortId(0);
        app.connections.push(app.make_connection(
            id,
            "probe A1".into(),
            added_identity,
            PortConfig::default(),
            inert_handle(id),
        ));

        app.open_config_dialog();

        assert_eq!(
            app.config_dialog.as_ref().unwrap().selected_path.as_deref(),
            Some("/dev/ttyUSB1")
        );
    }

    #[test]
    fn first_enumerator_snapshot_adds_the_live_path_to_a_restored_tab_label() {
        let (mut app, enum_tx) = test_app("restored-tab-path");
        let mut saved_identity = identity("A1");
        saved_identity.product = Some("Debug probe".into());
        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            saved_identity.label(),
            saved_identity.clone(),
            PortConfig::default(),
            inert_handle(id),
        );
        conn.name = Some("left target".into());
        app.connections.push(conn);

        let mut detected_identity = saved_identity;
        detected_identity.path_fallback = "/dev/ttyUSB7".into();
        enum_tx
            .send(EnumEvent::Snapshot(vec![DiscoveredPort {
                path: "/dev/ttyUSB7".into(),
                identity: detected_identity,
            }]))
            .unwrap();
        app.poll_enumerator();

        let conn = &app.connections[0];
        assert_eq!(conn.display_label(), "left target");
        assert_eq!(conn.label, "Debug probe (/dev/ttyUSB7)");
    }

    #[test]
    fn a_path_only_tab_label_does_not_repeat_the_path() {
        let identity = PortIdentity {
            path_fallback: "COM3".into(),
            ..Default::default()
        };
        assert_eq!(detected_connection_label(&identity, "COM3"), "COM3");
    }

    #[test]
    fn one_added_serialless_device_does_not_hide_its_identical_sibling() {
        let (mut app, _enum_tx) = test_app("dialog-identical-devices");
        let twin = |path: &str| PortIdentity {
            vid: Some(0x1234),
            pid: Some(0x5678),
            path_fallback: path.into(),
            ..Default::default()
        };
        app.available = vec![
            DiscoveredPort {
                path: "/dev/ttyUSB0".into(),
                identity: twin("/dev/ttyUSB0"),
            },
            DiscoveredPort {
                path: "/dev/ttyUSB1".into(),
                identity: twin("/dev/ttyUSB1"),
            },
        ];
        let id = PortId(0);
        let conn = app.make_connection(
            id,
            "first twin".into(),
            twin("/dev/ttyUSB0"),
            PortConfig::default(),
            inert_handle(id),
        );
        app.connections.push(conn);

        assert!(available_port_is_added(
            0,
            &app.available,
            &app.connections,
            None
        ));
        assert!(!available_port_is_added(
            1,
            &app.available,
            &app.connections,
            None
        ));
    }

    #[test]
    fn port_options_allow_the_edited_device_but_not_another_tabs_device() {
        let (mut app, _enum_tx) = test_app("dialog-edit-added-devices");
        app.available = ["A1", "B2"]
            .into_iter()
            .enumerate()
            .map(|(index, serial)| DiscoveredPort {
                path: format!("/dev/ttyUSB{index}"),
                identity: identity(serial),
            })
            .collect();
        for (index, port) in app.available.clone().into_iter().enumerate() {
            let id = PortId(index as u32);
            let conn = app.make_connection(
                id,
                port.identity.label(),
                port.identity,
                PortConfig::default(),
                inert_handle(id),
            );
            app.connections.push(conn);
        }

        let editing = Some(PortId(0));
        assert!(!available_port_is_added(
            0,
            &app.available,
            &app.connections,
            editing
        ));
        assert!(available_port_is_added(
            1,
            &app.available,
            &app.connections,
            editing
        ));
    }

    #[test]
    fn port_options_preselect_the_live_path_after_a_serial_device_moves() {
        let (mut app, _enum_tx) = test_app("dialog-edit-moved-device");
        let id = PortId(0);
        let saved = identity("A1");
        let mut live = saved.clone();
        live.path_fallback = "/dev/ttyUSB7".into();
        app.available.push(DiscoveredPort {
            path: "/dev/ttyUSB7".into(),
            identity: live,
        });
        let conn = app.make_connection(
            id,
            saved.label(),
            saved,
            PortConfig::default(),
            inert_handle(id),
        );
        app.connections.push(conn);

        app.open_port_options(0);

        assert_eq!(
            app.config_dialog.unwrap().selected_path.as_deref(),
            Some("/dev/ttyUSB7")
        );
    }

    /// Same for "Port options…" on a tab: it must not discard a dialog that
    /// is already up, whichever tab that dialog belongs to.
    #[test]
    fn open_port_options_keeps_an_in_progress_dialog() {
        let (mut app, _enum_tx) = test_app("dialog-clobber-options");

        for (i, serial) in ["A1", "B2"].iter().enumerate() {
            let id = PortId(i as u32);
            let conn = app.make_connection(
                id,
                format!("probe {serial}"),
                identity(serial),
                PortConfig::default(),
                inert_handle(id),
            );
            app.connections.push(conn);
        }

        app.open_port_options(0);
        app.config_dialog.as_mut().unwrap().config.baud = 4800;

        app.open_port_options(1);

        let dialog = app.config_dialog.as_ref().expect("dialog still open");
        assert_eq!(
            dialog.editing,
            Some(PortId(0)),
            "the dialog still edits the tab it was opened for"
        );
        assert_eq!(dialog.config.baud, 4800, "its edits are not discarded");
    }

    /// Applying port options to a tab that is no longer open must say so
    /// rather than closing the dialog as if the reconnect had happened
    /// (issue #16).
    #[test]
    fn reconnect_with_config_reports_a_vanished_tab() {
        let (mut app, _enum_tx) = test_app("reconnect-vanished");

        app.reconnect_with_config(PortId(7), None, PortConfig::default());

        let err = app
            .connect_errors
            .front()
            .expect("a vanished tab is reported, not silently ignored");
        assert_eq!(err.title, "Couldn't reconnect");
    }

    /// The same rule for a *new* connection: the port list is refreshed in
    /// the background, so the selected path can stop naming a detected port
    /// between filling the dialog in and pressing Connect. Closing the dialog
    /// with no tab and no message would look exactly like success (issue #16).
    #[test]
    fn connect_from_dialog_reports_a_vanished_port() {
        let (mut app, _enum_tx) = test_app("connect-vanished");

        app.config_dialog = Some(ConfigDialog {
            selected_path: Some("/dev/ttyUSB0".into()),
            config: PortConfig::default(),
            preset_name: String::new(),
            editing: None,
        });
        // `available` is empty: the device is gone by the time Connect lands.
        app.connect_from_dialog();

        assert!(app.config_dialog.is_none(), "the dialog still closes");
        assert!(app.connections.is_empty(), "no tab is opened");
        let err = app
            .connect_errors
            .front()
            .expect("a vanished port is reported, not silently ignored");
        assert_eq!(err.title, "Couldn't connect");
        assert!(
            err.message.contains("/dev/ttyUSB0"),
            "the message names the port that went away: {}",
            err.message
        );
    }

    /// Regression test for the PR #6 fix: a command dropped while
    /// reconnecting is reported as a session-scoped error, but the tab only
    /// has room for one `last_error`. It must not clobber a connection error
    /// already explaining *why* the link is down — that message is more
    /// useful, and unlike a connection error, nothing later clears a session
    /// error, so overwriting it would hide the real cause for the rest of
    /// the outage.
    #[test]
    fn dropped_command_error_does_not_hide_the_connection_error() {
        let (mut app, _enum_tx) = test_app("dropped-command-error");

        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            identity("A1"),
            PortConfig::default(),
            inert_handle(id),
        );
        // Swap in a channel this test controls, so it can inject events
        // without a real reader thread.
        let (tx, rx) = crossbeam_channel::unbounded();
        conn.handle.events = rx;
        app.connections.push(conn);

        tx.send(ReaderEvent::Error {
            scope: ErrorScope::Connection,
            msg: "device not present".into(),
        })
        .unwrap();
        tx.send(ReaderEvent::Error {
            scope: ErrorScope::Session,
            msg: "transmit: dropped, not connected".into(),
        })
        .unwrap();
        app.connections[0].drain_events(1000);

        let err = app.connections[0]
            .last_error
            .as_ref()
            .expect("an error is showing");
        assert_eq!(
            err.msg, "device not present",
            "the connection error must survive a same-batch session error, \
             or the real disconnect reason is gone for the rest of the outage"
        );

        // A session error is still shown when nothing more important is
        // already up.
        app.connections[0].last_error = None;
        tx.send(ReaderEvent::Error {
            scope: ErrorScope::Session,
            msg: "transmit: dropped, not connected".into(),
        })
        .unwrap();
        app.connections[0].drain_events(1000);
        assert_eq!(
            app.connections[0].last_error.as_ref().unwrap().msg,
            "transmit: dropped, not connected"
        );
    }

    /// The bounded reader backlog reports a gap before the output it retained.
    /// Both views must show that boundary: silently joining the raw bytes would
    /// make the hex offsets claim two non-contiguous regions were one stream.
    #[test]
    fn dropped_reader_output_marks_the_console_and_hex_view() {
        let (mut app, _enum_tx) = test_app("dropped-reader-output");
        let id = PortId(0);
        let tx = conn_with_injected_events(&mut app, id);
        let clock = SessionClock::new();
        let at = clock.now();
        let later = |micros: i64| Timestamp {
            wall: at.wall + chrono::Duration::microseconds(micros),
            micros: at.micros + micros,
        };
        let line = |text: &str, ts| serialcore::framer::FramedLine {
            text: text.into(),
            ts,
            flags: LineFlags::default(),
            cursor: None,
        };

        tx.send(ReaderEvent::Batch(reader::Batch {
            lines: vec![line("before", at)],
            raw: b"old".to_vec(),
        }))
        .unwrap();
        tx.send(ReaderEvent::OutputDropped {
            raw_bytes: 4096,
            line_updates: 23,
            at,
        })
        .unwrap();
        tx.send(ReaderEvent::Batch(reader::Batch {
            lines: vec![line("after", later(1))],
            raw: b"new".to_vec(),
        }))
        .unwrap();

        app.connections[0].drain_events(1000);
        let conn = &app.connections[0];
        assert_eq!(conn.store.len(), 3);
        assert_eq!(conn.store.get(0).unwrap().text, "before");
        let marker = conn.store.get(1).unwrap();
        assert!(marker.meta.flags.contains(LineFlags::RECONNECT_MARKER));
        assert!(marker.text.contains("4096 bytes, 23 line updates"));
        assert_eq!(conn.store.get(2).unwrap().text, "after");

        assert_eq!(conn.raw_ring.iter().copied().collect::<Vec<_>>(), b"oldnew");
        assert_eq!(conn.raw_contiguous_start, 3);
        assert_eq!(conn.raw_sessions.len(), 2);
        assert!(conn.raw_sessions[0]
            .label
            .as_deref()
            .is_some_and(|label| label.contains("output dropped")));
        assert_eq!(conn.raw_sessions[1].start, 3);
        assert!(conn.raw_sessions[1].label.is_none());

        app.maintain_merged();
        let merged_text: Vec<&str> = app
            .merged
            .iter()
            .map(|entry| app.connections[0].store.get(entry.abs).unwrap().text)
            .collect();
        assert_eq!(
            merged_text,
            vec![
                "before",
                "output dropped · 4096 bytes, 23 line updates · display was busy",
                "after"
            ],
            "the merged view must not sort retained output ahead of its gap marker"
        );
    }

    /// Issue #46: the footer's "N new" badge counted line *events*, but a
    /// `CONTINUATION` does not add a line — it replaces the open provisional
    /// one in place and keeps its index. A line the device is still writing is
    /// re-sent every ~20ms as it grows (a prompt echoing what is typed at it, a
    /// status line being filled in), so the badge climbed once per redraw while
    /// the console gained nothing. That number is what the user decides
    /// "should I jump back to live?" on.
    #[test]
    fn the_unread_count_counts_lines_not_redraws_of_one() {
        let (mut app, _enum_tx) = test_app("new-since-scroll");
        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            identity("A1"),
            PortConfig::default(),
            inert_handle(id),
        );
        let (tx, rx) = crossbeam_channel::unbounded();
        conn.handle.events = rx;
        // Scrolled back, which is the only state the badge is shown in.
        conn.follow = false;
        app.connections.push(conn);

        let clock = SessionClock::new();
        let mut framer = Framer::new();

        let send = |tx: &crossbeam_channel::Sender<ReaderEvent>, lines| {
            tx.send(ReaderEvent::Batch(reader::Batch {
                lines,
                raw: Vec::new(),
            }))
            .unwrap();
        };

        // One settled line, then one the device has left open.
        let mut lines = Vec::new();
        framer.push(b"booting\n", clock.now(), &mut lines);
        framer.push(b"ready>", clock.now(), &mut lines);
        lines.push(framer.flush_provisional().unwrap());
        send(&tx, lines);
        app.connections[0].drain_events(1000);
        assert_eq!(
            app.connections[0].new_since_scroll, 2,
            "the settled line and the open one are two lines"
        );

        // Twenty more bytes land on that same open line, each one flushed as it
        // arrives — the shape of a prompt echoing back what is typed at it.
        for byte in b"abcdefghijklmnopqrst" {
            let mut lines = Vec::new();
            framer.push(&[*byte], clock.now(), &mut lines);
            lines.push(framer.flush_provisional().unwrap());
            send(&tx, lines);
            app.connections[0].drain_events(1000);
        }

        let conn = &app.connections[0];
        assert_eq!(conn.store.len(), 2, "still two lines on screen");
        assert_eq!(
            conn.store.get(1).unwrap().text,
            "ready>abcdefghijklmnopqrst",
            "the one open line grew, rather than twenty more arriving"
        );
        assert_eq!(
            conn.new_since_scroll, 2,
            "and still two unread; redrawing a line is not new output"
        );
    }

    /// The counter still has to move for lines that really are new.
    #[test]
    fn the_unread_count_still_counts_real_lines() {
        let (mut app, _enum_tx) = test_app("new-since-scroll-real");
        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            identity("A1"),
            PortConfig::default(),
            inert_handle(id),
        );
        let (tx, rx) = crossbeam_channel::unbounded();
        conn.handle.events = rx;
        conn.follow = false;
        app.connections.push(conn);

        let clock = SessionClock::new();
        let mut framer = Framer::new();
        let mut lines = Vec::new();
        framer.push(b"one\ntwo\nthree\n", clock.now(), &mut lines);
        tx.send(ReaderEvent::Batch(reader::Batch {
            lines,
            raw: Vec::new(),
        }))
        .unwrap();
        app.connections[0].drain_events(1000);
        assert_eq!(app.connections[0].new_since_scroll, 3);
    }

    /// A connection with an injected event channel, so a test can feed it
    /// batches without a live device. Returns the sender.
    fn conn_with_injected_events(
        app: &mut App,
        id: PortId,
    ) -> crossbeam_channel::Sender<ReaderEvent> {
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            identity("A1"),
            PortConfig::default(),
            inert_handle(id),
        );
        let (tx, rx) = crossbeam_channel::unbounded();
        conn.handle.events = rx;
        app.connections.push(conn);
        tx
    }

    /// Issue #38: a line the device is still writing reaches the UI twice —
    /// once as a `PROVISIONAL` flush after ~20ms of silence, once as the
    /// `CONTINUATION` that completes it — and `LineStore::append` replaces the
    /// first in place, keeping its absolute index. Extracting the provisional
    /// plots a number that is still being typed, and nothing takes it back
    /// when the real one lands.
    ///
    /// The batch here is produced by the real `Framer`, not hand-written, so
    /// this stays bound to what the reader actually emits.
    #[test]
    fn a_half_written_line_does_not_plant_a_plot_point() {
        let (mut app, _enum_tx) = test_app("provisional-plot");

        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            identity("A1"),
            PortConfig::default(),
            inert_handle(id),
        );
        let (tx, rx) = crossbeam_channel::unbounded();
        conn.handle.events = rx;
        conn.extract_rules = vec![ExtractRule {
            mode: serialcore::config::ExtractMode::Kv,
            prefix: None,
            pattern: None,
            kv_separators: None,
        }];
        conn.extract_dirty = true;
        app.connections.push(conn);

        // `temp:23.4\n` split across two reads, as a slow device (or a USB
        // latency boundary) delivers it: the first half is flushed while the
        // line is still open.
        let clock = SessionClock::new();
        let mut framer = Framer::new();
        let mut lines = Vec::new();
        framer.push(b"temp:23", clock.now(), &mut lines);
        assert!(lines.is_empty(), "nothing is terminated yet");
        let provisional = framer
            .flush_provisional()
            .expect("the open line is shown provisionally");
        assert!(provisional.flags.contains(LineFlags::PROVISIONAL));
        assert_eq!(provisional.text, "temp:23");
        lines.push(provisional);
        framer.push(b".4\n", clock.now(), &mut lines);
        assert!(
            lines[1].flags.contains(LineFlags::CONTINUATION),
            "the completed line replaces the provisional one in place"
        );
        assert_eq!(lines[1].text, "temp:23.4");

        tx.send(ReaderEvent::Batch(reader::Batch {
            lines,
            raw: b"temp:23.4\n".to_vec(),
        }))
        .unwrap();

        let conn = &mut app.connections[0];
        conn.maintain_extract();
        conn.drain_events(1000);

        assert_eq!(conn.store.len(), 1, "the two events are one line");
        let temp = conn
            .series
            .iter()
            .find(|e| e.series.name() == "temp")
            .expect("temp is plotted");
        assert_eq!(
            temp.series.len(),
            1,
            "one line is one sample, not one per redraw of it"
        );
        assert_eq!(
            temp.series.last().unwrap().value,
            23.4,
            "and it is the value the line finally held, not the half of it \
             that happened to be on screen first"
        );
    }

    /// Issue #39: search tested every absolute index exactly once, but the
    /// newest index is the one that can still change — a line the device is
    /// still writing is shown provisionally and then replaced in place, keeping
    /// its index. A match that only appears once the line finishes was
    /// therefore unfindable for good, short of retyping the query.
    ///
    /// `FilterIndex::extend` re-tests the newest line for exactly this reason;
    /// this pins the same rule onto search.
    #[test]
    fn search_finds_a_match_that_appears_only_once_the_line_finishes() {
        let (mut app, _enum_tx) = test_app("search-provisional");
        let tx = conn_with_injected_events(&mut app, PortId(0));
        app.active = 0;
        app.connections[0].search_query = "timeout".into();

        let clock = SessionClock::new();
        let mut framer = Framer::new();

        // The device has written half a line and paused, so it is flushed
        // provisionally. Nothing in it matches yet.
        let mut lines = Vec::new();
        framer.push(b"error: sen", clock.now(), &mut lines);
        lines.push(framer.flush_provisional().expect("the open line is shown"));
        tx.send(ReaderEvent::Batch(reader::Batch {
            lines,
            raw: b"error: sen".to_vec(),
        }))
        .unwrap();
        app.connections[0].drain_events(1000);
        app.maintain_search();
        assert!(
            app.connections[0].search_matches.is_empty(),
            "nothing matches the half-written line, correctly"
        );

        // The rest of the line lands, replacing it in place at the same index.
        let mut lines = Vec::new();
        framer.push(b"sor timeout\n", clock.now(), &mut lines);
        assert!(lines[0].flags.contains(LineFlags::CONTINUATION));
        tx.send(ReaderEvent::Batch(reader::Batch {
            lines,
            raw: b"sor timeout\n".to_vec(),
        }))
        .unwrap();
        app.connections[0].drain_events(1000);
        app.maintain_search();

        let conn = &app.connections[0];
        assert_eq!(conn.store.len(), 1, "still the one line");
        assert_eq!(conn.store.get(0).unwrap().text, "error: sensor timeout");
        assert_eq!(
            conn.search_matches,
            vec![0],
            "the completed line has to be findable; re-testing only new indices \
             leaves it hidden for the rest of the session"
        );
    }

    /// The converse of the above: a provisional line that *did* match and then
    /// changed into something that does not must lose its entry, rather than
    /// leaving Next/Prev landing on a line with no highlight on it.
    #[test]
    fn search_drops_a_match_the_line_no_longer_holds() {
        let (mut app, _enum_tx) = test_app("search-unmatch");
        let tx = conn_with_injected_events(&mut app, PortId(0));
        app.active = 0;
        app.connections[0].search_query = "abort".into();

        let clock = SessionClock::new();
        // VT100, where a bare CR overwrites the line in place — Classic treats
        // it as a terminator, which is a different story entirely.
        let mut framer = Framer::with_mode(serialcore::config::TerminalMode::Vt100);

        let mut lines = Vec::new();
        framer.push(b"abort", clock.now(), &mut lines);
        lines.push(framer.flush_provisional().unwrap());
        tx.send(ReaderEvent::Batch(reader::Batch {
            lines,
            raw: b"abort".to_vec(),
        }))
        .unwrap();
        app.connections[0].drain_events(1000);
        app.maintain_search();
        assert_eq!(app.connections[0].search_matches, vec![0]);

        // A bare CR wipes the line and the device writes something else over
        // it — the progress-line idiom. The replacement no longer matches.
        let mut lines = Vec::new();
        framer.push(b"\rdone\n", clock.now(), &mut lines);
        tx.send(ReaderEvent::Batch(reader::Batch {
            lines,
            raw: b"\rdone\n".to_vec(),
        }))
        .unwrap();
        app.connections[0].drain_events(1000);
        app.maintain_search();

        let conn = &app.connections[0];
        assert!(
            conn.search_matches.is_empty(),
            "the text that matched is gone, so the match is too; got {:?} for \
             store {:?}",
            conn.search_matches,
            (0..conn.store.len())
                .filter_map(|i| conn.store.get(i as u64).map(|l| l.text.to_string()))
                .collect::<Vec<_>>()
        );
        assert_eq!(conn.search_pos, None, "and no stale cursor into it");
    }

    /// `search_pos` is an index into `search_matches`, so dropping evicted
    /// entries off the front has to bring it down too — otherwise the hit the
    /// user is standing on silently becomes a later one.
    #[test]
    fn evicting_matches_keeps_the_cursor_on_the_same_line() {
        let (mut app, _enum_tx) = test_app("search-evict");
        let _tx = conn_with_injected_events(&mut app, PortId(0));
        app.active = 0;
        let conn = &mut app.connections[0];
        conn.search_query = "hit".into();
        conn.store.set_max_lines(100);
        for n in 0..40 {
            conn.store.append(IncomingLine {
                text: format!("hit {n}"),
                ts: Timestamp {
                    wall: chrono::Utc::now(),
                    micros: n as i64,
                },
                port: PortId(0),
                flags: LineFlags::default(),
                spans: Default::default(),
                cursor: None,
            });
        }
        app.maintain_search();

        // Stand on a specific hit.
        // A hit that survives the eviction below, so "the same line" is a
        // question with an answer.
        app.connections[0].search_pos = Some(35);
        let standing_on = app.connections[0].search_matches[35];
        assert_eq!(standing_on, 35);

        // Tighten the cap so the front is evicted, then let search catch up.
        app.connections[0].store.set_max_lines(10);
        app.maintain_search();

        let conn = &app.connections[0];
        let pos = conn.search_pos.expect("still standing somewhere");
        assert_eq!(
            conn.search_matches[pos], standing_on,
            "the cursor has to follow its line through the eviction, not stay \
             at the same slot and point at a different hit"
        );
    }

    /// If *every* recorded match is evicted at once, `search_matches` has to
    /// be cleared rather than left holding indices the store can no longer
    /// resolve — `maintain_search` only runs for the active connection, so a
    /// background tab can evict its whole store between search passes.
    #[test]
    fn evicting_every_match_clears_search_state() {
        let (mut app, _enum_tx) = test_app("search-evict-all");
        let _tx = conn_with_injected_events(&mut app, PortId(0));
        app.active = 0;
        let conn = &mut app.connections[0];
        conn.search_query = "hit".into();
        conn.store.set_max_lines(200);
        for n in 0..10 {
            conn.store.append(IncomingLine {
                text: format!("hit {n}"),
                ts: Timestamp {
                    wall: chrono::Utc::now(),
                    micros: n as i64,
                },
                port: PortId(0),
                flags: LineFlags::default(),
                spans: Default::default(),
                cursor: None,
            });
        }
        app.maintain_search();
        assert_eq!(app.connections[0].search_matches.len(), 10);
        app.connections[0].search_pos = Some(9);

        // Push enough non-matching lines to evict every matched line, without
        // another search pass in between (mirrors an inactive tab).
        let conn = &mut app.connections[0];
        for n in 0..200 {
            conn.store.append(IncomingLine {
                text: format!("miss {n}"),
                ts: Timestamp {
                    wall: chrono::Utc::now(),
                    micros: (100 + n) as i64,
                },
                port: PortId(0),
                flags: LineFlags::default(),
                spans: Default::default(),
                cursor: None,
            });
        }
        app.maintain_search();

        let conn = &app.connections[0];
        assert!(
            conn.search_matches.is_empty(),
            "every match was evicted, so none should remain: {:?}",
            conn.search_matches
        );
        assert_eq!(conn.search_pos, None);
    }

    /// If the match the cursor is standing on is itself among the evicted
    /// entries, the cursor has to be invalidated — clamping it into range
    /// would silently land it on an unrelated match instead.
    #[test]
    fn evicting_the_current_match_resets_the_cursor() {
        let (mut app, _enum_tx) = test_app("search-evict-cursor");
        let _tx = conn_with_injected_events(&mut app, PortId(0));
        app.active = 0;
        let conn = &mut app.connections[0];
        conn.search_query = "hit".into();
        conn.store.set_max_lines(100);
        for n in 0..40 {
            conn.store.append(IncomingLine {
                text: format!("hit {n}"),
                ts: Timestamp {
                    wall: chrono::Utc::now(),
                    micros: n as i64,
                },
                port: PortId(0),
                flags: LineFlags::default(),
                spans: Default::default(),
                cursor: None,
            });
        }
        app.maintain_search();

        // Stand on an early hit that will be evicted below.
        app.connections[0].search_pos = Some(1);
        assert_eq!(app.connections[0].search_matches[1], 1);

        app.connections[0].store.set_max_lines(10);
        app.maintain_search();

        let conn = &app.connections[0];
        if let Some(pos) = conn.search_pos {
            assert_ne!(
                conn.search_matches.get(pos).copied(),
                Some(1),
                "the evicted line is gone; the cursor must not silently land \
                 on a different match in its old slot"
            );
        }
    }

    #[test]
    fn merged_unread_count_tracks_new_entries_and_resets_on_rebuild() {
        let (mut app, _enum_tx) = test_app("merged-unread");
        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            Default::default(),
            PortConfig::default(),
            inert_handle(id),
        );
        let line = |text: &str, micros| IncomingLine {
            text: text.into(),
            ts: Timestamp {
                wall: chrono::Utc::now(),
                micros,
            },
            port: id,
            flags: LineFlags::default(),
            spans: Default::default(),
            cursor: None,
        };
        conn.store.append(line("old", 1));
        app.connections.push(conn);
        app.merged_follow = false;
        app.merged_dirty = true;

        app.maintain_merged();
        assert_eq!(app.merged_new_since_scroll, 0);

        app.connections[0].store.append(line("new", 2));
        app.maintain_merged();
        assert_eq!(app.merged_new_since_scroll, 1);

        app.merged_dirty = true;
        app.maintain_merged();
        assert_eq!(app.merged_new_since_scroll, 0);
    }
}
