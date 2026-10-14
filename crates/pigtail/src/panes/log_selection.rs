//! Document selection for virtualized logs. Endpoints refer to stored lines,
//! not widgets that must be visited each frame. Only Copy reads the selected
//! history; painting and dragging visit the viewport alone.

use std::sync::Arc;

use egui::text::{CCursor, CharIndex};
use egui::text_selection::{CCursorRange, TextCursorState};
use serialcore::store::PortId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Row {
    pub port: PortId,
    pub abs: u64,
    pub micros: i64,
    pub entry: usize,
}

#[derive(Clone, Copy, Debug)]
struct Endpoint {
    row: Row,
    character: CharIndex,
}

#[derive(Clone, Copy, Debug)]
struct Selection {
    view: egui::Id,
    anchor: Endpoint,
    cursor: Endpoint,
}

impl Selection {
    fn sorted(self) -> [Endpoint; 2] {
        if (self.anchor.row.entry, self.anchor.character)
            <= (self.cursor.row.entry, self.cursor.character)
        {
            [self.anchor, self.cursor]
        } else {
            [self.cursor, self.anchor]
        }
    }

    fn range(self, row: Row, galley: &egui::Galley) -> Option<CCursorRange> {
        let [start, end] = self.sorted();
        if row.entry < start.row.entry || row.entry > end.row.entry {
            return None;
        }
        let first = if row.entry == start.row.entry {
            galley.clamp_cursor(&CCursor::new(start.character.0))
        } else {
            galley.begin()
        };
        let last = if row.entry == end.row.entry {
            galley.clamp_cursor(&CCursor::new(end.character.0))
        } else {
            galley.end()
        };
        Some(CCursorRange::two(first, last))
    }
}

struct PaintedRow {
    view: egui::Id,
    row: Row,
    layer: egui::LayerId,
    shape: egui::layers::ShapeIdx,
    galley: Arc<egui::Galley>,
}

#[derive(Default)]
struct State {
    selection: Option<Selection>,
    dragging: bool,
    pressed_text: bool,
    seen_view: bool,
    painted: Vec<PaintedRow>,
    viewports: egui::IdMap<egui::Rect>,
}

#[derive(Default)]
pub(super) struct DocumentSelection {
    states: egui::ViewportIdMap<State>,
}

impl egui::Plugin for DocumentSelection {
    fn debug_name(&self) -> &'static str {
        "LogDocumentSelection"
    }

    fn on_begin_pass(&mut self, ui: &mut egui::Ui) {
        let state = self.states.entry(ui.ctx().viewport_id()).or_default();
        state.pressed_text = false;
        state.seen_view = false;
        state.painted.clear();
        state.viewports.clear();
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            state.selection = None;
            state.dragging = false;
        }
    }

    fn on_end_pass(&mut self, ui: &mut egui::Ui) {
        let state = self.states.entry(ui.ctx().viewport_id()).or_default();
        if !state.seen_view || (ui.input(|i| i.pointer.primary_pressed()) && !state.pressed_text) {
            state.selection = None;
            state.dragging = false;
        }
        if !ui.input(|i| i.pointer.primary_down()) {
            state.dragging = false;
        }
        if state.selection.is_none() {
            state.painted.clear();
            return;
        }
        // The drag endpoint can change after earlier rows were visited. Paint
        // against the final range so both drag directions update in one frame.
        let mut visuals = ui.visuals().clone();
        let (background, foreground) = if visuals.dark_mode {
            (
                egui::Color32::from_rgb(35, 94, 166),
                egui::Color32::from_rgb(250, 252, 255),
            )
        } else {
            (
                egui::Color32::from_rgb(145, 195, 250),
                egui::Color32::from_rgb(10, 25, 45),
            )
        };
        visuals.selection.bg_fill = background;
        visuals.selection.stroke.color = foreground;
        for row in state.painted.drain(..) {
            let range = state
                .selection
                .filter(|s| s.view == row.view)
                .and_then(|s| s.range(row.row, &row.galley));
            if let Some(range) = range.filter(|range| !range.is_empty()) {
                let mut galley = row.galley;
                egui::text_selection::visuals::paint_text_selection(
                    &mut galley,
                    &visuals,
                    &range,
                    None,
                );
                ui.ctx().graphics_mut(|layers| {
                    if let Some(list) = layers.get_mut(row.layer) {
                        list.mutate_shape(row.shape, |shape| {
                            if let egui::Shape::Text(text) = &mut shape.shape {
                                text.galley = galley;
                            }
                        });
                    }
                });
            }
        }
    }
}

