//! Shared window chrome, independent of the console's focused pane.
use crate::app::App;
use egui::{Color32, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2};

pub(super) const HEADER_HEIGHT: f32 = 27.0;

pub(super) fn header_style(ui: &mut Ui) {
    ui.spacing_mut().interact_size.y = HEADER_HEIGHT;
    // The largest header glyph is the 20-point "+". Its padding must not
    // force the row taller than the tabs.
    ui.spacing_mut().button_padding.y = ui
        .spacing()
        .button_padding
        .y
        .min(((HEADER_HEIGHT - 20.0) / 2.0).max(0.0));
}

/// The tab strip blends with terminal text beneath it; the toolbar and active
/// tab share an opaque surface so they join without a seam. The footer uses
/// the same surface color.
pub(super) fn header_fill(dark: bool, toolbar: bool) -> Color32 {
    match (dark, toolbar) {
        (true, false) => Color32::from_rgba_unmultiplied(15, 20, 27, 225),
        (true, true) => Color32::from_rgb(32, 41, 52),
        (false, false) => Color32::from_rgba_unmultiplied(235, 239, 245, 225),
        (false, true) => Color32::from_rgb(248, 250, 253),
    }
}

/// A paint slot underneath the header. The console populates it later in the
/// same pass with the actual rows behind the overlay, without giving those
/// rows pointer input or text selection in the header.
#[derive(Clone)]
pub(super) struct HeaderBackdrop {
    pub rect: Rect,
    pub painter: egui::Painter,
    pub slot: egui::layers::ShapeIdx,
    pass: u64,
}

impl HeaderBackdrop {
    pub fn reserve(painter: egui::Painter) -> Self {
        let slot = painter.add(egui::Shape::Noop);
        let pass = painter.ctx().cumulative_pass_nr();
        Self {
            rect: Rect::NOTHING,
            painter,
            slot,
            pass,
        }
    }

    pub fn current(ctx: &egui::Context, id: egui::Id) -> Option<Self> {
        ctx.data(|data| data.get_temp::<Self>(id))
            .filter(|backdrop| backdrop.pass == ctx.cumulative_pass_nr())
    }
}

pub(super) fn surface(ui: &Ui, raised: bool) -> Color32 {
    match (ui.visuals().dark_mode, raised) {
        (true, false) => Color32::from_rgb(15, 20, 27),
        (true, true) => Color32::from_rgb(25, 33, 43),
        (false, false) => Color32::from_rgb(235, 239, 245),
        (false, true) => Color32::from_rgb(248, 250, 253),
    }
}

/// Draw tabs ourselves so the status dot and close affordance have their own
/// space and the label remains neutral regardless of the connection state.
pub(super) fn device_tab(
    ui: &mut Ui,
    label: &str,
    selected: bool,
    status: Color32,
) -> (Response, bool) {
    let label_color = if selected {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    let font = egui::FontId::proportional(13.0);
    let galley = ui.painter().layout_no_wrap(label.into(), font, label_color);
    let width = galley.size().x + 64.0;
    let id = ui.next_auto_id();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, HEADER_HEIGHT), Sense::hover());
    let close_rect = Rect::from_center_size(
        Pos2::new(rect.right() - 17.0, rect.center().y),
        Vec2::new(24.0, HEADER_HEIGHT.min(24.0)),
    );
    let body = Rect::from_min_max(rect.min, Pos2::new(close_rect.left(), rect.bottom()));
    let response = ui.interact(body, id.with("tab"), Sense::click_and_drag());
    let close = ui.interact(close_rect, id.with("close"), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            label,
        )
    });
    close.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            ui.is_enabled(),
            format!("Close {label}"),
        )
    });
    let fill = if selected {
        header_fill(ui.visuals().dark_mode, true)
    } else if !ui.visuals().dark_mode {
        if response.hovered() {
            Color32::from_rgb(229, 235, 243)
        } else {
            Color32::from_rgb(216, 224, 234)
        }
    } else {
        surface(ui, response.hovered())
    };
    ui.painter().rect_filled(
        rect,
        egui::Rounding {
            nw: 8.0,
            ne: 8.0,
            sw: 0.0,
            se: 0.0,
        },
        fill,
    );
    ui.painter()
        .circle_filled(Pos2::new(rect.left() + 17.0, rect.center().y), 4.5, status);
    ui.painter().galley(
        Pos2::new(rect.left() + 31.0, rect.center().y - galley.size().y / 2.0),
        galley,
        label_color,
    );
    let center = close_rect.center();
    let color = if close.hovered() {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    for sign in [-1.0, 1.0] {
        ui.painter().line_segment(
            [
                center + Vec2::new(-3.5, -3.5 * sign),
                center + Vec2::new(3.5, 3.5 * sign),
            ],
            Stroke::new(1.1_f32, color),
        );
    }
    (response, close.on_hover_text("Close tab").clicked())
}

