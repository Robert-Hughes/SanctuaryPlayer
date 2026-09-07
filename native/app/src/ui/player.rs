use crate::app::AppState;
use crate::model::{AppCommand, PlaybackState};
use crate::time_format::{format_age, format_colon_time};

use super::{menu, theme};

pub fn render(ui: &mut egui::Ui, state: &mut AppState) -> Vec<AppCommand> {
    let mut commands = Vec::new();
    let rect = ui.max_rect();
    let background = ui.interact(
        rect,
        egui::Id::new("player-background"),
        egui::Sense::click(),
    );
    ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);
    paint_dummy_video(ui, state);

    if state.ui.controls_visible {
        paint_top_info(ui, state);
        menu::render_button(ui, state);
        paint_centre_controls(ui, state, &mut commands);
        paint_bottom_controls(ui, state, &mut commands);
        paint_lock_slider(ui, state, &mut commands);
        menu::render(ui, state, &mut commands);
    }

    if background.clicked() && state.ui.dialog.is_none() {
        commands.push(AppCommand::ToggleControlsVisibility);
    }

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

fn paint_top_info(ui: &mut egui::Ui, state: &AppState) -> egui::Rect {
    let ctx = ui.ctx().clone();
    let vmin = theme::vmin(ui);
    let area = egui::Area::new(egui::Id::new("player-top-info"))
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
    area.response.rect
}

#[derive(Clone, Copy)]
enum PlayerIcon {
    Play,
    Pause,
    Seeking,
    Ended,
    Fullscreen,
}

fn icon_image(icon: PlayerIcon) -> egui::Image<'static> {
    match icon {
        PlayerIcon::Play => {
            egui::Image::new(egui::include_image!("../../assets/player-icons/play.svg"))
        }
        PlayerIcon::Pause => {
            egui::Image::new(egui::include_image!("../../assets/player-icons/pause.svg"))
        }
        PlayerIcon::Seeking => egui::Image::new(egui::include_image!(
            "../../assets/player-icons/seeking.svg"
        )),
        PlayerIcon::Ended => {
            egui::Image::new(egui::include_image!("../../assets/player-icons/ended.svg"))
        }
        PlayerIcon::Fullscreen => egui::Image::new(egui::include_image!(
            "../../assets/player-icons/fullscreen.svg"
        )),
    }
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

    let mut image = icon_image(icon);
    if !enabled {
        image = image.tint(egui::Color32::from_white_alpha(128));
    }
    image.paint_at(ui, rect);
    response
}

fn paint_centre_controls(
    ui: &mut egui::Ui,
    state: &AppState,
    commands: &mut Vec<AppCommand>,
) -> egui::Rect {
    let ctx = ui.ctx().clone();
    let vmin = theme::vmin(ui);
    let size = 20.0 * vmin;
    let gap = 1.0 * vmin;
    let radius = 1.0 * vmin;
    let area = egui::Area::new(egui::Id::new("centre-controls"))
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
    area.response.rect
}

fn text_control_button(
    ui: &mut egui::Ui,
    label: &str,
    font_size: f32,
    padding: f32,
    radius: f32,
    enabled: bool,
    active_fill: Option<egui::Color32>,
) -> egui::Response {
    let font_id = egui::FontId::proportional(font_size);
    let colour = if enabled {
        theme::PURPLE
    } else {
        egui::Color32::from_rgba_unmultiplied(
            theme::PURPLE.r(),
            theme::PURPLE.g(),
            theme::PURPLE.b(),
            128,
        )
    };
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font_id, colour);
    let desired = galley.size() + egui::vec2(2.0 * padding, 2.0 * padding);
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(desired, sense);
    let fill = active_fill.unwrap_or_else(|| {
        if enabled && response.hovered() {
            theme::LIGHT_PURPLE
        } else {
            theme::WHITE
        }
    });
    ui.painter().rect_filled(rect, radius, fill);
    ui.painter().galley(
        rect.center() - galley.size() * 0.5,
        galley,
        egui::Color32::WHITE,
    );
    response
}

fn text_control_size(ui: &egui::Ui, label: &str, font_size: f32, padding: f32) -> egui::Vec2 {
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::FontId::proportional(font_size),
        theme::PURPLE,
    );
    galley.size() + egui::vec2(2.0 * padding, 2.0 * padding)
}

