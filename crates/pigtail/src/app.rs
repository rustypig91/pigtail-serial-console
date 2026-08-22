//! Application state and the `update()` dispatch. Draws by delegating to the
//! `panes` modules; holds no IO — data arrives from reader threads via channels.

use crate::paths::AppPaths;
use crate::wrap::WrapIndex;
use crossbeam_channel::Receiver;
use serialcore::clock::{SessionClock, Timestamp};
use serialcore::config::{Config, ExtractRule, PortConfig, PortIdentity, SavedConnection};
use serialcore::enumerate::{
    match_identity, spawn_enumerator, DiscoveredPort, EnumEvent, MatchResult,
};
use serialcore::extract::CompiledExtract;
use serialcore::filter::{Combine, FilterIndex, FilterRule, FilterSet};
use serialcore::framer::Framer;
use serialcore::reader::{self, ConnState, ReaderEvent, SourceSpec};
use serialcore::series::{Series, DEFAULT_CAPACITY};
use serialcore::session::{self, SessionMeta};
use serialcore::store::{IncomingLine, LineFlags, LineStore, PortId};
use serialcore::update;
use serialcore::wake::Wake;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

/// Cap on how much of a prior capture is preloaded per port on startup. The
/// store evicts down to `max_lines` anyway; this just bounds the framing work.
const PRELOAD_TAIL_BYTES: usize = 8 * 1024 * 1024;

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

/// A compiled highlight rule cached for render-time use. (`bold` from the rule
/// is not represented here: egui has no per-run bold without a bold font family,
/// so highlighting is expressed through colour.)
pub struct CompiledHighlight {
    pub re: regex::Regex,
    pub color: egui::Color32,
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
/// History is gathered from the newest capture backwards until
/// `PRELOAD_TAIL_BYTES` is filled, rather than from the newest one alone. A
/// single run can leave several captures behind — applying new port options
/// respawns the reader onto a fresh one — and what the console showed at exit
/// was itself part restored, part live, so stopping at the newest capture
/// throws away output that was on screen when the app closed.
fn preload_last_session(
    conn: &mut Connection,
    captures: &[(PathBuf, SessionMeta)],
    identity: &PortIdentity,
    clock: &SessionClock,
) {
    let restored = gather_history(captures, identity, PRELOAD_TAIL_BYTES);

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
            text: format!(
                "previous session · {}",
                // Local time, like every other wall-clock stamp on screen; only
                // storage is UTC.
                meta.start_wall
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
            ),
            ts: last_ts,
            port: conn.id,
            flags: LineFlags::RECONNECT_MARKER,
            spans: Default::default(),
            cursor: None,
        });
    }
}

/// Maximum raw bytes kept per connection for the live hex view.
const RAW_RING_CAP: usize = 1 << 20; // 1 MiB