pub(super) fn window_controls(ui: &mut Ui) -> [Response; 3] {
    let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
    let controls = [
        ("Close window", egui::ViewportCommand::Close),
        (
            if maximized {
                "Restore window"
            } else {
                "Maximize window"
            },
            egui::ViewportCommand::Maximized(!maximized),
        ),
        ("Minimize window", egui::ViewportCommand::Minimized(true)),
    ];
    std::array::from_fn(|index| {
        let (tooltip, command) = &controls[index];
        let response = ui
            .add_sized([36.0, HEADER_HEIGHT], egui::Button::new(" ").frame(false))
            .on_hover_text(*tooltip);
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), *tooltip)
        });
        let c = response.rect.center();
        if response.hovered() || response.is_pointer_button_down_on() {
            let fill = if index == 0 {
                Color32::from_rgb(182, 48, 62)
            } else {
                ui.visuals().widgets.hovered.bg_fill
            };
            ui.painter().rect_filled(response.rect, 4.0, fill);
        }
        let stroke = Stroke::new(1.2_f32, ui.visuals().text_color());
        match index {
            0 => {
                for sign in [-1.0, 1.0] {
                    ui.painter().line_segment(
                        [
                            c + Vec2::new(-4.0, -4.0 * sign),
                            c + Vec2::new(4.0, 4.0 * sign),
                        ],
                        stroke,
                    );
                }
            }
            1 => {
                if maximized {
                    ui.painter().rect_stroke(
                        Rect::from_center_size(c + Vec2::new(1.5, -1.5), Vec2::splat(9.0)),
                        0.0,
                        stroke,
                    );
                }
                ui.painter()
                    .rect_stroke(Rect::from_center_size(c, Vec2::splat(10.0)), 0.0, stroke);
            }
            _ => {
                ui.painter()
                    .line_segment([c + Vec2::new(-5.0, 0.0), c + Vec2::new(5.0, 0.0)], stroke);
            }
        }
        if response.clicked() {
            ui.ctx().send_viewport_cmd(command.clone());
        }
        response
    })
}

pub(super) fn drag_window(ui: &mut Ui) {
    let (_, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(0.0), HEADER_HEIGHT),
        Sense::click_and_drag(),
    );
    let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
    let press_id = response.id.with("last_window_press");
    let options = ui.ctx().options(|options| options.input_options.clone());
    let (time, pos) = ui.input(|input| (input.time, input.pointer.interact_pos()));
    // Native dragging can consume the release event. Detect the second press
    // ourselves, before starting another native drag, so double-click works
    // even when egui never sees a complete first click.
    if let Some(pos) = pos {
        ui.ctx().data_mut(|data| {
            if data
                .get_temp::<(f64, Pos2)>(press_id)
                .is_some_and(|(last_time, last_pos)| {
                    time - last_time > options.max_double_click_delay
                        || pos.distance(last_pos) > options.max_click_dist
                })
            {
                data.remove::<(f64, Pos2)>(press_id);
            }
        });
    }
    // Native window dragging must start on the press, before egui's
    // click-and-drag motion threshold (and never again while moving).
    if response.is_pointer_button_down_on()
        && ui.input(|input| input.pointer.button_pressed(egui::PointerButton::Primary))
    {
        let double_press = ui.ctx().data_mut(|data| {
            let previous = data.get_temp::<(f64, Pos2)>(press_id);
            data.remove::<(f64, Pos2)>(press_id);
            if previous.is_some() {
                true
            } else {
                if let Some(pos) = pos {
                    data.insert_temp(press_id, (time, pos));
                }
                false
            }
        });
        ui.ctx().send_viewport_cmd(if double_press {
            egui::ViewportCommand::Maximized(!maximized)
        } else {
            egui::ViewportCommand::StartDrag
        });
    }
    response.on_hover_text("Drag to move · Double-click to maximize");
}

impl App {
    /// Frameless windows need explicit resize hit targets, including corners.
    pub(crate) fn show_window_resize(&self, ctx: &egui::Context) {
        if ctx.input(|i| {
            i.viewport().maximized.unwrap_or(false) || i.viewport().fullscreen.unwrap_or(false)
        }) {
            return;
        }
        let rect = ctx.screen_rect();
        let edge = 8.0;
        let corner = 20.0;
        use egui::ResizeDirection::*;
        let targets = [
            (
                Rect::from_min_max(
                    rect.min,
                    egui::pos2(rect.left() + corner, rect.top() + corner),
                ),
                NorthWest,
                egui::CursorIcon::ResizeNwSe,
            ),
            (
                Rect::from_min_max(
                    egui::pos2(rect.right() - corner, rect.top()),
                    egui::pos2(rect.right(), rect.top() + corner),
                ),
                NorthEast,
                egui::CursorIcon::ResizeNeSw,
            ),
            (
                Rect::from_min_max(
                    egui::pos2(rect.left(), rect.bottom() - corner),
                    egui::pos2(rect.left() + corner, rect.bottom()),
                ),
                SouthWest,
                egui::CursorIcon::ResizeNeSw,
            ),
            (
                Rect::from_min_max(
                    egui::pos2(rect.right() - corner, rect.bottom() - corner),
                    rect.max,
                ),
                SouthEast,
                egui::CursorIcon::ResizeNwSe,
            ),
            (
                Rect::from_min_max(
                    egui::pos2(rect.left() + corner, rect.top()),
                    egui::pos2(rect.right() - corner, rect.top() + edge),
                ),
                North,
                egui::CursorIcon::ResizeVertical,
            ),
            (
                Rect::from_min_max(
                    egui::pos2(rect.left() + corner, rect.bottom() - edge),
                    egui::pos2(rect.right() - corner, rect.bottom()),
                ),
                South,
                egui::CursorIcon::ResizeVertical,
            ),
            (
                Rect::from_min_max(
                    egui::pos2(rect.left(), rect.top() + corner),
                    egui::pos2(rect.left() + edge, rect.bottom() - corner),
                ),
                West,
                egui::CursorIcon::ResizeHorizontal,
            ),
            (
                Rect::from_min_max(
                    egui::pos2(rect.right() - edge, rect.top() + corner),
                    egui::pos2(rect.right(), rect.bottom() - corner),
                ),
                East,
                egui::CursorIcon::ResizeHorizontal,
            ),
        ];
        let hovered_target = ctx
            .input(|input| input.pointer.hover_pos())
            .and_then(|pos| {
                targets
                    .iter()
                    .find(|(target, _, _)| target.contains(pos))
                    .map(|(_, direction, cursor)| (*direction, *cursor))
            });
        for (index, (target, _, _)) in targets.into_iter().enumerate() {
            // An Area registers the layer for hit testing. A standalone Ui
            // on a foreground layer can lose hover and presses to the panels.
            let id = egui::Id::new("window_resize").with(index);
            ctx.move_to_top(egui::LayerId::new(egui::Order::Foreground, id));
            egui::Area::new(id)
                .order(egui::Order::Foreground)
                .fixed_pos(target.min)
                .movable(false)
                .constrain(false)
                .default_size(target.size())
                .show(ctx, |ui| {
                    ui.allocate_exact_size(target.size(), Sense::click_and_drag());
                });
        }
        // Hover must also work on the first frame after the pointer enters
        // the window or its bounds change, before widget hit tests settle.
        if let Some((_, cursor)) = hovered_target {
            ctx.set_cursor_icon(cursor);
        }
        // Use the press event's position: subsequent motion in the same frame
        // can move the pointer into or out of an edge. Hit-test current bounds
        // so presses also work immediately after a window size change.
        let pressed_target = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    ..
                } => targets
                    .iter()
                    .find(|(target, _, _)| target.contains(*pos))
                    .map(|(_, direction, _)| *direction),
                _ => None,
            })
        });
        if let Some(direction) = pressed_target {
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
    }
}

