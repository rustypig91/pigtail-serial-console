//! Device-aware plot state and paced extraction. Rendering never parses source history.
use crate::app::{
    feeds_plot, history_limits, App, Connection, SeriesEntry, TabId, MAX_EXTRACTED_SERIES,
    MAX_TOTAL_SERIES_POINTS, SERIES_PALETTE,
};
use serialcore::config::{
    DeviceExtractRule, PlotSeriesKey, PlotSeriesStyle, PlotSource, SavedPlotChart, SavedPlotTab,
};
use serialcore::extract::CompiledExtract;
use serialcore::reader::ConnState;
use serialcore::series::Series;
use serialcore::store::LineFlags;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const MAX_PLOT_TABS: usize = 32;
pub(crate) const MAX_CHARTS: usize = 16;
pub(crate) const MAX_RULES: usize = 64;

static NEXT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn reserve_ids(config: &serialcore::config::Config) {
    let reserve = |id: u64| {
        if id < i64::MAX as u64 / 2 {
            NEXT.fetch_max(id, Ordering::Relaxed);
        }
    };
    for connection in &config.last_open {
        reserve(connection.id);
    }
    for plot in &config.plots {
        reserve(plot.id);
        for chart in &plot.charts {
            reserve(chart.id);
            for rule in &chart.rules {
                reserve(rule.id);
                if let Some(source) = &rule.source {
                    reserve(source.connection_id);
                }
            }
        }
    }
}

pub(crate) fn new_id() -> u64 {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    // Fits TOML's signed integers and stays unique across successive application runs.
    NEXT.fetch_max(seed, Ordering::Relaxed);
    NEXT.fetch_add(1, Ordering::Relaxed) + 1
}

pub(crate) fn source_of(conn: &Connection) -> PlotSource {
    PlotSource {
        connection_id: conn.persistent_id,
        identity: conn.identity.clone(),
        label: conn.display_label().to_owned(),
    }
}

pub(crate) struct RuleState {
    pub compiled: Option<CompiledExtract>,
    pub error: Option<String>,
    next: Option<u64>,
    boundary: u64,
    boundaries: VecDeque<u64>,
    boundaries_dropped: bool,
    clear_generation: Option<u64>,
}
impl RuleState {
    fn boundary_at(&self, first: u64) -> u64 {
        self.boundaries
            .iter()
            .rev()
            .find(|boundary| **boundary <= first)
            .copied()
            .unwrap_or_else(|| {
                if self.boundaries_dropped {
                    first.saturating_add(1)
                } else {
                    0
                }
            })
    }

    fn mark_boundary(&mut self, boundary: u64) {
        self.boundary = boundary;
        let position = self.boundaries.partition_point(|stored| *stored < boundary);
        if self.boundaries.get(position) != Some(&boundary) {
            self.boundaries.insert(position, boundary);
        }
        if self.boundaries.len() > 64 {
            self.boundaries.pop_front();
            self.boundaries_dropped = true;
        }
    }

    fn needs_update(&self, conn: &Connection, source: &PlotSource) -> bool {
        self.compiled.is_some()
            && (self.clear_generation != Some(conn.plot_clear_generation)
                || self
                    .next
                    .is_none_or(|next| next < conn.store.next_abs_index())
                || source.label != conn.display_label())
    }

    fn new(rule: &DeviceExtractRule) -> Self {
        let (compiled, error) = match CompiledExtract::compile(&rule.extraction) {
            Ok(compiled) => (Some(compiled), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            compiled,
            error,
            next: None,
            boundary: 0,
            boundaries: VecDeque::new(),
            boundaries_dropped: false,
            clear_generation: None,
        }
    }
}

pub(crate) struct PlotChart {
    pub saved: SavedPlotChart,
    pub series: Vec<SeriesEntry>,
    pub keys: Vec<PlotSeriesKey>,
    index: HashMap<PlotSeriesKey, usize>,
    pub rules: Vec<RuleState>,
    capacity: usize,
    requested_capacity: usize,
    pub fit: bool,
    pub limited: bool,
    pub history_unavailable: bool,
}
impl PlotChart {
    pub fn new(saved: SavedPlotChart) -> Self {
        let fit = saved.bounds.is_none();
        let rules = saved.rules.iter().map(RuleState::new).collect();
        Self {
            saved,
            series: vec![],
            keys: vec![],
            index: HashMap::new(),
            rules,
            capacity: 2,
            requested_capacity: 2,
            fit,
            limited: false,
            history_unavailable: false,
        }
    }

    pub fn remember_styles(&mut self) {
        for (key, entry) in self.keys.iter().zip(&self.series) {
            let style = PlotSeriesStyle {
                key: key.clone(),
                color: format!(
                    "#{:02x}{:02x}{:02x}",
                    entry.color.r(),
                    entry.color.g(),
                    entry.color.b()
                ),
                visible: entry.visible,
                secondary_axis: entry.own_axis,
            };
            if let Some(old) = self.saved.styles.iter_mut().find(|s| s.key == *key) {
                *old = style;
            } else if self.saved.styles.len() < MAX_EXTRACTED_SERIES * MAX_RULES {
                self.saved.styles.push(style);
            }
        }
    }

    fn point_capacity(&self, count: usize) -> usize {
        self.requested_capacity
            .min(self.capacity / count.max(1))
            .max(2)
    }

    fn apply_definition(&mut self, saved: SavedPlotChart) {
        self.remember_styles();
        let old_rules = std::mem::take(&mut self.saved.rules);
        let mut old: Vec<_> = old_rules
            .into_iter()
            .zip(std::mem::take(&mut self.rules))
            .collect();
        let retained: HashSet<_> = saved.rules.iter().map(|rule| rule.id).collect();
        let removed: Vec<_> = self
            .keys
            .iter()
            .filter(|key| !retained.contains(&key.rule_id))
            .map(|key| key.rule_id)
            .collect();
        for id in removed {
            self.remove_rule_data(id);
        }
        for rule in &saved.rules {
            let unchanged = old.iter().position(|(previous, _)| {
                previous.id == rule.id
                    && previous.extraction == rule.extraction
                    && previous.source.as_ref().map(|s| s.connection_id)
                        == rule.source.as_ref().map(|s| s.connection_id)
            });
            if let Some(i) = unchanged {
                self.rules.push(old.remove(i).1);
            } else {
                self.remove_rule_data(rule.id);
                self.rules.push(RuleState::new(rule));
                self.fit = true;
            }
        }
        self.saved = saved;
        for (key, entry) in self.keys.iter().zip(&mut self.series) {
            if let Some((r, rule)) = self
                .saved
                .rules
                .iter()
                .enumerate()
                .find(|(_, rule)| rule.id == key.rule_id)
            {
                if let Some(source) = &rule.source {
                    entry.series.rename(format!(
                        "{} · {} · rule {}",
                        source.label,
                        key.name,
                        r + 1
                    ));
                }
            }
        }
    }

