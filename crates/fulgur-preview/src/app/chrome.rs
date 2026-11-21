//! Window chrome: theme, welcome screen, title-bar buttons and borderless resizing.

use super::*;

pub(super) fn show_empty_welcome_state(ui: &mut egui::Ui) {
    ui.centered_and_justified(|ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.32);
            ui.label(
                RichText::new("FulgurPreView")
                    .size(28.0)
                    .strong()
                    .color(Color32::from_rgb(160, 205, 255)),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "Drop a FITS, XISF, TIFF, JPEG, PNG, or WebP image (or folder) here, or press Ctrl+O.",
                )
                .size(15.0)
                .weak(),
            );
            ui.add_space(12.0);
            ui.label(
                RichText::new(
                    "Shortcuts:  ◀ / ▶ Navigate   ·   Wheel: Zoom   ·   S: Plate Solve   ·   O: Astrometry   ·   T: Stretch Mode   ·   B: Lock View   ·   X / Del: Reject   ·   Ctrl+Z: Undo",
                )
                .size(12.5)
                .weak(),
            );
        });
    });
}

pub(super) fn configure_dark_astro_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.scroll = egui::style::ScrollStyle {
        floating: false,
        bar_width: 10.0,
        handle_min_length: 24.0,
        bar_inner_margin: 2.0,
        bar_outer_margin: 2.0,
        floating_width: 8.0,
        floating_allocated_width: 10.0,
        foreground_color: false,
        dormant_background_opacity: 0.65,
        active_background_opacity: 0.90,
        interact_background_opacity: 0.95,
        dormant_handle_opacity: 0.85,
        active_handle_opacity: 1.0,
        interact_handle_opacity: 1.0,
    };
    ctx.set_style(style);

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = MAIN_BG_COLOR;
    visuals.window_fill = MAIN_BG_COLOR;
    visuals.extreme_bg_color = Color32::from_rgb(12, 36, 36);
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(20, 56, 56);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(24, 66, 66);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(32, 84, 84);
    visuals.widgets.active.bg_fill = Color32::from_rgb(42, 105, 105);
    visuals.selection.bg_fill = Color32::from_rgb(38, 115, 125);
    ctx.set_visuals(visuals);
}

/// Draws the FulgurPreView application icon (Variant 1B: single isometric plate + solid yellow lightning bolt) in the title bar.
pub(super) fn draw_app_star_icon(painter: &egui::Painter, center: egui::Pos2, radius: f32) {
    let s = radius / 54.0;
    let map = |x: f32, y: f32| egui::pos2(center.x + (x - 64.0) * s, center.y + (y - 64.0) * s);

    // 1. Extruded royal-blue bottom wall of the single isometric plate
    painter.add(egui::Shape::convex_polygon(
        vec![
            map(16.0, 60.0),
            map(64.0, 87.0),
            map(112.0, 60.0),
            map(112.0, 75.0),
            map(64.0, 102.0),
            map(16.0, 75.0),
        ],
        Color32::from_rgb(0, 72, 173),
        Stroke::NONE,
    ));

    // 2. Glowing cyan rim diamond
    painter.add(egui::Shape::convex_polygon(
        vec![
            map(64.0, 32.0),
            map(112.0, 60.0),
            map(64.0, 88.0),
            map(16.0, 60.0),
        ],
        Color32::from_rgb(0, 166, 255),
        Stroke::NONE,
    ));

    // 3. Midnight-navy starfield top surface
    painter.add(egui::Shape::convex_polygon(
        vec![
            map(64.0, 36.5),
            map(105.0, 60.0),
            map(64.0, 83.5),
            map(23.0, 60.0),
        ],
        Color32::from_rgb(4, 21, 45),
        Stroke::NONE,
    ));

    // 4. White stars on plate
    painter.circle_filled(map(42.0, 56.0), (1.3 * s * 8.0).max(1.0), Color32::WHITE);
    painter.circle_filled(map(84.0, 63.0), (1.1 * s * 8.0).max(0.9), Color32::WHITE);

    // 5. Solid bright yellow (#FFDD00) lightning bolt decomposed into convex triangles
    let bolt_yellow = Color32::from_rgb(255, 221, 0);
    let p_top = map(90.0, 8.0);
    let p_lknee = map(45.0, 66.0);
    let p_lnotch = map(62.0, 64.5);
    let p_bot = map(35.0, 121.0);
    let p_rknee = map(85.0, 56.5);
    let p_rnotch = map(68.0, 58.5);

    painter.add(egui::Shape::convex_polygon(
        vec![p_top, p_lknee, p_rnotch],
        bolt_yellow,
        Stroke::NONE,
    ));
    painter.add(egui::Shape::convex_polygon(
        vec![p_lknee, p_rknee, p_rnotch],
        bolt_yellow,
        Stroke::NONE,
    ));
    painter.add(egui::Shape::convex_polygon(
        vec![p_lknee, p_lnotch, p_rknee],
        bolt_yellow,
        Stroke::NONE,
    ));
    painter.add(egui::Shape::convex_polygon(
        vec![p_lnotch, p_bot, p_rknee],
        bolt_yellow,
        Stroke::NONE,
    ));
}