pub(super) enum ActionIcon {
    Search,
    Clear,
    Plot,
}

pub(super) enum SearchControl {
    Previous,
    Next,
    Close,
}

pub(super) fn search_control(ui: &mut Ui, control: SearchControl) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(28.0, 28.0), Sense::click());
    let visuals = ui.style().interact(&response);
    ui.painter()
        .rect(rect, 5.0, visuals.bg_fill, visuals.bg_stroke);
    let center = rect.center();
    let point = |x, y| center + Vec2::new(x, y);
    let stroke = Stroke::new(1.4_f32, visuals.fg_stroke.color);
    let tooltip = match control {
        SearchControl::Previous | SearchControl::Next => {
            let up = matches!(control, SearchControl::Previous);
            let sign = if up { -1.0 } else { 1.0 };
            ui.painter()
                .line_segment([point(0.0, -5.0), point(0.0, 5.0)], stroke);
            ui.painter().add(egui::Shape::line(
                vec![point(-4.0, sign), point(0.0, 5.0 * sign), point(4.0, sign)],
                stroke,
            ));
            if up {
                "Previous match · Shift+Enter"
            } else {
                "Next match · Enter"
            }
        }
        SearchControl::Close => {
            ui.painter()
                .line_segment([point(-4.0, -4.0), point(4.0, 4.0)], stroke);
            ui.painter()
                .line_segment([point(-4.0, 4.0), point(4.0, -4.0)], stroke);
            "Close search · Esc"
        }
    };
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), tooltip)
    });
    response.on_hover_text(tooltip)
}

pub(super) fn search_warning(ui: &mut Ui, error: &str) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(22.0, 28.0), Sense::hover());
    let c = rect.center();
    let stroke = Stroke::new(1.2_f32, ui.visuals().warn_fg_color);
    ui.painter().add(egui::Shape::closed_line(
        vec![
            c + Vec2::new(0.0, -7.0),
            c + Vec2::new(7.0, 6.0),
            c + Vec2::new(-7.0, 6.0),
        ],
        stroke,
    ));
    ui.painter()
        .line_segment([c + Vec2::new(0.0, -2.0), c + Vec2::new(0.0, 1.0)], stroke);
    ui.painter()
        .circle_filled(c + Vec2::new(0.0, 3.5), 0.8, stroke.color);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), "Invalid regex")
    });
    response.on_hover_text(format!("Invalid regex\n{error}"));
}

pub(super) fn pin_button(ui: &mut Ui, pinned: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(28.0, 18.0), Sense::click());
    let visuals = ui.style().interact_selectable(&response, pinned);
    let color = if pinned {
        ui.visuals().selection.stroke.color
    } else {
        visuals.fg_stroke.color
    };
    let fill = if pinned {
        ui.visuals().selection.bg_fill
    } else if response.hovered() {
        surface(ui, true)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 4.0, fill);
    let rotation = egui::emath::Rot2::from_angle(if pinned { 0.0 } else { -0.6 });
    let point = |x, y| rect.center() + rotation * Vec2::new(x, y);
    let stroke = Stroke::new(1.2_f32, color);
    ui.painter().add(egui::Shape::closed_line(
        vec![
            point(-3.0, -5.0),
            point(3.0, -5.0),
            point(2.0, -3.0),
            point(2.0, -1.0),
            point(4.0, 1.0),
            point(-4.0, 1.0),
            point(-2.0, -1.0),
            point(-2.0, -3.0),
        ],
        stroke,
    ));
    ui.painter()
        .line_segment([point(0.0, 1.0), point(0.0, 6.0)], stroke);
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            ui.is_enabled(),
            pinned,
            "Pin to bottom",
        )
    });
    response.on_hover_text(if pinned {
        "Pinned to bottom · Click to unpin · Ctrl+Shift+Space"
    } else {
        "Pin to bottom and autoscroll · Ctrl+Shift+Space"
    })
}

