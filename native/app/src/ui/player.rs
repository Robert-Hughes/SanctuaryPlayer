use crate::app::AppState;
use crate::model::{AppCommand, PlaybackState};
use crate::time_format::{format_age, format_colon_time};

use super::menu;

pub fn render(ui: &mut egui::Ui, state: &mut AppState) -> Vec<AppCommand> {
    let mut commands = Vec::new();
    let rect = ui.max_rect();
    ui.painter()
        .rect_filled(rect, 0.0, egui::Color32::from_rgb(2, 2, 4));
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
    let rect = ui.max_rect().shrink2(egui::vec2(80.0, 75.0));
    ui.painter()
        .rect_filled(rect, 6.0, egui::Color32::from_rgb(14, 16, 24));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        format!(
            "DUMMY VIDEO\n{} {}\n1280 × 720",
            state.source().unwrap().platform,
            state.source().unwrap().id
        ),
        egui::FontId::proportional(22.0),
        egui::Color32::from_gray(90),
    );
}

fn paint_top_info(ui: &mut egui::Ui, state: &AppState) {
    let ctx = ui.ctx().clone();
    egui::Area::new(egui::Id::new("player-top-info"))
        .fixed_pos(egui::pos2(16.0, 12.0))
        .show(&ctx, |ui| {
            ui.set_max_width((ui.ctx().content_rect().width() - 160.0).max(100.0));
            if let Some(title) = state.safe_title() {
                ui.label(
                    egui::RichText::new(title)
                        .size(24.0)
                        .color(egui::Color32::from_rgb(190, 185, 255)),
                );
            }
            if let Some(age) = state.release_age() {
                ui.label(
                    egui::RichText::new(format_age(age))
                        .size(16.0)
                        .color(egui::Color32::from_gray(180)),
                );
            }
        });
}

fn paint_centre_controls(ui: &mut egui::Ui, state: &AppState, commands: &mut Vec<AppCommand>) {
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::new("centre-controls"))
        .fixed_pos(egui::pos2(
            screen.center().x - 95.0,
            screen.center().y - 36.0,
        ))
        .show(&ctx, |ui| {
            ui.add_enabled_ui(!state.ui.controls_locked, |ui| {
                ui.horizontal(|ui| {
                    let label = match state.playback_state() {
                        PlaybackState::Playing => "Pause",
                        PlaybackState::Seeking => "Seeking…",
                        PlaybackState::Ended => "Ended",
                        _ => "Play",
                    };
                    let enabled = !matches!(
                        state.playback_state(),
                        PlaybackState::Seeking | PlaybackState::Ended
                    );
                    if ui
                        .add_enabled(
                            enabled,
                            egui::Button::new(egui::RichText::new(label).size(24.0)),
                        )
                        .clicked()
                    {
                        commands.push(AppCommand::TogglePlayback);
                    }
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new("Fullscreen").size(20.0),
                        ))
                        .clicked()
                    {
                        commands.push(AppCommand::ToggleFullscreen);
                    }
                });
            });
        });
}

fn paint_bottom_controls(ui: &mut egui::Ui, state: &mut AppState, commands: &mut Vec<AppCommand>) {
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::new("bottom-controls"))
        .fixed_pos(egui::pos2(12.0, screen.bottom() - 70.0))
        .show(&ctx, |ui| {
            ui.add_enabled_ui(!state.ui.controls_locked, |ui| {
                ui.horizontal(|ui| {
                    for (label, offset) in [("-10m", -600), ("-1m", -60), ("-5s", -5)] {
                        if ui.button(label).clicked() {
                            commands.push(AppCommand::SeekRelative(offset));
                        }
                    }

                    ui.add_space(8.0);
                    let rates = state.available_rates().to_vec();
                    let mut selected_rate = state.playback_rate();
                    egui::ComboBox::from_id_salt("speed-select")
                        .selected_text(format!("{selected_rate}x"))
                        .show_ui(ui, |ui| {
                            for rate in rates {
                                if ui
                                    .selectable_value(&mut selected_rate, rate, format!("{rate}x"))
                                    .changed()
                                {
                                    commands.push(AppCommand::SetPlaybackRate(rate));
                                }
                            }
                        });
                    if ui
                        .button(
                            egui::RichText::new(format_colon_time(state.position()))
                                .size(20.0)
                                .strong(),
                        )
                        .clicked()
                    {
                        state.open_seek_dialog();
                    }
                    ui.add_space(8.0);

                    for (label, offset) in [("+5s", 5), ("+1m", 60), ("+10m", 600)] {
                        if ui.button(label).clicked() {
                            commands.push(AppCommand::SeekRelative(offset));
                        }
                    }
                });
            });
        });
}

fn paint_lock_slider(ui: &mut egui::Ui, state: &mut AppState, commands: &mut Vec<AppCommand>) {
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    let travel = (screen.width() * 0.25).max(80.0);
    let x = 8.0 + state.ui.lock_drag_fraction * travel;
    let y = screen.center().y - 26.0;

    egui::Area::new(egui::Id::new("control-lock-slider"))
        .fixed_pos(egui::pos2(x, y))
        .order(egui::Order::Foreground)
        .show(&ctx, |ui| {
            let (rect, response) =
                ui.allocate_exact_size(egui::vec2(58.0, 52.0), egui::Sense::drag());
            let ready = state.ui.lock_drag_fraction >= 0.95;
            let fill = if ready {
                egui::Color32::from_rgb(181, 23, 158)
            } else {
                egui::Color32::from_rgb(245, 245, 250)
            };
            ui.painter().rect_filled(rect, 6.0, fill);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                if state.ui.controls_locked {
                    "LOCK"
                } else {
                    "OPEN"
                },
                egui::FontId::proportional(13.0),
                egui::Color32::from_rgb(80, 70, 180),
            );
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
            }
            response.on_hover_text("Drag right to lock/unlock controls");
        });
}
