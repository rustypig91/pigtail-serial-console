//! The plot pane: live series with min/max decimation, auto-follow, and
//! bidirectional plot↔log selection (spec §7.13).

use crate::app::{App, SeriesEntry};
use egui_plot::{AxisHints, HPlacement, Legend, Line, Plot, PlotBounds, PlotPoints, VLine};
use std::collections::HashSet;
use std::time::Duration;

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

#[derive(Clone, Copy)]
enum ChartAction {
    Add,
    Up(usize),
    Down(usize),
    Duplicate(usize),
    Remove(usize),
}

fn time_union(charts: &[crate::plots::PlotChart]) -> Option<(f64, f64)> {
    charts
        .iter()
        .flat_map(|chart| &chart.series)
        .filter(|entry| entry.visible && entry.series.value_range().is_some())
        .filter_map(|entry| entry.series.t_range())
        .fold(None, merge_range)
}

fn parse_range(values: &[String; 2]) -> Option<(f64, f64)> {
    let lo = values[0].parse::<f64>().ok()?;
    let hi = values[1].parse::<f64>().ok()?;
    (lo.is_finite() && hi.is_finite() && lo < hi).then_some((lo, hi))
}

impl App {
    pub(crate) fn show_plot_tab(&mut self, ui: &mut egui::Ui) {
        egui::Frame::NONE
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| self.show_plot_tab_in(ui));
    }

    fn show_plot_tab_in(&mut self, ui: &mut egui::Ui) {
        let Some(index) = self
            .active_plot
            .and_then(|id| self.plot_tabs.iter().position(|tab| tab.saved.id == id))
        else {
            return;
        };
        self.plot_tabs[index].saved.time_anchor_micros =
            Some(self.clock.start_wall().timestamp_micros());
        let id = self.plot_tabs[index].saved.id;
        let mut action = None;
        let mut settings = None;
        let mut changed = false;
        ui.spacing_mut().interact_size.y = 28.0;
        ui.spacing_mut().button_padding = egui::vec2(8.0, 5.0);
        ui.horizontal_wrapped(|ui| {
            ui.heading(&self.plot_tabs[index].saved.name);
            if ui
                .add_enabled(
                    self.plot_tabs[index].charts.len() < crate::plots::MAX_CHARTS,
                    egui::Button::new("Add chart"),
                )
                .clicked()
            {
                action = Some(ChartAction::Add);
            }
            if ui.button("Tab options").clicked() {
                settings = Some(0);
            }
            changed |= ui
                .checkbox(
                    &mut self.plot_tabs[index].saved.independent_time_axes,
                    "Independent time axes",
                )
                .changed();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.show_app_menu(ui)
            });
        });
        ui.separator();
        let selected: Vec<Vec<_>> = self.plot_tabs[index]
            .charts
            .iter()
            .map(|chart| {
                chart
                    .keys
                    .iter()
                    .zip(&chart.series)
                    .filter_map(|(key, entry)| {
                        entry
                            .series
                            .point_for_line(self.plot_selected_line(key.source_id)?)
                            .map(|point| point.t)
                    })
                    .collect()
            })
            .collect();
        let statuses: Vec<Vec<_>> = self.plot_tabs[index]
            .charts
            .iter()
            .map(|chart| {
                chart
                    .saved
                    .rules
                    .iter()
                    .filter_map(|rule| rule.source.as_ref())
                    .map(|source| self.plot_source_status(source))
                    .collect()
            })
            .collect();
        let mut navigate = None;
        let tab = &mut self.plot_tabs[index];
        let linked = !tab.saved.independent_time_axes;
        let union = time_union(&tab.charts);
        // An unavailable source or a pending backfill can leave the whole tab
        // empty on its first paint. Keep the shared Fit pending as well as each
        // chart's Fit so late samples establish one linked time range.
        let fit_all = union.is_some() && std::mem::take(&mut tab.fit);
        let mut shared = tab.saved.time_range.as_ref().and_then(parse_range);
        if linked {
            if fit_all {
                shared = union.map(|(lo, hi)| {
                    if lo == hi {
                        (lo - 0.5, hi + 0.5)
                    } else {
                        (lo, hi)
                    }
                });
            } else if tab.saved.follow {
                if let Some((_, latest)) = union {
                    let span = shared.map(|(lo, hi)| hi - lo).unwrap_or(10.0).max(1e-6);
                    shared = Some((latest - span, latest));
                }
            }
        }
        if tab.charts.is_empty() {
            ui.centered_and_justified(|ui| {
                if ui.button("Add chart").clicked() {
                    action = Some(ChartAction::Add);
                }
            });
        } else {
            let count = tab.charts.len();
            let height = ui.available_height().max(count as f32 * 180.0);
            let usable = (height - (count - 1) as f32 * 8.0).max(1.0);
            let total_weight: f32 = tab
                .charts
                .iter()
                .map(|chart| chart.saved.weight.max(1) as f32)
                .sum();
            let weights: Vec<_> = tab
                .charts
                .iter()
                .map(|chart| chart.saved.weight.max(1) as f32 / total_weight)
                .collect();
            let mut divider = None;
            egui::ScrollArea::vertical().id_salt(("plot_stack", id)).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (c, chart) in tab.charts.iter_mut().enumerate() {
                    let card_height = (usable * weights[c]).max(140.0);
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), card_height), egui::Sense::hover());
                    let mut card = ui.new_child(egui::UiBuilder::new().id_salt(chart.saved.id).max_rect(rect.shrink2(egui::vec2(0.0, 4.0))));
                    card.set_clip_rect(rect.intersect(ui.clip_rect()));
                    card.horizontal_wrapped(|ui| {
                        ui.strong(&chart.saved.title);
                        let follow = if linked { &mut tab.saved.follow } else { &mut chart.saved.follow };
                        if ui.selectable_label(*follow, "Follow").clicked() { *follow = !*follow; changed = true; }
                        if ui.button("Fit").clicked() {
                            if linked { tab.fit = true; } else { chart.fit = true; }
                        }
                        if ui.button("Settings").clicked() { settings = Some(c); }
                        ui.menu_button("Series", |ui| {
                            egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                                for entry in &mut chart.series {
                                    ui.horizontal(|ui| {
                                        changed |= ui.checkbox(&mut entry.visible, "").changed();
                                        changed |= ui.color_edit_button_srgba(&mut entry.color).changed();
                                        ui.label(entry.series.name());
                                        changed |= ui.checkbox(&mut entry.own_axis, "Y2").changed();
                                    });
                                }
                                if chart.series.is_empty() { ui.weak("No series yet. Configure extraction rules in Settings."); }
                            });
                        });
                        ui.menu_button("⋯", |ui| {
                            for (label, enabled, request) in [
                                ("Move up", c > 0, ChartAction::Up(c)),
                                ("Move down", c + 1 < count, ChartAction::Down(c)),
                                ("Duplicate", count < crate::plots::MAX_CHARTS, ChartAction::Duplicate(c)),
                                ("Remove chart", true, ChartAction::Remove(c)),
                            ] { if ui.add_enabled(enabled, egui::Button::new(label)).clicked() { action = Some(request); ui.close(); } }
                        });
                    });
                    for (r, runtime) in chart.rules.iter().enumerate() {
                        if let Some(error) = &runtime.error { card.colored_label(egui::Color32::YELLOW, format!("Rule {}: {error}", r+1)); }
                    }
                    if let Some(status) = statuses[c].iter().find(|s| !s.ends_with("Connected")) { card.colored_label(egui::Color32::YELLOW, status).on_hover_text(statuses[c].join("\n")); }
                    if chart.limited || chart.history_unavailable { card.weak("Retained history only; older output or samples may be unavailable."); }
                    if chart.series.is_empty() { card.weak("No samples yet. Settings can preview retained source output."); }
                    let follow = if linked { tab.saved.follow } else { chart.saved.follow };
                    let result = draw_chart(&mut card, chart, if linked { shared } else { None }, follow, fit_all && linked, &selected[c], linked.then(||egui::Id::new(("plot_cursor",id))));
                    if let Some((source, line)) = result.navigate { navigate = Some((source, line)); }
                    changed |= result.presentation_changed;
                    if result.interacted {
                        if linked { tab.saved.follow = false; shared = Some(result.time); }
                        else { chart.saved.follow = false; }
                        changed = true;
                        crate::graphics::request_animation_frame(ui.ctx());
                    }
                    if changed || result.interacted { chart.remember_styles(); }
                    if c+1 < count {
                        let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 8.0), egui::Sense::drag());
                        ui.painter().line_segment([rect.left_center(), rect.right_center()], egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color));
                        response.clone().on_hover_cursor(egui::CursorIcon::ResizeVertical);
                        if response.dragged() { divider = Some((c, ui.input(|i| i.pointer.delta().y))); }
                        if response.double_clicked() { divider = Some((c, f32::NAN)); }
                    }
                }
            });
            if let Some((c, delta)) = divider {
                let sum = tab.charts[c].saved.weight as f32 + tab.charts[c + 1].saved.weight as f32;
                let weight = if delta.is_nan() {
                    sum / 2.0
                } else {
                    tab.charts[c].saved.weight as f32 + delta / usable * total_weight
                };
                tab.charts[c].saved.weight =
                    weight.clamp(sum * 0.1, sum * 0.9).round().max(1.0) as u16;
                tab.charts[c + 1].saved.weight =
                    (sum - tab.charts[c].saved.weight as f32).round().max(1.0) as u16;
                changed = true;
            }
        }
        if linked {
            tab.saved.time_range = shared.map(|(lo, hi)| [lo.to_string(), hi.to_string()]);
        }
        if let Some(action) = action {
            match action {
                ChartAction::Add => {
                    tab.charts.push(crate::plots::PlotChart::new(
                        serialcore::config::SavedPlotChart {
                            id: crate::plots::new_id(),
                            title: format!("Chart {}", tab.charts.len() + 1),
                            ..Default::default()
                        },
                    ));
                    settings = Some(tab.charts.len() - 1);
                }
                ChartAction::Up(c) => tab.charts.swap(c, c - 1),
                ChartAction::Down(c) => tab.charts.swap(c, c + 1),
                ChartAction::Remove(c) => {
                    tab.charts.remove(c);
                }
                ChartAction::Duplicate(c) => {
                    tab.charts[c].remember_styles();
                    let mut saved = tab.charts[c].saved.clone();
                    saved.id = crate::plots::new_id();
                    saved.title = format!("{} copy", saved.title);
                    for rule in &mut saved.rules {
                        let old = rule.id;
                        rule.id = crate::plots::new_id();
                        for style in &mut saved.styles {
                            if style.key.rule_id == old {
                                style.key.rule_id = rule.id;
                            }
                        }
                    }
                    tab.charts
                        .insert(c + 1, crate::plots::PlotChart::new(saved));
                }
            }
            changed = true;
            self.plot_routes_dirty = true;
            self.resize_plot_history();
        }
        // Persist bounds through the normal debounce, without serializing samples.
        let saved = self
            .plot_tabs
            .iter_mut()
            .map(crate::plots::PlotTab::definition)
            .collect::<Vec<_>>();
        if self.config.plots != saved {
            self.config.plots = saved;
            self.write_config();
        }
        if changed {
            self.save_session();
        }
        if let Some(chart) = settings {
            self.edit_plot(id);
            if let Some(draft) = &mut self.plot_draft {
                draft.selected_chart = chart;
            }
        }
        if let Some((source, line)) = navigate {
            self.request_plot_sample_navigation(source, line);
        }
    }
}