pub(super) fn has_selection(ctx: &egui::Context) -> bool {
    ctx.plugin_opt::<DocumentSelection>().is_some_and(|plugin| {
        plugin
            .lock()
            .states
            .get(&ctx.viewport_id())
            .is_some_and(|state| state.selection.is_some())
    }) || ctx
        .plugin::<egui::text_selection::LabelSelectionState>()
        .lock()
        .has_selection()
}

pub(super) fn clear_selection(ctx: &egui::Context) {
    if let Some(plugin) = ctx.plugin_opt::<DocumentSelection>() {
        plugin.lock().states.clear();
    }
    ctx.plugin::<egui::text_selection::LabelSelectionState>()
        .lock()
        .clear_selection();
}

pub(super) fn is_dragging(ctx: &egui::Context) -> bool {
    ctx.plugin_opt::<DocumentSelection>().is_some_and(|plugin| {
        plugin
            .lock()
            .states
            .get(&ctx.viewport_id())
            .is_some_and(|state| state.dragging)
    })
}

/// Re-resolve two endpoints, never the span between them. The resolver uses
/// stable port/absolute-line identities so eviction, filtering and late merged
/// data cannot silently transfer selection to different text.
pub(super) fn begin_view(
    ctx: &egui::Context,
    view: egui::Id,
    viewport: egui::Rect,
    resolve: impl Fn(Row) -> Option<Row>,
) {
    let plugin = ctx.plugin_or_default::<DocumentSelection>();
    let mut plugin = plugin.lock();
    let state = plugin.states.entry(ctx.viewport_id()).or_default();
    state.viewports.insert(view, viewport);
    if let Some(mut selection) = state.selection.filter(|s| s.view == view) {
        state.seen_view = true;
        if let (Some(anchor), Some(cursor)) =
            (resolve(selection.anchor.row), resolve(selection.cursor.row))
        {
            selection.anchor.row = anchor;
            selection.cursor.row = cursor;
            state.selection = Some(selection);
        } else {
            state.selection = None;
            state.dragging = false;
        }
    }
}

/// Paint one visible row, with hit-testing in character coordinates. Word and
/// line clicks use egui's existing Unicode selection rules.
pub(super) fn paint(
    ui: &egui::Ui,
    response: &egui::Response,
    rect: egui::Rect,
    galley: Arc<egui::Galley>,
    view: egui::Id,
    row: Row,
) {
    let plugin = ui.ctx().plugin::<DocumentSelection>();
    let mut plugin = plugin.lock();
    let state = plugin.states.entry(ui.ctx().viewport_id()).or_default();
    if state.selection.is_none() && !response.contains_pointer() {
        // Most rows on an ordinary frame need neither hit-testing nor any
        // selection bookkeeping. Keep their path as small as plain labels.
        drop(plugin);
        paint_galley(ui, response, rect, &galley, None);
        return;
    }
    let (pointer, primary_pressed, primary_down, shift) = ui.input(|i| {
        (
            i.pointer.interact_pos(),
            i.pointer.primary_pressed(),
            i.pointer.primary_down(),
            i.modifiers.shift,
        )
    });
    let clip = ui.clip_rect();
    let viewport = state.viewports.get(&view).copied().unwrap_or(clip);
    if response.hovered() || (state.dragging && primary_down) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
    }
    if clip.intersects(rect) {
        if let Some(pointer) = pointer {
            let cursor = galley.cursor_from_pos(pointer - rect.min);
            // Floating scrollbars overlap row bounds. Unlike contains_pointer,
            // hovered respects the widget owning the click/drag, so a scrollbar
            // press cannot start a document selection underneath it.
            if (primary_pressed && response.hovered())
                || response.double_clicked()
                || response.triple_clicked()
            {
                state.pressed_text = true;
                state.seen_view = true;
                state.dragging = primary_down;
                let mut local = TextCursorState::default();
                local.pointer_interaction(ui, response, cursor, &galley, false);
                let range = local
                    .range(&galley)
                    .unwrap_or_else(|| CCursorRange::one(cursor));
                let anchor = if shift {
                    state.selection.filter(|s| s.view == view).map(|s| s.anchor)
                } else {
                    None
                }
                .unwrap_or(Endpoint {
                    row,
                    character: range.secondary.index,
                });
                state.selection = Some(Selection {
                    view,
                    anchor,
                    cursor: Endpoint {
                        row,
                        character: range.primary.index,
                    },
                });
            } else if state.dragging
                && primary_down
                && state.selection.is_some_and(|s| s.view == view)
                && ((pointer.y >= rect.top() && pointer.y < rect.bottom())
                    || (pointer.y < viewport.top() && rect.top() <= viewport.top())
                    || (pointer.y >= viewport.bottom() && rect.bottom() >= viewport.bottom()))
            {
                state.selection.as_mut().unwrap().cursor = Endpoint {
                    row,
                    character: cursor.index,
                };
            }
            if response.secondary_clicked() {
                if let Some(range) = state
                    .selection
                    .filter(|s| s.view == view)
                    .and_then(|s| s.range(row, &galley))
                {
                    let [first, last] = range.sorted_cursors();
                    if first.index <= cursor.index && cursor.index < last.index {
                        super::log::mark_selection_menu_hit(ui.ctx());
                    }
                }
            }
        }
    }
    if let Some(selection) = state
        .selection
        .as_mut()
        .filter(|s| s.view == view && s.cursor.row == row)
    {
        let secondary = if selection.anchor.row == row {
            CCursor::new(selection.anchor.character.0)
        } else if selection.anchor.row.entry < row.entry {
            galley.begin()
        } else {
            galley.end()
        };
        let mut range = CCursorRange::two(secondary, CCursor::new(selection.cursor.character.0));
        let before = range;
        let os = ui.ctx().os();
        let changed = ui.input(|input| {
            let mut changed = false;
            for event in &input.events {
                changed |= range.on_event(os, event, &galley, response.id);
            }
            changed
        });
        if changed {
            selection.cursor.character = range.primary.index;
            if range.secondary != before.secondary {
                selection.anchor = Endpoint {
                    row,
                    character: range.secondary.index,
                };
            }
        }
    }
    let range = state
        .selection
        .filter(|s| s.view == view)
        .and_then(|s| s.range(row, &galley));
    let shape = paint_galley(ui, response, rect, &galley, range);
    if state.selection.is_some() {
        state.painted.push(PaintedRow {
            view,
            row,
            layer: ui.layer_id(),
            shape,
            galley: galley.clone(),
        });
    }
}