    fn remove_rule_data(&mut self, rule_id: u64) {
        self.remember_styles();
        let mut entries = std::mem::take(&mut self.series).into_iter();
        self.keys.retain(|key| {
            let entry = entries.next().expect("one entry per key");
            if key.rule_id == rule_id {
                false
            } else {
                self.series.push(entry);
                true
            }
        });
        self.index = self
            .keys
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, key)| (key, i))
            .collect();
    }

    fn push(
        &mut self,
        key: PlotSeriesKey,
        label: &str,
        t: f64,
        value: f64,
        line: u64,
        boundary: u64,
    ) {
        let index = if let Some(index) = self.index.get(&key) {
            *index
        } else {
            if self.series.len() >= MAX_EXTRACTED_SERIES {
                self.limited = true;
                return;
            }
            let capacity = self.point_capacity(self.series.len() + 1);
            for entry in &mut self.series {
                self.limited |= entry.series.set_capacity(capacity);
                entry.series.shrink_to_fit();
            }
            let style = self.saved.styles.iter().find(|s| s.key == key);
            let color = style
                .and_then(|s| egui::Color32::from_hex(&s.color).ok())
                .unwrap_or(SERIES_PALETTE[self.series.len() % SERIES_PALETTE.len()]);
            self.series.push(SeriesEntry {
                series: Series::new(label, capacity),
                color,
                visible: style.is_none_or(|s| s.visible),
                own_axis: style.is_some_and(|s| s.secondary_axis),
            });
            self.keys.push(key.clone());
            let index = self.series.len() - 1;
            self.index.insert(key, index);
            index
        };
        self.limited |= self.series[index]
            .series
            .push_in_capture(t, value, line, boundary);
    }

    fn update_rule(
        &mut self,
        r: usize,
        conn: &Connection,
        max_lines: usize,
        deadline: Instant,
    ) -> bool {
        if self.rules[r].compiled.is_none() {
            return false;
        }
        let rule_id = self.saved.rules[r].id;
        let source = self.saved.rules[r]
            .source
            .as_mut()
            .expect("routed rule has a source");
        if source.label != conn.display_label() {
            source.label = conn.display_label().to_owned();
            for (key, entry) in self
                .keys
                .iter()
                .zip(&mut self.series)
                .filter(|(key, _)| key.rule_id == rule_id)
            {
                entry.series.rename(format!(
                    "{} · {} · rule {}",
                    conn.display_label(),
                    key.name,
                    r + 1
                ));
            }
        }
        if self.rules[r]
            .clear_generation
            .is_some_and(|generation| generation != conn.plot_clear_generation)
        {
            self.remove_rule_data(rule_id);
            self.rules[r].next = None;
            self.rules[r].boundary = 0;
            self.rules[r].boundaries.clear();
            self.rules[r].boundaries_dropped = false;
        }
        self.rules[r].clear_generation = Some(conn.plot_clear_generation);
        let first = conn.store.first_abs_index();
        self.history_unavailable |= first > 0;
        if self.rules[r].next.is_some_and(|next| next < first) {
            self.rules[r].mark_boundary(first);
            self.history_unavailable = true;
        }
        let mut next = self.rules[r].next.unwrap_or(first).max(first);
        let mut count = 0;
        let mut pairs = vec![];
        while next < conn.store.next_abs_index() && count < max_lines {
            if count > 0 && Instant::now() >= deadline {
                break;
            }
            let Some(line) = conn.store.get(next) else {
                next += 1;
                continue;
            };
            if line.meta.flags.contains(LineFlags::PROVISIONAL) {
                // Only the tail can still be replaced by a continuation. Local
                // echo can strand an earlier provisional line in the store;
                // skip that partial value without stalling all later output.
                if next + 1 == conn.store.next_abs_index() {
                    break;
                }
                next += 1;
                count += 1;
                continue;
            }
            if line.meta.flags.contains(LineFlags::RECONNECT_MARKER) {
                self.rules[r].mark_boundary(next + 1);
            }
            if feeds_plot(line.meta.flags) {
                self.rules[r]
                    .compiled
                    .as_ref()
                    .unwrap()
                    .extract(line.text, &mut pairs);
                let boundary = self.rules[r].boundary;
                for (name, value) in pairs.drain(..) {
                    let label = format!("{} · {} · rule {}", conn.display_label(), name, r + 1);
                    self.push(
                        PlotSeriesKey {
                            source_id: conn.persistent_id,
                            rule_id,
                            name,
                        },
                        &label,
                        line.meta.ts.micros as f64 / 1_000_000.0,
                        value,
                        next,
                        boundary,
                    );
                }
            }
            next += 1;
            count += 1;
        }
        self.rules[r].next = Some(next);
        next < conn.store.next_abs_index()
            && conn.store.get(next).is_some_and(|line| {
                !line.meta.flags.contains(LineFlags::PROVISIONAL)
                    || next + 1 < conn.store.next_abs_index()
            })
    }
}

pub(crate) struct PlotTab {
    pub saved: SavedPlotTab,
    pub charts: Vec<PlotChart>,
    pub fit: bool,
}
impl PlotTab {
    pub fn new(mut saved: SavedPlotTab) -> Self {
        let fit = saved.time_range.is_none();
        let charts = std::mem::take(&mut saved.charts)
            .into_iter()
            .map(PlotChart::new)
            .collect();
        Self { saved, charts, fit }
    }
    pub fn definition(&mut self) -> SavedPlotTab {
        let mut saved = self.saved.clone();
        saved.charts = self
            .charts
            .iter_mut()
            .map(|chart| {
                chart.remember_styles();
                chart.saved.clone()
            })
            .collect();
        saved
    }
}