/// One open connection (a tab).
pub struct Connection {
    pub id: PortId,
    pub label: String,
    pub identity: PortIdentity,
    pub port_config: PortConfig,
    pub handle: reader::ReaderHandle,
    pub store: LineStore,
    pub state: ConnState,
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
    pub last_error: Option<String>,
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
    pub plot_follow: bool,
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

impl Connection {
    fn drain_events(&mut self, max_lines: usize) -> bool {
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
                    }
                    self.state = s;
                }
                ReaderEvent::Error(e) => {
                    tracing::warn!(port = self.id.0, "{e}");
                    self.last_error = Some(e);
                }
                ReaderEvent::Batch(batch) => {
                    self.recompile_extract_if_dirty();
                    for b in &batch.raw {
                        if self.raw_ring.len() == RAW_RING_CAP {
                            self.raw_ring.pop_front();
                        }
                        self.raw_ring.push_back(*b);
                    }
                    let _ = max_lines;
                    let mut pairs: Vec<(String, f64)> = Vec::new();
                    for line in batch.lines {
                        // Parse SGR colours and strip other escapes (spec §2, §7.9).
                        let styled = serialcore::ansi::parse_line(&line.text, line.cursor);
                        let is_data = !line.flags.contains(LineFlags::RECONNECT_MARKER)
                            && !line.flags.contains(LineFlags::TX_ECHO);
                        let text = styled.text;
                        let abs = self.store.append(IncomingLine {
                            text: text.clone(),
                            ts: line.ts,
                            port: self.id,
                            flags: line.flags,
                            spans: styled.spans,
                            cursor: styled.cursor.map(|c| c as u32),
                        });
                        if !self.follow {
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

    fn recompile_extract_if_dirty(&mut self) {
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
    }

    fn push_series_point(&mut self, name: &str, t: f64, value: f64, line: u64) {
        let idx = match self.series_index.get(name) {
            Some(&i) => i,
            None => {
                let color = SERIES_PALETTE[self.series.len() % SERIES_PALETTE.len()];
                self.series.push(SeriesEntry {
                    series: Series::new(name.to_string(), DEFAULT_CAPACITY),
                    color,
                    visible: true,
                    own_axis: false,
                });
                let i = self.series.len() - 1;
                self.series_index.insert(name.to_string(), i);
                i
            }
        };
        self.series[idx].series.push(t, value, line);
    }
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

/// The update notice: what it says, and what its buttons do.
pub struct UpdateDialog {
    pub title: String,
    pub message: String,
    /// Release page the "Download" button opens. `None` when there is nothing to
    /// download and the dialog is a plain acknowledgement.
    pub download_url: Option<String>,
    /// Version the "Skip this version" button records. `None` hides that button.
    pub skip_version: Option<String>,
}

pub struct App {
    pub clock: SessionClock,
    pub config: Config,
    pub paths: AppPaths,
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
    /// Compiled global highlight rules; rebuilt when `highlight_dirty`.
    pub highlight_cache: Vec<CompiledHighlight>,
    pub highlight_dirty: bool,
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
    /// True when the merged pseudo-tab is active.
    pub merged_selected: bool,
    // Floating tool windows, toggled from the console right-click menu, so the
    // main window stays uncluttered.
    pub show_settings: bool,
    pub show_filters_win: bool,
    pub show_highlight_win: bool,
    pub show_extract_win: bool,
    pub show_search: bool,
    /// Set when search should grab keyboard focus next frame (e.g. after Ctrl+F).
    pub search_focus_request: bool,
    /// `Some` while an update check is in flight.
    pub update_rx: Option<Receiver<update::CheckResult>>,
    /// True when the in-flight check came from Settings → "Check for updates".
    /// A manual check always reports a result and ignores a previous skip; the
    /// startup check stays silent unless there is a new version to announce.
    pub update_manual: bool,
    /// `Some` while the update notice is showing.
    pub update_dialog: Option<UpdateDialog>,
    /// Console text size to flash over the middle of the window, and the time
    /// it was set. Ctrl+wheel has nothing else to show for itself: the change
    /// it makes is legible only if you already know what you are looking for.
    pub font_toast: Option<(u8, f64)>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, paths: AppPaths, config: Config) -> App {
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
        spawn_enumerator(tx, Duration::from_millis(500), wake.clone());

        let mut app = App {
            clock: SessionClock::new(),
            config,
            paths,
            wake,
            enum_rx: rx,
            available: Vec::new(),
            connections: Vec::new(),
            active: 0,
            next_port_id: 0,
            config_dialog: None,
            highlight_cache: Vec::new(),
            highlight_dirty: true,
            merged: Vec::new(),
            merged_seq: 0,
            merged_dirty: false,
            merged_wrap: WrapIndex::new(),
            merged_generation: 0,
            merged_selected: false,
            show_settings: false,
            show_filters_win: false,
            show_highlight_win: false,
            show_extract_win: false,
            show_search: false,
            search_focus_request: false,
            update_rx: None,
            update_manual: false,
            update_dialog: None,
            font_toast: None,
        };

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
            app.open_connection(saved.identity.clone(), None, saved.config);
            if let Some(conn) = app.connections.last_mut() {
                preload_last_session(conn, &prior_captures, &saved.identity, &clock);
            }
        }
        app.active = 0;
        app.merged_selected = false;

        app
    }

    /// Open the modal new-connection dialog, seeded with sensible defaults.
    pub fn open_config_dialog(&mut self) {
        let selected_path = self.available.first().map(|p| p.path.clone());
        self.config_dialog = Some(ConfigDialog {
            selected_path,
            config: PortConfig::default(),
            preset_name: String::new(),
            editing: None,
        });
    }

    /// Open the config dialog to edit an existing tab's port options. Applying
    /// it reconnects that tab with the new settings.
    pub fn open_port_options(&mut self, index: usize) {
        let Some(conn) = self.connections.get(index) else {
            return;
        };
        let config = conn.port_config.clone();
        // Pre-select the device's current path if it's present right now.
        let selected_path = self
            .available
            .iter()
            .find(|p| p.identity == conn.identity)
            .map(|p| p.path.clone());
        self.config_dialog = Some(ConfigDialog {
            selected_path,
            config,
            preset_name: String::new(),
            editing: Some(conn.id),
        });
    }

    /// Spawn a reader for a live serial device (shared by open and reconnect).
    fn spawn_serial_reader(
        &self,
        id: PortId,
        identity: &PortIdentity,
        config: &PortConfig,
        initial_path: Option<String>,
    ) -> reader::ReaderHandle {
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
            return;
        };
        let identity = initial_path
            .as_ref()
            .and_then(|p| self.available.iter().find(|d| &d.path == p))
            .map(|d| d.identity.clone())
            .unwrap_or_else(|| self.connections[index].identity.clone());

        // Close the old reader *before* opening the new one. Both address the
        // same device, and a serial port is opened exclusively — with the old
        // reader still holding it the new one's first open fails, and it goes
        // away into its reconnect backoff for up to two seconds before trying
        // again. The wait is no new cost: this join already happened, just
        // after the spawn rather than before it.
        self.connections[index].handle.shutdown_in_place();

        // Keep the same port id so the preserved lines still map to this tab in
        // the merged view; only the reader (and its capture file) is replaced.
        let handle = self.spawn_serial_reader(port_id, &identity, &config, initial_path.clone());

        let path_label = initial_path.unwrap_or_else(|| identity.label());
        let label = format!("{} ({})", identity.label(), path_label);
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
            conn.state = ConnState::Connecting;
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
    pub fn connect_from_dialog(&mut self) {
        let Some(dialog) = self.config_dialog.take() else {
            return;
        };
        let Some(path) = dialog.selected_path else {
            return;
        };
        if let Some(port) = self.available.iter().find(|p| p.path == path).cloned() {
            self.open_connection(port.identity, Some(port.path), dialog.config);
        }
    }

    /// Lower-level open used by manual connect and profile auto-connect.
    pub(crate) fn open_connection(
        &mut self,
        identity: PortIdentity,
        initial_path: Option<String>,
        port_config: PortConfig,
    ) {
        let id = PortId(self.next_port_id);
        self.next_port_id += 1;
        let path_label = initial_path.clone().unwrap_or_else(|| identity.label());
        let label = format!("{} ({})", identity.label(), path_label);

        let handle = self.spawn_serial_reader(id, &identity, &port_config, initial_path);
        let conn = self.make_connection(id, label, identity, port_config, handle);
        self.connections.push(conn);
        self.active = self.connections.len() - 1;
        self.merged_selected = false;
        self.merged_dirty = true;
        self.save_session();
    }

    /// Persist the set of currently-open connections so they reopen next launch.
    fn save_session(&mut self) {
        self.config.last_open = self
            .connections
            .iter()
            .map(|c| SavedConnection {
                identity: c.identity.clone(),
                config: c.port_config.clone(),
            })
            .collect();
        self.write_config();
    }

    fn make_connection(
        &self,
        id: PortId,
        label: String,
        identity: PortIdentity,
        port_config: PortConfig,
        handle: reader::ReaderHandle,
    ) -> Connection {
        let dtr = port_config.dtr_on_open;
        let rts = port_config.rts_on_open;
        Connection {
            id,
            label,
            identity,
            port_config,
            handle,
            store: LineStore::new(self.config.settings.max_lines),
            state: ConnState::Connecting,
            follow: true,
            new_since_scroll: 0,
            pin_view_h: 0.0,
            wrap_index: WrapIndex::new(),
            filter_generation: 0,
            top_line: None,
            console_layout: None,
            raw_ring: VecDeque::new(),
            last_error: None,
            mark_micros: None,
            hex_view: false,
            filter_rules: Vec::new(),
            filter_combine: Combine::And,
            filter_index: FilterIndex::new(),
            filter_dirty: false,
            filter_errors: Vec::new(),
            search_query: String::new(),
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
            plot_follow: true,
            show_plot: false,
            tx_input: String::new(),
            tx_history: Vec::new(),
            tx_history_pos: None,
            dtr,
            rts,
        }
    }

    /// Serialize config to the platform config file.
    pub fn write_config(&mut self) {
        let path = &self.paths.config_file;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match self.config.to_toml() {
            Ok(toml) => {
                if let Err(e) = std::fs::write(path, toml) {
                    tracing::warn!("writing config: {e}");
                }
            }
            Err(e) => tracing::warn!("serializing config: {e}"),
        }
    }

    /// Start a background check for a newer release. `manual` marks the explicit
    /// Settings → "Check for updates" action, which reports a result either way;
    /// the startup check only speaks up when there is a new version.
    pub fn start_update_check(&mut self, manual: bool) {
        if self.update_rx.is_some() {
            return; // one already in flight
        }
        self.update_manual = manual;
        self.update_rx = Some(update::spawn_check(self.wake.clone()));
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
                    "v{} is available — you're on v{current}.",
                    version.trim_start_matches('v')
                ),
                download_url: Some(url),
                skip_version: Some(version),
            },
            update::Notice::UpToDate => UpdateDialog {
                title: "Up to date".into(),
                message: format!("You're running the latest version (v{current})."),
                download_url: None,
                skip_version: None,
            },
            update::Notice::Failed(why) => UpdateDialog {
                title: "Update check failed".into(),
                message: why,
                download_url: None,
                skip_version: None,
            },
        });
    }

    /// Auto-connect any profile marked `auto_connect` whose device is present and
    /// not already open (spec §7.14).
    fn auto_connect_profiles(&mut self) {
        // Collect actions first to avoid borrowing conflicts.
        let mut to_open: Vec<(PortIdentity, String, PortConfig)> = Vec::new();
        for profile in &self.config.profiles {
            if !profile.auto_connect {
                continue;
            }
            let already = self
                .connections
                .iter()
                .any(|c| c.identity == profile.identity);
            if already {
                continue;
            }
            if let MatchResult::Definite(i) = match_identity(&profile.identity, &self.available) {
                to_open.push((
                    profile.identity.clone(),
                    self.available[i].path.clone(),
                    profile.port.clone(),
                ));
            }
        }
        for (identity, path, config) in to_open {
            self.open_connection(identity, Some(path), config);
        }
    }

    /// Disconnect and close the active connection tab.
    pub fn close_connection(&mut self, index: usize) {
        if index >= self.connections.len() {
            return;
        }
        let conn = self.connections.remove(index);
        conn.handle.shutdown();
        if self.active >= self.connections.len() {
            self.active = self.connections.len().saturating_sub(1);
        }
        self.merged_dirty = true;
        self.save_session();
    }

    fn poll_enumerator(&mut self) {
        let mut updated = false;
        while let Ok(ev) = self.enum_rx.try_recv() {
            if let EnumEvent::Snapshot(snap) = ev {
                self.available = snap;
                updated = true;
            }
        }
        if updated {
            self.auto_connect_profiles();
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
                .case_insensitive(true)
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
        // Compile query as regex, falling back to a literal search.
        let re = regex::RegexBuilder::new(&conn.search_query)
            .case_insensitive(true)
            .build()
            .or_else(|_| {
                regex::RegexBuilder::new(&regex::escape(&conn.search_query))
                    .case_insensitive(true)
                    .build()
            });
        let Ok(re) = re else {
            return;
        };

        if conn.search_dirty {
            conn.search_dirty = false;
            conn.search_matches.clear();
            conn.search_tested_upto = conn.store.first_abs_index();
        }
        // Drop evicted matches.
        let first = conn.store.first_abs_index();
        if let Some(p) = conn.search_matches.iter().position(|&i| i >= first) {
            if p > 0 {
                conn.search_matches.drain(..p);
            }
        }
        let start = conn.search_tested_upto.max(first);
        let end = conn.store.next_abs_index();
        for abs in start..end {
            if let Some(line) = conn.store.get(abs) {
                if re.is_match(line.text) {
                    conn.search_matches.push(abs);
                }
            }
        }
        conn.search_tested_upto = end;
        if conn.search_pos.is_none() && !conn.search_matches.is_empty() {
            conn.search_pos = Some(conn.search_matches.len() - 1);
        }
    }

    /// Maintain the timestamp-interleaved merged view (spec §7.12). Rebuilds on
    /// connect/close; otherwise a fast append of each port's new tail.
    fn maintain_merged(&mut self) {
        if self.merged_dirty {
            self.merged_dirty = false;
            self.merged.clear();
            self.merged_seq = 0;
            self.merged_generation += 1;
            for conn in &mut self.connections {
                conn.merged_upto = conn.store.first_abs_index();
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

    /// Drop merged entries whose line has been evicted from its port's store.
    ///
    /// Without this the merged view is the one place in the app that grows
    /// without bound: every port's store evicts at `max_lines`, but an entry
    /// here outlives the line it points at, and is then a row that can only
    /// ever draw as blank space. It is also built on every frame that carries
    /// data, whether or not the merged tab is the one on screen, so a long
    /// session at speed leaks it steadily.
    ///
    /// Only the front is walked. Entries are in timestamp order and each port
    /// evicts its own oldest first, so that is where the dead ones collect —
    /// and stopping at the first live entry keeps this proportional to what is
    /// actually dropped rather than to the length of the view.
    fn prune_merged(&mut self) {
        let dead = self
            .merged
            .iter()
            .take_while(|e| {
                match self.connections.iter().find(|c| c.id == e.port) {
                    Some(conn) => e.abs < conn.store.first_abs_index(),
                    // The port is gone; closing one rebuilds the view anyway.
                    None => true,
                }
            })
            .count();
        if dead > 0 {
            self.merged.drain(..dead);
        }
    }

    /// Clear the console: drop every line on screen *and* the capture on disk.
    ///
    /// Deliberately destructive — the point is that cleared output is gone, not
    /// merely scrolled away, so it also can't come back as preloaded history on
    /// the next launch. From the merged view every port is cleared, since that
    /// is what the window is showing; otherwise just the active tab.
    ///
    /// Bytes already in flight (read but not yet drained from the reader
    /// channel) still land afterwards. That's a line or two at most, and they
    /// are output that arrived after the click.
    pub fn clear_console(&mut self) {
        let targets: Vec<usize> = if self.merged_selected {
            (0..self.connections.len()).collect()
        } else {
            self.active_index().into_iter().collect()
        };
        for i in targets {
            let conn = &mut self.connections[i];
            conn.handle.clear_log();
            conn.store.clear();
            conn.raw_ring.clear();
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

        let max_lines = self.config.settings.max_lines;
        let mut any_data = false;
        for conn in &mut self.connections {
            if conn.drain_events(max_lines) {
                any_data = true;
            }
        }

        // Maintain derived indices.
        self.rebuild_highlight_if_dirty();
        self.maintain_filters();
        self.maintain_search();
        if any_data || self.merged_dirty {
            self.maintain_merged();
        }

        // Minimal chrome: a header of tabs on top, a status footer at the
        // bottom, and the console filling everything in between. Tool panels are
        // floating windows toggled from the console's right-click menu.
        self.show_header(ctx);
        self.show_footer(ctx);
        self.show_plot(ctx); // bottom panel, only when enabled for the tab
        self.show_console(ctx);

        // Floating windows.
        self.show_config_dialog(ctx);
        self.show_tool_windows(ctx);
        self.show_settings_window(ctx);
        self.show_update_dialog(ctx);
        self.show_font_toast(ctx);

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
}

#[cfg(test)]
mod tests {
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
}