pub(super) fn action_button(
    ui: &mut Ui,
    icon: ActionIcon,
    selected: bool,
    tooltip: &str,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(36.0, 32.0), Sense::click());
    paint_action_frame(ui, &response, selected);
    if matches!(icon, ActionIcon::Clear) {
        let pulse_id = response.id.with("clear_pulse");
        let now = ui.input(|input| input.time);
        // clicked() reads context input; do not call it while holding data_mut's lock.
        let clicked = response.clicked();
        let started = ui.ctx().data_mut(|data| {
            if clicked {
                data.insert_temp(pulse_id, now);
            }
            data.get_temp::<f64>(pulse_id)
        });
        if let Some(started) = started {
            let progress = ((now - started) / 0.6).clamp(0.0, 1.0) as f32;
            if progress < 1.0 {
                let strength = (1.0 - progress).powi(2);
                ui.painter().rect(
                    rect,
                    5.0,
                    ui.visuals().selection.bg_fill.gamma_multiply(strength),
                    Stroke::new(
                        1.0_f32,
                        ui.visuals().selection.stroke.color.gamma_multiply(strength),
                    ),
                );
                ui.ctx().request_repaint();
            } else {
                ui.ctx().data_mut(|data| data.remove::<f64>(pulse_id));
            }
        }
    }
    let visuals = ui.style().interact_selectable(&response, selected);
    let c = rect.center();
    let stroke = Stroke::new(1.4_f32, visuals.fg_stroke.color);
    match icon {
        ActionIcon::Search => {
            ui.painter()
                .circle_stroke(c - Vec2::splat(2.0), 5.0, stroke);
            ui.painter()
                .line_segment([c + Vec2::splat(2.0), c + Vec2::splat(6.0)], stroke);
        }
        ActionIcon::Clear => {
            ui.painter().line_segment(
                [c + Vec2::new(-6.0, -4.0), c + Vec2::new(6.0, -4.0)],
                stroke,
            );
            ui.painter().rect_stroke(
                Rect::from_min_max(c + Vec2::new(-4.0, -4.0), c + Vec2::new(4.0, 6.0)),
                1.0,
                stroke,
            );
            ui.painter().line_segment(
                [c + Vec2::new(-2.0, -7.0), c + Vec2::new(2.0, -7.0)],
                stroke,
            );
            for x in [-1.5, 1.5] {
                ui.painter()
                    .line_segment([c + Vec2::new(x, -1.0), c + Vec2::new(x, 3.0)], stroke);
            }
        }
        ActionIcon::Plot => paint_view_content(
            ui,
            rect.translate(Vec2::new(3.0, 0.0)),
            ViewIcon::Plot,
            "",
            visuals.fg_stroke.color,
        ),
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, ui.is_enabled(), selected, tooltip)
    });
    response.on_hover_text(tooltip)
}

/// The same outline, hit area and active treatment for every toolbar action.
pub(super) fn paint_action_frame(ui: &Ui, response: &Response, selected: bool) {
    let rect = response.rect;
    let fill = if selected {
        ui.visuals().selection.bg_fill
    } else if response.hovered() {
        surface(ui, true)
    } else {
        Color32::TRANSPARENT
    };
    let border = if selected {
        ui.visuals().selection.stroke
    } else {
        Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color)
    };
    ui.painter().rect(rect, 5.0, fill, border);
}

pub(super) fn paint_export(ui: &Ui, rect: Rect) {
    let c = rect.center();
    let stroke = Stroke::new(1.4_f32, ui.visuals().text_color());
    ui.painter()
        .line_segment([c + Vec2::new(0.0, -7.0), c + Vec2::new(0.0, 2.0)], stroke);
    ui.painter()
        .line_segment([c + Vec2::new(-4.0, -1.0), c + Vec2::new(0.0, 3.0)], stroke);
    ui.painter()
        .line_segment([c + Vec2::new(0.0, 3.0), c + Vec2::new(4.0, -1.0)], stroke);
    ui.painter().add(egui::Shape::line(
        vec![
            c + Vec2::new(-6.0, 3.0),
            c + Vec2::new(-6.0, 7.0),
            c + Vec2::new(6.0, 7.0),
            c + Vec2::new(6.0, 3.0),
        ],
        stroke,
    ));
}

pub(crate) fn app_visuals(dark: bool) -> egui::Visuals {
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.menu_rounding = egui::Rounding::same(8.0);
    visuals.selection.bg_fill = if dark {
        Color32::from_rgb(29, 68, 104)
    } else {
        Color32::from_rgb(213, 233, 255)
    };
    visuals.selection.stroke = Stroke::new(
        1.0_f32,
        if dark {
            Color32::from_rgb(92, 177, 255)
        } else {
            Color32::from_rgb(28, 105, 186)
        },
    );
    if dark {
        visuals.widgets.noninteractive.fg_stroke.color = Color32::from_rgb(195, 205, 218);
        visuals.widgets.inactive.fg_stroke.color = Color32::from_rgb(195, 205, 218);
        visuals.panel_fill = Color32::from_rgb(12, 17, 23);
        visuals.window_fill = Color32::from_rgb(25, 33, 43);
        visuals.window_stroke = Stroke::new(1.0_f32, Color32::from_rgb(42, 54, 68));
        visuals.extreme_bg_color = Color32::from_rgb(10, 15, 21);
        visuals.faint_bg_color = Color32::from_rgb(20, 28, 37);
        visuals.widgets.noninteractive.bg_stroke.color = Color32::from_rgb(42, 54, 68);
        visuals.widgets.inactive.bg_fill = Color32::from_rgb(29, 38, 49);
        visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(22, 30, 40);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(39, 53, 70);
        visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(35, 48, 64);
        visuals.widgets.active.bg_fill = Color32::from_rgb(37, 65, 92);
    }
    for widgets in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
    ] {
        widgets.rounding = egui::Rounding::same(5.0);
    }
    visuals
}