pub(crate) struct PlotDraft {
    pub definition: SavedPlotTab,
    pub editing: Option<u64>,
    pub preview: Vec<String>,
    pub preview_dirty: bool,
    preview_engine: Option<PlotTab>,
    preview_work_cursor: usize,
}

impl App {
    pub(crate) fn update_plot_preview(&self, draft: &mut PlotDraft, budget: Duration) -> bool {
        if draft.preview_dirty {
            draft.preview_engine = Some(PlotTab::new(draft.definition.clone()));
            for chart in &mut draft.preview_engine.as_mut().unwrap().charts {
                chart.capacity = 128;
                chart.requested_capacity = 128;
            }
            draft.preview_dirty = false;
        }
        let deadline = Instant::now() + budget;
        let mut backlog = false;
        let mut messages = vec![];
        if let Some(tab) = &mut draft.preview_engine {
            let mut work = vec![];
            for (c, chart) in tab.charts.iter().enumerate() {
                for (r, rule) in chart.saved.rules.iter().enumerate() {
                    if let Some(source) = &rule.source {
                        let candidates: Vec<_> = self
                            .connections
                            .iter()
                            .enumerate()
                            .filter(|(_, conn)| conn.persistent_id == source.connection_id)
                            .collect();
                        if let [(i, conn)] = candidates.as_slice() {
                            if chart.rules[r].needs_update(conn, source) {
                                work.push((c, r, *i));
                            }
                        } else {
                            messages.push(format!(
                                "{}: {}; configuration can be saved.",
                                chart.saved.title,
                                self.plot_source_status(source)
                            ));
                        }
                    } else {
                        messages.push(format!(
                            "{} · rule {}: Select a device.",
                            chart.saved.title,
                            r + 1
                        ));
                    }
                }
            }
            if !work.is_empty() {
                let start = draft.preview_work_cursor % work.len();
                for offset in 0..work.len() {
                    let index = (start + offset) % work.len();
                    let (c, r, i) = work[index];
                    backlog |= tab.charts[c].update_rule(r, &self.connections[i], 4000, deadline);
                    draft.preview_work_cursor = index + 1;
                    if Instant::now() >= deadline && offset + 1 < work.len() {
                        backlog = true;
                        break;
                    }
                }
            }
            for chart in &tab.charts {
                for (r, state) in chart.rules.iter().enumerate() {
                    if let Some(error) = &state.error {
                        messages.push(format!("{} · rule {}: {error}", chart.saved.title, r + 1));
                    }
                }
                let found = if chart.series.is_empty() {
                    if backlog {
                        "Scanning retained history…".into()
                    } else {
                        "No matching series in retained output.".into()
                    }
                } else {
                    chart
                        .series
                        .iter()
                        .map(|entry| entry.series.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                messages.push(format!("{}: {found}", chart.saved.title));
            }
        }
        draft.preview = messages;
        backlog
    }

    pub(crate) fn restore_plots(&mut self) {
        reserve_ids(&self.config);
        let mut ids = HashSet::new();
        for conn in &mut self.connections {
            if conn.persistent_id == 0 {
                conn.persistent_id = new_id();
            }
            ids.insert(conn.persistent_id);
        }
        for mut saved in self.config.plots.clone().into_iter().take(MAX_PLOT_TABS) {
            let anchor = self.clock.start_wall().timestamp_micros();
            if let Some(previous) = saved.time_anchor_micros {
                let shift = (anchor as i128 - previous as i128) as f64 / 1_000_000.0;
                if let Some(range) = &mut saved.time_range {
                    for bound in range {
                        if let Ok(value) = bound.parse::<f64>() {
                            *bound = (value - shift).to_string();
                        }
                    }
                }
                for chart in &mut saved.charts {
                    if let Some(bounds) = &mut chart.bounds {
                        for index in [0, 2] {
                            if let Ok(value) = bounds[index].parse::<f64>() {
                                bounds[index] = (value - shift).to_string();
                            }
                        }
                    }
                }
            }
            saved.time_anchor_micros = Some(anchor);
            if saved.id == 0 || !ids.insert(saved.id) {
                saved.id = new_id();
                ids.insert(saved.id);
            }
            saved.charts.truncate(MAX_CHARTS);
            for chart in &mut saved.charts {
                if chart.id == 0 || !ids.insert(chart.id) {
                    chart.id = new_id();
                    ids.insert(chart.id);
                }
                chart.rules.truncate(MAX_RULES);
                chart.styles.truncate(MAX_EXTRACTED_SERIES * MAX_RULES);
                for rule in &mut chart.rules {
                    if rule.id == 0 || !ids.insert(rule.id) {
                        rule.id = new_id();
                        ids.insert(rule.id);
                    }
                }
            }
            self.plot_tabs.push(PlotTab::new(saved));
        }
        self.plot_routes_dirty = true;
        self.resize_plot_history();
    }

    pub(crate) fn persist_plots(&mut self) {
        for tab in &mut self.plot_tabs {
            tab.saved.time_anchor_micros = Some(self.clock.start_wall().timestamp_micros());
        }
        let plots = self
            .plot_tabs
            .iter_mut()
            .map(PlotTab::definition)
            .collect::<Vec<_>>();
        if self.config.plots != plots {
            self.config.plots = plots;
            self.write_config();
        }
    }

    pub(crate) fn resize_plot_history(&mut self) {
        let count = self
            .plot_tabs
            .iter()
            .map(|tab| tab.charts.len())
            .sum::<usize>()
            .max(1);
        let capacity = MAX_TOTAL_SERIES_POINTS / count;
        let requested = history_limits(self.config.settings.max_lines).series_points;
        for tab in &mut self.plot_tabs {
            for chart in &mut tab.charts {
                let grew = capacity > chart.capacity || requested > chart.requested_capacity;
                chart.capacity = capacity;
                chart.requested_capacity = requested;
                let point_capacity = chart.point_capacity(chart.series.len());
                for (key, entry) in chart.keys.iter().zip(&mut chart.series) {
                    chart.limited |= entry.series.set_capacity(point_capacity);
                    entry.series.shrink_to_fit();
                    if grew {
                        if let Some(conn) = self
                            .connections
                            .iter()
                            .find(|conn| conn.persistent_id == key.source_id)
                        {
                            entry
                                .series
                                .retain_before_line(conn.store.first_abs_index());
                        }
                    }
                }
                if grew {
                    for (rule, definition) in chart.rules.iter_mut().zip(&chart.saved.rules) {
                        rule.next = None;
                        if let Some(conn) = definition.source.as_ref().and_then(|source| {
                            self.connections
                                .iter()
                                .find(|conn| conn.persistent_id == source.connection_id)
                        }) {
                            // Restore provenance at the replay cutoff, even when its marker was evicted.
                            rule.boundary = rule.boundary_at(conn.store.first_abs_index());
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn maintain_plots(&mut self, budget: Duration) -> bool {
        if self.plot_routes_dirty {
            self.plot_routes.clear();
            for (p, tab) in self.plot_tabs.iter().enumerate() {
                for (c, chart) in tab.charts.iter().enumerate() {
                    for (r, rule) in chart.saved.rules.iter().enumerate() {
                        if let Some(source) = &rule.source {
                            self.plot_routes
                                .entry(source.connection_id)
                                .or_default()
                                .push((p, c, r));
                        }
                    }
                }
            }
            self.plot_routes_dirty = false;
        }
        let mut work: Vec<_> = self
            .connections
            .iter()
            .enumerate()
            .flat_map(|(i, conn)| {
                self.plot_routes
                    .get(&conn.persistent_id)
                    .into_iter()
                    .flatten()
                    .map(move |&(p, c, r)| (i, p, c, r))
            })
            .collect();
        work.retain(|&(i, p, c, r)| {
            let chart = &self.plot_tabs[p].charts[c];
            chart.rules[r].needs_update(
                &self.connections[i],
                chart.saved.rules[r].source.as_ref().expect("routed source"),
            )
        });
        let deadline = Instant::now() + budget;
        let mut backlog = false;
        if work.is_empty() {
            return false;
        }
        let start = self.plot_work_cursor % work.len();
        for offset in 0..work.len() {
            let index = (start + offset) % work.len();
            let (i, p, c, r) = work[index];
            // Equal-ID duplicates are ambiguous, never silently choose a source.
            if self
                .connections
                .iter()
                .filter(|conn| conn.persistent_id == self.connections[i].persistent_id)
                .count()
                != 1
            {
                continue;
            }
            backlog |=
                self.plot_tabs[p].charts[c].update_rule(r, &self.connections[i], 4000, deadline);
            self.plot_work_cursor = index + 1;
            if Instant::now() >= deadline && offset + 1 < work.len() {
                backlog = true;
                break;
            }
        }
        backlog
    }

    pub(crate) fn open_new_plot(&mut self) {
        if self.plot_draft.is_some() || self.plot_tabs.len() >= MAX_PLOT_TABS {
            return;
        }
        let source = if self.merged_selected {
            None
        } else {
            self.active_index().map(|i| source_of(&self.connections[i]))
        };
        let rule = DeviceExtractRule {
            id: new_id(),
            source,
            ..Default::default()
        };
        let chart = SavedPlotChart {
            id: new_id(),
            rules: vec![rule],
            ..Default::default()
        };
        self.plot_draft = Some(PlotDraft {
            definition: SavedPlotTab {
                id: new_id(),
                name: format!("Plot {}", self.plot_tabs.len() + 1),
                charts: vec![chart],
                ..Default::default()
            },
            editing: None,
            preview: vec![],
            preview_dirty: true,
            preview_engine: None,
            preview_work_cursor: 0,
        });
    }

    pub(crate) fn edit_plot(&mut self, id: u64) {
        if let Some(tab) = self.plot_tabs.iter_mut().find(|tab| tab.saved.id == id) {
            self.plot_draft = Some(PlotDraft {
                definition: tab.definition(),
                editing: Some(id),
                preview: vec![],
                preview_dirty: true,
                preview_engine: None,
                preview_work_cursor: 0,
            });
        }
    }

    pub(crate) fn apply_plot_draft(&mut self, mut draft: PlotDraft) {
        draft.definition.time_anchor_micros = Some(self.clock.start_wall().timestamp_micros());
        let id = draft.definition.id;
        if let Some(index) = draft
            .editing
            .and_then(|id| self.plot_tabs.iter().position(|tab| tab.saved.id == id))
        {
            // Keep untouched charts and view state, rebuild only edited extraction definitions.
            let tab = &mut self.plot_tabs[index];
            let mut saved = draft.definition;
            let mut old_charts = std::mem::take(&mut tab.charts);
            for definition in std::mem::take(&mut saved.charts) {
                if let Some(i) = old_charts
                    .iter()
                    .position(|chart| chart.saved.id == definition.id)
                {
                    let mut chart = old_charts.remove(i);
                    chart.apply_definition(definition);
                    tab.charts.push(chart);
                } else {
                    tab.charts.push(PlotChart::new(definition));
                }
            }
            tab.saved = saved;
        } else if draft.editing.is_none() && self.plot_tabs.len() < MAX_PLOT_TABS {
            self.tab_order = self.ordered_tabs();
            self.plot_tabs.push(PlotTab::new(draft.definition));
        } else {
            return;
        }
        self.plot_routes_dirty = true;
        self.resize_plot_history();
        self.select_workspace_tab(TabId::Plot(id));
        self.save_session();
    }

    pub(crate) fn select_plot(&mut self, id: u64) {
        if self.plot_tabs.iter().any(|tab| tab.saved.id == id) {
            self.active_plot = Some(id);
            self.merged_selected = false;
        }
    }

    pub(crate) fn plot_shortcut(&mut self) {
        if self.active_plot.is_some() || self.merged_selected {
            return;
        }
        let Some(index) = self.active_index() else {
            return;
        };
        let source = self.connections[index].persistent_id;
        let plot = self
            .plot_recent
            .iter()
            .rev()
            .copied()
            .chain(self.plot_tabs.iter().map(|tab| tab.saved.id))
            .find(|id| {
                self.plot_tabs.iter().any(|tab| {
                    tab.saved.id == *id
                        && tab.charts.iter().any(|chart| {
                            chart.saved.rules.iter().any(|r| {
                                r.source.as_ref().is_some_and(|s| s.connection_id == source)
                            })
                        })
                })
            });
        if let Some(id) = plot {
            self.select_workspace_tab(TabId::Plot(id));
        } else {
            self.open_new_plot();
        }
    }

    pub(crate) fn request_close_plot(&mut self, id: u64) {
        if self.config.settings.confirm_tab_close {
            self.tab_close_confirmation = Some((TabId::Plot(id), false));
        } else {
            self.close_plot(id);
        }
    }

    pub(crate) fn close_plot(&mut self, id: u64) {
        self.plot_tabs.retain(|tab| tab.saved.id != id);
        self.plot_recent.retain(|old| *old != id);
        if self.active_plot == Some(id) {
            self.active_plot = None;
            if let Some(tab) = self.ordered_tabs().last().copied() {
                self.select_workspace_tab(tab);
            }
        }
        self.plot_routes_dirty = true;
        self.resize_plot_history();
        self.save_session();
    }

    pub(crate) fn navigate_plot_sample(&mut self, source: u64, line: u64) {
        let matches: Vec<_> = self
            .connections
            .iter()
            .enumerate()
            .filter(|(_, conn)| conn.persistent_id == source)
            .map(|(i, _)| i)
            .collect();
        let message = match matches.as_slice() {
            [index] => {
                let conn = &mut self.connections[*index];
                if conn.store.get(line).is_some() {
                    conn.hex_view = false;
                    conn.screen_view = false;
                    conn.selected = Some(line);
                    conn.scroll_to = Some(line);
                    conn.follow = false;
                    // A filtered Log must reveal the actual source line.
                    conn.search_filter_suspended = true;
                    conn.filter_dirty = true;
                    let port = conn.id;
                    self.select_workspace_tab(TabId::Connection(port));
                    self.show_search = false;
                    self.save_session();
                    return;
                }
                "This source line has been evicted or cleared."
            }
            [] => "This source connection is unavailable. Rebind its rule in Settings.",
            _ => "This source reference is ambiguous. Rebind its rule in Settings.",
        };
        self.record_connect_error("Plot source", message.to_owned());
    }

    pub(crate) fn plot_source_status(&self, source: &PlotSource) -> String {
        let matches: Vec<_> = self
            .connections
            .iter()
            .filter(|conn| conn.persistent_id == source.connection_id)
            .collect();
        match matches.as_slice() {
            [conn] => format!(
                "{} — {}",
                conn.display_label(),
                match conn.state {
                    ConnState::Connected => "Connected",
                    ConnState::Reconnecting | ConnState::Connecting =>
                        "Device missing / reconnecting",
                    _ => "Unavailable",
                }
            ),
            [] => format!("{} — Unavailable", source.label),
            _ => format!("{} — Ambiguous source", source.label),
        }
    }

    pub(crate) fn plot_selected_line(&self, source: u64) -> Option<u64> {
        self.connections
            .iter()
            .find(|conn| conn.persistent_id == source)?
            .selected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{inert_handle, test_app};
    use crate::test_ui::ContextTestExt as _;
    use serialcore::clock::Timestamp;
    use serialcore::config::{ExtractMode, ExtractRule, PortConfig, PortIdentity};
    use serialcore::store::{IncomingLine, PortId};

    fn connection(app: &mut App, runtime: u32, persistent: u64, name: &str) {
        let id = PortId(runtime);
        let mut conn = app.make_connection(
            id,
            name.to_owned(),
            PortIdentity {
                path_fallback: format!("source-{persistent}"),
                ..Default::default()
            },
            PortConfig::default(),
            inert_handle(id),
        );
        conn.persistent_id = persistent;
        conn.state = ConnState::Connected;
        app.connections.push(conn);
    }
    fn line(app: &mut App, index: usize, text: &str, micros: i64, flags: LineFlags) -> u64 {
        let conn = &mut app.connections[index];
        conn.store.append(IncomingLine {
            text: text.to_owned(),
            ts: Timestamp {
                wall: chrono::Utc::now(),
                micros,
            },
            port: conn.id,
            flags,
            spans: Default::default(),
            cursor: None,
        })
    }
    fn rule(source: &Connection, id: u64) -> DeviceExtractRule {
        DeviceExtractRule {
            id,
            source: Some(source_of(source)),
            ..Default::default()
        }
    }
    fn plot(app: &mut App, rules: Vec<DeviceExtractRule>) -> u64 {
        let id = new_id();
        app.plot_tabs.push(PlotTab::new(SavedPlotTab {
            id,
            charts: vec![SavedPlotChart {
                id: new_id(),
                rules,
                ..Default::default()
            }],
            ..Default::default()
        }));
        app.plot_routes_dirty = true;
        app.resize_plot_history();
        id
    }
    fn settle(app: &mut App) {
        while app.maintain_plots(Duration::from_secs(1)) {}
    }

    #[test]
    fn inactive_plots_route_devices_and_overlapping_rules_without_collisions() {
        let (mut app, _tx) = test_app("plot-source-routing");
        connection(&mut app, 1, 11, "Device 1");
        connection(&mut app, 2, 22, "Device 2");
        line(&mut app, 0, "temp:1", -2_000_000, LineFlags::default());
        line(&mut app, 1, "temp:2", -1_000_000, LineFlags::default());
        let rules = vec![
            rule(&app.connections[0], 101),
            rule(&app.connections[1], 102),
            rule(&app.connections[0], 103),
        ];
        plot(&mut app, rules);
        let rules = vec![rule(&app.connections[0], 104)];
        plot(&mut app, rules);
        settle(&mut app);
        let chart = &app.plot_tabs[0].charts[0];
        assert_eq!(chart.series.len(), 3);
        for (key, entry) in chart.keys.iter().zip(&chart.series) {
            assert_eq!(
                entry.series.last().unwrap().value,
                if key.source_id == 11 { 1.0 } else { 2.0 }
            );
            assert!(entry.series.name().starts_with(if key.source_id == 11 {
                "Device 1 · temp"
            } else {
                "Device 2 · temp"
            }));
        }
        assert_ne!(
            chart.keys.iter().find(|key| key.rule_id == 101).unwrap(),
            chart.keys.iter().find(|key| key.rule_id == 103).unwrap()
        );
        line(&mut app, 0, "temp:3", 1_000_000, LineFlags::default());
        settle(&mut app);
        assert!(app.active_plot.is_none());
        assert_eq!(app.plot_tabs[1].charts[0].series[0].series.len(), 2);
        let chart = &app.plot_tabs[0].charts[0];
        let i = chart
            .keys
            .iter()
            .position(|key| key.source_id == 22)
            .unwrap();
        assert_eq!(chart.series[i].series.len(), 1);
    }

    #[test]
    fn finalized_output_is_independent_of_filters_and_partial_updates() {
        let (mut app, _tx) = test_app("plot-finalized-output");
        connection(&mut app, 1, 11, "Device");
        let rules = vec![rule(&app.connections[0], 101)];
        plot(&mut app, rules);
        app.connections[0].search_query = "excluded".into();
        app.connections[0].hex_view = true;
        let mut framer = serialcore::framer::Framer::new();
        let clock = serialcore::clock::SessionClock::new();
        let mut lines = vec![];
        framer.push(b"temp:23", clock.now(), &mut lines);
        let partial = framer.flush_provisional().unwrap();
        line(&mut app, 0, &partial.text, 0, partial.flags);
        settle(&mut app);
        assert!(app.plot_tabs[0].charts[0].series.is_empty());
        framer.push(b".4\n", clock.now(), &mut lines);
        line(&mut app, 0, &lines[0].text, 1, lines[0].flags);
        settle(&mut app);
        settle(&mut app);
        line(&mut app, 0, "temp:99", 2, LineFlags::TX_ECHO);
        settle(&mut app);
        let series = &app.plot_tabs[0].charts[0].series[0].series;
        assert_eq!(series.len(), 1);
        assert_eq!(series.last().unwrap().value, 23.4);
        assert_eq!(series.last().unwrap().line, 0);
    }

    #[test]
    fn local_echo_after_a_partial_line_does_not_block_later_samples() {
        let (mut app, _tx) = test_app("plot-partial-before-echo");
        connection(&mut app, 1, 11, "Device");
        let rules = vec![rule(&app.connections[0], 101)];
        plot(&mut app, rules);
        line(&mut app, 0, "noise", -1, LineFlags::default());
        line(&mut app, 0, "temp:2", 0, LineFlags::PROVISIONAL);
        settle(&mut app);
        line(&mut app, 0, "command", 1, LineFlags::TX_ECHO);
        let target = line(&mut app, 0, "temp:23.4", 2, LineFlags::CONTINUATION);
        // A batch ending immediately before a stranded partial still has work.
        app.plot_tabs[0].charts[0].rules[0].next = None;
        line(&mut app, 0, "temp:24", 3, LineFlags::PROVISIONAL);
        assert!(app.plot_tabs[0].charts[0].update_rule(0, &app.connections[0], 1, Instant::now(),));
        settle(&mut app);
        let chart = &app.plot_tabs[0].charts[0];
        assert_eq!(chart.series.len(), 1);
        let series = &chart.series[0].series;
        assert_eq!(series.len(), 1);
        assert_eq!(series.last().unwrap().value, 23.4);
        assert_eq!(series.last().unwrap().line, target);
    }

    #[test]
    fn restored_manual_chart_bounds_survive_sample_backfill() {
        for independent in [false, true] {
            let (mut app, _tx) = test_app("plot-restored-chart-bounds");
            connection(&mut app, 1, 11, "Device");
            let expected = ["-20", "-50", "-10", "50"].map(str::to_owned);
            app.config.plots = vec![SavedPlotTab {
                id: 5001,
                follow: false,
                independent_time_axes: independent,
                time_range: Some(["-20".into(), "-10".into()]),
                charts: vec![SavedPlotChart {
                    id: 5002,
                    follow: false,
                    bounds: Some(expected.clone()),
                    rules: vec![rule(&app.connections[0], 101)],
                    ..Default::default()
                }],
                ..Default::default()
            }];
            app.restore_plots();
            app.select_plot(5001);
            let ctx = egui::Context::default();
            let render = |app: &mut App| {
                ctx.run_test_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1000.0, 800.0),
                        )),
                        ..Default::default()
                    },
                    |ui| app.show_plot_tab(ui),
                );
            };
            // A restored device may remain unavailable until after the first paint.
            render(&mut app);
            line(&mut app, 0, "temp:23.4", -15_000_000, LineFlags::default());
            settle(&mut app);
            render(&mut app);
            render(&mut app);
            assert_eq!(app.plot_tabs[0].charts[0].saved.bounds, Some(expected));
        }
    }

    #[test]
    fn clear_invalidates_all_consumers_but_keeps_other_devices_and_styles() {
        let (mut app, _tx) = test_app("plot-source-clear");
        connection(&mut app, 1, 11, "One");
        connection(&mut app, 2, 22, "Two");
        line(&mut app, 0, "temp:1", 0, LineFlags::default());
        line(&mut app, 1, "temp:2", 0, LineFlags::default());
        let rules = vec![
            rule(&app.connections[0], 101),
            rule(&app.connections[1], 102),
        ];
        plot(&mut app, rules);
        settle(&mut app);
        app.plot_tabs[0].charts[0].series[0].color = egui::Color32::RED;
        app.clear_console(Some(PortId(1)));
        settle(&mut app);
        assert_eq!(app.plot_tabs[0].charts[0].series.len(), 1);
        assert_eq!(app.plot_tabs[0].charts[0].keys[0].source_id, 22);
        line(&mut app, 0, "temp:3", 1, LineFlags::default());
        settle(&mut app);
        let chart = &app.plot_tabs[0].charts[0];
        let i = chart
            .keys
            .iter()
            .position(|key| key.source_id == 11)
            .unwrap();
        assert_eq!(chart.series[i].series.len(), 1);
        assert_eq!(chart.series[i].color, egui::Color32::RED);
    }

    #[test]
    fn reconnect_preserves_samples_with_a_capture_gap_and_closure_never_retargets() {
        let (mut app, _tx) = test_app("plot-gaps-missing");
        connection(&mut app, 1, 11, "One");
        line(&mut app, 0, "temp:1", -3_000_000, LineFlags::default());
        let rules = vec![rule(&app.connections[0], 101)];
        plot(&mut app, rules);
        settle(&mut app);
        app.connections[0].state = ConnState::DeviceMissing;
        settle(&mut app);
        assert_eq!(app.plot_tabs[0].charts[0].series[0].series.len(), 1);
        line(
            &mut app,
            0,
            "capture",
            -2_000_000,
            LineFlags::RECONNECT_MARKER,
        );
        line(&mut app, 0, "temp:2", 1_000_000, LineFlags::default());
        app.connections[0].state = ConnState::Connected;
        settle(&mut app);
        assert_eq!(
            app.plot_tabs[0].charts[0].series[0]
                .series
                .decimate_segments(-4.0, 2.0, 100)
                .len(),
            2
        );
        app.config.settings.confirm_tab_close = false;
        app.close_connection(0);
        connection(&mut app, 2, 22, "One");
        line(&mut app, 0, "temp:9", 2_000_000, LineFlags::default());
        settle(&mut app);
        assert_eq!(app.plot_tabs[0].charts[0].series[0].series.len(), 2);
        assert!(app
            .plot_source_status(
                app.plot_tabs[0].charts[0].saved.rules[0]
                    .source
                    .as_ref()
                    .unwrap()
            )
            .ends_with("Unavailable"));
    }

    #[test]
    fn draft_cancel_and_apply_preserve_unedited_rules_and_series_identity() {
        let (mut app, _tx) = test_app("plot-draft-isolation");
        connection(&mut app, 1, 11, "One");
        connection(&mut app, 2, 22, "Two");
        line(&mut app, 0, "temp:1", 0, LineFlags::default());
        line(&mut app, 1, "temp:2", 0, LineFlags::default());
        let rules = vec![
            rule(&app.connections[0], 101),
            rule(&app.connections[1], 102),
        ];
        let id = plot(&mut app, rules);
        settle(&mut app);
        app.plot_tabs[0].charts[0].series[0].color = egui::Color32::RED;
        app.plot_tabs[0].charts[0].series[0].visible = false;
        app.plot_tabs[0].charts[0].series[0].own_axis = true;
        app.edit_plot(id);
        let mut cancelled = app.plot_draft.take().unwrap();
        cancelled.definition.charts[0].rules.clear();
        assert_eq!(app.plot_tabs[0].charts[0].saved.rules.len(), 2);
        app.edit_plot(id);
        let mut draft = app.plot_draft.take().unwrap();
        draft.definition.charts[0].rules[0].extraction.prefix = Some("temp:".into());
        // A missing second source has no history available to rebuild, so unchanged samples must survive Apply.
        app.connections.remove(1);
        app.apply_plot_draft(draft);
        settle(&mut app);
        let chart = &app.plot_tabs[0].charts[0];
        assert_eq!(chart.series.len(), 1);
        assert_eq!(chart.keys[0].source_id, 22);
        app.edit_plot(id);
        let mut draft = app.plot_draft.take().unwrap();
        draft.definition.charts[0].rules[0].extraction.prefix = None;
        app.apply_plot_draft(draft);
        settle(&mut app);
        let chart = &app.plot_tabs[0].charts[0];
        let i = chart
            .keys
            .iter()
            .position(|key| key.source_id == 11)
            .unwrap();
        assert_eq!(chart.series[i].color, egui::Color32::RED);
        assert!(!chart.series[i].visible);
        assert!(chart.series[i].own_axis);
    }

    #[test]
    fn navigation_selects_actual_source_and_rejects_evicted_missing_and_ambiguous_lines() {
        let (mut app, _tx) = test_app("plot-log-navigation");
        connection(&mut app, 1, 11, "Same");
        connection(&mut app, 2, 22, "Same");
        line(&mut app, 0, "temp:1", 0, LineFlags::default());
        let target = line(&mut app, 1, "temp:2", 0, LineFlags::default());
        app.navigate_plot_sample(22, target);
        assert_eq!(app.active_index(), Some(1));
        assert_eq!(app.connections[1].scroll_to, Some(target));
        app.connections[1].store.clear();
        app.navigate_plot_sample(22, target);
        assert!(app
            .connect_errors
            .back()
            .unwrap()
            .message
            .contains("evicted or cleared"));
        app.connections.remove(1);
        app.navigate_plot_sample(22, target);
        assert!(app
            .connect_errors
            .back()
            .unwrap()
            .message
            .contains("unavailable"));
        connection(&mut app, 3, 11, "Duplicate");
        app.navigate_plot_sample(11, 0);
        assert!(app
            .connect_errors
            .back()
            .unwrap()
            .message
            .contains("ambiguous"));
    }

    #[test]
    fn history_growth_backfills_resident_points_preserving_sparse_evicted_samples() {
        let (mut app, _tx) = test_app("plot-capacity-growth");
        connection(&mut app, 1, 11, "One");
        app.config.settings.max_lines = 10_000;
        let rules = vec![rule(&app.connections[0], 101)];
        plot(&mut app, rules);
        for i in 0..2000 {
            line(&mut app, 0, &format!("temp:{i}"), i, LineFlags::default());
        }
        settle(&mut app);
        assert_eq!(app.plot_tabs[0].charts[0].series[0].series.len(), 1000);
        app.config.settings.max_lines = 20_000;
        app.resize_plot_history();
        settle(&mut app);
        assert_eq!(app.plot_tabs[0].charts[0].series[0].series.len(), 2000);
        app.connections[0].store.set_max_lines(10);
        app.config.settings.max_lines = 30_000;
        app.resize_plot_history();
        settle(&mut app);
        assert_eq!(app.plot_tabs[0].charts[0].series[0].series.len(), 2000);
    }

    #[test]
    fn all_tabs_share_a_bounded_budget_and_series_count_limit() {
        let (mut app, _tx) = test_app("plot-global-budget");
        connection(&mut app, 1, 11, "One");
        app.config.settings.max_lines = usize::MAX;
        let text = (0..80)
            .map(|i| format!("key{i}:{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        line(&mut app, 0, &text, 0, LineFlags::default());
        for r in 1..=20 {
            let rules = vec![rule(&app.connections[0], r)];
            plot(&mut app, rules);
        }
        settle(&mut app);
        assert!(app
            .plot_tabs
            .iter()
            .all(|tab| tab.charts[0].series.len() == MAX_EXTRACTED_SERIES));
        let allocated_budget: usize = app
            .plot_tabs
            .iter()
            .flat_map(|tab| &tab.charts)
            .map(|chart| chart.point_capacity(chart.series.len()) * chart.series.len())
            .sum();
        assert!(allocated_budget <= MAX_TOTAL_SERIES_POINTS);
        assert!(app.plot_tabs.iter().all(|tab| tab.charts[0].limited));
    }

    #[test]
    fn definitions_and_sources_restore_with_new_runtime_ids_and_no_sample_copy() {
        let (mut app, _tx) = test_app("plot-persistence-source");
        connection(&mut app, 1, 11, "Device");
        line(&mut app, 0, "temp:4", -1000, LineFlags::default());
        let rules = vec![rule(&app.connections[0], 101)];
        let id = plot(&mut app, rules);
        settle(&mut app);
        app.select_workspace_tab(TabId::Plot(id));
        app.plot_tabs[0].charts[0].series[0].color = egui::Color32::RED;
        app.plot_tabs[0].charts[0].series[0].own_axis = true;
        app.plot_tabs[0].saved.independent_time_axes = true;
        app.save_session();
        let serialized = app.config.to_toml().unwrap();
        assert!(!serialized.contains("samples"));
        let (mut restored, _tx2) = test_app("plot-persistence-restored");
        connection(&mut restored, 90, 11, "Renamed");
        line(&mut restored, 0, "temp:4", -1000, LineFlags::default());
        restored.config = serialcore::config::Config::from_toml(&serialized).unwrap();
        restored.restore_plots();
        restored.restore_merged_views();
        restored.restore_workspace();
        settle(&mut restored);
        assert_eq!(restored.active_plot, Some(id));
        assert!(restored.plot_tabs[0].saved.independent_time_axes);
        let series = &restored.plot_tabs[0].charts[0].series[0];
        assert_eq!(series.color, egui::Color32::RED);
        assert!(series.own_axis);
        assert!(series.series.name().starts_with("Renamed"));
        restored.navigate_plot_sample(11, 0);
        assert_eq!(
            restored.selected_tab_id(),
            Some(TabId::Connection(PortId(90)))
        );
    }

    #[test]
    fn shortcut_reuses_most_recent_plot_and_plot_tabs_never_transmit() {
        let (mut app, _tx) = test_app("plot-shortcut-safety");
        connection(&mut app, 1, 11, "One");
        app.plot_shortcut();
        assert!(app.plot_draft.is_some());
        app.plot_draft = None;
        let rules = vec![rule(&app.connections[0], 101)];
        let first = plot(&mut app, rules.clone());
        let second = plot(&mut app, rules);
        app.select_workspace_tab(TabId::Plot(second));
        app.select_workspace_tab(TabId::Plot(first));
        app.select_workspace_tab(TabId::Connection(PortId(1)));
        app.plot_shortcut();
        assert_eq!(app.active_plot, Some(first));
        assert!(app.active_index().is_none());
        assert!(app.macro_target_port().is_none());
        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput {
            events: vec![egui::Event::Text("should not send".into())],
            ..Default::default()
        });
        assert!(!app.console_key_input(&ctx, 0));
        let _ = ctx.end_test_pass();
        assert!(app.connections[0].tx_input.is_empty());
    }

    #[test]
    fn invalid_regex_and_missing_sources_allow_draft_preview_and_creation() {
        let (mut app, _tx) = test_app("plot-invalid-preview");
        app.open_new_plot();
        let mut draft = app.plot_draft.take().unwrap();
        draft.definition.charts[0].rules[0].extraction = ExtractRule {
            mode: ExtractMode::Regex,
            pattern: Some("[".into()),
            prefix: None,
            kv_separators: None,
        };
        app.update_plot_preview(&mut draft, Duration::from_secs(1));
        assert!(draft.preview.iter().any(|s| s.contains("regex")));
        app.apply_plot_draft(draft);
        assert_eq!(app.plot_tabs.len(), 1);
        assert!(app.plot_tabs[0].charts[0].rules[0].error.is_some());
    }
    #[test]
    fn growing_history_keeps_capture_gaps_when_the_marker_was_evicted() {
        let (mut app, _tx) = test_app("plot-backfill-evicted-boundary");
        connection(&mut app, 1, 11, "One");
        app.config.settings.max_lines = 1;
        app.connections[0].store.set_max_lines(3);
        let rules = vec![rule(&app.connections[0], 101)];
        plot(&mut app, rules);
        for (i, text, flags) in [
            (0, "temp:1", LineFlags::default()),
            (1, "capture", LineFlags::RECONNECT_MARKER),
            (2, "noise", LineFlags::default()),
            (3, "noise", LineFlags::default()),
            (4, "temp:2", LineFlags::default()),
        ] {
            line(&mut app, 0, text, i * 1_000_000, flags);
            settle(&mut app);
        }
        assert!(app.connections[0].store.get(1).is_none());
        app.config.settings.max_lines = 100;
        app.resize_plot_history();
        settle(&mut app);
        assert_eq!(
            app.plot_tabs[0].charts[0].series[0]
                .series
                .decimate_segments(0.0, 5.0, 100)
                .len(),
            2
        );
    }
    #[test]
    fn restored_manual_time_ranges_rebase_to_the_new_app_clock() {
        let (mut app, _tx) = test_app("plot-clock-rebase");
        app.config.plots = vec![SavedPlotTab {
            id: 5001,
            follow: false,
            time_anchor_micros: Some(app.clock.start_wall().timestamp_micros() - 10_000_000),
            time_range: Some(["1".into(), "4".into()]),
            charts: vec![SavedPlotChart {
                id: 5002,
                follow: false,
                bounds: Some(["1".into(), "2".into(), "4".into(), "5".into()]),
                ..Default::default()
            }],
            ..Default::default()
        }];
        app.restore_plots();
        assert_eq!(
            app.plot_tabs[0].saved.time_range,
            Some(["-9".into(), "-6".into()])
        );
        assert_eq!(
            app.plot_tabs[0].charts[0].saved.bounds,
            Some(["-9".into(), "2".into(), "-6".into(), "5".into()])
        );
        assert!(!app.plot_tabs[0].saved.follow);
    }
}
