use crate::app::AppState;
use crate::model::{AppCommand, PlaybackState};
use crate::time_format::{format_age, format_colon_time};

use super::{menu, theme};

pub fn render(ui: &mut egui::Ui, state: &mut AppState) -> Vec<AppCommand> {
    let mut commands = Vec::new();
    let rect = ui.max_rect();
    ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);
    paint_dummy_video(ui, state);

    if !state.ui.controls_visible {
        let response = ui.interact(rect, egui::Id::new("reveal-controls"), egui::Sense::click());
        if response.clicked() {
            state.note_interaction();
        }
        return commands;
    }

    paint_top_info(ui, state);
    menu::render_button(ui, state);
    paint_centre_controls(ui, state, &mut commands);
    paint_bottom_controls(ui, state, &mut commands);
    paint_lock_slider(ui, state, &mut commands);
    menu::render(ui, state, &mut commands);
    commands
}

fn paint_dummy_video(ui: &mut egui::Ui, state: &AppState) {
    let rect = ui.max_rect();
    let vmin = theme::vmin(ui);
    ui.painter()
        .rect_filled(rect, 0.0, egui::Color32::from_rgb(8, 9, 13));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        format!(
            "DUMMY VIDEO\n{} {}\n1280 × 720",
            state.source().unwrap().platform,
            state.source().unwrap().id
        ),
        egui::FontId::proportional((2.2 * vmin).max(14.0)),
        egui::Color32::from_gray(80),
    );
}

fn paint_top_info(ui: &mut egui::Ui, state: &AppState) {
    let ctx = ui.ctx().clone();
    let vmin = theme::vmin(ui);
    egui::Area::new(egui::Id::new("player-top-info"))
        .fixed_pos(egui::pos2(vmin, 0.5 * vmin))
        .show(&ctx, |ui| {
            ui.set_max_width((ui.ctx().content_rect().width() - 14.0 * vmin).max(10.0 * vmin));
            let font_size = 5.0 * vmin;
            if let Some(title) = state.safe_title() {
                ui.label(
                    egui::RichText::new(title)
                        .size(font_size)
                        .color(theme::TOP_INFO),
                );
            }
            if let Some(age) = state.release_age() {
                ui.label(
                    egui::RichText::new(format_age(age))
                        .size(font_size)
                        .color(theme::TOP_INFO),
                );
            }
        });
}

#[derive(Clone, Copy)]
enum PlayerIcon {
    Play,
    Pause,
    Seeking,
    Ended,
    Fullscreen,
}

