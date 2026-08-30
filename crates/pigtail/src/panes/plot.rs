//! The plot pane: live series with min/max decimation, auto-follow, and
//! bidirectional plot↔log selection (spec §7.13).

use crate::app::{App, SeriesEntry};
use egui_plot::{AxisHints, HPlacement, Legend, Line, Plot, PlotBounds, PlotPoints, VLine};
use std::collections::HashSet;

/// How a "Y2" series' own units are laid over the plot's single coordinate
/// space.
///
/// `egui_plot` draws as many axis rulers as you ask for, but every line it
/// draws shares one set of coordinates — there is no second data space to plot
/// into. So a series on its own axis is *mapped* into the primary space before
/// being drawn, and the right-hand ruler's tick labels are run back through the
/// inverse, which is what makes them read in the series' real units.
///
/// The map is derived from the two groups' full data extents rather than from
/// the current view, so panning and zooming move both traces together instead
/// of one sliding under the other.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AxisMap {
    scale: f64,
    offset: f64,
}

impl AxisMap {
    /// Leaves values where they are: what a plot with nothing on a second axis
    /// uses, so the mapped and unmapped paths are the same code.
    pub(crate) const IDENTITY: AxisMap = AxisMap {
        scale: 1.0,
        offset: 0.0,
    };

    fn to_primary(self, value: f64) -> f64 {
        value * self.scale + self.offset
    }

    fn to_secondary(self, y: f64) -> f64 {
        (y - self.offset) / self.scale
    }

    /// The map that actually applies to one series: the shared map for a Y2
    /// series, identity for everything else. Centralizes the "which axis is
    /// this on" branch so callers don't each re-derive it.
    fn for_entry(self, own_axis: bool) -> AxisMap {
        if own_axis {
            self
        } else {
            AxisMap::IDENTITY
        }
    }
}

/// The map laying `secondary`'s extent over `primary`'s, so two quantities of
/// wildly different magnitude (rpm in the thousands, duty in 0.0-1.0) fill the
/// same window and their shapes can be compared.
///
/// `scale` is never zero, so [`AxisMap::to_secondary`] is always defined:
/// a secondary trace with no extent of its own (one sample, or dead flat) is
/// parked in the middle of the window rather than stretched across it, and
/// anything with no primary extent to stretch onto is left alone.
fn axis_map(primary: Option<(f64, f64)>, secondary: Option<(f64, f64)>) -> AxisMap {
    let (Some((p0, p1)), Some((s0, s1))) = (primary, secondary) else {
        return AxisMap::IDENTITY;
    };
    let (p_span, s_span) = (p1 - p0, s1 - s0);
    if s_span <= 0.0 {
        return AxisMap {
            scale: 1.0,
            offset: (p0 + p1) / 2.0 - s0,
        };
    }
    if p_span <= 0.0 {
        return AxisMap::IDENTITY;
    }
    let scale = p_span / s_span;
    // An extreme enough mismatch between the two spans underflows or
    // overflows the division despite both spans being ordinary finite
    // numbers on their own; a zero or non-finite scale would make
    // `to_secondary` divide by zero, so treat it like `p_span <= 0.0`
    // above — no mapping can meaningfully represent it.
    if scale == 0.0 || !scale.is_finite() {
        return AxisMap::IDENTITY;
    }
    AxisMap {
        scale,
        offset: p0 - s0 * scale,
    }
}

/// One axis tick, at a precision taken from the gap to the next one — the same
/// rule `egui_plot`'s own default formatter uses, applied to the value *after*
/// it has been mapped back into the secondary series' units.
fn format_tick(value: f64, step: f64) -> String {
    let decimals = if step.abs() > 0.0 {
        (-step.abs().log10().round()) as i32
    } else {
        0
    };
    let decimals = decimals.clamp(0, 6) as usize;
    format!("{value:.decimals$}")
}

