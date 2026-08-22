//! The console: a virtualized log filling the whole window between header and
//! footer. Sending is raw — the console forwards the keyboard straight to the
//! device (see `transmit`) — with an optional search bar and a right-click menu
//! that toggles everything else so the main view stays clean.

use crate::app::{App, CompiledHighlight, Connection, MergedEntry};
use crate::wrap::{rows_for, WrapIndex};
use egui::text::LayoutJob;
use serialcore::clock::Timestamp;
use serialcore::config::{TimestampFormat, MAX_CONSOLE_FONT_SIZE, MIN_CONSOLE_FONT_SIZE};
use serialcore::store::{LineFlags, LineRef, LineStore, PortId};

/// Horizontal gap between a row's markers (port tag, timestamp, the sent-line
/// ">") and the text they precede.
const ROW_GAP: f32 = 6.0;

/// How long the text-size readout stays up after a ctrl+wheel step, and how
/// much of that time it spends fading out.
const SIZE_TOAST_SECS: f64 = 0.9;
const SIZE_TOAST_FADE_SECS: f64 = 0.35;

const PORT_PALETTE: [egui::Color32; 6] = [
    egui::Color32::from_rgb(0x6c, 0xb6, 0xff),
    egui::Color32::from_rgb(0x8d, 0xdb, 0x8c),
    egui::Color32::from_rgb(0xf2, 0xc5, 0x5c),
    egui::Color32::from_rgb(0xe8, 0x7d, 0xba),
    egui::Color32::from_rgb(0xc3, 0x9d, 0xf5),
    egui::Color32::from_rgb(0x5f, 0xd6, 0xcf),
];

/// Actions collected from the right-click menu, applied after drawing so the
/// menu closures don't need `&mut self`.
#[derive(Default)]
struct MenuAction {
    set_ts: Option<TimestampFormat>,
    toggle_hex: bool,
    toggle_plot: bool,
    open_filters: bool,
    open_highlight: bool,
    open_extract: bool,
    toggle_search: bool,
    set_mark: bool,
    /// Line the mark goes on: whichever row the menu was opened over, or `None`
    /// when it was opened over empty space.
    mark_line: Option<u64>,
    clear_mark: bool,
    clear_console: bool,
    export: Option<bool>,
    toggle_dtr: bool,
    toggle_rts: bool,
    send_break: bool,
}

/// The console's layout for one frame: the font, the pitch every visual row
/// shares, and how the width is split between the fixed marker column and the
/// text to the right of it.
///
/// The whole console is laid out in these terms rather than in whatever each
/// row happens to need, because the virtualized scroll area can only skip to a
/// row it can compute the position of — see [`crate::wrap`].
struct Metrics {
    /// The console's monospace font, sized by the `console_font_size` setting.
    /// Everything in a row — gutter, port tag, text — is drawn with it, so the
    /// whole line scales together and `row_height` stays exact.
    font: egui::FontId,
    row_height: f32,
    char_w: f32,
    /// Width reserved for the markers at the head of every row, whether or not
    /// that row has them. Uniform so wrapped rows line up under the text above
    /// them, and so the ">" on a line you sent stops shoving its text sideways.
    prefix_w: f32,
    /// Characters of text per visual row; 0 when wrapping is off.
    cols: usize,
}

impl Metrics {
    fn new(
        ui: &egui::Ui,
        font: egui::FontId,
        ts: TimestampFormat,
        wrap: bool,
        port_tag: bool,
        tx_marker: bool,
    ) -> Metrics {
        let (row_height, advance) = ui.fonts(|f| (f.row_height(&font), f.glyph_width(&font, '0')));
        // epaint snaps the pen to a whole pixel after every glyph, so the
        // advance a row is actually laid out with is the rounded one — and
        // since the pen lands back on a pixel each time, that rounding never
        // accumulates. Predicting with the unrounded advance instead drifts by
        // a column every few characters, which is enough to put a line's real
        // height a row off what the index reserved for it.
        let ppp = ui.ctx().pixels_per_point();
        let char_w = ((advance * ppp).round() / ppp).max(1.0);

        // What precedes the text, reserved on every row so the text column never
        // moves. Kept as narrow as it can be: this is dead space on the majority
        // of rows, which carry no marker at all.
        let mut prefix_w = 0.0;
        if port_tag {
            prefix_w += 8.0 * char_w + ROW_GAP; // "[abcdef]"
        }
        let ts_cols = match ts {
            TimestampFormat::None => 0,
            // Exactly "12:34:56.789"; the from-mark form is shorter.
            TimestampFormat::Absolute | TimestampFormat::Mark => 12,
            // "+1234.567s" and friends, with a column to spare.
            TimestampFormat::Delta => 11,
        };
        if ts_cols > 0 {
            prefix_w += ts_cols as f32 * char_w + ROW_GAP;
        }
        // One column for the ">" that marks a line you sent, reserved on every
        // row so a sent line's text does not sit further right than the device
        // output around it — and only where such a line can actually appear,
        // since otherwise this is a column of dead space on every row.
        if tx_marker {
            prefix_w += char_w + ROW_GAP;
        }

        // The scrollbar's width is taken off here rather than discovered inside
        // the scroll area, since `cols` has to be known before the rows are laid
        // out. A whole column goes with it: floating scrollbars allocate nothing
        // and overlay the text instead, and a glyph ending flush against the
        // window edge is a glyph drawn half outside it.
        let text_w = ui.available_width()
            - prefix_w
            - ui.spacing().scroll.allocated_width()
            - char_w
            - ROW_GAP;
        let cols = if wrap {
            (text_w / char_w).floor().max(8.0) as usize
        } else {
            0
        };
        Metrics {
            font,
            row_height,
            char_w,
            prefix_w,
            cols,
        }
    }

    /// Width the text is laid out to: room for exactly `cols` characters and
    /// not the next one, so the galley breaks where the row index predicted it
    /// would rather than wherever the row happens to end.
    fn wrap_width(&self) -> f32 {
        if self.cols == 0 {
            f32::INFINITY
        } else {
            // The slack is a fraction of a character: enough to absorb float
            // error, never enough to admit one more glyph.
            self.cols as f32 * self.char_w + self.char_w * 0.25
        }
    }
}

/// Whether this console needs the ">" marker column reserved. Not simply
/// "local echo is on": lines echoed earlier keep their marker for as long as
/// they are on screen, and they outlive the setting — turning local echo off in
/// the port options keeps the console, and a restored session brings back what
/// was sent in it. Without the column those markers paint over the text.
fn tx_marker(conn: &Connection) -> bool {
    conn.port_config.local_echo || conn.store.tx_echo_any()
}

struct RowCtx<'a> {
    ts_format: TimestampFormat,
    m: &'a Metrics,
    /// Visual rows this line was given. The slot is sized from it, and the text
    /// is capped to it, so a mispredicted line loses its overflow instead of
    /// spilling into its neighbour.
    rows: u32,
    prev_micros: Option<u64>,
    mark: Option<u64>,
    highlight: &'a [CompiledHighlight],
    search_re: Option<&'a regex::Regex>,
    is_search_current: bool,
    port_tag: Option<(String, egui::Color32)>,
    /// Stable identity for this line, independent of where it lands in the
    /// virtualized viewport. The viewport recycles screen positions across
    /// frames — under `follow`, the same slot shows a different abs line every
    /// time new data arrives. egui's default auto-IDs are derived from
    /// per-frame allocation order (position), not content, so without this,
    /// text selection and the right-click menu (both keyed by widget id) would
    /// silently detach or close the instant the line they're anchored to
    /// scrolls to a different position — i.e. on every new line while pinned.
    row_id: egui::Id,
}

