//! Two pane workspace. Connections stay unique; only the presentation is split.
use crate::app::{App, TabId};
use serialcore::config::{SavedPane, SavedSplit, SavedSplitDirection, SavedTab};
use serialcore::reader::ConnState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SplitDirection {
    Right,
    Below,
}

#[derive(Default)]
struct Pane {
    tabs: Vec<TabId>,
    selected: Option<TabId>,
    show_search: bool,
    search_focus_request: bool,
    selection_rows: Option<(egui::Id, u64, u64)>,
}

#[derive(Clone, Copy)]
enum LayoutAction {
    Split(TabId, SplitDirection),
    Move(TabId),
    Drop {
        tab: TabId,
        pane: usize,
        boundary: usize,
    },
    Join,
}

pub(crate) struct Workspace {
    pub(crate) split: Option<SplitDirection>,
    panes: [Pane; 2],
    focused: usize,
    rendering: Option<usize>,
    ratio: f32,
    pub(crate) resizing: bool,
    rects: [Option<egui::Rect>; 2],
    pending: Option<LayoutAction>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            split: None,
            panes: Default::default(),
            focused: 0,
            rendering: None,
            ratio: 0.5,
            resizing: false,
            rects: [None; 2],
            pending: None,
        }
    }
}

pub(super) fn show_panel(
    panel: egui::TopBottomPanel,
    ctx: &egui::Context,
    parent: Option<&mut egui::Ui>,
    draw: impl FnOnce(&mut egui::Ui),
) {
    if let Some(ui) = parent {
        panel.show_inside(ui, draw);
    } else {
        panel.show(ctx, draw);
    }
}

impl App {
    fn saved_workspace_tab(&self, tab: TabId) -> Option<SavedTab> {
        match tab {
            TabId::Connection(id) => self
                .connections
                .iter()
                .find(|conn| conn.id == id && conn.state != ConnState::Closed)
                .map(|conn| SavedTab::Connection {
                    identity: conn.identity.clone(),
                }),
            TabId::Merged(id) => self
                .merged_tabs
                .iter()
                .position(|tab| tab.id == id)
                .map(|index| SavedTab::Merged { index }),
        }
    }

    fn restored_workspace_tab(&self, saved: &SavedTab) -> Option<TabId> {
        match saved {
            SavedTab::Connection { identity } => self
                .connections
                .iter()
                .find(|conn| &conn.identity == identity)
                .map(|conn| TabId::Connection(conn.id)),
            SavedTab::Merged { index } => self
                .merged_tabs
                .get(*index)
                .map(|tab| TabId::Merged(tab.id)),
        }
    }

    fn saved_split(&self) -> Option<SavedSplit> {
        let direction = match self.workspace.split? {
            SplitDirection::Right => SavedSplitDirection::Right,
            SplitDirection::Below => SavedSplitDirection::Below,
        };
        let focused = self.workspace.focused;
        let order = self.ordered_tabs();
        let panes = std::array::from_fn(|index| {
            let tabs: Vec<_> = order
                .iter()
                .copied()
                .filter(|tab| {
                    self.workspace.panes[index].tabs.contains(tab)
                        || (index == focused
                            && !self
                                .workspace
                                .panes
                                .iter()
                                .any(|pane| pane.tabs.contains(tab)))
                })
                .filter_map(|tab| self.saved_workspace_tab(tab))
                .collect();
            let selected = if index == focused {
                self.selected_tab_id()
            } else {
                self.workspace.panes[index].selected
            }
            .and_then(|tab| self.saved_workspace_tab(tab))
            .filter(|tab| tabs.contains(tab))
            .or_else(|| tabs.first().cloned());
            SavedPane { tabs, selected }
        });
        if panes.iter().any(|pane| pane.tabs.is_empty()) {
            return None;
        }
        Some(SavedSplit {
            direction,
            ratio_per_mille: (self.workspace.ratio.clamp(0.1, 0.9) * 1000.0).round() as u16,
            focused_pane: focused,
            panes,
        })
    }

    /// Debounced with other settings; never serialize temporary render context.
    pub(crate) fn persist_workspace(&mut self) {
        if self.workspace.rendering.is_some() {
            return;
        }
        let saved = self.saved_split();
        if self.config.split != saved {
            self.config.split = saved;
            self.write_config();
        }
    }

    /// Called only after restoring connections, merged views, and tab order.
    pub(crate) fn restore_workspace(&mut self) {
        let Some(saved) = self.config.split.clone() else {
            return;
        };
        self.workspace = Workspace::default();
        self.workspace.split = Some(match saved.direction {
            SavedSplitDirection::Right => SplitDirection::Right,
            SavedSplitDirection::Below => SplitDirection::Below,
        });
        self.workspace.ratio = f32::from(saved.ratio_per_mille.clamp(100, 900)) / 1000.0;
        self.workspace.focused = saved.focused_pane.min(1);
        let mut assigned = Vec::new();
        for (index, pane) in saved.panes.iter().enumerate() {
            let tabs: Vec<_> = pane
                .tabs
                .iter()
                .filter_map(|tab| self.restored_workspace_tab(tab))
                .filter(|tab| {
                    if assigned.contains(tab) {
                        false
                    } else {
                        assigned.push(*tab);
                        true
                    }
                })
                .collect();
            let selected = pane
                .selected
                .as_ref()
                .and_then(|tab| self.restored_workspace_tab(tab))
                .filter(|tab| tabs.contains(tab))
                .or_else(|| tabs.first().copied());
            self.workspace.panes[index].tabs = tabs;
            self.workspace.panes[index].selected = selected;
        }
        // Load before reconciling so the startup default doesn't overwrite
        // the remembered selection of the focused pane.
        self.load_pane(self.workspace.focused);
        self.reconcile_workspace();
    }

    fn selected_tab_id(&self) -> Option<TabId> {
        if self.merged_selected {
            self.loaded_merged_tab
                .and_then(|i| self.merged_tabs.get(i))
                .map(|tab| TabId::Merged(tab.id))
        } else {
            self.connections
                .get(self.active)
                .map(|conn| TabId::Connection(conn.id))
        }
    }

