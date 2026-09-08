//! Fixed-cell VT display. Parsing runs on raw RX even while another view is open.
use crate::app::App;
use egui::{Color32, FontId, Rect, Sense, Stroke, Vec2};

#[derive(Default)]
pub(crate) struct ScreenSearch {
    pub matches: Vec<(usize, std::ops::Range<u16>)>,
    pub position: Option<usize>,
    pub dirty: bool,
    pub scroll_to: Option<usize>,
    query: String,
    case_sensitive: bool,
}

impl ScreenSearch {
    pub fn refresh(&mut self, screen: &vt100::Screen, query: &str, case_sensitive: bool) {
        let changed = self.query != query || self.case_sensitive != case_sensitive;
        if !changed && !self.dirty {
            return;
        }
        self.dirty = false;
        let selected = self.position.and_then(|i| self.matches.get(i)).cloned();
        self.query = query.to_owned();
        self.case_sensitive = case_sensitive;
        self.matches.clear();
        if let Some(re) = crate::app::compile_search(query, case_sensitive) {
            let mut screen = screen.clone();
            let (rows, cols) = screen.size();
            screen.set_scrollback(usize::MAX);
            let history = screen.scrollback();
            let total = history + usize::from(rows);
            for line in 0..total {
                let base = (line / usize::from(rows) * usize::from(rows)).min(history);
                screen.set_scrollback(history - base);
                let row = (line - base) as u16;
                let mut text = String::new();
                let mut cells = Vec::new();
                for col in 0..cols {
                    let Some(cell) = screen.cell(row, col) else {
                        continue;
                    };
                    if cell.is_wide_continuation() {
                        continue;
                    }
                    let start = text.len();
                    text.push_str(if cell.has_contents() {
                        cell.contents()
                    } else {
                        " "
                    });
                    cells.push((
                        start..text.len(),
                        col..(col + if cell.is_wide() { 2 } else { 1 }).min(cols),
                    ));
                }
                for found in re.find_iter(&text) {
                    if found.is_empty() {
                        continue;
                    }
                    let mut matching = cells.iter().filter(|(bytes, _)| {
                        bytes.start < found.end() && bytes.end > found.start()
                    });
                    if let Some((_, first)) = matching.next() {
                        let end = matching.next_back().map_or(first.end, |(_, cell)| cell.end);
                        self.matches.push((line, first.start..end));
                    }
                }
            }
        }
        self.position = if changed {
            None
        } else {
            selected.and_then(|selected| self.matches.iter().position(|m| *m == selected))
        };
    }

    pub fn step(&mut self, dir: i64) {
        if self.matches.is_empty() {
            self.position = None;
            return;
        }
        let len = self.matches.len() as i64;
        self.position = Some(
            self.position
                .map_or(if dir < 0 { len - 1 } else { 0 }, |pos| {
                    (pos as i64 + dir).rem_euclid(len)
                }) as usize,
        );
        self.scroll_to = self.position.map(|i| self.matches[i].0);
    }
}