impl App {
    /// The console's monospace font. Clamped here rather than at load, so a
    /// hand-edited config with an absurd size still renders.
    pub(crate) fn console_font(&self) -> egui::FontId {
        egui::FontId::monospace(
            self.config
                .settings
                .console_font_size
                .clamp(MIN_CONSOLE_FONT_SIZE, MAX_CONSOLE_FONT_SIZE) as f32,
        )
    }

    /// Ctrl+wheel over the console changes the text size by whole points, the
    /// gesture browsers and editors use. Persisted, so it survives a restart.
    fn zoom_console_font(&mut self, steps: i32, ctx: &egui::Context) {
        let cur = i32::from(self.config.settings.console_font_size);
        let next = (cur + steps).clamp(
            i32::from(MIN_CONSOLE_FONT_SIZE),
            i32::from(MAX_CONSOLE_FONT_SIZE),
        ) as u8;
        if next != self.config.settings.console_font_size {
            self.config.settings.console_font_size = next;
            self.write_config();
        }
        // Flashed even when the size did not move, so that scrolling on past
        // either limit reads as "this is as far as it goes" rather than as a
        // dropped gesture.
        self.font_toast = Some((next, ctx.input(|i| i.time)));
    }

    /// The size readout: the new point size over the middle of the window,
    /// fading out. Drawn last, over everything, and interactable by nothing.
    pub(crate) fn show_font_toast(&mut self, ctx: &egui::Context) {
        let Some((size, shown_at)) = self.font_toast else {
            return;
        };
        let age = ctx.input(|i| i.time) - shown_at;
        if age >= SIZE_TOAST_SECS {
            self.font_toast = None;
            return;
        }
        let fade = ((SIZE_TOAST_SECS - age) / SIZE_TOAST_FADE_SECS).clamp(0.0, 1.0) as f32;
        let visuals = ctx.style().visuals.clone();
        egui::Area::new(egui::Id::new("console_size_toast"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_opacity(fade);
                egui::Frame::none()
                    .fill(visuals.window_fill)
                    .stroke(visuals.window_stroke)
                    .rounding(8.0)
                    .inner_margin(egui::vec2(18.0, 12.0))
                    .show(ui, |ui| {
                        // An `Area` is laid out around its content, so there is
                        // no width to wrap against yet: left to wrap, the
                        // readout stacks itself one glyph per row.
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!("{size} pt"))
                                    .size(28.0)
                                    .color(visuals.strong_text_color()),
                            )
                            .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    });
            });
        // An idle console schedules no frames of its own, so the fade has to ask
        // for them — including the one that clears the readout.
        ctx.request_repaint();
    }

    pub(crate) fn show_console(&mut self, ctx: &egui::Context) {
        let mut menu = MenuAction::default();
        let mut open_dialog = false;
        let mut font_steps = 0;

        // Ctrl+Shift+F opens search — carved out the same way Ctrl+Shift+C/V
        // are (see `transmit`), so plain Ctrl+F still reaches the device.
        // Consuming it here removes it from the event queue before
        // `console_key_input` runs later this frame, so it isn't also sent.
        let open_search = ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers {
                    ctrl: true,
                    shift: true,
                    ..Default::default()
                },
                egui::Key::F,
            )
        });
        if open_search {
            menu.toggle_search = true;
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // Only while the pointer is actually over the console, so ctrl+wheel
            // still belongs to whatever else is under it (the plot panel, a
            // floating window — `rect_contains_pointer` is layer-aware, so one
            // covering the console takes the gesture). `max_rect`, not
            // `ui_contains_pointer`: that tests `min_rect`, which is still empty
            // this early in the panel.
            if ui.rect_contains_pointer(ui.max_rect()) {
                font_steps = ctrl_wheel_steps(ui.ctx());
            }
            if self.connections.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.heading("No connection");
                    if ui.button("+ New connection").clicked() {
                        open_dialog = true;
                    }
                });
                return;
            }
            let active = self.active.min(self.connections.len() - 1);
            self.active = active;

            // Optional search bar pinned to the top of the console.
            if self.show_search && !self.merged_selected {
                egui::TopBottomPanel::top("search_bar")
                    .show_separator_line(false)
                    .show_inside(ui, |ui| self.show_search_bar(ui, active));
            }

            // The console body fills the remaining space. The send prompt is now
            // an inline REPL row at the end of the single-connection view.
            if self.merged_selected {
                self.show_merged_rows(ui, &mut menu);
            } else if self.connections[active].hex_view {
                self.show_hex_rows(ui, active, &mut menu);
            } else {
                self.show_single_rows(ui, active, &mut menu);
            }
        });

        if open_dialog {
            self.open_config_dialog();
        }
        if font_steps != 0 {
            self.zoom_console_font(font_steps, ctx);
        }
        let active = self.active_index().unwrap_or(0);
        self.apply_menu(active, menu);

        // Raw console input: forward the keyboard to the device whenever a live
        // tab is showing and nothing else (search box, a dialog) holds focus.
        // Runs after drawing so this frame's focus state is settled.
        if self.config_dialog.is_none()
            && !self.merged_selected
            && ctx.memory(|m| m.focused().is_none())
        {
            if let Some(active) = self.active_index() {
                self.console_key_input(ctx, active);
            }
        }
    }

    fn show_search_bar(&mut self, ui: &mut egui::Ui, active: usize) {
        let mut next = false;
        let mut prev = false;
        let mut close = false;
        let mut focus = std::mem::take(&mut self.search_focus_request);
        ui.horizontal(|ui| {
            ui.label("🔍");
            let conn = &mut self.connections[active];
            let resp = ui.add(
                egui::TextEdit::singleline(&mut conn.search_query)
                    .hint_text("search (regex)…")
                    .desired_width(240.0),
            );
            if resp.changed() {
                conn.search_dirty = true;
                conn.search_pos = None;
            }
            // Focus only when explicitly requested (opening), not every frame —
            // otherwise the search box would keep grabbing focus.
            if focus {
                resp.request_focus();
                focus = false;
            }
            // While the search box owns the keyboard, Enter/Escape drive search
            // and are *consumed* so they don't also reach the device (Enter makes
            // a singleline lose focus, which would otherwise leak it to the
            // console's raw input). Enter keeps focus so you can search again.
            if resp.has_focus() || resp.lost_focus() {
                if ui.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::Enter)) {
                    prev = true;
                    resp.request_focus();
                } else if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                    next = true;
                    resp.request_focus();
                }
                if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                    close = true;
                }
            }
            if ui.small_button("Prev").clicked() {
                prev = true;
            }
            if ui.small_button("Next").clicked() {
                next = true;
            }
            if !conn.search_matches.is_empty() {
                let n = conn.search_pos.map(|p| p + 1).unwrap_or(0);
                ui.weak(format!("{n}/{}", conn.search_matches.len()));
            }
            if ui.small_button("Close").clicked() {
                close = true;
            }
        });
        if next {
            self.search_step(1);
        }
        if prev {
            self.search_step(-1);
        }
        if close {
            self.show_search = false;
            self.connections[active].search_query.clear();
            self.connections[active].search_dirty = true;
        }
    }

    fn show_single_rows(&mut self, ui: &mut egui::Ui, active: usize, menu: &mut MenuAction) {
        let ts_format = self.config.settings.timestamp_format;
        let m = Metrics::new(
            ui,
            self.console_font(),
            ts_format,
            self.config.settings.wrap_lines,
            false,
            tx_marker(&self.connections[active]),
        );
        // Bring the row index up to date before anything asks how tall the
        // content is or which line a row belongs to.
        self.connections[active].sync_wrap(m.cols);
        // A change of column count or text size moves every row: the same pixel
        // offset now lands on a different line. Noting it here is what lets the
        // view be re-pinned to the line the reader was looking at, below.
        let layout = (m.cols, self.config.settings.console_font_size);
        let relayout = self.connections[active]
            .console_layout
            .replace(layout)
            .is_some_and(|prev| prev != layout);

        let App {
            connections,
            highlight_cache,
            ..
        } = self;
        let conn = &mut connections[active];
        let follow = conn.follow;
        // Whether the *user* set a mark, which is what the menu offers to clear.
        let has_mark = conn.mark_micros.is_some();
        // With none set, the console counts from its own first line rather than
        // falling back to a wall clock: "from mark" should always read as an
        // offset. That reference follows the scrollback, so once the oldest
        // lines are evicted it becomes the oldest line still held.
        let mark = conn.mark_micros.or_else(|| {
            conn.store
                .get(conn.store.first_abs_index())
                .map(|l| l.meta.ts.micros)
        });
        let filter_active = !conn
            .filter_rules
            .iter()
            .all(|r| !r.enabled || r.pattern.is_empty());

        let search_re = compile_search(&conn.search_query);
        let cur_match_abs = conn
            .search_pos
            .and_then(|p| conn.search_matches.get(p))
            .copied();

        let matching = conn.filter_index.matching();
        let first_abs = conn.store.first_abs_index();
        let entries = conn.wrap_index.len();
        let abs_of = |i: usize| {
            if filter_active {
                matching[i]
            } else {
                first_abs + i as u64
            }
        };
        // With local echo on, the in-progress input line is shown as a trailing
        // row (for devices that don't echo). Otherwise the device's own echo is
        // the only thing on screen.
        let show_echo = conn.port_config.local_echo;
        let echo_start = conn.wrap_index.total_rows();
        let echo_rows = if show_echo {
            // +1 for the block cursor drawn after the text.
            u64::from(rows_for(conn.tx_input.len() as u32 + 1, m.cols))
        } else {
            0
        };
        let n_rows = echo_start + echo_rows;
        let row_height = m.row_height;
        // No vertical gap between rows, so the selectable text of adjacent lines
        // touches: a click-drag can begin *between* lines, not only on them. This
        // also makes the pin pitch exactly `row_height` (see `pin_bottom`).
        ui.spacing_mut().item_spacing.y = 0.0;

        // Background right-click menu for empty areas.
        let bg = ui.interact(
            ui.available_rect_before_wrap(),
            ui.id().with("console_bg"),
            egui::Sense::click(),
        );
        bg.context_menu(|ui| console_menu(ui, menu, None, ts_format, has_mark));

        let row_of_line = |target: u64| -> Option<u64> {
            let entry = if filter_active {
                matching.binary_search(&target).ok()?
            } else {
                target.checked_sub(first_abs)? as usize
            };
            Some(conn.wrap_index.start_row(entry))
        };
        let goto = conn.scroll_to.take();
        let scroll_offset = goto
            .and_then(|target| {
                Some(row_of_line(target)? as f32 * row_height - ui.available_height() * 0.5)
            })
            .or_else(|| {
                // Rows changed height under the reader (text resized, window
                // resized): put the line that was at the top back at the top,
                // instead of leaving the old pixel offset pointing at whatever
                // line now happens to live there.
                if !relayout || conn.follow {
                    return None;
                }
                Some(row_of_line(conn.top_line?)? as f32 * row_height)
            });

        // The user touching the wheel or dragging the scrollbar unpins.
        let user_scrolled = ui.input(user_scrolled);
        if goto.is_some() {
            // Navigating to a specific line (search/plot) unpins so we
            // stay there instead of snapping back to the bottom. Re-pinning the
            // top line after a relayout is not navigation and leaves follow
            // alone — while following, the bottom pin below wins anyway.
            conn.follow = false;
        } else if follow && user_scrolled {
            conn.follow = false;
        }
        let following = conn.follow && scroll_offset.is_none();

        let mut area = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .drag_to_scroll(false);
        if let Some(off) = scroll_offset {
            area = area.vertical_scroll_offset(off.max(0.0));
        } else if following {
            // Pin to the bottom by setting the offset explicitly every frame, so
            // a burst of new lines can't outrun the pin, and so toggling Pin on
            // jumps straight to the bottom. We overwrite the offset before layout
            // every frame, which bypasses egui's own end-of-frame clamp — so we
            // feed the already-clamped bottom ourselves. Rows are all exactly
            // `row_height` with zero inter-row spacing, so the content height is
            // exactly `n_rows * row_height`; computing it directly (rather than
            // feeding back the measured content size) keeps the offset stable
            // instead of chasing measurement noise and jittering.
            let view_h = if conn.pin_view_h > 0.0 {
                conn.pin_view_h
            } else {
                ui.available_height()
            };
            let bottom = ((n_rows as f32) * row_height - view_h).max(0.0);
            area = area.vertical_scroll_offset(bottom);
        }

        let output = area.show_viewport(ui, |ui, viewport| {
            ui.set_width(ui.available_width());
            ui.set_height(n_rows as f32 * row_height);
            let (first_entry, rect) = viewport_entries(ui, viewport, &conn.wrap_index, row_height);
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rect), |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let last_row = (viewport.max.y / row_height).ceil() as u64 + 1;
                let mut entry = first_entry;
                while entry < entries && conn.wrap_index.start_row(entry) < last_row {
                    let abs = abs_of(entry);
                    let Some(line) = conn.store.get(abs) else {
                        // Still take up the space the index promised, or every
                        // row below this one shifts up out of place.
                        ui.add_space(conn.wrap_index.rows(entry) as f32 * row_height);
                        entry += 1;
                        continue;
                    };
                    let prev_micros =
                        prev_micros_for(&conn.store, filter_active, matching, entry, abs);
                    let rctx = RowCtx {
                        ts_format,
                        m: &m,
                        rows: conn.wrap_index.rows(entry),
                        prev_micros,
                        mark,
                        highlight: highlight_cache,
                        search_re: search_re.as_ref(),
                        is_search_current: cur_match_abs == Some(abs),
                        port_tag: None,
                        row_id: egui::Id::new(("console_row", conn.id, abs)),
                    };
                    let resp = render_row(ui, &line, &rctx);
                    resp.context_menu(|ui| console_menu(ui, menu, Some(abs), ts_format, has_mark));
                    entry += 1;
                }
                // The echo row trails the last line, so it is drawn only when
                // the viewport reached the end of them.
                if show_echo && entry == entries {
                    let row_id = ui.id().with(("console_echo", conn.id));
                    render_echo_line(
                        ui,
                        &conn.tx_input,
                        &m,
                        echo_rows as f32 * row_height,
                        row_id,
                    );
                }
            });
        });

        rearm_follow_at_bottom(&mut conn.follow, user_scrolled, &output, row_height);
        conn.pin_view_h = output.inner_rect.height();
        // Remember what is at the top, so a relayout can put it back there.
        let top_row = (output.state.offset.y / row_height).floor().max(0.0) as u64;
        conn.top_line = (entries > 0).then(|| abs_of(conn.wrap_index.entry_at_row(top_row)));

        draw_search_ticks(ui, output.inner_rect, conn, row_of_line, n_rows);
    }

    fn show_merged_rows(&mut self, ui: &mut egui::Ui, menu: &mut MenuAction) {
        let ts_format = self.config.settings.timestamp_format;
        let m = Metrics::new(
            ui,
            self.console_font(),
            ts_format,
            self.config.settings.wrap_lines,
            true,
            self.connections.iter().any(tx_marker),
        );
        let App {
            connections,
            highlight_cache,
            merged,
            merged_wrap,
            merged_generation,
            ..
        } = self;
        let row_height = m.row_height;
        let index_of = |id: PortId| connections.iter().position(|c| c.id == id);
        let entries = merged.len();
        // Entries are in timestamp order, so the first is the zero everything
        // else counts from in the "from mark" format.
        let earliest = merged.first().map(|e| e.micros);
        // Entries are ordered by timestamp, and only ever appended to or thrown
        // away wholesale (which bumps the generation), so the micros of an entry
        // stand in for the monotonic key the index needs.
        merged_wrap.sync(
            m.cols,
            *merged_generation,
            entries,
            |i| merged[i].micros,
            |i| {
                let MergedEntry { port, abs, .. } = merged[i];
                match index_of(port) {
                    Some(ci) => wrap_len(&connections[ci].store, abs),
                    None => 0,
                }
            },
        );
        let n_rows = merged_wrap.total_rows();

        let bg = ui.interact(
            ui.available_rect_before_wrap(),
            ui.id().with("merged_bg"),
            egui::Sense::click(),
        );
        bg.context_menu(|ui| console_menu(ui, menu, None, ts_format, false));
        ui.spacing_mut().item_spacing.y = 0.0;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .drag_to_scroll(false)
            .stick_to_bottom(true)
            .show_viewport(ui, |ui, viewport| {
                ui.set_width(ui.available_width());
                ui.set_height(n_rows as f32 * row_height);
                let (first_entry, rect) = viewport_entries(ui, viewport, merged_wrap, row_height);
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rect), |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let last_row = (viewport.max.y / row_height).ceil() as u64 + 1;
                    let mut entry = first_entry;
                    while entry < entries && merged_wrap.start_row(entry) < last_row {
                        let MergedEntry { port, abs, .. } = merged[entry];
                        let rows = merged_wrap.rows(entry);
                        entry += 1;
                        // A row that can't be resolved still holds its space, or
                        // everything below it shifts up out of place.
                        let Some(ci) = index_of(port) else {
                            ui.add_space(rows as f32 * row_height);
                            continue;
                        };
                        let conn = &connections[ci];
                        let Some(line) = conn.store.get(abs) else {
                            ui.add_space(rows as f32 * row_height);
                            continue;
                        };
                        let color = PORT_PALETTE[ci % PORT_PALETTE.len()];
                        let rctx = RowCtx {
                            ts_format,
                            m: &m,
                            rows,
                            prev_micros: None,
                            mark: earliest,
                            highlight: highlight_cache,
                            search_re: None,
                            is_search_current: false,
                            port_tag: Some((short_tag(&conn.label), color)),
                            row_id: egui::Id::new(("merged_row", port, abs)),
                        };
                        render_row(ui, &line, &rctx);
                    }
                });
            });
    }

    fn show_hex_rows(&mut self, ui: &mut egui::Ui, active: usize, menu: &mut MenuAction) {
        let ts_format = self.config.settings.timestamp_format;
        let has_mark = self.connections[active].mark_micros.is_some();
        let bg = ui.interact(
            ui.available_rect_before_wrap(),
            ui.id().with("hex_bg"),
            egui::Sense::click(),
        );
        bg.context_menu(|ui| console_menu(ui, menu, None, ts_format, has_mark));

        let user_scrolled = ui.input(user_scrolled);
        if self.connections[active].follow && user_scrolled {
            self.connections[active].follow = false;
        }
        let following = self.connections[active].follow;
        // A hex dump is already fixed at 16 bytes a row, so it never wraps.
        let m = Metrics::new(
            ui,
            self.console_font(),
            TimestampFormat::None,
            false,
            false,
            false,
        );
        let font = m.font.clone();
        let conn = &self.connections[active];
        let total_bytes = conn.raw_ring.len();
        let rows = total_bytes.div_ceil(16);
        let row_height = m.row_height;
        ui.spacing_mut().item_spacing.y = 0.0;

        let mut area = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .drag_to_scroll(false);
        if following {
            // Pin to the bottom; see the console's forced-offset pin. Hex rows are
            // full-height monospace labels with zero inter-row spacing, so the
            // content height is exactly `rows * row_height`.
            let view_h = if conn.pin_view_h > 0.0 {
                conn.pin_view_h
            } else {
                ui.available_height()
            };
            let bottom = ((rows as f32) * row_height - view_h).max(0.0);
            area = area.vertical_scroll_offset(bottom);
        }
        let output = area.show_rows(ui, row_height, rows, |ui, row_range| {
            let conn = &self.connections[active];
            ui.set_width(ui.available_width());
            for row in row_range {
                let start = row * 16;
                let mut hex = String::with_capacity(48);
                let mut ascii = String::with_capacity(16);
                for col in 0..16 {
                    match conn.raw_ring.get(start + col) {
                        Some(&b) => {
                            hex.push_str(&format!("{b:02X} "));
                            ascii.push(if (0x20..0x7f).contains(&b) {
                                b as char
                            } else {
                                '.'
                            });
                        }
                        None => hex.push_str("   "),
                    }
                }
                // Full-width interactive row so the right-click menu works
                // over hex content, not just empty margins. The id is keyed on
                // `row` (a stable byte offset) rather than egui's default
                // per-frame allocation-order id, so the row keeps its identity
                // — and thus its open context menu — as the pinned view
                // scrolls a different row into this same screen slot every
                // time new bytes arrive.
                let row_id = ui.id().with(("hex_row", active, row));
                let mut job = LayoutJob::default();
                job.append(
                    &format!("{start:08X}  {hex} |{ascii}|"),
                    0.0,
                    egui::TextFormat {
                        // The console font, whose own row height is what
                        // `row_height` reserves per row — rows must stay exactly
                        // that tall for the pin-to-bottom math below (and
                        // `show_rows`' own virtualization) to line up with what
                        // is actually painted.
                        font_id: font.clone(),
                        color: ui.visuals().strong_text_color(),
                        ..Default::default()
                    },
                );
                let resp = wrapped_text(ui, job, &m, u32::MAX, row_height, row_id);
                resp.context_menu(|ui| console_menu(ui, menu, None, ts_format, has_mark));
            }
        });
        rearm_follow_at_bottom(
            &mut self.connections[active].follow,
            user_scrolled,
            &output,
            row_height,
        );
        self.connections[active].pin_view_h = output.inner_rect.height();
    }

    /// Apply the actions collected from the right-click menu.
    fn apply_menu(&mut self, active: usize, menu: MenuAction) {
        if let Some(fmt) = menu.set_ts {
            self.config.settings.timestamp_format = fmt;
            self.write_config();
        }
        if menu.set_mark && self.config.settings.timestamp_format != TimestampFormat::Mark {
            // A mark only shows in the "from mark" format, so setting one while
            // in any other would look like the command did nothing at all.
            self.config.settings.timestamp_format = TimestampFormat::Mark;
            self.write_config();
        }
        if menu.open_filters {
            self.show_filters_win = !self.show_filters_win;
        }
        if menu.open_highlight {
            self.show_highlight_win = !self.show_highlight_win;
        }
        if menu.open_extract {
            self.show_extract_win = !self.show_extract_win;
        }
        if menu.toggle_search {
            self.show_search = !self.show_search;
            if self.show_search {
                self.search_focus_request = true;
            }
        }
        if let Some(as_csv) = menu.export {
            self.export_active_view(active, as_csv);
        }
        if menu.clear_console {
            self.clear_console();
        }
        if let Some(conn) = self.connections.get_mut(active) {
            if menu.toggle_hex {
                conn.hex_view = !conn.hex_view;
            }
            if menu.toggle_plot {
                conn.show_plot = !conn.show_plot;
            }
            if menu.set_mark {
                // The line the menu was opened over — "here" — falling back to
                // the newest line when it was opened over empty space.
                let target = menu
                    .mark_line
                    .or_else(|| conn.store.next_abs_index().checked_sub(1));
                conn.mark_micros = target
                    .and_then(|i| conn.store.get(i))
                    .map(|l| l.meta.ts.micros);
            }
            if menu.clear_mark {
                conn.mark_micros = None;
            }
            if menu.toggle_dtr {
                conn.dtr = !conn.dtr;
                conn.handle.set_dtr(conn.dtr);
            }
            if menu.toggle_rts {
                conn.rts = !conn.rts;
                conn.handle.set_rts(conn.rts);
            }
            if menu.send_break {
                conn.handle.send_break();
            }
        }
    }

    /// Export the active connection's current (filtered) view to a file.
    pub(crate) fn export_active_view(&mut self, active: usize, as_csv: bool) {
        let Some(conn) = self.connections.get(active) else {
            return;
        };
        let filter_active = conn.filter_index_active();
        let matching = conn.filter_index.matching();
        let first = conn.store.first_abs_index();
        let end = conn.store.next_abs_index();
        let indices: Vec<u64> = if filter_active {
            matching.to_vec()
        } else {
            (first..end).collect()
        };

        let mut out = String::new();
        if as_csv {
            out.push_str("wall_time,micros,flags,text\n");
        }
        for abs in indices {
            let Some(line) = conn.store.get(abs) else {
                continue;
            };
            if as_csv {
                out.push_str(&format!(
                    "{},{},{},{}\n",
                    // Local time like the console shows, but with the UTC offset
                    // appended so the exported column stays unambiguous to
                    // whatever reads it.
                    line.meta
                        .ts
                        .wall
                        .with_timezone(&chrono::Local)
                        .format("%Y-%m-%dT%H:%M:%S%.6f%:z"),
                    line.meta.ts.micros,
                    line.meta.flags.0,
                    csv_escape(line.text),
                ));
            } else {
                out.push_str(&format!("{}  {}\n", wall_clock(line.meta.ts), line.text));
            }
        }

        let ext = if as_csv { "csv" } else { "txt" };
        if let Some(path) = rfd::FileDialog::new()
            .add_filter(ext, &[ext])
            .set_file_name(format!("export.{ext}"))
            .save_file()
        {
            if let Err(e) = std::fs::write(&path, out) {
                self.connections[active].last_error = Some(format!("export failed: {e}"));
            }
        }
    }
}

