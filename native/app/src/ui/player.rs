use crate::app::AppState;
use crate::model::{AppCommand, PlaybackState};
use crate::time_format::{format_age, format_colon_time};

pub fn render(ui: &mut egui::Ui, state: &AppState) -> Vec<AppCommand> {
    let mut commands = Vec::new();
    let rect = ui.max_rect();
    ui.painter()
        .rect_filled(rect, 0.0, egui::Color32::from_rgb(2, 2, 4));

    paint_dummy_video(ui, state);
    paint_top_info(ui, state);
    paint_menu_button(ui);
    paint_centre_controls(ui, state, &mut commands);
    paint_bottom_controls(ui, state, &mut commands);

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

fn paint_menu_button(ui: &mut egui::Ui) {
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::new("menu-button"))
        .fixed_pos(egui::pos2(screen.right() - 92.0, 10.0))
        .show(&ctx, |ui| {
            let _ = ui.button("Menu");
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
}

fn paint_bottom_controls(ui: &mut egui::Ui, state: &AppState, commands: &mut Vec<AppCommand>) {
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::new("bottom-controls"))
        .fixed_pos(egui::pos2(12.0, screen.bottom() - 70.0))
        .show(&ctx, |ui| {
            ui.horizontal(|ui| {
                for (label, offset) in [("-10m", -600), ("-1m", -60), ("-5s", -5)] {
                    if ui.button(label).clicked() {
                        commands.push(AppCommand::SeekRelative(offset));
                    }
                }

                ui.add_space(8.0);
                let mut selected_rate = state.playback_rate();
                egui::ComboBox::from_id_salt("speed-select")
                    .selected_text(format!("{selected_rate}x"))
                    .show_ui(ui, |ui| {
                        for &rate in state.available_rates() {
                            if ui
                                .selectable_value(&mut selected_rate, rate, format!("{rate}x"))
                                .changed()
                            {
                                commands.push(AppCommand::SetPlaybackRate(rate));
                            }
                        }
                    });
                ui.label(
                    egui::RichText::new(format_colon_time(state.position()))
                        .size(20.0)
                        .strong(),
                );
                ui.add_space(8.0);

                for (label, offset) in [("+5s", 5), ("+1m", 60), ("+10m", 600)] {
                    if ui.button(label).clicked() {
                        commands.push(AppCommand::SeekRelative(offset));
                    }
                }
            });
        });
}