struct ChartResult {
    navigate: Option<(u64, u64)>,
    time: (f64, f64),
    interacted: bool,
    presentation_changed: bool,
}

fn draw_chart(
    ui: &mut egui::Ui,
    chart: &mut crate::plots::PlotChart,
    shared: Option<(f64, f64)>,
    follow: bool,
    fit_shared: bool,
    selected: &[f64],
    link_group: Option<egui::Id>,
) -> ChartResult {
    let secondary_range = group_range(&chart.series, true);
    let secondary: HashSet<String> = if secondary_range.is_some() {
        chart
            .series
            .iter()
            .filter(|e| e.visible && e.own_axis && e.series.value_range().is_some())
            .map(|e| e.series.name().to_string())
            .collect()
    } else {
        HashSet::new()
    };
    let map = match secondary_range {
        Some(sec) => axis_map(group_range(&chart.series, false).or(Some(sec)), Some(sec)),
        None => AxisMap::IDENTITY,
    };
    // Keep an empty chart's initial Fit pending until there are samples. A
    // restored manual view has no pending Fit and must survive backfill.
    let fit = (!chart.series.is_empty() && (std::mem::take(&mut chart.fit) || fit_shared))
        .then(|| fit_bounds(&chart.series, map))
        .flatten();
    let plot_id = egui::Id::new(("chart", chart.saved.id));
    let item_id = |i: usize| {
        chart
            .keys
            .get(i)
            .map(|key| egui::Id::new((chart.saved.id, key)))
            .unwrap_or_else(|| egui::Id::new((chart.saved.id, i)))
    };
    let hidden: Vec<_> = chart
        .series
        .iter()
        .enumerate()
        .filter(|(_, entry)| !entry.visible)
        .map(|(i, _)| item_id(i))
        .collect();
    let initial = egui_plot::PlotMemory::load(ui.ctx(), plot_id).is_none();
    if let Some(mut memory) = egui_plot::PlotMemory::load(ui.ctx(), plot_id) {
        memory.hidden_items = hidden.iter().copied().collect();
        memory.store(ui.ctx(), plot_id);
    }
    let saved_bounds = chart.saved.bounds.as_ref().and_then(|s| {
        let x = parse_range(&[s[0].clone(), s[2].clone()])?;
        let y = parse_range(&[s[1].clone(), s[3].clone()])?;
        Some(PlotBounds::from_min_max([x.0, y.0], [x.1, y.1]))
    });
    let mut plot = Plot::new(plot_id)
        .id(plot_id)
        .legend(
            Legend::default()
                .grouping(egui_plot::LegendGrouping::ById)
                .hidden_items(hidden),
        )
        .height(ui.available_height().max(40.0))
        .allow_scroll(true)
        .x_axis_label("t (s)");
    if let Some(group) = link_group {
        plot = plot.link_cursor(group, [true, false]);
    }
    if !secondary.is_empty() {
        plot = plot
            .custom_y_axes(vec![
                AxisHints::new_y(),
                AxisHints::new_y()
                    .placement(HPlacement::Right)
                    .formatter(move |mark, _| {
                        format_tick(map.to_secondary(mark.value), mark.step_size / map.scale)
                    }),
            ])
            .label_formatter(move |params| {
                let (name, point) = match params {
                    egui_plot::HoverPosition::NearDataPoint {
                        plot_name,
                        position,
                        ..
                    } => (*plot_name, position),
                    egui_plot::HoverPosition::Elsewhere { position } => ("", position),
                };
                let y = if secondary.contains(name) {
                    map.to_secondary(point.y)
                } else {
                    point.y
                };
                Some(format!("{name}\nt = {:.3}s\ny = {y:.4}", point.x))
            });
    }
    let response = plot.show(ui, |pui| {
        let mut bounds = pui.plot_bounds();
        if initial {
            if let Some(saved) = saved_bounds {
                bounds = saved;
            }
        }
        if let Some(fit) = fit {
            bounds = fit;
        }
        let mut x = (bounds.min()[0], bounds.max()[0]);
        if let Some(shared) = shared {
            x = shared;
        } else if follow && fit.is_none() {
            if let Some((_, last)) = chart
                .series
                .iter()
                .filter(|e| e.visible)
                .filter_map(|e| e.series.t_range())
                .fold(None, merge_range)
            {
                let span = (x.1 - x.0).max(1e-6);
                x = (last - span, last);
            }
        }
        pui.set_plot_bounds(PlotBounds::from_min_max(
            [x.0, bounds.min()[1]],
            [x.1, bounds.max()[1]],
        ));
        let width = (pui.response().rect.width() as usize).clamp(64, 4096);
        for (i, entry) in chart.series.iter().enumerate() {
            let segments = if entry.visible {
                entry.series.decimate_segments(x.0, x.1, width)
            } else {
                vec![]
            };
            if segments.is_empty() {
                // Keep legend identity and saved visibility even outside the time window.
                pui.line(
                    Line::new(
                        entry.series.name(),
                        PlotPoints::from(
                            entry
                                .series
                                .last()
                                .map(|point| {
                                    [
                                        point.t,
                                        map.for_entry(entry.own_axis).to_primary(point.value),
                                    ]
                                })
                                .into_iter()
                                .collect::<Vec<_>>(),
                        ),
                    )
                    .color(entry.color)
                    .id(item_id(i)),
                );
            }
            for mut pts in segments {
                for p in &mut pts {
                    p[1] = map.for_entry(entry.own_axis).to_primary(p[1]);
                }
                if pts.len() == 1 {
                    pui.points(
                        egui_plot::Points::new(entry.series.name(), PlotPoints::from(pts))
                            .color(entry.color)
                            .id(item_id(i))
                            .radius(3.0),
                    );
                } else {
                    pui.line(
                        Line::new(entry.series.name(), PlotPoints::from(pts))
                            .color(entry.color)
                            .id(item_id(i)),
                    );
                }
            }
        }
        for t in selected {
            pui.vline(VLine::new("", *t).color(egui::Color32::from_gray(180)));
        }
        (pui.pointer_coordinate(), pui.response().clicked())
    });
    let mut presentation_changed = false;
    if let Some(memory) = egui_plot::PlotMemory::load(ui.ctx(), plot_id) {
        for (i, entry) in chart.series.iter_mut().enumerate() {
            if entry.series.is_empty() {
                continue;
            }
            let id = chart
                .keys
                .get(i)
                .map(|key| egui::Id::new((chart.saved.id, key)))
                .unwrap_or_else(|| egui::Id::new((chart.saved.id, i)));
            let visible = !memory.hidden_items.contains(&id);
            presentation_changed |= entry.visible != visible;
            entry.visible = visible;
        }
    }
    let bounds = *response.transform.bounds();
    chart.saved.bounds = Some([
        bounds.min()[0].to_string(),
        bounds.min()[1].to_string(),
        bounds.max()[0].to_string(),
        bounds.max()[1].to_string(),
    ]);
    let mut navigate = None;
    if response.inner.1 {
        if let Some(pointer) = response.inner.0 {
            let mut best: Option<(f64, u64, u64)> = None;
            for (key, entry) in chart
                .keys
                .iter()
                .zip(&chart.series)
                .filter(|(_, e)| e.visible)
            {
                if let Some(point) = entry.series.nearest_point(pointer.x) {
                    let dx = (point.t - pointer.x) / (bounds.max()[0] - bounds.min()[0]).max(1e-6);
                    let dy = (map.for_entry(entry.own_axis).to_primary(point.value) - pointer.y)
                        / (bounds.max()[1] - bounds.min()[1]).max(1e-6);
                    let distance = dx * dx + dy * dy;
                    if distance.is_finite() && best.is_none_or(|(d, _, _)| distance < d) {
                        best = Some((distance, key.source_id, point.line));
                    }
                }
            }
            navigate = best.map(|(_, source, line)| (source, line));
        }
    }
    let interacted = response.response.dragged()
        || (response.response.hovered()
            && ui.input(|i| i.smooth_scroll_delta != egui::Vec2::ZERO || i.zoom_delta() != 1.0));
    ChartResult {
        navigate,
        time: (bounds.min()[0], bounds.max()[0]),
        interacted,
        presentation_changed,
    }
}