/// The right-click menu, common to rows and empty areas.
fn console_menu(
    ui: &mut egui::Ui,
    menu: &mut MenuAction,
    line: Option<u64>,
    cur_ts: TimestampFormat,
    has_mark: bool,
) {
    ui.menu_button("Timestamps", |ui| {
        for f in [
            TimestampFormat::Absolute,
            TimestampFormat::Delta,
            TimestampFormat::Mark,
            TimestampFormat::None,
        ] {
            if ui
                .selectable_label(cur_ts == f, ts_format_label(f))
                .clicked()
            {
                menu.set_ts = Some(f);
                ui.close_menu();
            }
        }
    });
    if ui.button("Toggle hex view").clicked() {
        menu.toggle_hex = true;
        ui.close_menu();
    }
    if ui.button("Toggle plot").clicked() {
        menu.toggle_plot = true;
        ui.close_menu();
    }
    ui.separator();
    if ui.button("Search…").clicked() {
        menu.toggle_search = true;
        ui.close_menu();
    }
    if ui.button("Filters…").clicked() {
        menu.open_filters = true;
        ui.close_menu();
    }
    if ui.button("Highlight rules…").clicked() {
        menu.open_highlight = true;
        ui.close_menu();
    }
    if ui.button("Plot extraction…").clicked() {
        menu.open_extract = true;
        ui.close_menu();
    }
    ui.separator();
    ui.menu_button("Control lines", |ui| {
        if ui
            .button("Toggle DTR")
            .on_hover_text("Resets many boards")
            .clicked()
        {
            menu.toggle_dtr = true;
            ui.close_menu();
        }
        if ui.button("Toggle RTS").clicked() {
            menu.toggle_rts = true;
            ui.close_menu();
        }
        if ui.button("Send break").clicked() {
            menu.send_break = true;
            ui.close_menu();
        }
    });
    ui.separator();
    if ui
        .button("Set time mark here")
        .on_hover_text("Timestamps switch to counting from this line, before it and after")
        .clicked()
    {
        menu.set_mark = true;
        menu.mark_line = line;
        ui.close_menu();
    }
    if has_mark && ui.button("Clear time mark").clicked() {
        menu.clear_mark = true;
        ui.close_menu();
    }
    ui.separator();
    ui.menu_button("Export view", |ui| {
        if ui.button("Text (.txt)").clicked() {
            menu.export = Some(false);
            ui.close_menu();
        }
        if ui.button("CSV (.csv)").clicked() {
            menu.export = Some(true);
            ui.close_menu();
        }
    });
    ui.separator();
    if ui
        .button("Clear console")
        .on_hover_text("Discards every line here and in the session capture on disk")
        .clicked()
    {
        menu.clear_console = true;
        ui.close_menu();
    }
}