pub(super) enum ViewIcon {
    Log,
    Hex,
    Terminal,
    Plot,
}

fn view_width(ui: &Ui, label: &str) -> f32 {
    let galley = ui.painter().layout_no_wrap(
        label.into(),
        egui::FontId::proportional(13.0),
        ui.visuals().text_color(),
    );
    (galley.size().x + 38.0).max(64.0)
}

/// One shared track makes the mutually exclusive console modes explicit.
/// Animate local edges so moving/resizing a pane does not move the highlight.
pub(super) fn view_selector(
    ui: &mut Ui,
    id: egui::Id,
    selected: usize,
    merged: bool,
) -> Option<usize> {
    let modes = [
        (ViewIcon::Log, "Log", "Chronological log · Ctrl+Shift+Q"),
        (ViewIcon::Hex, "Hex", "Raw bytes · Ctrl+Shift+W"),
        (
            ViewIcon::Terminal,
            "ANSI/VT",
            "Terminal screen · Ctrl+Shift+E",
        ),
    ];
    let count = if merged { 1 } else { modes.len() };
    let widths: Vec<_> = modes[..count]
        .iter()
        .map(|(_, label, _)| view_width(ui, label))
        .collect();
    let (track, _) = ui.allocate_exact_size(
        Vec2::new(widths.iter().sum::<f32>() + 6.0, 32.0),
        Sense::hover(),
    );
    ui.painter().rect(
        track,
        7.0,
        surface(ui, false),
        Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );
    let mut left = track.left() + 3.0;
    let mut responses = Vec::new();
    let mut clicked = None;
    for (index, width) in widths.iter().enumerate() {
        let rect = Rect::from_min_size(Pos2::new(left, track.top() + 3.0), Vec2::new(*width, 26.0));
        let response = ui.interact(rect, id.with(index), Sense::click());
        if response.clicked() {
            clicked = Some(index);
        }
        responses.push(response);
        left += width;
    }
    let active = clicked.unwrap_or(selected).min(count - 1);
    // Hover belongs behind the selection, including while it slides between modes.
    for (index, response) in responses.iter().enumerate() {
        if response.hovered() && index != active {
            ui.painter()
                .rect_filled(response.rect, 5.0, surface(ui, true));
        }
    }
    let target = responses[active].rect;
    let left = ui.ctx().animate_value_with_time(
        id.with("highlight_left"),
        target.left() - track.left(),
        0.18,
    );
    let right = ui.ctx().animate_value_with_time(
        id.with("highlight_right"),
        target.right() - track.left(),
        0.18,
    );
    let highlight = Rect::from_min_max(
        Pos2::new(track.left() + left, target.top()),
        Pos2::new(track.left() + right, target.bottom()),
    );
    ui.painter().rect(
        highlight,
        5.0,
        ui.visuals().selection.bg_fill,
        ui.visuals().selection.stroke,
    );
    for (index, (response, (icon, label, tooltip))) in responses.into_iter().zip(modes).enumerate()
    {
        let color = if index == active {
            ui.visuals().selection.stroke.color
        } else {
            ui.visuals().text_color()
        };
        paint_view_content(ui, response.rect, icon, label, color);
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::SelectableLabel,
                ui.is_enabled(),
                index == active,
                label,
            )
        });
        response.on_hover_text(if merged {
            "Merged chronological log"
        } else {
            tooltip
        });
    }
    clicked
}

