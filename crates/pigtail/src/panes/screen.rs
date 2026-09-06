//! Fixed-cell VT display. Parsing runs on raw RX even while another view is open.
use crate::app::App;
use egui::{Color32, FontId, Rect, Sense, Stroke, Vec2};

impl App {
    pub(crate) fn show_terminal_controls(&mut self, ui: &mut egui::Ui, active: usize) {
        let conn = &mut self.connections[active];
        ui.horizontal(|ui| {
            ui.label("View:");
            if ui.selectable_label(!conn.screen_view && !conn.hex_view, "Log").clicked() {
                conn.screen_view = false;
                conn.hex_view = false;
            }
            if ui.selectable_label(!conn.screen_view && conn.hex_view, "Hex").clicked() {
                conn.screen_view = false;
                conn.hex_view = true;
            }
            if ui.selectable_label(conn.screen_view, "ANSI/VT").clicked() {
                conn.screen_view = true;
            }
            if conn.screen_view {
                let (mut rows, mut cols) = conn.terminal.screen().size();
                ui.label("Columns");
                let width = ui.add(egui::DragValue::new(&mut cols).range(20..=300));
                ui.label("Rows");
                let height = ui.add(egui::DragValue::new(&mut rows).range(5..=150));
                if width.changed() || height.changed() {
                    conn.terminal.screen_mut().set_size(rows, cols);
                }
                ui.label("Size is local")
                    .on_hover_text("Plain serial has no resize protocol. Configure the same dimensions in your device shell if needed.");
                if ui.button("Reset screen").clicked() {
                    conn.reset_terminal();
                }
            }
        });
    }

    pub(crate) fn show_terminal_screen(&mut self, ui: &mut egui::Ui, active: usize) {
        let font = FontId::monospace(f32::from(self.config.settings.console_font_size));
        let cell_size = ui.fonts(|f| Vec2::new(f.glyph_width(&font, 'M'), f.row_height(&font)));
        let screen = self.connections[active].terminal.screen();
        let (rows, cols) = screen.size();
        egui::ScrollArea::both()
            .id_salt(("vt", active))
            .show(ui, |ui| {
                let (rect, response) = ui.allocate_exact_size(
                    Vec2::new(cell_size.x * f32::from(cols), cell_size.y * f32::from(rows)),
                    Sense::click(),
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
                if !screen.hide_cursor() {
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