/// Explicit tooltip rendering avoids egui's hover delay and click suppression.
trait PlotHelp {
    fn plot_help(self, text: &str) -> Self;
}

impl PlotHelp for egui::Response {
    fn plot_help(self, text: &str) -> Self {
        if self.contains_pointer() {
            egui::Tooltip::for_widget(&self).show(|ui| {
                ui.set_max_width(ui.spacing().tooltip_width);
                ui.label(text);
            });
        }
        self
    }
}

fn show_rule_preview(ui: &mut egui::Ui, preview: Option<&crate::plots::RulePreview>) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.strong("Preview").plot_help("A real received line that produced numeric values with this rule. Preview uses retained history; older discarded output is unavailable.");
            if let Some(preview) = preview {
                ui.label(&preview.status).plot_help(&preview.details);
            } else {
                ui.weak("Preparing preview…");
            }
        });
        if let Some(preview) = preview {
            egui::ScrollArea::vertical().id_salt("preview")
                .max_height(120.0).show(ui, |ui| {
                    if let Some(example) = &preview.example {
                        ui.weak("Received line").plot_help("An actual finalized line received from this device. Local echo and session markers are excluded.");
                        ui.add(egui::Label::new(egui::RichText::new(&example.line).monospace()).wrap())
                            .plot_help("The latest matching line scanned for this preview. Select text to copy it.");
                        ui.horizontal_wrapped(|ui| {
                            ui.weak("Values").plot_help("Numeric values extracted from the received line above.");
                            for (name, value) in &example.values {
                                ui.label(format!("{name} = {value}"));
                            }
                        });
                    }
                    if !preview.series.is_empty() {
                        ui.label(format!("Series: {}", preview.series.join(", ")))
                            .plot_help("All series found so far for this rule in retained output, including those absent from the example line.");
                    }
                });
        }
    });
}