/// Re-engage `follow` once the user's own scrolling (wheel or scrollbar drag)
/// lands the view at the bottom, so returning to "live" doesn't require
/// reaching for the Pin button — matches how terminals and chat logs behave.
/// Only fires on a frame where the user actually scrolled, so it can't
/// immediately re-pin a deliberate un-pin via the Pin button while already
/// sitting at the bottom, and only when there was real scroll range, so it's
/// inert while all content already fits in view.
fn rearm_follow_at_bottom(
    follow: &mut bool,
    user_scrolled: bool,
    output: &egui::scroll_area::ScrollAreaOutput<()>,
    row_height: f32,
) {
    if *follow || !user_scrolled {
        return;
    }
    let max_offset = (output.content_size.y - output.inner_rect.height()).max(0.0);
    if max_offset > 0.0 && output.state.offset.y >= max_offset - row_height * 0.5 {
        *follow = true;
    }
}

fn prev_micros_for(
    store: &serialcore::store::LineStore,
    filter_active: bool,
    matching: &[u64],
    row: usize,
    abs: u64,
) -> Option<u64> {
    let prev_abs = if filter_active {
        row.checked_sub(1).map(|r| matching[r])
    } else {
        abs.checked_sub(1)
    }?;
    store.get(prev_abs).map(|l| l.meta.ts.micros)
}