fn paint_galley(
    ui: &egui::Ui,
    response: &egui::Response,
    rect: egui::Rect,
    galley: &Arc<egui::Galley>,
    range: Option<CCursorRange>,
) -> egui::layers::ShapeIdx {
    let shape = ui.painter().add(egui::Shape::galley(
        rect.min,
        galley.clone(),
        ui.visuals().text_color(),
    ));
    let transform = ui
        .ctx()
        .layer_transform_to_global(ui.layer_id())
        .unwrap_or_default()
        * egui::emath::TSTransform::from_translation(rect.min.to_vec2());
    egui::text_selection::accesskit_text::update_accesskit_for_text_widget(
        ui.ctx(),
        response.id,
        range,
        egui::accesskit::Role::Label,
        transform,
        galley,
    );
    shape
}

pub(super) fn copy<'a>(
    ctx: &egui::Context,
    view: egui::Id,
    text_at: impl Fn(usize) -> Option<&'a str>,
) {
    if !ctx.input(|i| {
        i.events
            .iter()
            .any(|e| matches!(e, egui::Event::Copy | egui::Event::Cut))
    }) {
        return;
    }
    let selection = ctx
        .plugin::<DocumentSelection>()
        .lock()
        .states
        .get(&ctx.viewport_id())
        .and_then(|state| state.selection)
        .filter(|selection| selection.view == view);
    let Some(selection) = selection else {
        return;
    };
    let [start, end] = selection.sorted();
    let mut text = String::new();
    for entry in start.row.entry..=end.row.entry {
        let Some(line) = text_at(entry) else {
            continue;
        };
        let first = if entry == start.row.entry {
            byte_at(line, start.character)
        } else {
            0
        };
        let last = if entry == end.row.entry {
            byte_at(line, end.character)
        } else {
            line.len()
        };
        if entry > start.row.entry {
            text.push('\n');
        }
        text.push_str(&line[first..last.max(first)]);
    }
    if !text.is_empty() {
        ctx.copy_text(text);
    }
}