impl App {
    pub(crate) fn show_plot_setup(&mut self, ctx: &egui::Context) {
        let Some(mut draft) = self.plot_draft.take() else {
            return;
        };
        let mut sources: Vec<_> = self
            .connections
            .iter()
            .map(crate::plots::source_of)
            .collect();
        for source in draft
            .definition
            .charts
            .iter()
            .flat_map(|chart| &chart.rules)
            .filter_map(|rule| rule.source.as_ref())
        {
            if !sources
                .iter()
                .any(|s| s.connection_id == source.connection_id)
            {
                sources.push(source.clone());
            }
        }
        for saved in &self.config.last_open {
            if saved.id != 0 && !sources.iter().any(|s| s.connection_id == saved.id) {
                sources.push(serialcore::config::PlotSource {
                    connection_id: saved.id,
                    identity: saved.identity.clone(),
                    label: saved.name.clone().unwrap_or_else(|| saved.identity.label()),
                });
            }
        }
        let source_labels: Vec<_> = sources
            .iter()
            .map(|source| self.plot_source_status(source))
            .collect();
        let mut open = true;
        let mut commit = false;
        let mut cancel = false;
        let mut changed = false;
        egui::Window::new(if draft.editing.is_some() { "Plot options" } else { "New plot" })
            .id(egui::Id::new("plot_setup"))
            .open(&mut open)
            .default_width(600.0)
            .default_height((ctx.content_rect().height() - 40.0).clamp(180.0, 820.0))
            .frame(super::chrome::dialog_frame(ctx))
            .show(ctx, |ui| {
                super::chrome::dialog_style(ui);
                egui::ScrollArea::vertical()
                    .max_height((ui.available_height() - 62.0).max(80.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Plot name").plot_help("The name shown on this plot tab.");
                            changed |= ui.add(egui::TextEdit::singleline(&mut draft.definition.name)
                                .desired_width(ui.available_width()))
                                .plot_help("Rename this plot tab.").changed();
                        });
                        let mut linked = !draft.definition.independent_time_axes;
                        if ui.checkbox(&mut linked, "Link chart time axes")
                            .plot_help("Share the time range, pan, zoom, Follow and Fit across charts in this tab. Clear this to navigate each chart independently.")
                            .changed()
                        {
                            draft.definition.independent_time_axes = !linked;
                            changed = true;
                        }
                        ui.separator();
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Charts").plot_help("Select a chart to edit it. All charts remain in the plot tab.");
                            for (index, chart) in draft.definition.charts.iter().enumerate() {
                                ui.selectable_value(&mut draft.selected_chart, index,
                                    format!("{} · {}", index + 1, chart.title))
                                    .plot_help("Edit this chart's name, devices and parsing rules.");
                            }
                            if ui.add_enabled(draft.definition.charts.len() < crate::plots::MAX_CHARTS,
                                egui::Button::new("+ Add chart"))
                                .plot_help("Add another chart to this tab. Existing charts and devices stay open.")
                                .clicked()
                            {
                                let source = draft.definition.charts.get(draft.selected_chart)
                                    .and_then(|chart| chart.rules.first())
                                    .and_then(|rule| rule.source.clone())
                                    .or_else(|| sources.first().cloned());
                                draft.selected_chart = draft.definition.charts.len();
                                draft.definition.charts.push(serialcore::config::SavedPlotChart {
                                    id: crate::plots::new_id(),
                                    title: format!("Chart {}", draft.selected_chart + 1),
                                    rules: vec![serialcore::config::DeviceExtractRule {
                                        id: crate::plots::new_id(), source, ..Default::default()
                                    }],
                                    ..Default::default()
                                });
                                changed = true;
                            }
                        });
                        draft.selected_chart = draft.selected_chart.min(draft.definition.charts.len().saturating_sub(1));
                        let mut remove_chart = false;
                        if let Some(chart) = draft.definition.charts.get_mut(draft.selected_chart) {
                            ui.push_id(chart.id, |ui| {
                                ui.separator();
                                ui.horizontal(|ui| {
                                    ui.label("Chart name").plot_help("The title displayed above this chart.");
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        remove_chart = ui.small_button("Remove chart")
                                            .plot_help("Remove this chart when you apply the changes. Devices remain connected.").clicked();
                                        changed |= ui.add(egui::TextEdit::singleline(&mut chart.title)
                                            .desired_width(ui.available_width()))
                                            .plot_help("Give the chart a descriptive name, such as Temperature.").changed();
                                    });
                                });
                                let multiple_rules = chart.rules.len() > 1;
                                let mut remove_rule = None;
                                for (r, rule) in chart.rules.iter_mut().enumerate() {
                                    ui.push_id(rule.id, |ui| {
                                        ui.separator();
                                        if multiple_rules { ui.strong(format!("Rule {}", r + 1)); }
                                        let current = rule.source.as_ref().map(|s| self.plot_source_status(s))
                                            .unwrap_or_else(|| "Select a device".into());
                                        let device_help = "Choose a source connection. Unavailable remembered connections can be kept and saved; a different connection requires explicit selection.";
                                        ui.horizontal(|ui| {
                                            ui.label("Device").plot_help("The connection supplying output for this rule. Several rules can read the same device.");
                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                if ui.small_button("Remove rule")
                                                    .plot_help("Remove this parsing rule when you apply the changes. Its device stays connected.").clicked()
                                                { remove_rule = Some(r); }
                                                egui::ComboBox::from_id_salt("source")
                                                    .selected_text(current.as_str())
                                                    .truncate()
                                                    .width(ui.available_width())
                                                    .show_ui(ui, |ui| {
                                                        if ui.selectable_label(rule.source.is_none(), "No device")
                                                            .plot_help("Keep this rule without a source device.").clicked()
                                                        { rule.source = None; changed = true; }
                                                        for (source, label) in sources.iter().zip(&source_labels) {
                                                            if ui.selectable_label(rule.source.as_ref().is_some_and(|s| s.connection_id == source.connection_id), label)
                                                                .plot_help(device_help).clicked()
                                                            { rule.source = Some(source.clone()); changed = true; }
                                                        }
                                                    }).response.plot_help(&format!("{current}\n\n{device_help}"));
                                            });
                                        });
                                        ui.horizontal_wrapped(|ui| {
                                            ui.label("Read values as").plot_help("Choose how numeric values are extracted from received lines.");
                                            changed |= ui.selectable_value(&mut rule.extraction.mode,
                                                serialcore::config::ExtractMode::Kv, "Name/value pairs")
                                                .plot_help("Read pairs such as temp:23.4 or rpm=1200. Each name becomes a separate series.").changed();
                                            changed |= ui.selectable_value(&mut rule.extraction.mode,
                                                serialcore::config::ExtractMode::Regex, "Regex")
                                                .plot_help("Use a regular expression. Each named numeric capture becomes a series; for example temp=(?P<temp>[\\d.]+).").changed();
                                        });
                                        let prefix_help = "Only read lines beginning with this text, ignoring leading whitespace. The prefix is removed before parsing. Leave empty to read any line.";
                                        ui.horizontal(|ui| {
                                            ui.label("Line prefix").plot_help(prefix_help);
                                            let mut prefix = rule.extraction.prefix.clone().unwrap_or_default();
                                            if ui.add(egui::TextEdit::singleline(&mut prefix)
                                                .hint_text("Any line").desired_width(ui.available_width()))
                                                .plot_help(prefix_help).changed()
                                            { rule.extraction.prefix = (!prefix.is_empty()).then_some(prefix); changed = true; }
                                        });
                                        match rule.extraction.mode {
                                            serialcore::config::ExtractMode::Kv => {
                                                let separators = rule.extraction.kv_separators.clone().unwrap_or_else(|| vec![':', '=']);
                                                let mut spaced = separators.iter().any(|c| c.is_whitespace());
                                                let mut typed: String = separators.iter().filter(|c| !c.is_whitespace()).collect();
                                                let separator_help = "Characters separating each name from its number, such as : and = in temp:23.4 rpm=1200. Spaces, commas and semicolons separate pairs. Empty uses : and =.";
                                                let mut edited = false;
                                                ui.horizontal(|ui| {
                                                    ui.label("Value separators").plot_help(separator_help);
                                                    edited |= ui.add(egui::TextEdit::singleline(&mut typed)
                                                        .desired_width(ui.available_width()))
                                                        .plot_help(separator_help).changed();
                                                });
                                                edited |= ui.checkbox(&mut spaced, "Allow space-separated values")
                                                    .plot_help("Also read pairs such as temp 23.4. Ordinary text followed by numbers may then become a series.").changed();
                                                if edited {
                                                    let mut seps: Vec<_> = typed.chars().filter(|c| !c.is_whitespace()).collect();
                                                    if spaced { seps.push(' '); }
                                                    rule.extraction.kv_separators = Some(seps);
                                                    changed = true;
                                                }
                                            }
                                            serialcore::config::ExtractMode::Regex => {
                                                let pattern_help = "Use named numeric captures, for example temp=(?P<temp>[\\d.]+). The capture name becomes the series name, and the captured text must be a number.";
                                                ui.label("Pattern").plot_help(pattern_help);
                                                let mut pattern = rule.extraction.pattern.clone().unwrap_or_default();
                                                if ui.add(egui::TextEdit::singleline(&mut pattern)
                                                    .hint_text(r"temp=(?P<temp>[\d.]+)").desired_width(ui.available_width()))
                                                    .plot_help(pattern_help).changed()
                                                { rule.extraction.pattern = Some(pattern); changed = true; }
                                            }
                                        }
                                        show_rule_preview(ui, if changed { None } else {
                                            draft.preview.iter().find(|preview| preview.chart_id == chart.id && preview.rule_id == rule.id)
                                        });
                                    });
                                }
                                if let Some(r) = remove_rule { chart.rules.remove(r); changed = true; }
                                if ui.add_enabled(chart.rules.len() < crate::plots::MAX_RULES,
                                    egui::Button::new("+ Add rule"))
                                    .plot_help("Read another device or use another parsing pattern on this chart. All rule settings stay visible.").clicked()
                                {
                                    let source = chart.rules.first().and_then(|rule| rule.source.clone())
                                        .or_else(|| sources.first().cloned());
                                    chart.rules.push(serialcore::config::DeviceExtractRule {
                                        id: crate::plots::new_id(), source, ..Default::default()
                                    });
                                    changed = true;
                                }
                            });
                        }
                        if remove_chart {
                            draft.definition.charts.remove(draft.selected_chart);
                            draft.selected_chart = draft.selected_chart.min(draft.definition.charts.len().saturating_sub(1));
                            changed = true;
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    commit = ui.button(if draft.editing.is_some() { "Apply" } else { "Create" })
                        .plot_help("Save this plot configuration. Samples are rebuilt from retained source history; discarded output cannot be recovered.").clicked();
                    cancel = ui.button("Cancel").plot_help("Discard changes to this plot configuration.").clicked();
                });
            });
        if commit {
            if draft.definition.name.trim().is_empty() {
                draft.definition.name = "Plot".into();
            }
            self.apply_plot_draft(draft);
        } else if open && !cancel {
            draft.preview_dirty |= changed;
            let previous_preview = std::mem::take(&mut draft.preview);
            let backlog = self.update_plot_preview(&mut draft, Duration::from_millis(4));
            if backlog || previous_preview != draft.preview {
                crate::graphics::request_animation_frame(ctx);
            }
            self.plot_draft = Some(draft);
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
    fn chart_app() -> App {
        let (mut app, _tx) = crate::app::tests::test_app("plot-chart-ui");
        let mut first = crate::plots::PlotChart::new(serialcore::config::SavedPlotChart {
            id: 1001,
            title: "First".into(),
            follow: false,
            ..Default::default()
        });
        first.series = vec![
            entry(true, &[(-20.0, 1.0), (10.0, 2.0)]),
            entry(false, &[(-1000.0, 3.0), (1000.0, 4.0)]),
        ];
        let mut second = crate::plots::PlotChart::new(serialcore::config::SavedPlotChart {
            id: 1002,
            title: "Second".into(),
            follow: false,
            ..Default::default()
        });
        second.series = vec![entry(true, &[(-5.0, 100.0), (40.0, 200.0)])];
        let mut tab = crate::plots::PlotTab::new(serialcore::config::SavedPlotTab {
            id: 1000,
            name: "Charts".into(),
            follow: false,
            ..Default::default()
        });
        tab.charts = vec![first, second];
        app.plot_tabs.push(tab);
        app.select_plot(1000);
        app
    }
    fn chart_frame(
        app: &mut App,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        use crate::test_ui::ContextTestExt as _;
        ctx.run_test_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 800.0),
                )),
                events,
                ..Default::default()
            },
            |ui| app.show_plot_tab(ui),
        )
    }
    fn bounds(ctx: &egui::Context, id: u64) -> PlotBounds {
        *egui_plot::PlotMemory::load(ctx, egui::Id::new(("chart", id)))
            .unwrap()
            .bounds()
    }
    fn chart_pointer(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn chart_settings_select_the_requested_chart_in_the_draft() {
        let mut app = chart_app();
        let ctx = egui::Context::default();
        chart_frame(&mut app, &ctx, vec![]);
        let output = chart_frame(&mut app, &ctx, vec![]);
        let position = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Settings" => {
                    Some(text.galley.rect.translate(text.pos.to_vec2()).center())
                }
                _ => None,
            })
            .nth(1)
            .unwrap();
        chart_frame(
            &mut app,
            &ctx,
            vec![
                egui::Event::PointerMoved(position),
                chart_pointer(position, true),
            ],
        );
        chart_frame(&mut app, &ctx, vec![chart_pointer(position, false)]);
        let draft = app.plot_draft.as_ref().unwrap();
        assert_eq!(draft.selected_chart, 1);
        assert_eq!(
            draft.definition.charts[draft.selected_chart].title,
            "Second"
        );
    }

    #[test]
    fn an_empty_chart_keeps_its_initial_fit_until_samples_arrive() {
        let mut app = chart_app();
        app.plot_tabs[0].charts.truncate(1);
        app.plot_tabs[0].charts[0].series.clear();
        let ctx = egui::Context::default();
        chart_frame(&mut app, &ctx, vec![]);
        chart_frame(&mut app, &ctx, vec![]);
        app.plot_tabs[0].charts[0].series = vec![entry(true, &[(20.0, 23.4)])];
        chart_frame(&mut app, &ctx, vec![]);
        let actual = bounds(&ctx, 1001);
        assert!(actual.min()[0] < 20.0 && actual.max()[0] > 20.0);
        assert!(actual.min()[1] < 23.4 && actual.max()[1] > 23.4);
    }

    #[test]
    fn initially_empty_linked_charts_fit_the_shared_range_when_samples_arrive() {
        for follow in [false, true] {
            let mut app = chart_app();
            app.plot_tabs[0].saved.follow = follow;
            for chart in &mut app.plot_tabs[0].charts {
                chart.series.clear();
            }
            let ctx = egui::Context::default();
            chart_frame(&mut app, &ctx, vec![]);
            chart_frame(&mut app, &ctx, vec![]);
            app.plot_tabs[0].charts[0].series = vec![entry(true, &[(-20.0, 1.0), (10.0, 2.0)])];
            app.plot_tabs[0].charts[1].series = vec![entry(true, &[(-5.0, 100.0), (40.0, 200.0)])];
            chart_frame(&mut app, &ctx, vec![]);
            chart_frame(&mut app, &ctx, vec![]);
            for id in [1001, 1002] {
                let actual = bounds(&ctx, id);
                assert_eq!((actual.min()[0], actual.max()[0]), (-20.0, 40.0));
            }
            assert_eq!(
                app.plot_tabs[0].saved.time_range,
                Some(["-20".into(), "40".into()])
            );
        }
    }

    #[test]
    fn linked_fit_uses_visible_time_union_and_independent_value_bounds() {
        let mut app = chart_app();
        let ctx = egui::Context::default();
        chart_frame(&mut app, &ctx, vec![]);
        chart_frame(&mut app, &ctx, vec![]);
        let first = bounds(&ctx, 1001);
        let second = bounds(&ctx, 1002);
        assert_eq!((first.min()[0], first.max()[0]), (-20.0, 40.0));
        assert_eq!((second.min()[0], second.max()[0]), (-20.0, 40.0));
        assert!(first.max()[1] < 10.0);
        assert!(second.min()[1] > 90.0);
        app.plot_tabs[0].saved.independent_time_axes = true;
        for chart in &mut app.plot_tabs[0].charts {
            chart.fit = true;
        }
        chart_frame(&mut app, &ctx, vec![]);
        assert_eq!(
            (bounds(&ctx, 1001).min()[0], bounds(&ctx, 1001).max()[0]),
            (-20.0, 10.0)
        );
        assert_eq!(
            (bounds(&ctx, 1002).min()[0], bounds(&ctx, 1002).max()[0]),
            (-5.0, 40.0)
        );
    }

    #[test]
    fn panning_second_chart_synchronizes_linked_time_but_independent_pan_stays_local() {
        for independent in [false, true] {
            let mut app = chart_app();
            app.plot_tabs[0].saved.independent_time_axes = independent;
            let ctx = egui::Context::default();
            chart_frame(&mut app, &ctx, vec![]);
            chart_frame(&mut app, &ctx, vec![]);
            let initial_first = bounds(&ctx, 1001);
            let initial_second = bounds(&ctx, 1002);
            let start = egui_plot::PlotMemory::load(&ctx, egui::Id::new(("chart", 1002_u64)))
                .unwrap()
                .transform()
                .frame()
                .center();
            let end = start + egui::vec2(60.0, 0.0);
            chart_frame(
                &mut app,
                &ctx,
                vec![egui::Event::PointerMoved(start), chart_pointer(start, true)],
            );
            chart_frame(&mut app, &ctx, vec![egui::Event::PointerMoved(end)]);
            chart_frame(&mut app, &ctx, vec![chart_pointer(end, false)]);
            chart_frame(&mut app, &ctx, vec![]);
            let first = bounds(&ctx, 1001);
            let second = bounds(&ctx, 1002);
            assert_ne!(second.min()[0], initial_second.min()[0]);
            if independent {
                assert_eq!(first, initial_first);
            } else {
                assert!((first.min()[0] - second.min()[0]).abs() < 1e-8);
                assert!((first.max()[0] - second.max()[0]).abs() < 1e-8);
            }
        }
    }

    #[test]
    fn follow_uses_shared_latest_sample_and_preserves_horizontal_span() {
        let mut app = chart_app();
        let ctx = egui::Context::default();
        chart_frame(&mut app, &ctx, vec![]);
        chart_frame(&mut app, &ctx, vec![]);
        app.plot_tabs[0].saved.follow = true;
        app.plot_tabs[0].charts[1].series[0]
            .series
            .push(100.0, 150.0, 3);
        chart_frame(&mut app, &ctx, vec![]);
        assert_eq!(
            (bounds(&ctx, 1001).min()[0], bounds(&ctx, 1001).max()[0]),
            (40.0, 100.0)
        );
        assert_eq!(
            (bounds(&ctx, 1002).min()[0], bounds(&ctx, 1002).max()[0]),
            (40.0, 100.0)
        );
    }
    #[test]
    fn chart_menu_duplicates_with_fresh_ids_reorders_and_keeps_last_chart_removal_usable() {
        let mut app = chart_app();
        let ctx = egui::Context::default();
        app.plot_tabs[0].charts[0]
            .saved
            .rules
            .push(serialcore::config::DeviceExtractRule {
                id: 1101,
                ..Default::default()
            });
        app.plot_tabs[0].charts[0]
            .saved
            .styles
            .push(serialcore::config::PlotSeriesStyle {
                key: serialcore::config::PlotSeriesKey {
                    source_id: 11,
                    rule_id: 1101,
                    name: "temp".into(),
                },
                ..Default::default()
            });
        let click = |app: &mut App, label: &str| {
            chart_frame(app, &ctx, vec![]);
            let output = chart_frame(app, &ctx, vec![]);
            let pos = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == label => {
                        Some(text.galley.rect.translate(text.pos.to_vec2()).center())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("missing chart action {label}"));
            chart_frame(
                app,
                &ctx,
                vec![egui::Event::PointerMoved(pos), chart_pointer(pos, true)],
            );
            chart_frame(app, &ctx, vec![chart_pointer(pos, false)]);
        };
        click(&mut app, "⋯");
        click(&mut app, "Duplicate");
        let tab = &app.plot_tabs[0];
        assert_eq!(tab.charts.len(), 3);
        let copy = tab.charts[1].saved.id;
        assert_ne!(copy, 1001);
        assert_ne!(tab.charts[1].saved.rules[0].id, 1101);
        assert_eq!(
            tab.charts[1].saved.styles[0].key.rule_id,
            tab.charts[1].saved.rules[0].id
        );
        click(&mut app, "⋯");
        click(&mut app, "Move down");
        assert_eq!(app.plot_tabs[0].charts[0].saved.id, copy);
        for _ in 0..3 {
            click(&mut app, "⋯");
            click(&mut app, "Remove chart");
        }
        assert_eq!(app.active_plot, Some(1000));
        assert!(app.plot_tabs[0].charts.is_empty());
        click(&mut app, "Add chart");
        assert_eq!(app.plot_tabs[0].charts.len(), 1);
        assert!(app.plot_draft.is_some());
    }
}