/// The trailing local-echo line: the input typed so far plus a block cursor, in
/// a slot sized like every other row's.
fn render_echo_line(ui: &mut egui::Ui, input: &str, m: &Metrics, height: f32, row_id: egui::Id) {
    let (mut cui, _) = row_slot(ui, height, egui::Sense::hover(), row_id);
    cui.spacing_mut().item_spacing.x = 0.0;
    let color = egui::Color32::from_rgb(0x88, 0xbb, 0xff);
    let fmt = egui::TextFormat {
        font_id: m.font.clone(),
        color,
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    job.append(input, 0.0, fmt.clone());
    job.append("▏", 0.0, fmt);
    // In the text column like every other row, so what is being typed lines up
    // under the output above it.
    text_ui(&mut cui, m, row_id, |ui| {
        wrapped_text(ui, job, m, u32::MAX, height, row_id)
    });
}

/// The bytes a line's row count is predicted from: its own, plus room for the
/// caret drawn on a line the device is still writing.
pub fn wrap_len(store: &LineStore, abs: u64) -> u32 {
    match store.get(abs) {
        // A reconnect marker is drawn as a one-row separator whatever it says.
        Some(line) if line.meta.flags.contains(LineFlags::RECONNECT_MARKER) => 0,
        Some(line) => line.meta.len + u32::from(line.meta.cursor.is_some()),
        None => 0,
    }
}

/// The first entry the viewport touches, and the rect its row starts at. Mirrors
/// what `ScrollArea::show_rows` does for uniform rows: place a child `Ui` at the
/// exact y of the first drawn entry, so everything after it stacks into place
/// without the rows above being built at all.
fn viewport_entries(
    ui: &egui::Ui,
    viewport: egui::Rect,
    index: &WrapIndex,
    row_height: f32,
) -> (usize, egui::Rect) {
    let first_row = (viewport.min.y / row_height).floor().max(0.0) as u64;
    let first_entry = index.entry_at_row(first_row);
    let y = ui.max_rect().top() + index.start_row(first_entry) as f32 * row_height;
    let rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), y..=ui.max_rect().bottom());
    (first_entry, rect)
}

