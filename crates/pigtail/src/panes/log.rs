//! The console: a virtualized log filling the whole window between header and
//! footer. Sending is raw — the console forwards the keyboard straight to the
//! device (see `transmit`) — with an optional search bar and a right-click menu
//! that toggles everything else so the main view stays clean.

use crate::app::{App, CompiledHighlight, MergedEntry};
use egui::text::LayoutJob;
use serialcore::clock::Timestamp;
use serialcore::config::TimestampFormat;
use serialcore::store::{LineFlags, LineRef, PortId};

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
    bookmark_line: Option<u64>,
    clear_selection: bool,
    clear_console: bool,
    export: Option<bool>,
    toggle_dtr: bool,
    toggle_rts: bool,
    send_break: bool,
}

struct RowCtx<'a> {
    ts_format: TimestampFormat,
    prev_micros: Option<u64>,
    mark: Option<u64>,
    highlight: &'a [CompiledHighlight],
    search_re: Option<&'a regex::Regex>,
    selected: bool,
    is_search_current: bool,
    port_tag: Option<(String, egui::Color32)>,
    /// Height of the slot `show_rows` reserves for this row. Each row is forced
    /// to exactly this height so the painted content fills its slot; otherwise
    /// rows without a full-height element (e.g. timestamps off, so no gutter)
    /// render shorter than reserved, and pinning to the bottom leaves a gap.
    row_height: f32,
    /// Stable identity for this line, independent of where it lands in the
    /// virtualized viewport. `show_rows` recycles screen positions across
    /// frames — under `follow`, the same slot shows a different abs line every
    /// time new data arrives. egui's default auto-IDs are derived from
    /// per-frame allocation order (position), not content, so without this,
    /// text selection and the right-click menu (both keyed by widget id) would
    /// silently detach or close the instant the line they're anchored to
    /// scrolls to a different position — i.e. on every new line while pinned.
    row_id: egui::Id,
}