    fn select_pane_tab(&mut self, id: TabId) {
        match id {
            TabId::Connection(id) => {
                if let Some(index) = self.connections.iter().position(|conn| conn.id == id) {
                    self.active = index;
                    self.merged_selected = false;
                }
            }
            TabId::Merged(id) => {
                if let Some(index) = self.merged_tabs.iter().position(|tab| tab.id == id) {
                    self.select_merged_tab(index);
                }
            }
        }
    }

    fn save_pane(&mut self, index: usize) {
        let selected = self.selected_tab_id();
        let pane = &mut self.workspace.panes[index];
        if selected.is_some_and(|id| pane.tabs.contains(&id)) {
            pane.selected = selected;
        }
        pane.show_search = self.show_search;
        pane.search_focus_request = self.search_focus_request;
        pane.selection_rows = self.selection_rows;
    }

    fn load_pane(&mut self, index: usize) {
        if let Some(id) = self.workspace.panes[index].selected {
            self.select_pane_tab(id);
        }
        let pane = &self.workspace.panes[index];
        self.show_search = pane.show_search;
        self.search_focus_request = pane.search_focus_request;
        self.selection_rows = pane.selection_rows;
    }

    /// Reconcile stable identities after connection creation/closure, including
    /// changes made by dialogs outside the workspace rendering pass.
    fn reconcile_workspace(&mut self) {
        let tabs = self.ordered_tabs();
        let selected = self.selected_tab_id();
        if self.workspace.split.is_none() {
            self.workspace.focused = 0;
            self.workspace.panes[0].tabs = tabs;
            self.workspace.panes[0].selected = selected;
            self.workspace.panes[1] = Pane::default();
            self.save_pane(0);
            return;
        }
        for pane in &mut self.workspace.panes {
            pane.tabs.retain(|id| tabs.contains(id));
        }
        for id in &tabs {
            if !self
                .workspace
                .panes
                .iter()
                .any(|pane| pane.tabs.contains(id))
            {
                self.workspace.panes[self.workspace.focused].tabs.push(*id);
            }
        }
        self.save_pane(self.workspace.focused);
        for pane in &mut self.workspace.panes {
            pane.tabs
                .sort_by_key(|id| tabs.iter().position(|tab| tab == id));
            if !pane.selected.is_some_and(|id| pane.tabs.contains(&id)) {
                pane.selected = pane.tabs.first().copied();
            }
        }
        if self.workspace.panes.iter().any(|pane| pane.tabs.is_empty()) {
            // Removing a pane never closes the connections it used to display.
            let survivor = usize::from(self.workspace.panes[0].tabs.is_empty());
            self.load_pane(survivor);
            self.workspace.split = None;
            self.workspace.focused = 0;
            self.reconcile_workspace();
        } else {
            self.load_pane(self.workspace.focused);
        }
    }

    fn focus_workspace_pane(&mut self, ctx: &egui::Context, index: usize) {
        if index == self.workspace.focused {
            return;
        }
        self.save_pane(self.workspace.focused);
        self.workspace.panes[self.workspace.focused].search_focus_request = false;
        let mut selection = egui::text_selection::LabelSelectionState::load(ctx);
        selection.clear_selection();
        selection.store(ctx);
        self.workspace.focused = index;
        self.load_pane(index);
        // A search field in the old pane must not retain input.
        if let Some(id) = ctx.memory(|m| m.focused()) {
            ctx.memory_mut(|m| m.surrender_focus(id));
        }
    }