impl App {
    pub(crate) fn show_terminal_screen(&mut self, ui: &mut egui::Ui, active: usize) {
        let pages = self.consume_page_scroll(ui.ctx());
        let font = FontId::monospace(f32::from(self.config.settings.console_font_size));
        let cell_size = ui.fonts(|f| Vec2::new(f.glyph_width(&font, 'M'), f.row_height(&font)));
        let available = ui.available_size();
        let rows = (available.y / cell_size.y).floor().max(1.0) as u16;
        let cols = (available.x / cell_size.x).floor().max(1.0) as u16;
        let conn = &mut self.connections[active];
        if conn.terminal.screen().size() != (rows, cols) {
            conn.terminal.screen_mut().set_size(rows, cols);
            conn.screen_search.dirty = true;
        }
        let screen = conn.terminal.screen_mut();
        let old_offset = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let history = screen.scrollback();
        screen.set_scrollback(old_offset);
        conn.screen_search
            .refresh(screen, &conn.search_query, conn.search_case_sensitive);
        let requested = conn.screen_search.scroll_to.take();
        if requested.is_some() {
            conn.follow = false;
        }
        let top = if let Some(line) = requested {
            line.min(history)
        } else if conn.follow {
            history
        } else {
            history.saturating_sub(old_offset)
        };
        let top = (top as f32 + pages * f32::from(rows)).clamp(0.0, history as f32);
        let output = egui::ScrollArea::vertical()
            .id_salt(("vt-scrollback", conn.id.0))
            .auto_shrink([false, false])
            .vertical_scroll_offset(top * cell_size.y)
            .show_viewport(ui, |ui, viewport| {
                let first = ((viewport.min.y / cell_size.y).round() as usize).min(history);
                screen.set_scrollback(history - first);
                let search = &conn.screen_search;
                let (content, response) = ui.allocate_exact_size(
                    Vec2::new(
                        ui.available_width(),
                        available.y + history as f32 * cell_size.y,
                    ),
                    Sense::click(),
                );
                let rect = Rect::from_min_size(
                    content.min + Vec2::new(0.0, viewport.min.y),
                    Vec2::new(content.width(), available.y),
                );
                if response.clicked() {
                    ui.memory_mut(|m| {
                        if let Some(id) = m.focused() {
                            m.surrender_focus(id);
                        }
                    });
                }
                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, 0.0, Color32::BLACK);
                for row in 0..rows {
                    let line = first + usize::from(row);
                    let start = search.matches.partition_point(|(r, _)| *r < line);
                    let end = search.matches.partition_point(|(r, _)| *r <= line);
                    for col in 0..cols {
                        let Some(cell) = screen.cell(row, col) else {
                            continue;
                        };
                        if cell.is_wide_continuation() {
                            continue;
                        }
                        let pos = rect.min
                            + Vec2::new(f32::from(col) * cell_size.x, f32::from(row) * cell_size.y);
                        let size = Vec2::new(
                            cell_size.x * if cell.is_wide() { 2.0 } else { 1.0 },
                            cell_size.y,
                        );
                        let cell_rect = Rect::from_min_size(pos, size);
                        if !ui.is_rect_visible(cell_rect) {
                            continue;
                        }
                        let mut fg = color(cell.fgcolor(), Color32::LIGHT_GRAY);
                        let mut bg = color(cell.bgcolor(), Color32::BLACK);
                        if cell.inverse() {
                            std::mem::swap(&mut fg, &mut bg);
                        }
                        if cell.dim() {
                            fg = fg.gamma_multiply(0.5);
                        }
                        if let Some((index, _)) = search.matches[start..end]
                            .iter()
                            .enumerate()
                            .find(|(_, (_, range))| range.contains(&col))
                        {
                            bg = if search.position == Some(start + index) {
                                Color32::from_rgb(255, 150, 40)
                            } else {
                                Color32::from_rgb(230, 210, 80)
                            };
                            fg = Color32::BLACK;
                        }
                        painter.rect_filled(cell_rect, 0.0, bg);
                        let mut format = egui::TextFormat {
                            font_id: font.clone(),
                            color: fg,
                            italics: cell.italic(),
                            ..Default::default()
                        };
                        if cell.underline() {
                            format.underline = Stroke::new(1.0_f32, fg);
                        }
                        let job = egui::text::LayoutJob::single_section(
                            cell.contents().to_owned(),
                            format,
                        );
                        let galley = ui.fonts(|f| f.layout_job(job));
                        painter.galley(pos, galley.clone(), fg);
                        if cell.bold() {
                            painter.galley(pos + Vec2::new(0.5, 0.0), galley, fg);
                        }
                    }
                }
                if screen.scrollback() == 0 && !screen.hide_cursor() {
                    let (row, col) = screen.cursor_position();
                    let pos = rect.min
                        + Vec2::new(
                            f32::from(col) * cell_size.x,
                            (f32::from(row) + 1.0) * cell_size.y - 2.0,
                        );
                    painter.line_segment(
                        [pos, pos + Vec2::new(cell_size.x, 0.0)],
                        Stroke::new(2.0_f32, Color32::WHITE),
                    );
                }
            });
        conn.follow = output.state.offset.y >= history as f32 * cell_size.y - 1.0;
        let first = ((output.state.offset.y / cell_size.y).round() as usize).min(history);
        conn.terminal.screen_mut().set_scrollback(history - first);
        if conn.follow {
            conn.new_since_scroll = 0;
        }
    }
}