fn icon_button(
    ui: &mut egui::Ui,
    icon: PlayerIcon,
    size: f32,
    radius: f32,
    enabled: bool,
) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), sense);
    let fill = if enabled && response.hovered() {
        theme::LIGHT_PURPLE
    } else {
        theme::WHITE
    };
    ui.painter().rect_filled(rect, radius, fill);

    let alpha = if enabled { 255 } else { 128 };
    let colour = egui::Color32::from_rgba_unmultiplied(
        theme::ICON_PURPLE.r(),
        theme::ICON_PURPLE.g(),
        theme::ICON_PURPLE.b(),
        alpha,
    );
    let c = rect.center();
    let s = size;
    match icon {
        PlayerIcon::Play => {
            ui.painter().add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(c.x - 0.18 * s, c.y - 0.28 * s),
                    egui::pos2(c.x - 0.18 * s, c.y + 0.28 * s),
                    egui::pos2(c.x + 0.28 * s, c.y),
                ],
                colour,
                egui::Stroke::NONE,
            ));
        }
        PlayerIcon::Pause => {
            for x in [-0.13_f32, 0.13_f32] {
                let bar = egui::Rect::from_center_size(
                    egui::pos2(c.x + x * s, c.y),
                    egui::vec2(0.12 * s, 0.48 * s),
                );
                ui.painter().rect_filled(bar, 0.0, colour);
            }
        }
        PlayerIcon::Seeking => {
            for x in [-0.16_f32, 0.0, 0.16] {
                let dot = egui::Rect::from_center_size(
                    egui::pos2(c.x + x * s, c.y),
                    egui::vec2(0.08 * s, 0.08 * s),
                );
                ui.painter().rect_filled(dot, 0.0, colour);
            }
        }
        PlayerIcon::Ended => {
            let stroke = egui::Stroke::new((0.06 * s).max(2.0), colour);
            ui.painter().line_segment(
                [
                    egui::pos2(c.x - 0.22 * s, c.y - 0.22 * s),
                    egui::pos2(c.x + 0.22 * s, c.y + 0.22 * s),
                ],
                stroke,
            );
            ui.painter().line_segment(
                [
                    egui::pos2(c.x + 0.22 * s, c.y - 0.22 * s),
                    egui::pos2(c.x - 0.22 * s, c.y + 0.22 * s),
                ],
                stroke,
            );
        }
        PlayerIcon::Fullscreen => {
            let stroke = egui::Stroke::new((0.035 * s).max(2.0), colour);
            let outer = 0.28 * s;
            let inner = 0.10 * s;
            for (sx, sy) in [(-1.0_f32, -1.0_f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let corner = egui::pos2(c.x + sx * outer, c.y + sy * outer);
                ui.painter().line_segment(
                    [corner, egui::pos2(corner.x - sx * inner, corner.y)],
                    stroke,
                );
                ui.painter().line_segment(
                    [corner, egui::pos2(corner.x, corner.y - sy * inner)],
                    stroke,
                );
            }
        }
    }
    response
}

fn paint_centre_controls(ui: &mut egui::Ui, state: &AppState, commands: &mut Vec<AppCommand>) {
    let ctx = ui.ctx().clone();
    let vmin = theme::vmin(ui);
    let size = 20.0 * vmin;
    let gap = 1.0 * vmin;
    let radius = 1.0 * vmin;
    egui::Area::new(egui::Id::new("centre-controls"))
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(&ctx, |ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.horizontal(|ui| {
                let (icon, state_enabled) = match state.playback_state() {
                    PlaybackState::Playing => (PlayerIcon::Pause, true),
                    PlaybackState::Seeking => (PlayerIcon::Seeking, false),
                    PlaybackState::Ended => (PlayerIcon::Ended, false),
                    _ => (PlayerIcon::Play, true),
                };
                if icon_button(
                    ui,
                    icon,
                    size,
                    radius,
                    state_enabled && !state.ui.controls_locked,
                )
                .clicked()
                {
                    commands.push(AppCommand::TogglePlayback);
                }
                if icon_button(
                    ui,
                    PlayerIcon::Fullscreen,
                    size,
                    radius,
                    !state.ui.controls_locked,
                )
                .clicked()
                {
                    commands.push(AppCommand::ToggleFullscreen);
                }
            });
        });
}

fn seek_button(ui: &mut egui::Ui, label: &str, vmin: f32, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        theme::rounded_button(
            egui::RichText::new(label)
                .size(5.0 * vmin)
                .color(theme::PURPLE),
            vmin,
        ),
    )
}