/// Folds one more `(lo, hi)` span into a running range, or starts one.
fn merge_range(acc: Option<(f64, f64)>, (lo, hi): (f64, f64)) -> Option<(f64, f64)> {
    Some(match acc {
        Some((a, b)) => (a.min(lo), b.max(hi)),
        None => (lo, hi),
    })
}

/// The full value extent of the visible series on one side of the Y2 split.
fn group_range(all: &[SeriesEntry], own_axis: bool) -> Option<(f64, f64)> {
    let mut range: Option<(f64, f64)> = None;
    for entry in all.iter().filter(|e| e.visible && e.own_axis == own_axis) {
        if let Some(span) = entry.series.value_range() {
            range = merge_range(range, span);
        }
    }
    range
}

/// Bounds holding every visible series in full, or `None` when there is nothing
/// plotted to fit.
///
/// Both axes are padded, and a trace with no extent of its own — one sample, or
/// a dead-flat line — is given a window anyway: a zero-sized one is not
/// something the plot can draw.
fn fit_bounds(all: &[SeriesEntry], map: AxisMap) -> Option<PlotBounds> {
    let mut x: Option<(f64, f64)> = None;
    let mut y: Option<(f64, f64)> = None;
    for entry in all.iter().filter(|e| e.visible) {
        // Both or neither: a series whose every value is unplottable (all NaN)
        // has a time extent but nothing to show at it.
        if let (Some(t), Some((lo, hi))) = (entry.series.t_range(), entry.series.value_range()) {
            x = merge_range(x, t);
            // Where the trace is actually drawn, which for a Y2 series is not
            // where its numbers say — fitting on raw values would leave it off
            // screen.
            let m = map.for_entry(entry.own_axis);
            y = merge_range(y, (m.to_primary(lo), m.to_primary(hi)));
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
                ui.color_edit_button_srgba(&mut entry.color)
                    .on_hover_text("Change series colour");
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

        // The Y2 split. Scanning the series for their extents is only worth
        // anything when something is actually on the second axis, and nothing
        // is unless the user ticks the box — so the common case pays nothing.
        //
        // `secondary` has to agree exactly with what `group_range` counts:
        // a Y2 series whose only samples are non-finite has a range of
        // `None`, and if `secondary` disagreed by including it anyway, the
        // ruler and label formatter below would switch on for an axis that
        // `map` was never actually built to cover.
        let secondary_range = group_range(&conn.series, true);
        let secondary: HashSet<String> = match secondary_range {
            Some(_) => conn
                .series
                .iter()
                .filter(|e| e.visible && e.own_axis && e.series.value_range().is_some())
                .map(|e| e.series.name().to_string())
                .collect(),
            None => HashSet::new(),
        };
        let map = match secondary_range {
            Some(sec) => axis_map(group_range(&conn.series, false).or(Some(sec)), Some(sec)),
            None => AxisMap::IDENTITY,
        };

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
            .then(|| fit_bounds(&conn.series, map))
            .flatten();

        let mut clicked_t: Option<f64> = None;
        let mut dragged = false;

        let mut plot = Plot::new(egui::Id::new(("plot", conn.id.0)))
            .legend(Legend::default())
            .allow_scroll(true)
            .x_axis_label("t (s)");
        if !secondary.is_empty() {
            // A ruler down the right-hand side reading in the Y2 series' own
            // units. Its ticks stand at primary coordinates like everything
            // else the plot draws; only the *labels* are run back through the
            // map, which is what the second axis actually amounts to here.
            plot = plot
                .custom_y_axes(vec![
                    AxisHints::new_y(),
                    AxisHints::new_y().placement(HPlacement::Right).formatter(
                        move |mark, _range| {
                            format_tick(map.to_secondary(mark.value), mark.step_size / map.scale)
                        },
                    ),
                ])
                // Hovering a Y2 trace has to report the number the device
                // actually printed, not where the trace was moved to.
                .label_formatter(move |name, point| {
                    let y = if secondary.contains(name) {
                        map.to_secondary(point.y)
                    } else {
                        point.y
                    };
                    if name.is_empty() {
                        format!("t = {:.3}s\ny = {y:.4}", point.x)
                    } else {
                        format!("{name}\nt = {:.3}s\ny = {y:.4}", point.x)
                    }
                });
        }
        let response = plot.show(ui, |pui| {
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
                let mut pts = entry.series.decimate(x0, x1, width);
                // Decimation picked the min/max of each column from the raw
                // values; the map is affine with a positive scale, so those
                // are still the min/max after it. Identity for anything not
                // on Y2, so this applies uniformly.
                let m = map.for_entry(entry.own_axis);
                for p in &mut pts {
                    p[1] = m.to_primary(p[1]);
                }
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
        named_entry("s", visible, false, points)
    }

    fn named_entry(
        name: &str,
        visible: bool,
        own_axis: bool,
        points: &[(f64, f64)],
    ) -> SeriesEntry {
        let mut series = Series::new(name, 100);
        for (i, (t, v)) in points.iter().enumerate() {
            series.push(*t, *v, i as u64);
        }
        SeriesEntry {
            series,
            color: egui::Color32::WHITE,
            visible,
            own_axis,
        }
    }

    #[test]
    fn fitting_covers_every_visible_series() {
        let all = [
            entry(true, &[(1.0, 0.0), (3.0, 10.0)]),
            entry(true, &[(2.0, -5.0), (4.0, 4.0)]),
        ];
        let b = fit_bounds(&all, AxisMap::IDENTITY).unwrap();
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
        let b = fit_bounds(&all, AxisMap::IDENTITY).unwrap();
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
        let b = fit_bounds(&[entry(true, &[(5.0, 2.0)])], AxisMap::IDENTITY).unwrap();
        assert_eq!((b.min()[0], b.max()[0]), (4.5, 5.5));
        assert!(b.min()[1] < 2.0 && b.max()[1] > 2.0);

        let flat =
            fit_bounds(&[entry(true, &[(1.0, 0.0), (9.0, 0.0)])], AxisMap::IDENTITY).unwrap();
        assert_eq!((flat.min()[0], flat.max()[0]), (1.0, 9.0));
        assert!(flat.min()[1] < 0.0 && flat.max()[1] > 0.0);
    }

    #[test]
    fn nothing_visible_or_nothing_plottable_is_not_a_fit() {
        assert!(fit_bounds(&[], AxisMap::IDENTITY).is_none());
        assert!(fit_bounds(&[entry(false, &[(1.0, 1.0)])], AxisMap::IDENTITY).is_none());
        assert!(fit_bounds(&[entry(true, &[])], AxisMap::IDENTITY).is_none());
        assert!(
            fit_bounds(&[entry(true, &[(1.0, f64::NAN)])], AxisMap::IDENTITY).is_none(),
            "a series of unplottable values has nothing to fit to"
        );
    }

    /// The point of Y2: rpm in the thousands and duty in 0.0-1.0 both fill the
    /// window, so their shapes can be compared. The map stretches the secondary
    /// group's extent onto the primary group's.
    #[test]
    fn a_second_axis_stretches_its_series_over_the_primary_window() {
        let map = axis_map(Some((0.0, 1200.0)), Some((0.0, 1.0)));
        assert_eq!(map.to_primary(0.0), 0.0);
        assert_eq!(map.to_primary(1.0), 1200.0);
        assert_eq!(map.to_primary(0.5), 600.0);
        // And the right-hand ruler reads back in the series' real units.
        assert_eq!(map.to_secondary(600.0), 0.5);
        assert_eq!(map.to_secondary(1200.0), 1.0);
    }

    /// Offsets, not just scales: neither group has to start at zero.
    #[test]
    fn the_map_round_trips_between_two_offset_ranges() {
        let map = axis_map(Some((-40.0, 60.0)), Some((900.0, 1100.0)));
        for v in [900.0, 1000.0, 1100.0, 1234.5] {
            let there_and_back = map.to_secondary(map.to_primary(v));
            assert!(
                (there_and_back - v).abs() < 1e-9,
                "{v} came back as {there_and_back}"
            );
        }
        assert_eq!(map.to_primary(900.0), -40.0);
        assert_eq!(map.to_primary(1100.0), 60.0);
    }

    /// `to_secondary` divides by `scale`, so a zero one would put the whole
    /// right-hand ruler at infinity. None of the degenerate inputs may produce
    /// it.
    #[test]
    fn no_degenerate_range_can_produce_an_undrawable_map() {
        let cases = [
            (None, None),
            (Some((0.0, 10.0)), None),
            (None, Some((0.0, 10.0))),
            // A dead-flat or single-sample secondary trace: no extent to stretch.
            (Some((0.0, 10.0)), Some((5.0, 5.0))),
            // A dead-flat primary: nothing to stretch onto.
            (Some((7.0, 7.0)), Some((0.0, 1.0))),
            // A span ratio so extreme that `p_span / s_span` underflows to
            // exactly 0.0 despite both spans being ordinary finite numbers.
            (Some((0.0, 1e-300)), Some((0.0, 1e300))),
        ];
        for (primary, secondary) in cases {
            let map = axis_map(primary, secondary);
            assert!(
                map.scale != 0.0 && map.scale.is_finite(),
                "{primary:?}/{secondary:?} gave scale {}",
                map.scale
            );
            assert!(map.offset.is_finite());
        }
        // A flat secondary is parked in the middle of the window, not at its edge.
        assert_eq!(
            axis_map(Some((0.0, 10.0)), Some((5.0, 5.0))).to_primary(5.0),
            5.0
        );
    }

    /// Fit has to cover a Y2 series where it is actually *drawn*. Fitting on its
    /// raw numbers would leave it off screen — which is the failure the Y2
    /// toggle looked like before it did anything at all.
    #[test]
    fn fitting_covers_a_second_axis_series_where_it_is_drawn() {
        let all = [
            named_entry("rpm", true, false, &[(0.0, 0.0), (1.0, 1200.0)]),
            named_entry("duty", true, true, &[(0.0, 0.0), (1.0, 1.0)]),
        ];
        let map = axis_map(group_range(&all, false), group_range(&all, true));
        let b = fit_bounds(&all, map).unwrap();
        // Both traces now span 0..1200 in plot coordinates, so the window is
        // that plus the 5% padding — not 0..1200 *and* a stray 0..1.
        assert_eq!((b.min()[1], b.max()[1]), (-60.0, 1260.0));
    }

    /// The visible/own-axis split the map is derived from.
    #[test]
    fn group_range_splits_on_the_y2_checkbox_and_ignores_hidden_series() {
        let all = [
            named_entry("a", true, false, &[(0.0, 1.0), (1.0, 5.0)]),
            named_entry("b", true, true, &[(0.0, 100.0), (1.0, 300.0)]),
            named_entry("hidden", false, true, &[(0.0, -900.0)]),
        ];
        assert_eq!(group_range(&all, false), Some((1.0, 5.0)));
        assert_eq!(
            group_range(&all, true),
            Some((100.0, 300.0)),
            "a series you have hidden is not one you asked to scale the axis by"
        );
    }

    /// Ticks are labelled at the precision of the gap between them, after the
    /// value has come back into the secondary series' units.
    #[test]
    fn a_tick_is_labelled_at_the_precision_of_its_own_step() {
        assert_eq!(format_tick(0.5, 0.1), "0.5");
        assert_eq!(format_tick(0.25, 0.01), "0.25");
        assert_eq!(format_tick(1200.0, 100.0), "1200");
        assert_eq!(format_tick(3.0, 0.0), "3", "a zero step is still drawable");
    }
}
