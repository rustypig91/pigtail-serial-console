//! The plot pane: live series with min/max decimation, auto-follow, and
//! bidirectional plot↔log selection (spec §7.13).

use crate::app::{App, SeriesEntry};
use egui_plot::{Legend, Line, Plot, PlotBounds, PlotPoints, VLine};

/// Bounds holding every visible series in full, or `None` when there is nothing
/// plotted to fit.
///
/// Both axes are padded, and a trace with no extent of its own — one sample, or
/// a dead-flat line — is given a window anyway: a zero-sized one is not
/// something the plot can draw.
fn fit_bounds(all: &[SeriesEntry]) -> Option<PlotBounds> {
    let mut x: Option<(f64, f64)> = None;
    let mut y: Option<(f64, f64)> = None;
    let merge = |acc: Option<(f64, f64)>, (lo, hi): (f64, f64)| {
        Some(match acc {
            Some((a, b)) => (a.min(lo), b.max(hi)),
            None => (lo, hi),
        })
    };
    for entry in all.iter().filter(|e| e.visible) {
        // Both or neither: a series whose every value is unplottable (all NaN)
        // has a time extent but nothing to show at it.
        if let (Some(t), Some(v)) = (entry.series.t_range(), entry.series.value_range()) {
            x = merge(x, t);
            y = merge(y, v);
        }
    }
    let (mut x0, mut x1) = x?;
    let (mut y0, mut y1) = y?;

    if x1 - x0 < 1e-9 {
        x0 -= 0.5;
        x1 += 0.5;
    }
    // 5% of the spread, or of the value itself when the trace is flat.
    let pad = if y1 - y0 > 0.0 {
        (y1 - y0) * 0.05
    } else {
        y0.abs().max(1.0) * 0.05
    };
    y0 -= pad;
    y1 += pad;
    Some(PlotBounds::from_min_max([x0, y0], [x1, y1]))
}

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
            // Panning or zooming away from the data leaves no way back short of
            // guessing; this finds it again.
            let anything_to_fit = conn
                .series
                .iter()
                .any(|e| e.visible && !e.series.is_empty());
            if ui
                .add_enabled(anything_to_fit, egui::Button::new("Fit"))
                .on_hover_text("Zoom out to show every visible series in full")
                .on_disabled_hover_text("Nothing plotted yet")
                .clicked()
            {
                conn.plot_fit = true;
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

        // Taken now, so a click that lands while the plot is hidden or the tab
        // is elsewhere does not queue up a surprise later.
        let fit = std::mem::take(&mut conn.plot_fit)
            .then(|| fit_bounds(&conn.series))
            .flatten();

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

                // Fit wins over follow for the frame it happens on, and has to
                // be applied *here*: the decimation below reads `x0`/`x1`, so a
                // window set after it would draw this frame's lines against the
                // old one — a plot that looks empty until the next repaint.
                if let Some(b) = fit {
                    pui.set_plot_bounds(b);
                    x0 = b.min()[0];
                    x1 = b.max()[0];
                } else if follow {
                    // Auto-follow: shift the window so it ends at the latest
                    // sample, preserving the current span.
                    let span = (x1 - x0).max(1e-6);
                    x1 = max_t;
                    x0 = x1 - span;
                    pui.set_plot_bounds(PlotBounds::from_min_max(
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
#[cfg(test)]
mod tests {
    use super::*;
    use serialcore::series::Series;

    fn entry(visible: bool, points: &[(f64, f64)]) -> SeriesEntry {
        let mut series = Series::new("s", 100);
        for (i, (t, v)) in points.iter().enumerate() {
            series.push(*t, *v, i as u64);
        }
        SeriesEntry {
            series,
            color: egui::Color32::WHITE,
            visible,
            own_axis: false,
        }
    }

    #[test]
    fn fitting_covers_every_visible_series() {
        let all = [
            entry(true, &[(1.0, 0.0), (3.0, 10.0)]),
            entry(true, &[(2.0, -5.0), (4.0, 4.0)]),
        ];
        let b = fit_bounds(&all).unwrap();
        assert_eq!((b.min()[0], b.max()[0]), (1.0, 4.0));
        // 5% of the 15-unit spread above and below.
        assert_eq!((b.min()[1], b.max()[1]), (-5.75, 10.75));
    }

    /// Fit follows the checkboxes: a series you have hidden is not something you
    /// asked to see.
    #[test]
    fn fitting_ignores_hidden_series() {
        let all = [
            entry(true, &[(1.0, 0.0), (2.0, 1.0)]),
            entry(false, &[(50.0, 900.0)]),
        ];
        let b = fit_bounds(&all).unwrap();
        assert_eq!((b.min()[0], b.max()[0]), (1.0, 2.0));
        assert!(
            b.max()[1] < 2.0,
            "the hidden series did not stretch the view"
        );
    }

    /// A single sample, or a trace that never moves, has no extent of its own —
    /// and a zero-sized window is not something the plot can draw.
    #[test]
    fn a_trace_with_no_extent_still_gets_a_window() {
        let b = fit_bounds(&[entry(true, &[(5.0, 2.0)])]).unwrap();
        assert_eq!((b.min()[0], b.max()[0]), (4.5, 5.5));
        assert!(b.min()[1] < 2.0 && b.max()[1] > 2.0);

        let flat = fit_bounds(&[entry(true, &[(1.0, 0.0), (9.0, 0.0)])]).unwrap();
        assert_eq!((flat.min()[0], flat.max()[0]), (1.0, 9.0));
        assert!(flat.min()[1] < 0.0 && flat.max()[1] > 0.0);
    }

    #[test]
    fn nothing_visible_or_nothing_plottable_is_not_a_fit() {
        assert!(fit_bounds(&[]).is_none());
        assert!(fit_bounds(&[entry(false, &[(1.0, 1.0)])]).is_none());
        assert!(fit_bounds(&[entry(true, &[])]).is_none());
        assert!(
            fit_bounds(&[entry(true, &[(1.0, f64::NAN)])]).is_none(),
            "a series of unplottable values has nothing to fit to"
        );
    }
}