fn paint_bottom_controls(ui: &mut egui::Ui, state: &mut AppState, commands: &mut Vec<AppCommand>) {
    let ctx = ui.ctx().clone();
    let vmin = theme::vmin(ui);
    let enabled = !state.ui.controls_locked;
    egui::Area::new(egui::Id::new("bottom-controls"))
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -0.5 * vmin))
        .show(&ctx, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(vmin, vmin);
            ui.horizontal(|ui| {
                for (label, offset) in [("-10m", -600), ("-1m", -60), ("-5s", -5)] {
                    if seek_button(ui, label, vmin, enabled).clicked() {
                        commands.push(AppCommand::SeekRelative(offset));
                    }
                }

                ui.vertical_centered(|ui| {
                    let rates = state.available_rates().to_vec();
                    let mut selected_rate = state.playback_rate();
                    ui.scope(|ui| {
                        ui.style_mut().override_font_id =
                            Some(egui::FontId::proportional(3.5 * vmin));
                        ui.add_enabled_ui(enabled, |ui| {
                            egui::ComboBox::from_id_salt("speed-select")
                                .selected_text(format!("{selected_rate}x"))
                                .show_ui(ui, |ui| {
                                    for rate in rates {
                                        if ui
                                            .selectable_value(
                                                &mut selected_rate,
                                                rate,
                                                format!("{rate}x"),
                                            )
                                            .changed()
                                        {
                                            commands.push(AppCommand::SetPlaybackRate(rate));
                                        }
                                    }
                                });
                        });
                    });
                    if ui
                        .add_enabled(
                            enabled,
                            theme::rounded_button(
                                egui::RichText::new(format_colon_time(state.position()))
                                    .size(5.0 * vmin)
                                    .color(theme::PURPLE),
                                vmin,
                            ),
                        )
                        .clicked()
                    {
                        state.open_seek_dialog();
                    }
                });

                for (label, offset) in [("+5s", 5), ("+1m", 60), ("+10m", 600)] {
                    if seek_button(ui, label, vmin, enabled).clicked() {
                        commands.push(AppCommand::SeekRelative(offset));
                    }
                }
            });
        });
}

fn paint_lock_icon(painter: &egui::Painter, rect: egui::Rect, locked: bool) {
    let colour = theme::ICON_PURPLE;
    let stroke = egui::Stroke::new((rect.width() * 0.055).max(1.5), colour);
    let c = rect.center();
    let body = egui::Rect::from_center_size(
        egui::pos2(c.x, c.y + rect.height() * 0.12),
        egui::vec2(rect.width() * 0.50, rect.height() * 0.38),
    );
    painter.rect_stroke(body, rect.width() * 0.06, stroke, egui::StrokeKind::Inside);
    let y = body.top();
    let r = rect.width() * 0.16;
    if locked {
        painter.line_segment([egui::pos2(c.x - r, y), egui::pos2(c.x - r, y - r)], stroke);
        painter.line_segment([egui::pos2(c.x + r, y), egui::pos2(c.x + r, y - r)], stroke);
        painter.circle_stroke(egui::pos2(c.x, y - r), r, stroke);
    } else {
        painter.line_segment([egui::pos2(c.x + r, y), egui::pos2(c.x + r, y - r)], stroke);
        painter.circle_stroke(egui::pos2(c.x + 2.0 * r, y - r), r, stroke);
    }
}

fn paint_lock_slider(ui: &mut egui::Ui, state: &mut AppState, commands: &mut Vec<AppCommand>) {
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    let vmin = theme::vmin(ui);
    let travel = screen.width() * 0.25;
    let size = 7.0 * vmin;
    let x = state.ui.lock_drag_fraction * travel;

    egui::Area::new(egui::Id::new("control-lock-slider"))
        .fixed_pos(egui::pos2(x, screen.center().y - size * 0.5))
        .order(egui::Order::Foreground)
        .show(&ctx, |ui| {
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::drag());
            let ready = state.ui.lock_drag_fraction >= 0.95;
            let fill = if ready {
                theme::PINK
            } else if response.hovered() {
                theme::LIGHT_PURPLE
            } else {
                theme::WHITE
            };
            ui.painter().rect_filled(rect, vmin, fill);
            paint_lock_icon(ui.painter(), rect, state.ui.controls_locked);

            if response.dragged() {
                state.ui.lock_drag_fraction = (response.drag_delta().x / travel).clamp(0.0, 1.0);
                state.note_interaction();
                ctx.request_repaint();
            }
            if response.drag_stopped() {
                if state.ui.lock_drag_fraction >= 0.95 {
                    commands.push(AppCommand::ToggleControlsLock);
                }
                state.ui.lock_drag_fraction = 0.0;
                state.note_interaction();
                ctx.request_repaint();
            }
            response.on_hover_text("Drag right to lock/unlock controls");
        });
}