/// True when this frame's input is the user moving the view themselves — which
/// unpins follow-tail. Ctrl+wheel is the console's text-size gesture, not a
/// scroll; egui still reports it in `raw_scroll_delta` (it only withholds it
/// from the smoothed delta the `ScrollArea` consumes), so it has to be excluded
/// by hand or zooming would silently drop the tail.
fn user_scrolled(i: &egui::InputState) -> bool {
    let wheel =
        !i.modifiers.command && (i.raw_scroll_delta.y != 0.0 || i.smooth_scroll_delta.y != 0.0);
    wheel || i.pointer.is_decidedly_dragging()
}

/// Wheel notches turned this frame with Ctrl held, as whole steps. egui folds
/// ctrl+wheel into `zoom_delta`, a smoothed multiplier spread over several
/// frames — fine for continuous zoom, but a font size wants discrete points and
/// one config write per notch, so read the events directly. Capped at a step per
/// frame so a trackpad's stream of small deltas doesn't run away.
fn ctrl_wheel_steps(ctx: &egui::Context) -> i32 {
    ctx.input(|i| {
        let sum: f32 = i
            .events
            .iter()
            .filter_map(|e| match e {
                egui::Event::MouseWheel {
                    delta, modifiers, ..
                } if modifiers.command => Some(delta.y),
                _ => None,
            })
            .sum();
        match sum {
            s if s > 0.0 => 1,
            s if s < 0.0 => -1,
            _ => 0,
        }
    })
}

/// Allocate a slot exactly `height` tall spanning the full width and return a
/// left-to-right child `Ui` clipped to it. Reserving the space in the parent up
/// front (rather than letting content decide) guarantees a row is *exactly* as
/// tall as the row index said it would be, so the virtualized viewport and the
/// pin math stay exact and never jitter, regardless of what the row draws.
///
/// The interactive response is keyed on the caller-supplied `row_id` rather
/// than egui's default auto-id (which is assigned purely by allocation order
/// within the frame). The viewport recycles screen positions across frames, so
/// a position-based id would silently jump to whatever line now occupies that
/// slot — see `RowCtx::row_id`.
fn row_slot(
    ui: &mut egui::Ui,
    height: f32,
    sense: egui::Sense,
    row_id: egui::Id,
) -> (egui::Ui, egui::Response) {
    let avail = ui.available_width();
    let (_auto_id, rect) = ui.allocate_space(egui::vec2(avail, height));
    let response = ui.interact(rect, row_id, sense);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(row_id)
            .max_rect(rect)
            // Top-aligned, not centred: a wrapped line is taller than one row,
            // and its first row has to start at the top of the slot for the
            // markers beside it to line up with it.
            .layout(egui::Layout::left_to_right(egui::Align::Min)),
    );
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    (child, response)
}