fn paint_bottom_controls(ui: &mut egui::Ui, state: &mut AppState, commands: &mut Vec<AppCommand>) {
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    let vmin = theme::vmin(ui);
    let enabled = !state.ui.controls_locked;
    let gap = vmin;
    let font_size = 5.0 * vmin;
    let padding = 0.2 * vmin;
    let radius = vmin;
    let middle_width = 20.0 * vmin;
    let middle_height = 11.5 * vmin;
    let bottom = screen.bottom() - 0.5 * vmin;

    let left = [("-10m", -600), ("-1m", -60), ("-5s", -5)];
    let right = [("+5s", 5), ("+1m", 60), ("+10m", 600)];
    let left_sizes: Vec<_> = left
        .iter()
        .map(|(label, _)| text_control_size(ui, label, font_size, padding))
        .collect();
    let right_sizes: Vec<_> = right
        .iter()
        .map(|(label, _)| text_control_size(ui, label, font_size, padding))
        .collect();
    let total_width = left_sizes.iter().map(|size| size.x).sum::<f32>()
        + right_sizes.iter().map(|size| size.x).sum::<f32>()
        + middle_width
        + 6.0 * gap;
    let mut x = screen.center().x - total_width * 0.5;

    for ((label, offset), size) in left.into_iter().zip(left_sizes) {
        let y = bottom - size.y;
        let mut clicked = false;
        egui::Area::new(egui::Id::new(("bottom-seek", label)))
            .fixed_pos(egui::pos2(x, y))
            .order(egui::Order::Foreground)
            .show(&ctx, |ui| {
                clicked = text_control_button(ui, label, font_size, padding, radius, enabled, None)
                    .clicked();
            });
        if clicked {
            commands.push(AppCommand::SeekRelative(offset));
        }
        x += size.x + gap;
    }

    let middle_x = x;
    egui::Area::new(egui::Id::new("bottom-controls-middle"))
        .fixed_pos(egui::pos2(middle_x, bottom - middle_height))
        .order(egui::Order::Foreground)
        .show(&ctx, |ui| {
            ui.set_min_size(egui::vec2(middle_width, middle_height));
            ui.set_max_width(middle_width);
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.y = vmin;
                let rates = state.available_rates().to_vec();
                let mut selected_rate = state.playback_rate();
                let speed_width = 14.0 * vmin;
                ui.allocate_ui_with_layout(
                    egui::vec2(speed_width, 5.0 * vmin),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.style_mut().override_font_id =
                            Some(egui::FontId::proportional(3.5 * vmin));
                        ui.visuals_mut().widgets.inactive.weak_bg_fill = theme::WHITE;
                        ui.visuals_mut().widgets.hovered.weak_bg_fill = theme::LIGHT_PURPLE;
                        ui.visuals_mut().widgets.active.weak_bg_fill = theme::LIGHT_PURPLE;
                        ui.add_enabled_ui(enabled, |ui| {
                            egui::ComboBox::from_id_salt("speed-select")
                                .width(speed_width)
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
                    },
                );

                let time_text = format_colon_time(state.position());
                let time_fill =
                    matches!(state.playback_state(), PlaybackState::Seeking).then_some(theme::PINK);
                if text_control_button(
                    ui, &time_text, font_size, padding, radius, enabled, time_fill,
                )
                .clicked()
                {
                    state.open_seek_dialog();
                }
            });
        });
    x += middle_width + gap;

    for ((label, offset), size) in right.into_iter().zip(right_sizes) {
        let y = bottom - size.y;
        let mut clicked = false;
        egui::Area::new(egui::Id::new(("bottom-seek", label)))
            .fixed_pos(egui::pos2(x, y))
            .order(egui::Order::Foreground)
            .show(&ctx, |ui| {
                clicked = text_control_button(ui, label, font_size, padding, radius, enabled, None)
                    .clicked();
            });
        if clicked {
            commands.push(AppCommand::SeekRelative(offset));
        }
        x += size.x + gap;
    }
}

fn paint_lock_icon(ui: &egui::Ui, rect: egui::Rect, locked: bool) {
    let image = if locked {
        egui::Image::new(egui::include_image!("../../assets/player-icons/locked.svg"))
    } else {
        egui::Image::new(egui::include_image!(
            "../../assets/player-icons/unlocked.svg"
        ))
    };
    image.paint_at(ui, rect);
}

fn paint_lock_slider(
    ui: &mut egui::Ui,
    state: &mut AppState,
    commands: &mut Vec<AppCommand>,
) -> egui::Rect {
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    let vmin = theme::vmin(ui);
    let travel = screen.width() * 0.25;
    let size = 7.0 * vmin;
    let x = state.ui.lock_drag_fraction * travel;

    let area = egui::Area::new(egui::Id::new("control-lock-slider"))
        .fixed_pos(egui::pos2(x, screen.center().y - size * 0.5))
        .order(egui::Order::Foreground)
        .show(&ctx, |ui| {
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::drag());
            let ready = state.ui.lock_drag_fraction >= 1.0;
            let fill = if response.hovered() {
                theme::LIGHT_PURPLE
            } else {
                theme::WHITE
            };
            if ready {
                for (expand, alpha) in [(1.0, 90), (2.0, 55), (3.0, 30)] {
                    ui.painter().rect_stroke(
                        rect.expand(expand * vmin),
                        vmin,
                        egui::Stroke::new(
                            0.45 * vmin,
                            theme::PINK.gamma_multiply(alpha as f32 / 255.0),
                        ),
                        egui::StrokeKind::Outside,
                    );
                }
            }
            ui.painter().rect_filled(rect, vmin, fill);
            paint_lock_icon(ui, rect, state.ui.controls_locked);

            if response.drag_started() {
                state.begin_lock_drag();
                ctx.request_repaint();
            }
            if response.dragged()
                && let Some(delta) = response.total_drag_delta()
            {
                state.set_lock_drag_delta(delta.x / travel);
                ctx.request_repaint();
            }
            if response.drag_stopped() {
                if state.end_lock_drag() {
                    commands.push(AppCommand::ToggleControlsLock);
                }
                ctx.request_repaint();
            }
            response.on_hover_text("Drag right to lock/unlock controls");
        });
    area.response.rect
}