fn color(color: vt100::Color, default: Color32) -> Color32 {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        vt100::Color::Idx(i) => {
            const ANSI: [[u8; 3]; 16] = [
                [0, 0, 0],
                [170, 0, 0],
                [0, 170, 0],
                [170, 85, 0],
                [0, 0, 170],
                [170, 0, 170],
                [0, 170, 170],
                [170, 170, 170],
                [85, 85, 85],
                [255, 85, 85],
                [85, 255, 85],
                [255, 255, 85],
                [85, 85, 255],
                [255, 85, 255],
                [85, 255, 255],
                [255, 255, 255],
            ];
            let [r, g, b] = if i < 16 {
                ANSI[usize::from(i)]
            } else if i < 232 {
                let n = i - 16;
                let level = |v| if v == 0 { 0 } else { 55 + 40 * v };
                [level(n / 36), level(n / 6 % 6), level(n % 6)]
            } else {
                [8 + (i - 232) * 10; 3]
            };
            Color32::from_rgb(r, g, b)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{inert_handle, test_app};
    use serialcore::config::DEFAULT_VT_SCROLLBACK_ROWS as VT_SCROLLBACK_ROWS;
    use serialcore::store::PortId;

    #[test]
    fn scrollback_setting_applies_to_new_connections_and_live_changes() {
        let (mut app, _enum_tx) = test_app("vt-setting");
        app.config.settings.vt_scrollback_rows = 20;
        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            Default::default(),
            Default::default(),
            inert_handle(id),
        );
        conn.terminal.screen_mut().set_size(5, 30);
        for i in 0..100 {
            conn.push_raw_bytes(format!("line {i}\r\n").as_bytes());
        }
        conn.terminal.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(conn.terminal.screen().scrollback(), 20);
        conn.terminal.screen_mut().set_scrollback(0);
        let live = conn.terminal.screen().contents();
        let raw_len = conn.raw_ring.len();
        for rows in [5, 50, 0] {
            conn.set_vt_scrollback_rows(rows);
            assert_eq!(conn.terminal.screen().contents(), live);
            conn.terminal.screen_mut().set_scrollback(usize::MAX);
            assert_eq!(conn.terminal.screen().scrollback(), rows);
            conn.terminal.screen_mut().set_scrollback(0);
            assert_eq!(conn.raw_ring.len(), raw_len);
        }
        conn.reset_terminal();
        conn.push_raw_bytes(b"1\r\n2\r\n3\r\n4\r\n5\r\n6");
        conn.terminal.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(conn.terminal.screen().scrollback(), 0);
    }

    #[test]
    fn vt_scrollback_is_bounded_searchable_and_survives_new_output_and_gaps() {
        let (mut app, _enum_tx) = test_app("vt-scrollback");
        let id = PortId(0);
        let conn = app.make_connection(
            id,
            "probe".into(),
            Default::default(),
            Default::default(),
            inert_handle(id),
        );
        app.connections.push(conn);
        let conn = &mut app.connections[0];
        conn.terminal.screen_mut().set_size(5, 30);
        for i in 0..VT_SCROLLBACK_ROWS + 50 {
            conn.push_raw_bytes(format!("line {i}\r\n").as_bytes());
        }
        conn.terminal.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(conn.terminal.screen().scrollback(), VT_SCROLLBACK_ROWS);
        conn.terminal.screen_mut().set_scrollback(100);
        let before = conn.terminal.screen().contents();
        conn.push_raw_bytes(b"more\r\n");
        assert_eq!(conn.terminal.screen().scrollback(), 101);
        assert_eq!(conn.terminal.screen().contents(), before);
        conn.mark_raw_discontinuity();
        assert_eq!(conn.terminal.screen().contents(), before);
        conn.screen_search
            .refresh(conn.terminal.screen(), r"line 100\b", true);
        assert_eq!(conn.screen_search.matches.len(), 1);
        conn.screen_search.step(1);
        assert!(conn.screen_search.scroll_to.is_some());
        conn.push_raw_bytes(b"\x1b[?1049hmenu");
        conn.terminal.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(conn.terminal.screen().scrollback(), 0);
        conn.push_raw_bytes(b"\x1b[?1049l");
        conn.terminal.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(conn.terminal.screen().scrollback(), VT_SCROLLBACK_ROWS);
        conn.reset_terminal();
        conn.terminal.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(conn.terminal.screen().scrollback(), 0);
    }

    #[test]
    fn wheel_scrolls_history_and_pin_returns_to_live_output() {
        let (mut app, _enum_tx) = test_app("vt-wheel");
        let id = PortId(0);
        let conn = app.make_connection(
            id,
            "probe".into(),
            Default::default(),
            Default::default(),
            inert_handle(id),
        );
        app.connections.push(conn);
        let ctx = egui::Context::default();
        let draw = |app: &mut App, events| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(500.0, 250.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.show_terminal_screen(ui, 0));
                },
            );
        };
        draw(&mut app, vec![]);
        for i in 0..100 {
            app.connections[0].push_raw_bytes(format!("line {i}\r\n").as_bytes());
        }
        draw(&mut app, vec![]);
        for key in [egui::Key::PageUp, egui::Key::PageDown] {
            draw(
                &mut app,
                vec![egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                }],
            );
            if key == egui::Key::PageUp {
                assert!(!app.connections[0].follow);
                assert_eq!(
                    app.connections[0].terminal.screen().scrollback(),
                    usize::from(app.connections[0].terminal.screen().size().0),
                );
            } else {
                assert!(app.connections[0].follow);
                assert_eq!(app.connections[0].terminal.screen().scrollback(), 0);
            }
        }
        draw(
            &mut app,
            vec![
                egui::Event::PointerMoved(egui::pos2(100.0, 100.0)),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: Vec2::new(0.0, 100.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(!app.connections[0].follow);
        assert!(app.connections[0].terminal.screen().scrollback() > 0);
        app.connections[0].follow = true;
        // Pin is in the footer, outside the scroll area's hover region.
        draw(
            &mut app,
            vec![egui::Event::PointerMoved(egui::pos2(100.0, 300.0))],
        );
        assert_eq!(app.connections[0].terminal.screen().scrollback(), 0);
        app.connections[0].screen_view = true;
        app.connections[0].search_query = r"line 0\b".into();
        app.search_step(1);
        draw(&mut app, vec![]);
        assert!(app.connections[0].terminal.screen().scrollback() > 0);
        assert!(app.connections[0]
            .terminal
            .screen()
            .contents()
            .contains("line 0"));
    }

    #[test]
    fn screen_search_maps_unicode_cells_and_tracks_redraws() {
        let mut terminal = vt100::Parser::new(5, 30, 0);
        terminal.process("界e\u{301} Error error".as_bytes());
        let mut search = ScreenSearch::default();
        search.refresh(terminal.screen(), "e\u{301}", true);
        assert_eq!(search.matches, vec![(0, 2..3)]);
        search.refresh(terminal.screen(), "界", true);
        assert_eq!(search.matches, vec![(0, 0..2)]);
        search.refresh(terminal.screen(), "error", false);
        assert_eq!(search.matches, vec![(0, 4..9), (0, 10..15)]);
        search.step(1);
        assert_eq!(search.position, Some(0));
        search.step(-1);
        assert_eq!(search.position, Some(1));
        search.refresh(terminal.screen(), "error", false);
        assert_eq!(search.position, Some(1));
        terminal.process(b"\r\x1b[2Kdone");
        search.dirty = true;
        search.refresh(terminal.screen(), "error", false);
        assert!(search.matches.is_empty());
        assert_eq!(search.position, None);
    }

    #[test]
    fn screen_search_honors_case_regex_and_active_buffer() {
        let mut terminal = vt100::Parser::new(5, 30, 0);
        terminal.process(b"Error error [\x1b[31mred");
        let mut search = ScreenSearch::default();
        search.refresh(terminal.screen(), "Error|red", true);
        assert_eq!(search.matches.len(), 2);
        search.refresh(terminal.screen(), "[", true);
        assert_eq!(search.matches, vec![(0, 12..13)]);
        search.refresh(terminal.screen(), "^", true);
        assert!(search.matches.is_empty());
        terminal.process(b"\x1b[?1049hmenu");
        search.refresh(terminal.screen(), "Error", false);
        assert!(search.matches.is_empty());
        terminal.process(b"\x1b[?1049l");
        search.dirty = true;
        search.refresh(terminal.screen(), "Error", false);
        assert_eq!(search.matches.len(), 2);
        search.refresh(terminal.screen(), "", false);
        assert!(search.matches.is_empty());
    }

    #[test]
    fn screen_fills_viewport_and_resizes_without_losing_contents() {
        let (mut app, _enum_tx) = test_app("vt-viewport");
        let id = PortId(0);
        let mut conn = app.make_connection(
            id,
            "probe".into(),
            Default::default(),
            Default::default(),
            inert_handle(id),
        );
        conn.push_raw_bytes(b"hello");
        app.connections.push(conn);
        let ctx = egui::Context::default();
        for size in [
            Vec2::new(800.0, 600.0),
            Vec2::new(400.0, 300.0),
            Vec2::new(1000.0, 700.0),
        ] {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, size)),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let area = ui.available_size();
                        let font =
                            FontId::monospace(f32::from(app.config.settings.console_font_size));
                        let cell =
                            ui.fonts(|f| Vec2::new(f.glyph_width(&font, 'M'), f.row_height(&font)));
                        app.show_terminal_screen(ui, 0);
                        assert_eq!(
                            app.connections[0].terminal.screen().size(),
                            (
                                (area.y / cell.y).floor() as u16,
                                (area.x / cell.x).floor() as u16
                            )
                        );
                        assert!((ui.min_rect().size() - area).length() < 1.0);
                    });
                },
            );
            assert_eq!(app.connections[0].terminal.screen().contents(), "hello");
        }
    }
}