fn render_row(ui: &mut egui::Ui, line: &LineRef<'_>, rctx: &RowCtx<'_>) -> egui::Response {
    let m = rctx.m;
    let height = rctx.rows as f32 * m.row_height;
    if line.meta.flags.contains(LineFlags::RECONNECT_MARKER) {
        let color = egui::Color32::from_rgb(0xe5, 0xc0, 0x40);
        let (mut cui, response) = row_slot(ui, height, egui::Sense::click(), rctx.row_id);
        cui.add(egui::Separator::default().horizontal());
        cui.label(
            egui::RichText::new(format!("── {} ──", line.text))
                .font(m.font.clone())
                .color(color),
        );
        cui.add(egui::Separator::default().horizontal());
        return response;
    }

    // The line's text is rendered as a full-width *selectable* galley so a
    // click-drag can begin anywhere on the row — including the empty margin past
    // the text — and extend across lines, making the log feel like a read-only
    // editor. The returned response is the text's, so the right-click menu hangs
    // off the line.
    let (mut cui, _slot) = row_slot(ui, height, egui::Sense::hover(), rctx.row_id);
    cui.spacing_mut().item_spacing.x = ROW_GAP;
    if let Some((tag, color)) = &rctx.port_tag {
        cui.label(egui::RichText::new(tag).font(m.font.clone()).color(*color));
    }
    let gutter = format_timestamp(line.meta.ts, rctx.ts_format, rctx.prev_micros, rctx.mark);
    if !gutter.is_empty() {
        cui.label(egui::RichText::new(gutter).font(m.font.clone()).weak());
    }
    if line.meta.flags.contains(LineFlags::TX_ECHO) {
        cui.label(
            egui::RichText::new(">")
                .font(m.font.clone())
                .color(egui::Color32::from_rgb(0x66, 0xaa, 0xff)),
        );
    }
    let job = build_job(&cui, line, rctx);
    text_ui(&mut cui, m, rctx.row_id, |ui| {
        wrapped_text(ui, job, m, rctx.rows, height, rctx.row_id)
    })
}

/// Run `add_text` in a child `Ui` starting at exactly the text column, which is
/// the same x on every row whatever markers preceded it. That is what lets a
/// wrapped row line up under the row above it, and what `Metrics::cols` was
/// measured against.
///
/// Positioned outright rather than reached by padding, because padding in a
/// horizontal layout also picks up the item spacing on either side of itself —
/// which is a marker column's worth of drift, on every row.
fn text_ui<R>(
    ui: &mut egui::Ui,
    m: &Metrics,
    row_id: egui::Id,
    add_text: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let row = ui.max_rect();
    let rect = egui::Rect::from_min_max(
        egui::pos2(row.left() + m.prefix_w, row.top()),
        egui::pos2(row.right().max(row.left() + m.prefix_w), row.bottom()),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(row_id.with("text_col"))
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Min)),
    );
    add_text(&mut child)
}

/// Render `job` as a selectable galley filling the rest of the row's slot, so a
/// text selection can start anywhere on the line — over glyphs or the empty
/// margin past them. Uses egui's cross-widget label selection, so dragging
/// across rows selects a block like a text editor.
///
/// The galley is laid out to exactly the width the row index counted columns
/// for, and capped to the rows it reserved: a line whose predicted height was
/// somehow too small loses its overflow rather than spilling over the line
/// below it.
///
/// Keyed on `row_id` (see `RowCtx::row_id`) rather than an auto-id, so a
/// selection anchored on this line survives the line being redrawn at a
/// different screen position — e.g. everything shifting up as new output
/// arrives while pinned to the bottom.
fn wrapped_text(
    ui: &mut egui::Ui,
    mut job: LayoutJob,
    m: &Metrics,
    max_rows: u32,
    height: f32,
    row_id: egui::Id,
) -> egui::Response {
    let fallback = ui.visuals().text_color();
    let avail = ui.available_width();
    job.wrap.max_width = m.wrap_width();
    // Break mid-word: a console wraps at the column, like the device's own
    // terminal would, not at whatever word happens to straddle the edge.
    job.wrap.break_anywhere = m.cols > 0;
    if m.cols > 0 && max_rows != u32::MAX {
        job.wrap.max_rows = max_rows as usize;
    }
    let galley = ui.fonts(|f| f.layout_job(job));
    let (_auto_id, rect) = ui.allocate_space(egui::vec2(avail, height.max(galley.size().y)));
    let response = ui.interact(rect, row_id.with("text"), egui::Sense::click_and_drag());
    egui::text_selection::LabelSelectionState::label_text_selection(
        ui,
        &response,
        rect.left_top(),
        galley,
        fallback,
        egui::Stroke::NONE,
    );
    response
}

fn build_job(ui: &egui::Ui, line: &LineRef<'_>, rctx: &RowCtx<'_>) -> LayoutJob {
    let text = line.text;
    let font = rctx.m.font.clone();

    // `text_color()` is egui's muted non-interactive label color (gray 140 in
    // dark theme); the console is a terminal, not UI chrome, so it wants the
    // brighter `strong_text_color()` (white/black) as its default foreground.
    let mut base = ui.visuals().strong_text_color();
    if line.meta.flags.contains(LineFlags::TX_ECHO) {
        base = egui::Color32::from_rgb(0x88, 0xbb, 0xff);
    } else if line.meta.flags.contains(LineFlags::INVALID_UTF8) {
        base = egui::Color32::from_rgb(0xcc, 0x99, 0x66);
    } else {
        for hl in rctx.highlight {
            if hl.re.is_match(text) {
                base = hl.color;
                break;
            }
        }
    }

    let mut job = LayoutJob::default();
    let search_ranges: Vec<(usize, usize)> = match rctx.search_re {
        Some(re) => re.find_iter(text).map(|m| (m.start(), m.end())).collect(),
        None => Vec::new(),
    };

    // The live edit cursor (e.g. mid-prompt on the device's own line editor),
    // only meaningful while the line is still open.
    let caret = if line.meta.flags.contains(LineFlags::PROVISIONAL) {
        line.meta
            .cursor
            .map(|c| (c as usize).min(text.len()))
            .filter(|&c| text.is_char_boundary(c))
    } else {
        None
    };

    let mut cuts = vec![0usize, text.len()];
    for s in &line.meta.spans {
        cuts.push(s.start as usize);
        cuts.push((s.start + s.len) as usize);
    }
    for (a, b) in &search_ranges {
        cuts.push(*a);
        cuts.push(*b);
    }
    if let Some(c) = caret {
        cuts.push(c);
    }
    cuts.retain(|&c| c <= text.len() && text.is_char_boundary(c));
    cuts.sort_unstable();
    cuts.dedup();

    // Draws the caret glyph in the line's default foreground — a real
    // terminal's block cursor doesn't tint with whatever color happens to sit
    // under it either.
    let caret_fmt = egui::TextFormat {
        font_id: font.clone(),
        color: base,
        ..Default::default()
    };
    let mut caret_drawn = false;

    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if Some(a) == caret {
            job.append("▏", 0.0, caret_fmt.clone());
            caret_drawn = true;
        }
        if a >= b {
            continue;
        }
        let seg = &text[a..b];
        let mut color = base;
        for s in &line.meta.spans {
            let ss = s.start as usize;
            let se = (s.start + s.len) as usize;
            if a >= ss && b <= se && s.rgb != serialcore::store::ColorSpan::NO_COLOR {
                color =
                    egui::Color32::from_rgb((s.rgb >> 16) as u8, (s.rgb >> 8) as u8, s.rgb as u8);
            }
        }
        let in_search = search_ranges.iter().any(|&(sa, sb)| a >= sa && b <= sb);
        let mut fmt = egui::TextFormat {
            font_id: font.clone(),
            color,
            ..Default::default()
        };
        if in_search {
            fmt.background = if rctx.is_search_current {
                egui::Color32::from_rgb(0xff, 0xd5, 0x4a)
            } else {
                egui::Color32::from_rgb(0x6b, 0x5a, 0x1f)
            };
            if rctx.is_search_current {
                fmt.color = egui::Color32::BLACK;
            }
        }
        job.append(seg, 0.0, fmt);
    }
    // The cursor sitting at the very end of the line (e.g. an empty prompt
    // awaiting input) is never some window's start, since `text.len()` is
    // always `cuts`' last element — draw it here instead.
    if !caret_drawn && caret == Some(text.len()) {
        job.append("▏", 0.0, caret_fmt);
    }
    job
}

