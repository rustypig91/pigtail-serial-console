//! The plot pane: live series with min/max decimation, auto-follow, and
//! bidirectional plot↔log selection (spec §7.13).

use crate::app::App;
use egui_plot::{Legend, Line, Plot, PlotPoints, VLine};

impl App {
    pub(crate) fn show_plot(&mut self, ctx: &egui::Context) {
        if self.merged_selected || self.connections.is_empty() {
            return;
        }
        let active = self.active.min(self.connections.len() - 1);
        // The plot is off by default; enable it from the console right-click menu.
        if !self.connections[active].show_plot {
            return;
        }

        egui::TopBottomPanel::bottom("plot")
            .resizable(true)
            .default_height(220.0)
            .show(ctx, |ui| {
                self.show_plot_header(ui, active);
                self.show_plot_body(ui, active);
            });
    }

    fn show_plot_header(&mut self, ui: &mut egui::Ui, active: usize) {
        ui.horizontal_wrapped(|ui| {
            let conn = &mut self.connections[active];
            if ui.button("Hide").on_hover_text("Hide plot").clicked() {
                conn.show_plot = false;
            }
            ui.separator();
            if conn.series.is_empty() {
                ui.weak(
                    "No series yet — add a plot-extraction rule (right-click > Plot extraction…).",
                );
            }
            let follow = conn.plot_follow;
            if ui
                .selectable_label(follow, "Follow")
                .on_hover_text("Pin the view to the latest data")
                .clicked()
            {
                conn.plot_follow = !follow;
            }
            ui.separator();
            // Per-series visibility + colour + own-axis toggles.
            for entry in &mut conn.series {
                ui.checkbox(&mut entry.visible, "");
                ui.colored_label(entry.color, "■");
                ui.label(entry.series.name());
                ui.checkbox(&mut entry.own_axis, "Y2")
                    .on_hover_text("Put this series on a separate right axis");
                ui.separator();
            }
        });
        for err in &self.connections[active].extract_errors {
            ui.colored_label(egui::Color32::from_rgb(0xff, 0x88, 0x55), err);
        }
    }

    fn show_plot_body(&mut self, ui: &mut egui::Ui, active: usize) {
        let conn = &mut self.connections[active];

        // Latest time across series, for auto-follow.
        let max_t = conn
            .series
            .iter()
            .filter_map(|e| e.series.t_range().map(|(_, b)| b))
            .fold(f64::MIN, f64::max);
        let follow = conn.plot_follow && max_t > f64::MIN;

        // Selected line's time, for the log→plot marker.
        let selected_t = conn
            .selected
            .and_then(|abs| conn.store.get(abs))
            .map(|l| l.meta.ts.micros as f64 / 1_000_000.0);

        let mut clicked_t: Option<f64> = None;
        let mut dragged = false;

        let response = Plot::new(egui::Id::new(("plot", conn.id.0)))
            .legend(Legend::default())
            .allow_scroll(true)
            .x_axis_label("t (s)")
            .show(ui, |pui| {
                let bounds = pui.plot_bounds();
                let (mut x0, mut x1) = (bounds.min()[0], bounds.max()[0]);
                let width = (pui.response().rect.width() as usize).clamp(64, 4096);

                // Auto-follow: shift the window so it ends at the latest sample,
                // preserving the current span.
                if follow {
                    let span = (x1 - x0).max(1e-6);
                    x1 = max_t;
                    x0 = x1 - span;
                    pui.set_plot_bounds(egui_plot::PlotBounds::from_min_max(
                        [x0, bounds.min()[1]],
                        [x1, bounds.max()[1]],
                    ));
                }

                for entry in &conn.series {
                    if !entry.visible {
                        continue;
                    }
                    let pts = entry.series.decimate(x0, x1, width);
                    let line = Line::new(PlotPoints::from(pts))
                        .color(entry.color)
                        .name(entry.series.name());
                    pui.line(line);
                }

                if let Some(t) = selected_t {
                    pui.vline(
                        VLine::new(t)
                            .color(egui::Color32::from_gray(180))
                            .width(1.0_f32),
                    );
                }

                if pui.response().clicked() {
                    if let Some(coord) = pui.pointer_coordinate() {
                        clicked_t = Some(coord.x);
                    }
                }
                dragged = pui.response().dragged();
            });

        let _ = response;

        // Pan disengages follow (spec §7.13).
        if dragged && conn.plot_follow {
            conn.plot_follow = false;
        }

        // Plot→log: clicking picks the nearest sample and scrolls the log to it.
        if let Some(t) = clicked_t {
            let mut best: Option<(f64, u64)> = None;
            for entry in &conn.series {
                if !entry.visible {
                    continue;
                }
                if let Some(p) = entry.series.nearest_point(t) {
                    let d = (p.t - t).abs();
                    if best.map(|(bd, _)| d < bd).unwrap_or(true) {
                        best = Some((d, p.line));
                    }
                }
            }
            if let Some((_, line)) = best {
                conn.selected = Some(line);
                conn.scroll_to = Some(line);
            }
        }
    }
}