fn paint_view_content(ui: &Ui, rect: Rect, icon: ViewIcon, label: &str, color: Color32) {
    let pen = Stroke::new(1.2_f32, color);
    let c = Pos2::new(rect.left() + 15.0, rect.center().y);
    match icon {
        ViewIcon::Log => {
            for y in [-4.0, 0.0, 4.0] {
                ui.painter()
                    .circle_filled(c + Vec2::new(-5.0, y), 0.8, color);
                ui.painter()
                    .line_segment([c + Vec2::new(-1.0, y), c + Vec2::new(5.0, y)], pen);
            }
        }
        ViewIcon::Hex => {
            let points: Vec<_> = (0..6)
                .map(|i| {
                    let angle =
                        std::f32::consts::TAU * i as f32 / 6.0 - std::f32::consts::FRAC_PI_2;
                    c + Vec2::new(angle.cos(), angle.sin()) * 6.0
                })
                .collect();
            ui.painter()
                .add(egui::Shape::closed_line(points.clone(), pen));
            for i in [0, 2, 4] {
                ui.painter().line_segment([c, points[i]], pen);
            }
            ui.painter().line_segment([points[1], points[5]], pen);
        }
        ViewIcon::Terminal => {
            ui.painter()
                .rect_stroke(Rect::from_center_size(c, Vec2::new(15.0, 12.0)), 2.0, pen);
            ui.painter().add(egui::Shape::line(
                vec![
                    c + Vec2::new(-4.5, -2.5),
                    c + Vec2::new(-1.5, 0.0),
                    c + Vec2::new(-4.5, 2.5),
                ],
                pen,
            ));
            ui.painter()
                .line_segment([c + Vec2::new(0.5, 2.5), c + Vec2::new(4.5, 2.5)], pen);
        }
        ViewIcon::Plot => {
            ui.painter().add(egui::Shape::line(
                vec![
                    c + Vec2::new(-6.0, -6.0),
                    c + Vec2::new(-6.0, 6.0),
                    c + Vec2::new(6.0, 6.0),
                ],
                pen,
            ));
            ui.painter().add(egui::Shape::line(
                vec![
                    c + Vec2::new(-4.0, 2.0),
                    c + Vec2::new(-1.0, -1.0),
                    c + Vec2::new(2.0, 1.0),
                    c + Vec2::new(6.0, -4.0),
                ],
                pen,
            ));
        }
    }
    let galley = ui
        .painter()
        .layout_no_wrap(label.into(), egui::FontId::proportional(13.0), color);
    ui.painter().galley(
        Pos2::new(rect.left() + 29.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
}

pub(super) fn paint_overflow(ui: &Ui, rect: Rect) {
    for y in [-5.0, 0.0, 5.0] {
        ui.painter().circle_filled(
            rect.center() + Vec2::new(0.0, y),
            1.2,
            ui.visuals().text_color(),
        );
    }
}

/// egui resets menu padding when a popup opens. Apply our row spacing inside
/// every menu so toolbars and context menus share the same treatment.
pub(super) fn menu_style(ui: &mut Ui) {
    ui.set_min_width(280.0);
    popup_style(ui);
}

pub(super) fn popup_style(ui: &mut Ui) {
    ui.style_mut().override_font_id = Some(egui::FontId::proportional(13.0));
    ui.spacing_mut().button_padding = Vec2::new(10.0, 7.0);
    ui.spacing_mut().interact_size.y = 32.0;
    ui.spacing_mut().item_spacing = Vec2::new(14.0, 4.0);
}

pub(super) fn menu_item(ui: &mut Ui, label: &str, shortcut: &str) -> Response {
    let muted = if ui.visuals().dark_mode {
        Color32::from_rgb(134, 147, 164)
    } else {
        Color32::from_rgb(111, 124, 139)
    };
    ui.add(
        egui::Button::new(label)
            .shortcut_text(egui::RichText::new(shortcut).size(12.0).color(muted)),
    )
}

pub(super) fn dialog_frame(ctx: &egui::Context) -> egui::Frame {
    egui::Frame::window(&ctx.style())
        .inner_margin(20.0)
        .rounding(10.0)
}

pub(super) fn dialog_style(ui: &mut Ui) {
    ui.style_mut().override_font_id = Some(egui::FontId::proportional(13.0));
    ui.spacing_mut().button_padding = Vec2::new(10.0, 7.0);
    ui.spacing_mut().interact_size.y = 32.0;
    ui.spacing_mut().item_spacing = Vec2::new(12.0, 10.0);
}

pub(super) fn app_modal(ctx: &egui::Context, name: &str) -> egui::Modal {
    egui::Modal::new(egui::Id::new(name))
        .backdrop_color(Color32::from_black_alpha(155))
        .frame(dialog_frame(ctx))
}

pub(super) fn modal_header(ui: &mut Ui, title: &str, open: &mut bool) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(title).size(18.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("×").on_hover_text("Close · Esc").clicked() {
                *open = false;
            }
        });
    });
    ui.add_space(8.0);
    ui.separator();
    ui.add_space(8.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_button_click_pulses_then_fades_and_can_restart() {
        let ctx = egui::Context::default();
        ctx.set_visuals(app_visuals(true));
        let render = |time, events| {
            let mut response = None;
            let output = ctx.run(
                egui::RawInput {
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        response =
                            Some(action_button(ui, ActionIcon::Clear, false, "Clear console"));
                    });
                },
            );
            let pulse_color = ctx.style().visuals.selection.bg_fill;
            let pulse_alpha = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Rect(rect)
                        if rect.fill != Color32::TRANSPARENT
                            && rect.stroke.color
                                != ctx.style().visuals.widgets.noninteractive.bg_stroke.color
                            && rect.rect == response.as_ref().unwrap().rect =>
                    {
                        Some(rect.fill.a())
                    }
                    _ => None,
                })
                .max()
                .unwrap_or(0);
            (response.unwrap(), pulse_alpha, pulse_color.a())
        };
        let (response, alpha, _) = render(0.0, vec![]);
        assert_eq!(alpha, 0);
        let pos = response.rect.center();
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        render(0.01, vec![egui::Event::PointerMoved(pos), button(true)]);
        let (response, alpha, full_alpha) = render(0.02, vec![button(false)]);
        assert!(response.clicked());
        assert_eq!(alpha, full_alpha);
        let (_, fading, _) = render(0.32, vec![]);
        assert!(fading > 0 && fading < alpha);
        render(0.33, vec![button(true)]);
        let (_, restarted, _) = render(0.34, vec![button(false)]);
        assert_eq!(restarted, full_alpha);
        let (response, alpha, _) = render(1.0, vec![]);
        assert_eq!(alpha, 0);
        assert!(ctx
            .data(|data| data.get_temp::<f64>(response.id.with("clear_pulse")))
            .is_none());
    }

    #[test]
    fn console_mode_selection_slides_and_accepts_clicks() {
        let ctx = egui::Context::default();
        ctx.set_visuals(app_visuals(true));
        let id = egui::Id::new("test_view_selector");
        let render = |time, selected, events| {
            let mut clicked = None;
            let output = ctx.run(
                egui::RawInput {
                    time: Some(time),
                    events,
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(500.0, 200.0))),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        clicked = view_selector(ui, id, selected, false);
                    });
                },
            );
            let highlight = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Rect(rect)
                        if rect.fill == ctx.style().visuals.selection.bg_fill =>
                    {
                        Some(rect.rect)
                    }
                    _ => None,
                })
                .unwrap();
            (output, highlight, clicked)
        };
        let (_, start, _) = render(0.0, 0, vec![]);
        render(0.01, 2, vec![]);
        let (_, middle, _) = render(0.10, 2, vec![]);
        let (output, end, _) = render(0.30, 2, vec![]);
        assert!(start.left() < middle.left() && middle.left() < end.left());
        let hex = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Hex" => {
                    Some(text.pos + text.galley.size() / 2.0)
                }
                _ => None,
            })
            .unwrap();
        let button = |pressed| egui::Event::PointerButton {
            pos: hex,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        render(0.31, 2, vec![egui::Event::PointerMoved(hex), button(true)]);
        let (_, _, clicked) = render(0.32, 2, vec![button(false)]);
        assert_eq!(clicked, Some(1));
    }

    #[test]
    fn header_double_press_toggles_maximize_even_without_native_drag_release() {
        for maximized in [false, true] {
            for release in [false, true] {
                let (mut app, _tx) = crate::app::tests::test_app("header-double-click");
                let ctx = egui::Context::default();
                let mut render = |time, events| {
                    let mut input = egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(700.0, 400.0))),
                        time: Some(time),
                        events,
                        ..Default::default()
                    };
                    input
                        .viewports
                        .get_mut(&egui::ViewportId::ROOT)
                        .unwrap()
                        .maximized = Some(maximized);
                    ctx.run(input, |ctx| {
                        app.show_header(ctx);
                    })
                };
                render(0.0, vec![]);
                let pos = egui::pos2(350.0, 20.0);
                let button = |pressed| egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                };
                render(0.1, vec![egui::Event::PointerMoved(pos), button(true)]);
                if release {
                    render(0.15, vec![button(false)]);
                }
                let output = render(0.2, vec![button(true)]);
                let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
                assert!(commands.contains(&egui::ViewportCommand::Maximized(!maximized)));
                assert!(!commands.contains(&egui::ViewportCommand::StartDrag));
                let output = render(0.25, vec![button(false)]);
                assert!(!output.viewport_output[&egui::ViewportId::ROOT]
                    .commands
                    .iter()
                    .any(|command| matches!(command, egui::ViewportCommand::Maximized(_))));
            }
        }
    }

    #[test]
    fn resize_uses_press_position_before_batched_pointer_motion() {
        let edge = Pos2::new(6.0, 200.0);
        let content = Pos2::new(350.0, 200.0);
        for (press, motion, expected) in [(edge, content, true), (content, edge, false)] {
            let (app, _tx) = crate::app::tests::test_app("resize-batched-motion");
            let ctx = egui::Context::default();
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(700.0, 400.0))),
                    events: vec![
                        egui::Event::PointerMoved(press),
                        egui::Event::PointerButton {
                            pos: press,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            modifiers: egui::Modifiers::NONE,
                        },
                        egui::Event::PointerMoved(motion),
                    ],
                    ..Default::default()
                },
                |ctx| app.show_window_resize(ctx),
            );
            assert_eq!(
                output.viewport_output[&egui::ViewportId::ROOT]
                    .commands
                    .contains(&egui::ViewportCommand::BeginResize(
                        egui::ResizeDirection::West
                    )),
                expected,
                "press={press:?}, motion={motion:?}"
            );
        }
    }

    #[test]
    fn resize_press_uses_current_window_bounds() {
        let (app, _tx) = crate::app::tests::test_app("resize-current-bounds");
        let ctx = egui::Context::default();
        let render = |size, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                    events,
                    ..Default::default()
                },
                |ctx| app.show_window_resize(ctx),
            )
        };
        render(Vec2::new(700.0, 400.0), vec![]);
        render(Vec2::new(700.0, 400.0), vec![]);
        let pos = Pos2::new(894.0, 494.0);
        let output = render(
            Vec2::new(900.0, 500.0),
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::BeginResize(
                egui::ResizeDirection::SouthEast
            )));
    }

    #[test]
    fn window_edges_show_resize_cursors_and_start_on_primary_press() {
        use egui::{
            CursorIcon::{ResizeHorizontal, ResizeNeSw, ResizeNwSe, ResizeVertical},
            ResizeDirection::*,
        };
        for (pos, direction, cursor) in [
            (Pos2::new(6.0, 6.0), NorthWest, ResizeNwSe),
            (Pos2::new(694.0, 6.0), NorthEast, ResizeNeSw),
            (Pos2::new(6.0, 394.0), SouthWest, ResizeNeSw),
            (Pos2::new(694.0, 394.0), SouthEast, ResizeNwSe),
            (Pos2::new(350.0, 6.0), North, ResizeVertical),
            (Pos2::new(350.0, 394.0), South, ResizeVertical),
            (Pos2::new(6.0, 200.0), West, ResizeHorizontal),
            (Pos2::new(694.0, 200.0), East, ResizeHorizontal),
        ] {
            let (mut app, _tx) = crate::app::tests::test_app("window-resize");
            let ctx = egui::Context::default();
            let mut render = |events| {
                ctx.run(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(700.0, 400.0))),
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        app.show_workspace(ctx, false);
                        app.show_window_resize(ctx);
                    },
                )
            };
            render(vec![]);
            render(vec![]);
            let output = render(vec![egui::Event::PointerMoved(pos)]);
            assert_eq!(output.platform_output.cursor_icon, cursor, "{direction:?}");
            let button = |button, pressed| egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            let output = render(vec![button(egui::PointerButton::Secondary, true)]);
            assert!(!output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .iter()
                .any(|cmd| matches!(cmd, egui::ViewportCommand::BeginResize(_))));
            render(vec![button(egui::PointerButton::Secondary, false)]);
            let output = render(vec![button(egui::PointerButton::Primary, true)]);
            assert!(
                output.viewport_output[&egui::ViewportId::ROOT]
                    .commands
                    .contains(&egui::ViewportCommand::BeginResize(direction)),
                "{direction:?}"
            );
            let output = render(vec![egui::Event::PointerMoved(pos + Vec2::splat(1.0))]);
            assert!(!output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .iter()
                .any(|cmd| matches!(cmd, egui::ViewportCommand::BeginResize(_))));
        }
    }

    #[test]
    fn corner_cursors_follow_window_size_changes() {
        let (mut app, _tx) = crate::app::tests::test_app("corner-resize-hover");
        let ctx = egui::Context::default();
        ctx.set_visuals(app_visuals(true));
        let mut render = |size, pos: Option<Pos2>| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)),
                    events: pos.map_or_else(
                        || vec![egui::Event::PointerGone],
                        |pos| vec![egui::Event::PointerMoved(pos)],
                    ),
                    ..Default::default()
                },
                |ctx| {
                    app.show_workspace(ctx, false);
                    // Simulate a content widget choosing its cursor first.
                    ctx.set_cursor_icon(egui::CursorIcon::Text);
                    app.show_window_resize(ctx);
                },
            )
        };
        for size in [
            Vec2::new(1100.0, 720.0),
            Vec2::new(700.0, 400.0),
            Vec2::new(900.0, 500.0),
        ] {
            for offset in [0.5, 2.0, 6.0, 12.0, 19.0] {
                for (pos, cursor) in [
                    (Pos2::new(offset, offset), egui::CursorIcon::ResizeNwSe),
                    (
                        Pos2::new(size.x - offset, offset),
                        egui::CursorIcon::ResizeNeSw,
                    ),
                    (
                        Pos2::new(offset, size.y - offset),
                        egui::CursorIcon::ResizeNeSw,
                    ),
                    (
                        Pos2::new(size.x - offset, size.y - offset),
                        egui::CursorIcon::ResizeNwSe,
                    ),
                ] {
                    let output = render(size, None);
                    assert_eq!(output.platform_output.cursor_icon, egui::CursorIcon::Text);
                    let output = render(size, Some(pos));
                    assert_eq!(
                        output.platform_output.cursor_icon, cursor,
                        "size={size:?}, pos={pos:?}"
                    );
                }
            }
            let output = render(size, Some(Pos2::new(size.x / 2.0, size.y / 2.0)));
            assert_eq!(output.platform_output.cursor_icon, egui::CursorIcon::Text);
        }
    }

    #[test]
    fn maximized_and_fullscreen_windows_have_no_resize_targets() {
        for (maximized, fullscreen) in [(true, false), (false, true)] {
            let (app, _tx) = crate::app::tests::test_app("window-resize-disabled");
            let ctx = egui::Context::default();
            let render = |events| {
                let mut input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(700.0, 400.0))),
                    events,
                    ..Default::default()
                };
                let viewport = input.viewports.get_mut(&egui::ViewportId::ROOT).unwrap();
                viewport.maximized = Some(maximized);
                viewport.fullscreen = Some(fullscreen);
                ctx.run(input, |ctx| app.show_window_resize(ctx))
            };
            render(vec![]);
            let output = render(vec![
                egui::Event::PointerMoved(Pos2::new(6.0, 200.0)),
                egui::Event::PointerButton {
                    pos: Pos2::new(6.0, 200.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            assert_eq!(
                output.platform_output.cursor_icon,
                egui::CursorIcon::Default
            );
            assert!(!output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .iter()
                .any(|cmd| matches!(cmd, egui::ViewportCommand::BeginResize(_))));
        }
    }

    #[test]
    fn window_drag_starts_on_primary_press_without_waiting_for_motion() {
        let ctx = egui::Context::default();
        let render = |events| {
            ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::TopBottomPanel::top("drag_header").show(ctx, drag_window);
                },
            )
        };
        render(vec![]);
        let pos = egui::pos2(100.0, 15.0);
        let output = render(vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::StartDrag));
        let output = render(vec![egui::Event::PointerMoved(pos + Vec2::new(20.0, 0.0))]);
        assert!(!output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::StartDrag));
    }

    #[test]
    fn window_buttons_use_the_native_close_minimize_and_maximize_commands() {
        for (index, maximized, expected) in [
            (0, false, egui::ViewportCommand::Close),
            (2, false, egui::ViewportCommand::Minimized(true)),
            (1, false, egui::ViewportCommand::Maximized(true)),
            (1, true, egui::ViewportCommand::Maximized(false)),
        ] {
            let ctx = egui::Context::default();
            let mut rectangles = [Rect::NOTHING; 3];
            let mut render = |events| {
                let mut input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(700.0, 400.0))),
                    events,
                    ..Default::default()
                };
                input
                    .viewports
                    .get_mut(&egui::ViewportId::ROOT)
                    .unwrap()
                    .maximized = Some(maximized);
                let output = ctx.run(input, |ctx| {
                    egui::TopBottomPanel::top("controls").show(ctx, |ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let responses = window_controls(ui);
                            rectangles = responses.map(|response| response.rect);
                        });
                    });
                });
                (output, rectangles)
            };
            let (_, rectangles) = render(vec![]);
            let pos = rectangles[index].center();
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            render(vec![egui::Event::PointerMoved(pos), button(true)]);
            let (output, _) = render(vec![button(false)]);
            assert!(output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .contains(&expected));
        }
    }
}