impl App {
    pub(crate) fn show_console(&mut self, ctx: &egui::Context) {
        let mut menu = MenuAction::default();
        let mut open_dialog = false;

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

        if std::mem::take(&mut self.pending_bookmark_toggle) {
            self.toggle_bookmark();
        }
        if let Some(dir) = self.pending_bookmark_nav.take() {
            self.goto_bookmark(dir);
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
        let App {
            connections,
            highlight_cache,
            config,
            ..
        } = self;
        let conn = &mut connections[active];
        let ts_format = config.settings.timestamp_format;
        let follow = conn.follow;
        let selected = conn.selected;
        let mark = conn.mark_micros;
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
        let total = if filter_active {
            matching.len()
        } else {
            conn.store.len()
        };
        // With local echo on, the in-progress input line is shown as a trailing
        // row (for devices that don't echo). Otherwise the device's own echo is
        // the only thing on screen.
        let show_echo = conn.port_config.local_echo;
        let n_rows = total + usize::from(show_echo);
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
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
        bg.context_menu(|ui| console_menu(ui, menu, None, ts_format));

        let scroll_offset = conn.scroll_to.take().and_then(|target| {
            let row = if filter_active {
                matching.binary_search(&target).ok()?
            } else {
                target.checked_sub(first_abs)? as usize
            };
            Some((row as f32) * row_height - ui.available_height() * 0.5)
        });

        // The user touching the wheel or dragging the scrollbar unpins.
        let user_scrolled = ui.input(|i| {
            i.raw_scroll_delta.y != 0.0
                || i.smooth_scroll_delta.y != 0.0
                || i.pointer.is_decidedly_dragging()
        });
        if scroll_offset.is_some() {
            // Navigating to a specific line (bookmark/search/plot) unpins so we
            // stay there instead of snapping back to the bottom.
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

        let output = area.show_rows(ui, row_height, n_rows, |ui, row_range| {
            ui.set_width(ui.available_width());
            for row in row_range {
                // The last row (when present) mirrors the in-progress input for
                // local echo.
                if show_echo && row == total {
                    let row_id = ui.id().with(("console_echo", conn.id));
                    render_echo_line(ui, &conn.tx_input, row_height, row_id);
                    continue;
                }
                let abs = if filter_active {
                    matching[row]
                } else {
                    first_abs + row as u64
                };
                let Some(line) = conn.store.get(abs) else {
                    continue;
                };
                let prev_micros = prev_micros_for(&conn.store, filter_active, matching, row, abs);
                let rctx = RowCtx {
                    ts_format,
                    prev_micros,
                    mark,
                    highlight: highlight_cache,
                    search_re: search_re.as_ref(),
                    selected: selected == Some(abs),
                    is_search_current: cur_match_abs == Some(abs),
                    port_tag: None,
                    row_height,
                    row_id: egui::Id::new(("console_row", conn.id, abs)),
                };
                let resp = render_row(ui, &line, &rctx);
                resp.context_menu(|ui| console_menu(ui, menu, Some(abs), ts_format));
            }
        });

        rearm_follow_at_bottom(&mut conn.follow, user_scrolled, &output, row_height);
        conn.pin_view_h = output.inner_rect.height();

        draw_search_ticks(
            ui,
            output.inner_rect,
            conn,
            filter_active,
            matching,
            first_abs,
            total,
        );
    }

    fn show_merged_rows(&mut self, ui: &mut egui::Ui, menu: &mut MenuAction) {
        let App {
            connections,
            highlight_cache,
            config,
            merged,
            ..
        } = self;
        let ts_format = config.settings.timestamp_format;
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
        let index_of = |id: PortId| connections.iter().position(|c| c.id == id);
        let total = merged.len();

        let bg = ui.interact(
            ui.available_rect_before_wrap(),
            ui.id().with("merged_bg"),
            egui::Sense::click(),
        );
        bg.context_menu(|ui| console_menu(ui, menu, None, ts_format));

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .drag_to_scroll(false)
            .stick_to_bottom(true)
            .show_rows(ui, row_height, total, |ui, row_range| {
                ui.set_width(ui.available_width());
                for row in row_range {
                    let MergedEntry { port, abs, .. } = merged[row];
                    let Some(ci) = index_of(port) else { continue };
                    let conn = &connections[ci];
                    let Some(line) = conn.store.get(abs) else {
                        continue;
                    };
                    let color = PORT_PALETTE[ci % PORT_PALETTE.len()];
                    let rctx = RowCtx {
                        ts_format,
                        prev_micros: None,
                        mark: None,
                        highlight: highlight_cache,
                        search_re: None,
                        selected: conn.selected == Some(abs),
                        is_search_current: false,
                        port_tag: Some((short_tag(&conn.label), color)),
                        row_height,
                        row_id: egui::Id::new(("merged_row", port, abs)),
                    };
                    render_row(ui, &line, &rctx);
                }
            });
    }

    fn show_hex_rows(&mut self, ui: &mut egui::Ui, active: usize, menu: &mut MenuAction) {
        let ts_format = self.config.settings.timestamp_format;
        let bg = ui.interact(
            ui.available_rect_before_wrap(),
            ui.id().with("hex_bg"),
            egui::Sense::click(),
        );
        bg.context_menu(|ui| console_menu(ui, menu, None, ts_format));

        let user_scrolled = ui.input(|i| {
            i.raw_scroll_delta.y != 0.0
                || i.smooth_scroll_delta.y != 0.0
                || i.pointer.is_decidedly_dragging()
        });
        if self.connections[active].follow && user_scrolled {
            self.connections[active].follow = false;
        }
        let following = self.connections[active].follow;
        let conn = &self.connections[active];
        let total_bytes = conn.raw_ring.len();
        let rows = total_bytes.div_ceil(16);
        let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
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
                        // Same 0.82 scale-down as the console's `build_job`: it keeps
                        // the laid-out galley's height within `row_height` (a raw
                        // font size equal to `row_height` renders *taller* than
                        // that, once line metrics/leading are added). Rows must
                        // stay exactly `row_height` tall for the pin-to-bottom math
                        // below (and `show_rows`' own virtualization) to line up
                        // with what's actually painted.
                        font_id: egui::FontId::monospace(
                            ui.text_style_height(&egui::TextStyle::Monospace) * 0.82,
                        ),
                        color: ui.visuals().strong_text_color(),
                        ..Default::default()
                    },
                );
                let resp = selectable_text(ui, job, row_height, row_id);
                resp.context_menu(|ui| console_menu(ui, menu, None, ts_format));
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
        if let Some(line) = menu.bookmark_line {
            if let Some(conn) = self.connections.get_mut(active) {
                conn.selected = Some(line);
            }
            self.toggle_bookmark();
        }
        if let Some(conn) = self.connections.get_mut(active) {
            if menu.toggle_hex {
                conn.hex_view = !conn.hex_view;
            }
            if menu.toggle_plot {
                conn.show_plot = !conn.show_plot;
            }
            if menu.set_mark {
                let last = conn.store.next_abs_index().checked_sub(1);
                conn.mark_micros = last
                    .and_then(|i| conn.store.get(i))
                    .map(|l| l.meta.ts.micros);
            }
            if menu.clear_selection {
                conn.selected = None;
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
    if let Some(abs) = line {
        if ui.button("Toggle bookmark on this line").clicked() {
            menu.bookmark_line = Some(abs);
            ui.close_menu();
        }
    }
    if ui.button("Set time mark here").clicked() {
        menu.set_mark = true;
        ui.close_menu();
    }
    if ui.button("Clear selection").clicked() {
        menu.clear_selection = true;
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
/// a fixed `row_height` slot so it matches every other row exactly.
fn render_echo_line(ui: &mut egui::Ui, input: &str, row_height: f32, row_id: egui::Id) {
    let (mut cui, _) = row_slot(ui, row_height, egui::Sense::hover(), row_id);
    cui.spacing_mut().item_spacing.x = 0.0;
    let color = egui::Color32::from_rgb(0x88, 0xbb, 0xff);
    cui.monospace(egui::RichText::new(input).color(color));
    cui.monospace(egui::RichText::new("▏").color(color));
}

/// Allocate exactly one `row_height`-tall slot spanning the full width and
/// return a left-to-right child `Ui` clipped to it. Reserving the space in the
/// parent up front (rather than letting content decide) guarantees every row is
/// *exactly* `row_height`, so `show_rows`' fixed-height virtualization and the
/// pin math stay exact and never jitter, regardless of what the row draws.
///
/// The interactive response is keyed on the caller-supplied `row_id` rather
/// than egui's default auto-id (which is assigned purely by allocation order
/// within the frame). `show_rows` recycles screen positions across frames, so
/// a position-based id would silently jump to whatever line now occupies that
/// slot — see `RowCtx::row_id`.
fn row_slot(
    ui: &mut egui::Ui,
    row_height: f32,
    sense: egui::Sense,
    row_id: egui::Id,
) -> (egui::Ui, egui::Response) {
    let avail = ui.available_width();
    let (_auto_id, rect) = ui.allocate_space(egui::vec2(avail, row_height));
    let response = ui.interact(rect, row_id, sense);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(row_id)
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    (child, response)
}

fn render_row(ui: &mut egui::Ui, line: &LineRef<'_>, rctx: &RowCtx<'_>) -> egui::Response {
    if line.meta.flags.contains(LineFlags::RECONNECT_MARKER) {
        let color = egui::Color32::from_rgb(0xe5, 0xc0, 0x40);
        let (mut cui, response) = row_slot(ui, rctx.row_height, egui::Sense::click(), rctx.row_id);
        cui.add(egui::Separator::default().horizontal());
        cui.label(
            egui::RichText::new(format!("── {} ──", line.text))
                .monospace()
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
    let (mut cui, _slot) = row_slot(ui, rctx.row_height, egui::Sense::hover(), rctx.row_id);
    if rctx.selected {
        let fill = cui.visuals().selection.bg_fill.linear_multiply(0.5);
        cui.painter().rect_filled(cui.max_rect(), 0.0, fill);
    }
    cui.spacing_mut().item_spacing.x = 6.0;
    if line.meta.flags.contains(LineFlags::BOOKMARK) {
        cui.label(egui::RichText::new("🔖").small());
    }
    if let Some((tag, color)) = &rctx.port_tag {
        cui.label(egui::RichText::new(tag).monospace().color(*color));
    }
    let gutter = format_timestamp(line.meta.ts, rctx.ts_format, rctx.prev_micros, rctx.mark);
    if !gutter.is_empty() {
        cui.monospace(egui::RichText::new(gutter).weak());
    }
    if line.meta.flags.contains(LineFlags::TX_ECHO) {
        cui.monospace(egui::RichText::new(">").color(egui::Color32::from_rgb(0x66, 0xaa, 0xff)));
    }
    let job = build_job(&cui, line, rctx);
    selectable_text(&mut cui, job, rctx.row_height, rctx.row_id)
}

/// Render `job` as a selectable galley that spans the full remaining row width
/// and at least `row_height`, so a text selection can start anywhere on the line
/// — over glyphs or the empty margin past them. Uses egui's cross-widget label
/// selection, so dragging across rows selects a block like a text editor.
///
/// Keyed on `row_id` (see `RowCtx::row_id`) rather than an auto-id, so a
/// selection anchored on this line survives the line being redrawn at a
/// different screen position — e.g. everything shifting up as new output
/// arrives while pinned to the bottom.
fn selectable_text(
    ui: &mut egui::Ui,
    mut job: LayoutJob,
    row_height: f32,
    row_id: egui::Id,
) -> egui::Response {
    let fallback = ui.visuals().text_color();
    let avail = ui.available_width();
    // Never wrap: every line is exactly one row tall so `show_rows`' fixed-height
    // virtualization (and the pin math that feeds off its measurements) stays
    // stable. Long lines extend past the edge and are clipped, as before.
    job.wrap.max_width = f32::INFINITY;
    let galley = ui.fonts(|f| f.layout_job(job));
    let height = galley.size().y.max(row_height);
    let (_auto_id, rect) = ui.allocate_space(egui::vec2(avail, height));
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
    let font = egui::FontId::monospace(ui.text_style_height(&egui::TextStyle::Monospace) * 0.82);

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
    conn: &crate::app::Connection,
    filter_active: bool,
    matching: &[u64],
    first_abs: u64,
    total: usize,
) {
    if conn.search_matches.is_empty() || total == 0 {
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
        let row = if filter_active {
            match matching.binary_search(&abs) {
                Ok(r) => r,
                Err(_) => continue,
            }
        } else {
            (abs.saturating_sub(first_abs)) as usize
        };
        let frac = row as f32 / total as f32;
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
            Some(m) => format!("@{}", fmt_delta(ts.micros.saturating_sub(m))),
            None => wall_clock(ts),
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
    if secs >= 1.0 {
        format!("{secs:.3}s")
    } else {
        format!("{:.3}ms", micros as f64 / 1000.0)
    }
}