/// Renders a single-line truncated label with an explicit single tooltip (avoiding egui::Label's duplicate elision tooltip).
pub(super) fn add_truncated_label(
    ui: &mut egui::Ui,
    text: RichText,
    max_width: f32,
    hover_text: &str,
) {
    ui.set_max_width(max_width);
    let widget_text: egui::WidgetText = text.into();
    let galley = widget_text.into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        max_width,
        egui::TextStyle::Body,
    );
    let desired_size = egui::vec2(galley.size().x.min(max_width), galley.size().y);
    let (rect, resp) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let text_color = ui.style().visuals.text_color();
        ui.painter().galley(rect.min, galley, text_color);
    }
    if !hover_text.is_empty() {
        resp.on_hover_text(hover_text);
    }
}

/// Draws the right-aligned window control triplet (Close, Maximize/Restore, Minimize) in right-to-left layout.
pub(super) fn draw_window_control_buttons(ui: &mut egui::Ui, is_maximized: bool) {
    let btn_size = egui::vec2(32.0, 22.0);

    // 1. Close button [ ✕ ] (rightmost in right_to_left layout)
    let (close_rect, close_resp) = ui.allocate_exact_size(btn_size, egui::Sense::click());
    if close_resp.hovered() {
        ui.painter()
            .rect_filled(close_rect, 3.0, Color32::from_rgb(196, 43, 28));
    }
    let close_fg = if close_resp.hovered() {
        Color32::WHITE
    } else {
        Color32::from_rgb(200, 208, 220)
    };
    let c = close_rect.center();
    let d = 4.2_f32;
    ui.painter().line_segment(
        [egui::pos2(c.x - d, c.y - d), egui::pos2(c.x + d, c.y + d)],
        Stroke::new(1.3_f32, close_fg),
    );
    ui.painter().line_segment(
        [egui::pos2(c.x + d, c.y - d), egui::pos2(c.x - d, c.y + d)],
        Stroke::new(1.3_f32, close_fg),
    );
    if close_resp.on_hover_text("Close").clicked() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
    }

    // 2. Maximize / Restore button [ □ ]
    let (max_rect, max_resp) = ui.allocate_exact_size(btn_size, egui::Sense::click());
    if max_resp.hovered() {
        ui.painter()
            .rect_filled(max_rect, 3.0, Color32::from_rgb(42, 52, 70));
    }
    let max_fg = if max_resp.hovered() {
        Color32::WHITE
    } else {
        Color32::from_rgb(200, 208, 220)
    };
    let mc = max_rect.center();
    let box_rect = egui::Rect::from_center_size(mc, egui::vec2(8.5, 8.5));
    ui.painter().rect_stroke(
        box_rect,
        0.5,
        Stroke::new(1.2_f32, max_fg),
        egui::StrokeKind::Middle,
    );
    let max_tip = if is_maximized { "Restore" } else { "Maximize" };
    if max_resp.on_hover_text(max_tip).clicked() {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Maximized(!is_maximized));
    }

    // 3. Minimize button [ — ]
    let (min_rect, min_resp) = ui.allocate_exact_size(btn_size, egui::Sense::click());
    if min_resp.hovered() {
        ui.painter()
            .rect_filled(min_rect, 3.0, Color32::from_rgb(42, 52, 70));
    }
    let min_fg = if min_resp.hovered() {
        Color32::WHITE
    } else {
        Color32::from_rgb(200, 208, 220)
    };
    let nc = min_rect.center();
    ui.painter().line_segment(
        [egui::pos2(nc.x - 4.5, nc.y), egui::pos2(nc.x + 4.5, nc.y)],
        Stroke::new(1.3_f32, min_fg),
    );
    if min_resp.on_hover_text("Minimize").clicked() {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
    }
}

/// Enables resizing the custom borderless window from any of its 8 edges/corners when not maximized.
pub(super) fn handle_borderless_window_resize(ctx: &egui::Context, fullscreen: bool) {
    if fullscreen {
        return;
    }
    let is_maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
    if is_maximized {
        return;
    }
    let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else {
        return;
    };
    let rect = ctx.screen_rect();
    let border = 5.0_f32;

    let left = pos.x <= rect.left() + border;
    let right = pos.x >= rect.right() - border;
    let top = pos.y <= rect.top() + border;
    let bottom = pos.y >= rect.bottom() - border;

    let dir = match (left, right, top, bottom) {
        (true, false, true, false) => Some(egui::ResizeDirection::NorthWest),
        (false, true, true, false) => Some(egui::ResizeDirection::NorthEast),
        (true, false, false, true) => Some(egui::ResizeDirection::SouthWest),
        (false, true, false, true) => Some(egui::ResizeDirection::SouthEast),
        (true, false, false, false) => Some(egui::ResizeDirection::West),
        (false, true, false, false) => Some(egui::ResizeDirection::East),
        (false, false, true, false) => Some(egui::ResizeDirection::North),
        (false, false, false, true) => Some(egui::ResizeDirection::South),
        _ => None,
    };

    if let Some(direction) = dir {
        let cursor = match direction {
            egui::ResizeDirection::North | egui::ResizeDirection::South => {
                egui::CursorIcon::ResizeVertical
            }
            egui::ResizeDirection::East | egui::ResizeDirection::West => {
                egui::CursorIcon::ResizeHorizontal
            }
            egui::ResizeDirection::NorthWest | egui::ResizeDirection::SouthEast => {
                egui::CursorIcon::ResizeNwSe
            }
            egui::ResizeDirection::NorthEast | egui::ResizeDirection::SouthWest => {
                egui::CursorIcon::ResizeNeSw
            }
        };
        ctx.set_cursor_icon(cursor);
        if ctx.input(|i| i.pointer.primary_pressed()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
    }
}