fn byte_at(text: &str, character: CharIndex) -> usize {
    text.char_indices()
        .nth(character.0)
        .map_or(text.len(), |(byte, _)| byte)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_ui::ContextTestExt as _;

    #[test]
    fn wrapped_unicode_word_and_line_clicks_copy_and_escape_clears() {
        let ctx = egui::Context::default();
        let view = egui::Id::new("word-selection");
        let id = view.with("text");
        let text = "alpha café 東京 omega";
        let row = Row {
            port: PortId(1),
            abs: 0,
            micros: 0,
            entry: 0,
        };
        let point = std::cell::Cell::new(egui::Pos2::ZERO);
        let draw = |events| {
            ctx.run_test_ui(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ui| {
                    begin_view(&ctx, view, ui.clip_rect(), Some);
                    let mut job = egui::text::LayoutJob::simple(
                        text.into(),
                        egui::FontId::monospace(14.0),
                        egui::Color32::WHITE,
                        70.0,
                    );
                    job.wrap.break_anywhere = true;
                    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
                    assert!(galley.rows.len() > 1);
                    let (rect, response) =
                        ui.allocate_exact_size(galley.size(), egui::Sense::hover());
                    point
                        .set(rect.min + galley.pos_from_cursor(CCursor::new(8)).center().to_vec2());
                    let response = ui
                        .interact(rect, id, egui::Sense::click_and_drag())
                        .union(response);
                    paint(ui, &response, rect, galley, view, row);
                    copy(&ctx, view, |_| Some(text));
                },
            )
        };
        let _ = draw(vec![]);
        let _ = draw(vec![]);
        let button = |pressed| egui::Event::PointerButton {
            pos: point.get(),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        for _ in 0..2 {
            let _ = draw(vec![egui::Event::PointerMoved(point.get()), button(true)]);
            let _ = draw(vec![button(false)]);
        }
        let output = draw(vec![egui::Event::Copy]);
        assert_eq!(
            super::super::log::copied_text(&output.platform_output),
            "café"
        );
        let _ = draw(vec![button(true)]);
        let _ = draw(vec![button(false)]);
        let output = draw(vec![egui::Event::Copy]);
        assert_eq!(
            super::super::log::copied_text(&output.platform_output),
            text
        );
        let _ = draw(vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        assert!(!has_selection(&ctx));
    }

    #[test]
    fn unicode_copy_reads_only_the_selected_lines_and_only_on_copy() {
        let ctx = egui::Context::default();
        ctx.add_plugin(DocumentSelection::default());
        let view = egui::Id::new("unicode-log");
        let row = |entry| Row {
            port: PortId(1),
            abs: entry as u64,
            micros: 0,
            entry,
        };
        ctx.plugin::<DocumentSelection>()
            .lock()
            .states
            .entry(ctx.viewport_id())
            .or_default()
            .selection = Some(Selection {
            view,
            anchor: Endpoint {
                row: row(1),
                character: 1.into(),
            },
            cursor: Endpoint {
                row: row(3),
                character: 2.into(),
            },
        });
        let visited = std::cell::RefCell::new(Vec::new());
        let draw = |events| {
            ctx.run_test_ui(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ui| {
                    begin_view(&ctx, view, ui.clip_rect(), Some);
                    copy(&ctx, view, |entry| {
                        visited.borrow_mut().push(entry);
                        Some(["outside", "é東京", "", "🦀café", "outside"][entry])
                    });
                },
            )
        };
        let _ = draw(vec![]);
        assert!(
            visited.borrow().is_empty(),
            "ordinary frames must not read selected history"
        );
        let output = draw(vec![egui::Event::Copy]);
        assert_eq!(*visited.borrow(), [1, 2, 3]);
        assert_eq!(
            super::super::log::copied_text(&output.platform_output),
            "東京\n\n🦀c"
        );
        clear_selection(&ctx);
        assert!(!has_selection(&ctx));
    }

    #[test]
    fn removed_endpoint_clears_selection_instead_of_selecting_replacement_text() {
        let ctx = egui::Context::default();
        ctx.add_plugin(DocumentSelection::default());
        let view = egui::Id::new("evicted-log");
        let row = Row {
            port: PortId(1),
            abs: 10,
            micros: 0,
            entry: 0,
        };
        ctx.plugin::<DocumentSelection>()
            .lock()
            .states
            .entry(ctx.viewport_id())
            .or_default()
            .selection = Some(Selection {
            view,
            anchor: Endpoint {
                row,
                character: 0.into(),
            },
            cursor: Endpoint {
                row,
                character: 4.into(),
            },
        });
        let output = ctx.run_test_ui(
            egui::RawInput {
                events: vec![egui::Event::Copy],
                ..Default::default()
            },
            |ui| {
                begin_view(&ctx, view, ui.clip_rect(), |_| None);
                copy(&ctx, view, |_| Some("replacement"));
            },
        );
        assert!(super::super::log::copied_text(&output.platform_output).is_empty());
        assert!(!has_selection(&ctx));
    }
}
