//! Shared window chrome, independent of the console's focused pane.
use crate::app::App;
use egui::{Color32, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2};

/// The header blends with terminal text painted beneath it.
pub(super) fn header_fill(dark: bool, toolbar: bool) -> Color32 {
    match (dark, toolbar) {
        (true, false) => Color32::from_rgba_unmultiplied(15, 20, 27, 225),
        (true, true) => Color32::from_rgba_unmultiplied(22, 28, 35, 238),
        (false, false) => Color32::from_rgba_unmultiplied(235, 239, 245, 225),
        (false, true) => Color32::from_rgba_unmultiplied(248, 250, 253, 238),
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
    let font = egui::FontId::proportional(13.0);
    let galley = ui
        .painter()
        .layout_no_wrap(label.into(), font, ui.visuals().text_color());
    let width = (galley.size().x + 64.0).max(148.0);
    let id = ui.next_auto_id();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 36.0), Sense::hover());
    let close_rect = Rect::from_center_size(
        Pos2::new(rect.right() - 17.0, rect.center().y),
        Vec2::splat(24.0),
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
    let fill = surface(ui, selected || response.hovered());
    let border = if selected {
        Color32::from_rgb(57, 151, 244)
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke.color
    };
    ui.painter().rect(
        rect,
        egui::Rounding {
            nw: 8.0,
            ne: 8.0,
            sw: 0.0,
            se: 0.0,
        },
        fill,
        Stroke::new(1.0_f32, border),
    );
    ui.painter()
        .circle_filled(Pos2::new(rect.left() + 17.0, rect.center().y), 4.5, status);
    ui.painter().galley(
        Pos2::new(rect.left() + 31.0, rect.center().y - galley.size().y / 2.0),
        galley,
        ui.visuals().text_color(),
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
            .add_sized([36.0, 30.0], egui::Button::new(" ").frame(false))
            .on_hover_text(*tooltip);
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), *tooltip)
        });
        let c = response.rect.center();
        if index == 0 && response.hovered() {
            ui.painter()
                .rect_filled(response.rect, 4.0, Color32::from_rgb(182, 48, 62));
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
        Vec2::new(ui.available_width().max(0.0), 30.0),
        Sense::click_and_drag(),
    );
    let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
    if response.double_clicked() {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
    }
    if response.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
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
        let edge = 4.0;
        let corner = 12.0;
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
        let ui = egui::Ui::new(
            ctx.clone(),
            egui::Id::new("window_resize"),
            egui::UiBuilder::new()
                .layer_id(egui::LayerId::new(
                    egui::Order::Foreground,
                    egui::Id::new("window_edges"),
                ))
                .max_rect(rect),
        );
        for (index, (target, direction, cursor)) in targets.into_iter().enumerate() {
            let response = ui
                .interact(target, ui.id().with(index), Sense::drag())
                .on_hover_cursor(cursor);
            if response.drag_started_by(egui::PointerButton::Primary) {
                ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
            }
        }
    }
}

pub(super) enum ActionIcon {
    Search,
    Clear,
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
    let visuals = ui.style().interact_selectable(&response, selected);
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
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, ui.is_enabled(), selected, tooltip)
    });
    response.on_hover_text(tooltip)
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

pub(super) fn view_button(ui: &mut Ui, icon: ViewIcon, label: &str, selected: bool) -> Response {
    let label_width = ui
        .painter()
        .layout_no_wrap(
            label.into(),
            egui::FontId::proportional(13.0),
            ui.visuals().text_color(),
        )
        .size()
        .x;
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new((label_width + 38.0).max(64.0), 32.0),
        Sense::click(),
    );
    let blue = Color32::from_rgb(64, 165, 255);
    let fill = if selected {
        if ui.visuals().dark_mode {
            Color32::from_rgb(27, 55, 83)
        } else {
            Color32::from_rgb(216, 235, 255)
        }
    } else if response.hovered() {
        surface(ui, true)
    } else {
        Color32::TRANSPARENT
    };
    let stroke = if selected {
        Stroke::new(1.0_f32, blue)
    } else {
        Stroke::NONE
    };
    ui.painter().rect(rect, 5.0, fill, stroke);
    let color = if selected {
        blue
    } else {
        ui.visuals().text_color()
    };
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
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::SelectableLabel,
            ui.is_enabled(),
            selected,
            label,
        )
    });
    response
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