    pub(crate) fn prepare_workspace(&mut self, ctx: &egui::Context) {
        self.reconcile_workspace();
        if self.workspace.split.is_none() || self.keyboard_overlay_open(ctx) {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F1)) {
            self.focus_workspace_pane(ctx, 1 - self.workspace.focused);
        }
        if let Some(pos) = ctx.input(|i| {
            (i.pointer.any_pressed() || !i.raw.dropped_files.is_empty())
                .then(|| i.pointer.interact_pos())
                .flatten()
        }) {
            if ctx.layer_id_at(pos) == Some(egui::LayerId::background()) {
                if let Some(index) = self
                    .workspace
                    .rects
                    .iter()
                    .position(|rect| rect.is_some_and(|rect| rect.contains(pos)))
                {
                    self.focus_workspace_pane(ctx, index);
                }
            }
        }
    }

    pub(crate) fn pane_input_enabled(&self) -> bool {
        self.workspace
            .rendering
            .is_none_or(|index| index == self.workspace.focused)
    }

    pub(crate) fn pane_widget_id(&self, name: &'static str) -> egui::Id {
        if let Some(index) = self.workspace.rendering {
            egui::Id::new(("workspace", index, name))
        } else {
            egui::Id::new(name)
        }
    }

    pub(crate) fn visible_pane_tabs(&self) -> Vec<TabId> {
        let tabs = self.ordered_tabs();
        if self.workspace.split.is_none() {
            return tabs;
        }
        let index = self.workspace.rendering.unwrap_or(self.workspace.focused);
        tabs.into_iter()
            .filter(|id| self.workspace.panes[index].tabs.contains(id))
            .collect()
    }

    /// Apply membership changes after rendering, so a dropped tab is never
    /// drawn twice and removing the last source tab can safely join the panes.
    pub(crate) fn drop_tab_in_header(&mut self, tab: TabId, boundary: usize) {
        let pane = self.workspace.rendering.unwrap_or(self.workspace.focused);
        if self.workspace.split.is_some() && !self.workspace.panes[pane].tabs.contains(&tab) {
            self.workspace.pending = Some(LayoutAction::Drop {
                tab,
                pane,
                boundary,
            });
        } else {
            self.reorder_tab(tab, boundary);
        }
    }

    pub(crate) fn show_tab_layout_menu(&mut self, ui: &mut egui::Ui, id: TabId) {
        if self.workspace.split.is_none() {
            let enabled = self.ordered_tabs().len() > 1;
            for (label, direction) in [
                ("Split right", SplitDirection::Right),
                ("Split below", SplitDirection::Below),
            ] {
                if ui
                    .add_enabled(enabled, egui::Button::new(label))
                    .on_disabled_hover_text("Open another connection or merged view to split")
                    .clicked()
                {
                    self.workspace.pending = Some(LayoutAction::Split(id, direction));
                    ui.close_menu();
                }
            }
        } else {
            if ui.button("Move to other pane").clicked() {
                self.workspace.pending = Some(LayoutAction::Move(id));
                ui.close_menu();
            }
            for (label, direction) in [
                ("Side by side", SplitDirection::Right),
                ("Stacked", SplitDirection::Below),
            ] {
                if ui
                    .selectable_label(self.workspace.split == Some(direction), label)
                    .clicked()
                {
                    self.workspace.split = Some(direction);
                    ui.close_menu();
                }
            }
            if ui.button("Close split (keep all tabs)").clicked() {
                self.workspace.pending = Some(LayoutAction::Join);
                ui.close_menu();
            }
        }
        ui.separator();
    }

    fn apply_layout_action(&mut self, action: LayoutAction) {
        self.reconcile_workspace();
        match action {
            LayoutAction::Split(id, direction) => {
                if self.workspace.split.is_some()
                    || self.ordered_tabs().len() < 2
                    || !self.workspace.panes[0].tabs.contains(&id)
                {
                    return;
                }
                self.workspace.split = Some(direction);
                self.workspace.ratio = 0.5;
                self.workspace.panes[0].tabs.retain(|tab| *tab != id);
                if self.workspace.panes[0].selected == Some(id) {
                    self.workspace.panes[0].selected =
                        self.workspace.panes[0].tabs.first().copied();
                }
                self.workspace.panes[1].tabs = vec![id];
                self.workspace.panes[1].selected = Some(id);
                self.workspace.focused = 1;
                self.load_pane(1);
            }
            LayoutAction::Move(id) => {
                if self.workspace.split.is_none() {
                    return;
                }
                if let Some(from) = self
                    .workspace
                    .panes
                    .iter()
                    .position(|pane| pane.tabs.contains(&id))
                {
                    let to = 1 - from;
                    self.workspace.panes[from].tabs.retain(|tab| *tab != id);
                    self.workspace.panes[to].tabs.push(id);
                    self.workspace.panes[to].selected = Some(id);
                    self.workspace.focused = to;
                    self.load_pane(to);
                }
            }
            LayoutAction::Drop {
                tab,
                pane,
                boundary,
            } => {
                if self.workspace.split.is_none() || !self.ordered_tabs().contains(&tab) {
                    return;
                }
                for group in &mut self.workspace.panes {
                    group.tabs.retain(|id| *id != tab);
                }
                self.workspace.panes[pane].tabs.push(tab);
                self.workspace.panes[pane].selected = Some(tab);
                self.reorder_tab(tab, boundary);
                self.workspace.focused = pane;
                self.load_pane(pane);
            }
            LayoutAction::Join => self.workspace.split = None,
        }
        self.reconcile_workspace();
        self.persist_workspace();
    }

    pub(crate) fn show_workspace(&mut self, ctx: &egui::Context, console_tab_claimed: bool) {
        self.reconcile_workspace();
        self.workspace.resizing = false;
        if let Some(direction) = self.workspace.split {
            egui::TopBottomPanel::top("workspace_toolbar").show(ctx, |ui| {
                ui.style_mut().interaction.selectable_labels = false;
                ui.horizontal(|ui| {
                    ui.weak("Click a pane to direct keyboard input");
                    self.show_app_menu(ui);
                });
            });
            egui::CentralPanel::default()
                .frame(egui::Frame::none())
                .show(ctx, |ui| {
                    let rect = ui.available_rect_before_wrap();
                    let horizontal = direction == SplitDirection::Right;
                    let length = if horizontal {
                        rect.width()
                    } else {
                        rect.height()
                    };
                    let gap = 6.0_f32.min(length.max(0.0));
                    let usable = (length - gap).max(0.0);
                    let minimum =
                        (if horizontal { 240.0_f32 } else { 150.0_f32 }).min(usable / 2.0);
                    let offset = (usable * self.workspace.ratio)
                        .clamp(minimum, (usable - minimum).max(minimum));
                    let (mut first, mut divider, mut second) = (rect, rect, rect);
                    if horizontal {
                        first.max.x = rect.min.x + offset;
                        divider.min.x = first.max.x;
                        divider.max.x = first.max.x + gap;
                        second.min.x = divider.max.x;
                    } else {
                        first.max.y = rect.min.y + offset;
                        divider.min.y = first.max.y;
                        divider.max.y = first.max.y + gap;
                        second.min.y = divider.max.y;
                    }
                    let drag = ui
                        .interact(
                            divider,
                            ui.id().with("divider"),
                            egui::Sense::click_and_drag(),
                        )
                        .on_hover_cursor(if horizontal {
                            egui::CursorIcon::ResizeHorizontal
                        } else {
                            egui::CursorIcon::ResizeVertical
                        });
                    self.workspace.resizing = drag.dragged() || drag.drag_stopped();
                    if drag.dragged() && usable > 0.0 {
                        if let Some(pos) = drag.interact_pointer_pos() {
                            let offset = if horizontal {
                                pos.x - rect.min.x
                            } else {
                                pos.y - rect.min.y
                            };
                            self.workspace.ratio = (offset / usable).clamp(0.1, 0.9);
                        }
                    }
                    if drag.double_clicked() {
                        self.workspace.ratio = 0.5;
                    }
                    ui.painter().rect_filled(
                        divider,
                        0.0,
                        ui.visuals().widgets.noninteractive.bg_stroke.color,
                    );
                    self.workspace.rects = [Some(first), Some(second)];
                    self.save_pane(self.workspace.focused);
                    // Draw focused content last so copying and search capture only
                    // see the selection belonging to the keyboard target.
                    // Allocate children in a fixed order even though the focused
                    // pane paints last. Otherwise changing focus changes auto
                    // widget IDs and loses the tab's in-progress pointer press.
                    let mut panes: [Option<egui::Ui>; 2] = std::array::from_fn(|index| {
                        Some(
                            ui.new_child(
                                egui::UiBuilder::new()
                                    .id_salt(("pane", index))
                                    .max_rect([first, second][index])
                                    .layout(egui::Layout::top_down(egui::Align::Min)),
                            ),
                        )
                    });
                    for index in [1 - self.workspace.focused, self.workspace.focused] {
                        let pane_rect = [first, second][index];
                        self.workspace.rendering = Some(index);
                        self.load_pane(index);
                        self.maintain_search();
                        let mut pane = panes[index].take().unwrap();
                        pane.set_clip_rect(pane_rect.intersect(ui.clip_rect()));
                        self.show_header_in(ctx, Some(&mut pane));
                        if self
                            .selected_tab_id()
                            .is_some_and(|id| self.visible_pane_tabs().contains(&id))
                        {
                            self.show_footer_in(ctx, Some(&mut pane));
                            self.show_plot_in(ctx, Some(&mut pane));
                            self.show_console_in(ctx, console_tab_claimed, Some(&mut pane));
                        }
                        self.save_pane(index);
                        if index == self.workspace.focused {
                            ui.painter().rect_stroke(
                                pane_rect.shrink(0.5),
                                0.0,
                                egui::Stroke::new(1.0_f32, ui.visuals().selection.bg_fill),
                            );
                        }
                    }
                    self.workspace.rendering = None;
                    self.load_pane(self.workspace.focused);
                });
        } else {
            self.workspace.rects = [None; 2];
            self.show_header(ctx);
            self.show_footer(ctx);
            self.show_plot(ctx);
            self.show_console(ctx, console_tab_claimed);
        }
        if let Some(action) = self.workspace.pending.take() {
            self.apply_layout_action(action);
        }
        self.persist_workspace();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{inert_handle, test_app};
    use serialcore::{
        config::PortConfig,
        reader::ConnState,
        store::{IncomingLine, LineFlags, PortId},
    };

    fn add_connection(app: &mut App, number: u32) {
        let id = PortId(number);
        let mut conn = app.make_connection(
            id,
            format!("device-{number}"),
            Default::default(),
            PortConfig {
                local_echo: true,
                ..Default::default()
            },
            inert_handle(id),
        );
        conn.state = ConnState::Connected;
        app.connections.push(conn);
    }

    fn append(app: &mut App, index: usize, text: &str) {
        let conn = &mut app.connections[index];
        conn.store.append(IncomingLine {
            text: text.into(),
            ts: app.clock.now(),
            port: conn.id,
            flags: LineFlags::default(),
            spans: Default::default(),
            cursor: None,
        });
    }

    fn frame(
        app: &mut App,
        ctx: &egui::Context,
        size: egui::Vec2,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ctx| {
                app.prepare_workspace(ctx);
                app.consume_tab_switch_shortcut(ctx);
                app.consume_console_view_shortcuts(ctx);
                let claimed = app.claim_console_tab_before_layout(ctx);
                app.show_workspace(ctx, claimed);
                app.release_console_tab_after_layout(ctx, claimed);
            },
        )
    }

    fn pointer(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn shortcut(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
        }
    }

    #[test]
    fn splitting_a_background_tab_preserves_the_original_selection() {
        let (mut app, _tx) = test_app("split-background-tab");
        for id in 1..=3 {
            add_connection(&mut app, id);
        }
        app.active = 1;
        app.apply_layout_action(LayoutAction::Split(
            TabId::Connection(PortId(3)),
            SplitDirection::Right,
        ));
        assert_eq!(
            app.workspace.panes[0].selected,
            Some(TabId::Connection(PortId(2)))
        );
    }

    #[test]
    fn closing_a_background_tab_preserves_the_panes_keyboard_target() {
        let (mut app, _tx) = test_app("split-close-background-tab");
        for id in 1..=4 {
            add_connection(&mut app, id);
        }
        app.apply_layout_action(LayoutAction::Split(
            TabId::Connection(PortId(4)),
            SplitDirection::Right,
        ));
        app.workspace.focused = 0;
        app.select_pane_tab(TabId::Connection(PortId(2)));
        app.reconcile_workspace();
        app.close_connection(0);
        app.reconcile_workspace();
        assert_eq!(app.selected_tab_id(), Some(TabId::Connection(PortId(2))));
        let ctx = egui::Context::default();
        frame(
            &mut app,
            &ctx,
            egui::vec2(1000.0, 600.0),
            vec![egui::Event::Text("command".into())],
        );
        assert_eq!(app.connections[0].tx_input, "command");
        assert!(app.connections[1].tx_input.is_empty());
    }

    #[test]
    fn moving_and_joining_panes_never_disconnects_or_duplicates_tabs() {
        let (mut app, _tx) = test_app("split-move-join");
        for id in 1..=3 {
            add_connection(&mut app, id);
        }
        app.apply_layout_action(LayoutAction::Split(
            TabId::Connection(PortId(2)),
            SplitDirection::Right,
        ));
        assert_eq!(app.workspace.panes[0].tabs.len(), 2);
        assert_eq!(
            app.workspace.panes[1].tabs,
            vec![TabId::Connection(PortId(2))]
        );
        app.apply_layout_action(LayoutAction::Move(TabId::Connection(PortId(3))));
        assert_eq!(app.workspace.panes[0].tabs.len(), 1);
        assert_eq!(app.workspace.panes[1].tabs.len(), 2);
        app.apply_layout_action(LayoutAction::Join);
        assert!(app.workspace.split.is_none());
        assert_eq!(app.visible_pane_tabs().len(), 3);
        assert_eq!(app.connections.len(), 3);
        assert!(app
            .connections
            .iter()
            .all(|conn| conn.state == ConnState::Connected));
    }

    #[test]
    fn closing_last_tab_in_a_pane_collapses_split_and_keeps_other_connection() {
        let (mut app, _tx) = test_app("split-close-tab");
        add_connection(&mut app, 1);
        add_connection(&mut app, 2);
        app.apply_layout_action(LayoutAction::Split(
            TabId::Connection(PortId(2)),
            SplitDirection::Below,
        ));
        app.close_connection(1);
        app.reconcile_workspace();
        assert!(app.workspace.split.is_none());
        assert_eq!(app.selected_tab_id(), Some(TabId::Connection(PortId(1))));
        assert_eq!(app.connections.len(), 1);
    }

    #[test]
    fn clicking_a_pane_routes_batched_typing_and_shortcuts_only_to_that_pane() {
        let (mut app, _tx) = test_app("split-input-focus");
        add_connection(&mut app, 1);
        add_connection(&mut app, 2);
        app.apply_layout_action(LayoutAction::Split(
            TabId::Connection(PortId(2)),
            SplitDirection::Right,
        ));
        let ctx = egui::Context::default();
        let size = egui::vec2(1000.0, 600.0);
        frame(&mut app, &ctx, size, vec![]);
        frame(
            &mut app,
            &ctx,
            size,
            vec![egui::Event::Text("right".into())],
        );
        assert_eq!(app.connections[1].tx_input, "right");
        assert!(app.connections[0].tx_input.is_empty());
        let pos = app.workspace.rects[0].unwrap().center();
        frame(
            &mut app,
            &ctx,
            size,
            vec![
                egui::Event::PointerMoved(pos),
                pointer(pos, true),
                pointer(pos, false),
                shortcut(egui::Key::W),
                egui::Event::Text("left".into()),
            ],
        );
        assert_eq!(app.workspace.focused, 0);
        assert_eq!(app.connections[0].tx_input, "left");
        assert_eq!(app.connections[1].tx_input, "right");
        assert!(app.connections[0].hex_view);
        assert!(!app.connections[1].hex_view);
    }

    #[test]
    fn f1_switches_panes_and_routes_typing_to_the_new_pane() {
        for direction in [SplitDirection::Right, SplitDirection::Below] {
            let (mut app, _tx) = test_app("split-f1-focus");
            add_connection(&mut app, 1);
            add_connection(&mut app, 2);
            app.apply_layout_action(LayoutAction::Split(TabId::Connection(PortId(2)), direction));
            let ctx = egui::Context::default();
            let size = egui::vec2(1000.0, 600.0);
            frame(&mut app, &ctx, size, vec![]);
            let f1 = || egui::Event::Key {
                key: egui::Key::F1,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            };
            for (pane, text) in [(0, "left"), (1, "right")] {
                frame(
                    &mut app,
                    &ctx,
                    size,
                    vec![f1(), egui::Event::Text(text.into())],
                );
                assert_eq!(app.workspace.focused, pane);
                assert_eq!(app.connections[pane].tx_input, text);
                assert!(!ctx.input(|i| i.events.iter().any(|event| matches!(
                    event,
                    egui::Event::Key {
                        key: egui::Key::F1,
                        ..
                    }
                ))));
            }
            app.show_keyboard_shortcuts = true;
            frame(&mut app, &ctx, size, vec![f1()]);
            assert_eq!(app.workspace.focused, 1);
            app.show_keyboard_shortcuts = false;
            app.apply_layout_action(LayoutAction::Join);
            frame(&mut app, &ctx, size, vec![f1()]);
            assert_eq!(app.workspace.focused, 0);
        }
    }

    #[test]
    fn tab_shortcut_preserves_focus_on_closed_connection() {
        let (mut app, _tx) = test_app("tab-closed-focus");
        add_connection(&mut app, 1);
        add_connection(&mut app, 2);
        app.connections[1].state = ConnState::Closed;
        let ctx = egui::Context::default();
        let size = egui::vec2(1000.0, 600.0);
        frame(&mut app, &ctx, size, vec![]);
        frame(&mut app, &ctx, size, vec![shortcut(egui::Key::Tab)]);
        assert_eq!(app.active, 1);
        assert!(ctx.memory(|m| m.focused().is_none()));
        frame(&mut app, &ctx, size, vec![shortcut(egui::Key::Tab)]);
        assert_eq!(app.active, 0, "Tab cycling must work after a closed tab");
    }

    #[test]
    fn tab_shortcuts_stay_in_the_focused_group_and_search_is_local() {
        let (mut app, _tx) = test_app("split-tab-search");
        for id in 1..=3 {
            add_connection(&mut app, id);
        }
        app.apply_layout_action(LayoutAction::Split(
            TabId::Connection(PortId(3)),
            SplitDirection::Right,
        ));
        app.workspace.focused = 0;
        app.load_pane(0);
        let ctx = egui::Context::default();
        let size = egui::vec2(1000.0, 600.0);
        frame(&mut app, &ctx, size, vec![shortcut(egui::Key::ArrowRight)]);
        assert_eq!(app.selected_tab_id(), Some(TabId::Connection(PortId(2))));
        frame(&mut app, &ctx, size, vec![shortcut(egui::Key::Tab)]);
        assert_eq!(app.selected_tab_id(), Some(TabId::Connection(PortId(1))));
        frame(&mut app, &ctx, size, vec![shortcut(egui::Key::Tab)]);
        assert_eq!(app.selected_tab_id(), Some(TabId::Connection(PortId(2))));
        assert_eq!(app.workspace.focused, 0);
        frame(&mut app, &ctx, size, vec![shortcut(egui::Key::F)]);
        assert!(app.workspace.panes[0].show_search);
        assert!(!app.workspace.panes[1].show_search);
        frame(&mut app, &ctx, size, vec![]);
        frame(
            &mut app,
            &ctx,
            size,
            vec![egui::Event::Text("needle".into())],
        );
        assert_eq!(app.connections[1].search_query, "needle");
        assert!(app.connections.iter().all(|conn| conn.tx_input.is_empty()));
    }

    #[test]
    fn two_merged_views_keep_independent_incremental_cursors_and_unread_counts() {
        let (mut app, _tx) = test_app("split-merged-cursors");
        add_connection(&mut app, 1);
        append(&mut app, 0, "first");
        app.create_merged_tab(vec![PortId(1)]);
        app.merged_follow = false;
        app.create_merged_tab(vec![PortId(1)]);
        let id = app.merged_tabs[1].id;
        app.apply_layout_action(LayoutAction::Split(
            TabId::Merged(id),
            SplitDirection::Right,
        ));
        // Show merged views in both panes, using the same serial source.
        let first = TabId::Merged(app.merged_tabs[0].id);
        app.workspace.panes[0].selected = Some(first);
        let ctx = egui::Context::default();
        let size = egui::vec2(1000.0, 600.0);
        frame(&mut app, &ctx, size, vec![]);
        app.select_merged_tab(0);
        let generation = app.merged_generation;
        app.select_merged_tab(1);
        append(&mut app, 0, "second");
        frame(&mut app, &ctx, size, vec![]);
        for index in [0, 1, 0] {
            app.select_merged_tab(index);
            assert_eq!(app.merged.len(), 2);
        }
        assert_eq!(
            app.merged_generation, generation,
            "switching panes must not rebuild history"
        );
        assert_eq!(app.merged_new_since_scroll, 1);
        app.clear_console(None);
        app.select_merged_tab(1);
        assert!(
            app.merged.is_empty(),
            "clear must invalidate parked views too"
        );
    }

    #[test]
    fn both_orientations_keep_headers_and_footers_inside_their_panes() {
        let (mut app, _tx) = test_app("split-layout-bounds");
        add_connection(&mut app, 1);
        add_connection(&mut app, 2);
        app.apply_layout_action(LayoutAction::Split(
            TabId::Connection(PortId(2)),
            SplitDirection::Right,
        ));
        let ctx = egui::Context::default();
        for direction in [SplitDirection::Right, SplitDirection::Below] {
            app.workspace.split = Some(direction);
            for size in [egui::vec2(800.0, 600.0), egui::vec2(500.0, 400.0)] {
                frame(&mut app, &ctx, size, vec![]);
                let output = frame(&mut app, &ctx, size, vec![]);
                let mut pins = 0;
                let mut headers = 0;
                for shape in &output.shapes {
                    if let egui::Shape::Text(text) = &shape.shape {
                        let label = text.galley.text();
                        if matches!(label, "Pin" | "Pinned") || label.starts_with("device-") {
                            let bounds = text.galley.rect.translate(text.pos.to_vec2());
                            assert!(
                                app.workspace
                                    .rects
                                    .iter()
                                    .flatten()
                                    .any(|pane| pane.contains_rect(bounds)),
                                "{label} outside pane: {bounds:?}"
                            );
                            if matches!(label, "Pin" | "Pinned") {
                                pins += 1;
                            } else {
                                headers += 1;
                            }
                        }
                    }
                }
                assert_eq!(pins, 2);
                assert_eq!(headers, 2);
            }
        }
    }

    #[test]
    fn scrolling_one_console_does_not_unpin_the_other() {
        let (mut app, _tx) = test_app("split-wheel-isolation");
        add_connection(&mut app, 1);
        add_connection(&mut app, 2);
        for index in 0..2 {
            for _ in 0..100 {
                append(&mut app, index, "a row of serial output");
            }
        }
        app.apply_layout_action(LayoutAction::Split(
            TabId::Connection(PortId(2)),
            SplitDirection::Right,
        ));
        let ctx = egui::Context::default();
        let size = egui::vec2(1000.0, 600.0);
        frame(&mut app, &ctx, size, vec![]);
        let pos = app.workspace.rects[0].unwrap().center();
        frame(
            &mut app,
            &ctx,
            size,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, 100.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(!app.connections[0].follow);
        assert!(app.connections[1].follow);
    }
    fn text_rect(output: &egui::FullOutput, label: &str) -> egui::Rect {
        output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == label => {
                    Some(text.galley.rect.translate(text.pos.to_vec2()))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing text: {label}"))
    }

    #[test]
    fn divider_resizes_both_orientations_without_unpinning_consoles() {
        let (mut app, _tx) = test_app("split-divider");
        add_connection(&mut app, 1);
        add_connection(&mut app, 2);
        app.apply_layout_action(LayoutAction::Split(
            TabId::Connection(PortId(2)),
            SplitDirection::Right,
        ));
        let ctx = egui::Context::default();
        let size = egui::vec2(1000.0, 600.0);
        for direction in [SplitDirection::Right, SplitDirection::Below] {
            app.workspace.split = Some(direction);
            app.workspace.ratio = 0.5;
            frame(&mut app, &ctx, size, vec![]);
            let first = app.workspace.rects[0].unwrap();
            let second = app.workspace.rects[1].unwrap();
            let start = if direction == SplitDirection::Right {
                egui::pos2((first.right() + second.left()) / 2.0, first.center().y)
            } else {
                egui::pos2(first.center().x, (first.bottom() + second.top()) / 2.0)
            };
            let end = start
                + if direction == SplitDirection::Right {
                    egui::vec2(70.0, 0.0)
                } else {
                    egui::vec2(0.0, 45.0)
                };
            frame(
                &mut app,
                &ctx,
                size,
                vec![egui::Event::PointerMoved(start), pointer(start, true)],
            );
            frame(&mut app, &ctx, size, vec![egui::Event::PointerMoved(end)]);
            frame(&mut app, &ctx, size, vec![pointer(end, false)]);
            assert!(app.workspace.ratio > 0.55);
            assert!(app.connections.iter().all(|conn| conn.follow));
        }
    }

    #[test]
    fn dragging_text_across_the_divider_never_copies_the_other_console() {
        let (mut app, _tx) = test_app("split-selection");
        add_connection(&mut app, 1);
        add_connection(&mut app, 2);
        append(&mut app, 0, "LEFT OUTPUT");
        append(&mut app, 1, "RIGHT OUTPUT");
        app.config.settings.timestamp_format = serialcore::config::TimestampFormat::None;
        app.apply_layout_action(LayoutAction::Split(
            TabId::Connection(PortId(2)),
            SplitDirection::Right,
        ));
        let ctx = egui::Context::default();
        let size = egui::vec2(1000.0, 600.0);
        frame(&mut app, &ctx, size, vec![]);
        let output = frame(&mut app, &ctx, size, vec![]);
        let left = text_rect(&output, "LEFT OUTPUT");
        let right = text_rect(&output, "RIGHT OUTPUT");
        let start = egui::pos2(left.left() + 1.0, left.center().y);
        let end = egui::pos2(right.right(), right.center().y);
        frame(
            &mut app,
            &ctx,
            size,
            vec![egui::Event::PointerMoved(start), pointer(start, true)],
        );
        frame(&mut app, &ctx, size, vec![egui::Event::PointerMoved(end)]);
        frame(&mut app, &ctx, size, vec![pointer(end, false)]);
        let output = frame(&mut app, &ctx, size, vec![egui::Event::Copy]);
        assert!(
            output.platform_output.copied_text.contains("LEFT OUTPUT"),
            "{:?}",
            output.platform_output.copied_text
        );
        assert!(!output.platform_output.copied_text.contains("RIGHT"));
        assert_eq!(app.workspace.focused, 0);
    }
    #[test]
    fn search_and_plots_leave_console_space_in_both_panes() {
        let (mut app, _tx) = test_app("split-plots-search");
        add_connection(&mut app, 1);
        add_connection(&mut app, 2);
        append(&mut app, 0, "left console");
        append(&mut app, 1, "right console");
        for conn in &mut app.connections {
            conn.show_plot = true;
        }
        app.apply_layout_action(LayoutAction::Split(
            TabId::Connection(PortId(2)),
            SplitDirection::Below,
        ));
        app.show_search = true;
        app.workspace.panes[0].show_search = true;
        let ctx = egui::Context::default();
        for direction in [SplitDirection::Below, SplitDirection::Right] {
            app.workspace.split = Some(direction);
            let size = egui::vec2(1000.0, 700.0);
            frame(&mut app, &ctx, size, vec![]);
            let output = frame(&mut app, &ctx, size, vec![]);
            for label in ["left console", "right console"] {
                let visible = output.shapes.iter().any(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == label => {
                        let bounds = text.galley.rect.translate(text.pos.to_vec2());
                        shape.clip_rect.intersect(bounds).height() > 5.0
                    }
                    _ => false,
                });
                assert!(visible, "{label} hidden with {direction:?} search and plot");
            }
        }
    }
    #[test]
    fn dragging_tabs_between_panes_moves_membership_and_selects_the_dropped_tab() {
        for direction in [SplitDirection::Right, SplitDirection::Below] {
            for blank_header in [false, true] {
                let (mut app, _tx) = test_app("split-tab-drag");
                for id in 1..=3 {
                    add_connection(&mut app, id);
                }
                app.apply_layout_action(LayoutAction::Split(
                    TabId::Connection(PortId(3)),
                    direction,
                ));
                let ctx = egui::Context::default();
                let size = egui::vec2(1200.0, 700.0);
                frame(&mut app, &ctx, size, vec![]);
                let output = frame(&mut app, &ctx, size, vec![]);
                let start = text_rect(&output, "device-1").center();
                let target = text_rect(&output, "device-3");
                let end = if blank_header {
                    egui::pos2(
                        app.workspace.rects[1].unwrap().right() - 20.0,
                        target.center().y,
                    )
                } else {
                    target.left_center()
                };
                frame(
                    &mut app,
                    &ctx,
                    size,
                    vec![egui::Event::PointerMoved(start), pointer(start, true)],
                );
                frame(&mut app, &ctx, size, vec![egui::Event::PointerMoved(end)]);
                frame(&mut app, &ctx, size, vec![pointer(end, false)]);
                assert_eq!(
                    app.workspace.panes[0].tabs,
                    vec![TabId::Connection(PortId(2))],
                    "direction={direction:?}, blank_header={blank_header}"
                );
                let expected = if blank_header {
                    vec![TabId::Connection(PortId(3)), TabId::Connection(PortId(1))]
                } else {
                    vec![TabId::Connection(PortId(1)), TabId::Connection(PortId(3))]
                };
                assert_eq!(app.workspace.panes[1].tabs, expected);
                assert_eq!(app.workspace.focused, 1);
                assert_eq!(app.selected_tab_id(), Some(TabId::Connection(PortId(1))));
                assert_eq!(app.connections.len(), 3);
            }
        }
    }
    #[test]
    fn dragging_last_connection_or_merged_tab_out_of_a_pane_joins_without_closing_it() {
        for merged in [false, true] {
            let (mut app, _tx) = test_app("split-last-tab-drag");
            for id in 1..=3 {
                add_connection(&mut app, id);
            }
            let (tab, label) = if merged {
                app.create_merged_tab(vec![PortId(1), PortId(2)]);
                (TabId::Merged(app.merged_tabs[0].id), "Merged 1")
            } else {
                (TabId::Connection(PortId(3)), "device-3")
            };
            app.apply_layout_action(LayoutAction::Split(tab, SplitDirection::Right));
            let ctx = egui::Context::default();
            let size = egui::vec2(1200.0, 700.0);
            frame(&mut app, &ctx, size, vec![]);
            let output = frame(&mut app, &ctx, size, vec![]);
            let start = text_rect(&output, label).center();
            let end = text_rect(&output, "device-1").left_center();
            frame(
                &mut app,
                &ctx,
                size,
                vec![egui::Event::PointerMoved(start), pointer(start, true)],
            );
            frame(&mut app, &ctx, size, vec![egui::Event::PointerMoved(end)]);
            frame(&mut app, &ctx, size, vec![pointer(end, false)]);
            assert!(app.workspace.split.is_none());
            assert_eq!(app.selected_tab_id(), Some(tab));
            assert_eq!(app.ordered_tabs()[0], tab);
            assert_eq!(app.connections.len(), 3);
            assert_eq!(app.merged_tabs.len(), usize::from(merged));
        }
    }

    #[test]
    fn overflowing_tab_strip_still_allows_reordering_by_drag() {
        let (mut app, _tx) = test_app("overflow-tab-drag");
        for id in 1..=8 {
            add_connection(&mut app, id);
        }
        let ctx = egui::Context::default();
        let size = egui::vec2(360.0, 600.0);
        frame(&mut app, &ctx, size, vec![]);
        let output = frame(&mut app, &ctx, size, vec![]);
        let start = text_rect(&output, "device-1").center();
        let end = text_rect(&output, "device-2").right_center();
        frame(
            &mut app,
            &ctx,
            size,
            vec![egui::Event::PointerMoved(start), pointer(start, true)],
        );
        frame(&mut app, &ctx, size, vec![egui::Event::PointerMoved(end)]);
        frame(&mut app, &ctx, size, vec![pointer(end, false)]);
        assert_eq!(
            &app.ordered_tabs()[..2],
            &[TabId::Connection(PortId(2)), TabId::Connection(PortId(1))]
        );
        assert_eq!(app.selected_tab_id(), Some(TabId::Connection(PortId(1))));
    }
    fn persistable_connections(app: &mut App) {
        for id in 1..=3 {
            add_connection(app, id);
            app.connections.last_mut().unwrap().identity.path_fallback =
                format!("test-device-{id}");
        }
        app.save_session();
    }

    #[test]
    fn split_roundtrips_through_disk_with_new_connection_and_merged_ids() {
        for direction in [SplitDirection::Right, SplitDirection::Below] {
            for focused in [0, 1] {
                let (mut app, _tx) = test_app("persist-split-source");
                persistable_connections(&mut app);
                app.create_merged_tab(vec![PortId(1), PortId(3)]);
                app.apply_layout_action(LayoutAction::Split(
                    TabId::Connection(PortId(2)),
                    direction,
                ));
                app.workspace.panes[0].selected = Some(TabId::Merged(app.merged_tabs[0].id));
                app.workspace.ratio = 0.637;
                app.workspace.focused = focused;
                app.load_pane(focused);
                // Exercise the actual exit hook, including a final unsaved resize.
                eframe::App::on_exit(&mut app, None);
                let saved = serialcore::config::Config::from_toml(
                    &std::fs::read_to_string(&app.paths.config_file).unwrap(),
                )
                .unwrap();
                assert_eq!(saved.split.as_ref().unwrap().ratio_per_mille, 637);
                let (mut restored, _tx2) = test_app("persist-split-restored");
                for (old, new) in [(3, 80), (1, 90), (2, 70)] {
                    add_connection(&mut restored, new);
                    restored
                        .connections
                        .last_mut()
                        .unwrap()
                        .identity
                        .path_fallback = format!("test-device-{old}");
                }
                restored.next_merged_id = 40;
                restored.config = saved.clone();
                restored.restore_merged_views();
                restored.restore_workspace();
                assert_eq!(restored.workspace.split, Some(direction));
                assert_eq!(restored.workspace.focused, focused);
                assert_eq!(restored.workspace.ratio, 0.637);
                assert_eq!(
                    restored.workspace.panes[0].selected,
                    Some(TabId::Merged(40))
                );
                assert_eq!(
                    restored.workspace.panes[1].selected,
                    Some(TabId::Connection(PortId(70)))
                );
                assert_eq!(restored.saved_split(), saved.split);
                assert_eq!(
                    restored.config, saved,
                    "restoration must not overwrite session metadata"
                );
            }
        }
    }

    #[test]
    fn closing_split_persists_single_view_and_old_configs_still_load() {
        let (mut app, _tx) = test_app("persist-close-split");
        persistable_connections(&mut app);
        app.apply_layout_action(LayoutAction::Split(
            TabId::Connection(PortId(2)),
            SplitDirection::Right,
        ));
        assert!(app.config.split.is_some());
        app.apply_layout_action(LayoutAction::Join);
        eframe::App::on_exit(&mut app, None);
        let saved = serialcore::config::Config::from_toml(
            &std::fs::read_to_string(&app.paths.config_file).unwrap(),
        )
        .unwrap();
        assert!(saved.split.is_none());
        assert_eq!(saved.last_open.len(), 3);
        assert!(serialcore::config::Config::from_toml("")
            .unwrap()
            .split
            .is_none());
    }

    #[test]
    fn stale_saved_tabs_are_ignored_and_an_empty_split_is_collapsed() {
        let (mut app, _tx) = test_app("persist-missing-tab");
        persistable_connections(&mut app);
        app.apply_layout_action(LayoutAction::Split(
            TabId::Connection(PortId(2)),
            SplitDirection::Below,
        ));
        let saved = app.config.clone();
        let (mut restored, _tx2) = test_app("persist-missing-restored");
        add_connection(&mut restored, 90);
        restored.connections[0].identity.path_fallback = "test-device-1".into();
        restored.config = saved;
        restored.restore_merged_views();
        restored.restore_workspace();
        assert!(restored.workspace.split.is_none());
        assert_eq!(
            restored.visible_pane_tabs(),
            vec![TabId::Connection(PortId(90))]
        );
        assert_eq!(
            restored.selected_tab_id(),
            Some(TabId::Connection(PortId(90)))
        );
    }

    #[test]
    fn restore_clamps_layout_values_and_deduplicates_tab_assignments() {
        let (mut app, _tx) = test_app("persist-layout-validation");
        persistable_connections(&mut app);
        app.apply_layout_action(LayoutAction::Split(
            TabId::Connection(PortId(2)),
            SplitDirection::Right,
        ));
        let saved = app.config.split.as_mut().unwrap();
        saved.ratio_per_mille = u16::MAX;
        saved.focused_pane = usize::MAX;
        saved.panes[1].tabs.push(saved.panes[0].tabs[0].clone());
        app.restore_workspace();
        assert_eq!(app.workspace.ratio, 0.9);
        assert_eq!(app.workspace.focused, 1);
        assert_eq!(app.workspace.panes[0].tabs.len(), 2);
        assert_eq!(
            app.workspace.panes[1].tabs,
            vec![TabId::Connection(PortId(2))]
        );
    }
}