fn compile_search(query: &str) -> Option<regex::Regex> {
    if query.is_empty() {
        return None;
    }
    regex::RegexBuilder::new(query)
        .case_insensitive(true)
        .build()
        .or_else(|_| {
            regex::RegexBuilder::new(&regex::escape(query))
                .case_insensitive(true)
                .build()
        })
        .ok()
}

fn draw_search_ticks(
    ui: &egui::Ui,
    rect: egui::Rect,
    conn: &Connection,
    row_of_line: impl Fn(u64) -> Option<u64>,
    total_rows: u64,
) {
    if conn.search_matches.is_empty() || total_rows == 0 {
        return;
    }
    let painter = ui.painter_at(rect);
    let x = rect.right() - 3.0;
    let color = egui::Color32::from_rgb(0xff, 0xd5, 0x4a);
    let step = (conn.search_matches.len() / 2000).max(1);
    for (k, &abs) in conn.search_matches.iter().enumerate() {
        if k % step != 0 {
            continue;
        }
        let Some(row) = row_of_line(abs) else {
            continue;
        };
        let frac = row as f32 / total_rows as f32;
        let y = rect.top() + frac * rect.height();
        painter.line_segment(
            [egui::pos2(x, y), egui::pos2(rect.right(), y)],
            egui::Stroke::new(2.0_f32, color),
        );
    }
}

/// Quote a CSV field if it contains a comma, quote, or newline.
fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn short_tag(label: &str) -> String {
    let base = label.trim_start_matches(['▶', ' ']);
    let base = base.split_whitespace().next().unwrap_or(base);
    let mut t: String = base.chars().take(6).collect();
    while t.chars().count() < 6 {
        t.push(' ');
    }
    format!("[{t}]")
}

fn ts_format_label(f: TimestampFormat) -> &'static str {
    match f {
        TimestampFormat::Absolute => "absolute",
        TimestampFormat::Delta => "delta",
        TimestampFormat::Mark => "from mark",
        TimestampFormat::None => "none",
    }
}

/// Format a line's timestamp per the display setting (spec §7.3).
pub fn format_timestamp(
    ts: Timestamp,
    format: TimestampFormat,
    prev_micros: Option<u64>,
    mark: Option<u64>,
) -> String {
    match format {
        TimestampFormat::None => String::new(),
        TimestampFormat::Absolute => wall_clock(ts),
        TimestampFormat::Delta => match prev_micros {
            Some(p) => format!("+{}", fmt_delta(ts.micros.saturating_sub(p))),
            None => format!(" {}", fmt_delta(0)),
        },
        TimestampFormat::Mark => match mark {
            // Signed: a line above the mark is *before* it. Saturating the
            // subtraction instead would render the whole scrollback above the
            // mark as a column of zeroes.
            Some(m) if ts.micros >= m => format!("+{}", fmt_delta(ts.micros - m)),
            Some(m) => format!("-{}", fmt_delta(m - ts.micros)),
            // Only reachable with an empty console, which has no rows to
            // stamp: callers pass the view's own first line when the user has
            // not marked one.
            None => String::new(),
        },
    }
}

/// A line's wall-clock time as the user's own clock reads it. Stamps are stored
/// in UTC (a clock that can't jump backwards over a DST change); the conversion
/// to local belongs at the point of display, and nowhere else.
fn wall_clock(ts: Timestamp) -> String {
    ts.wall
        .with_timezone(&chrono::Local)
        .format("%H:%M:%S%.3f")
        .to_string()
}

fn fmt_delta(micros: u64) -> String {
    let secs = micros as f64 / 1_000_000.0;
    if secs >= 3600.0 {
        // A device can sit silent overnight. In seconds that reads as a wall of
        // digits, and is wider than the columns the gutter reserves.
        format!("{:.3}h", secs / 3600.0)
    } else if secs >= 1.0 {
        format!("{secs:.3}s")
    } else {
        format!("{:.3}ms", micros as f64 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(micros: u64) -> Timestamp {
        Timestamp {
            wall: chrono::Utc::now(),
            micros,
        }
    }

    #[test]
    fn from_mark_counts_both_directions() {
        let mark = Some(10_000_000);
        // A line after the mark counts forward from it.
        assert_eq!(
            format_timestamp(ts(11_500_000), TimestampFormat::Mark, None, mark),
            "+1.500s"
        );
        // The marked line itself is the zero.
        assert_eq!(
            format_timestamp(ts(10_000_000), TimestampFormat::Mark, None, mark),
            "+0.000ms"
        );
        // And a line *above* the mark counts back from it, rather than
        // saturating to zero along with every other line in the scrollback.
        assert_eq!(
            format_timestamp(ts(9_750_000), TimestampFormat::Mark, None, mark),
            "-250.000ms"
        );
        assert_eq!(
            format_timestamp(ts(0), TimestampFormat::Mark, None, mark),
            "-10.000s"
        );
    }

    /// Callers hand "from mark" the view's own first line when the user has not
    /// marked one, so it never falls back to a wall clock — the format is an
    /// offset or it is nothing.
    #[test]
    fn from_mark_never_renders_a_wall_clock() {
        let from_first_line = format_timestamp(ts(1_000), TimestampFormat::Mark, None, Some(0));
        assert_eq!(from_first_line, "+1.000ms");
        // Only an empty console reaches this, and it has no rows to stamp.
        assert_eq!(
            format_timestamp(ts(1), TimestampFormat::Mark, None, None),
            ""
        );
    }

    #[test]
    fn delta_is_measured_against_the_previous_line() {
        assert_eq!(
            format_timestamp(ts(2_000_000), TimestampFormat::Delta, Some(1_500_000), None),
            "+500.000ms"
        );
        // Nothing above it to measure against.
        assert_eq!(
            format_timestamp(ts(2_000_000), TimestampFormat::Delta, None, None),
            " 0.000ms"
        );
    }

    /// The gutter is reserved a fixed number of columns per format, and text
    /// wraps against what is left; a stamp wider than its reserve would push
    /// into the text column.
    #[test]
    fn every_stamp_fits_the_columns_reserved_for_its_format() {
        // A session running for over a day, marked at the far end of it.
        let long = 100_000 * 1_000_000;
        for (format, reserved) in [
            (TimestampFormat::Absolute, 12),
            (TimestampFormat::Mark, 12),
            (TimestampFormat::Delta, 11),
        ] {
            for (t, prev, mark) in [
                (0, None, None),
                (long, Some(0), Some(0)),
                (0, Some(long), Some(long)),
            ] {
                let out = format_timestamp(ts(t), format, prev, mark);
                assert!(
                    out.chars().count() <= reserved,
                    "{format:?} rendered {out:?}, wider than its {reserved} columns"
                );
            }
        }
    }
}
