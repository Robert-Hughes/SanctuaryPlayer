use crate::app::{AppState, DialogState};
use crate::model::AppCommand;
use crate::time_format::parse_friendly_time;
use crate::video::VideoSource;

pub fn render(ui: &mut egui::Ui, state: &mut AppState, commands: &mut Vec<AppCommand>) {
    let Some(mut dialog) = state.ui.dialog.take() else {
        return;
    };
    let ctx = ui.ctx().clone();
    let mut keep_open = true;

    match &mut dialog {
        DialogState::ChangeVideo { input, error } => {
            egui::Window::new("Change Video")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.label("Enter a YouTube/Twitch video URL or video ID:");
                    let response = ui.text_edit_singleline(input);
                    if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        submit_video(input, error, commands, &mut keep_open);
                    }
                    if let Some(error) = error.as_ref() {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Open").clicked() {
                            submit_video(input, error, commands, &mut keep_open);
                        }
                        if ui.button("Cancel").clicked() {
                            keep_open = false;
                        }
                    });
                });
        }
        DialogState::SeekTo { input, error } => {
            egui::Window::new("Go to time")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.label("Enter a time (e.g. 1h23m45s or 1:23:45):");
                    ui.text_edit_singleline(input);
                    if let Some(error) = error.as_ref() {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Seek").clicked() {
                            if let Some(time) = parse_friendly_time(input) {
                                commands.push(AppCommand::SeekAbsolute(time));
                                keep_open = false;
                            } else {
                                *error = Some("Invalid time".into());
                            }
                        }
                        if ui.button("Cancel").clicked() {
                            keep_open = false;
                        }
                    });
                });
        }
        DialogState::FavouriteQualities { input } => {
            egui::Window::new("Favourite qualities")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.label("Comma- or semicolon-separated quality names, in preference order:");
                    ui.text_edit_singleline(input);
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            commands.push(AppCommand::SetFavouriteQualities(input.clone()));
                            keep_open = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep_open = false;
                        }
                    });
                });
        }
        DialogState::SignIn { user_id, device_id } => {
            egui::Window::new("Sign in")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.label("User ID (same ID on another device to sync):");
                    ui.text_edit_singleline(user_id);
                    ui.label("Device ID:");
                    ui.text_edit_singleline(device_id);
                    ui.small("Dummy/local only for now; no network requests are made.");
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                !user_id.trim().is_empty() && !device_id.trim().is_empty(),
                                egui::Button::new("Sign in"),
                            )
                            .clicked()
                        {
                            commands.push(AppCommand::SignIn {
                                user_id: user_id.trim().to_owned(),
                                device_id: device_id.trim().to_owned(),
                            });
                            keep_open = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep_open = false;
                        }
                    });
                });
        }
        DialogState::ConfirmSignOut => {
            egui::Window::new("Sign out")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.label(format!(
                        "Sign out from {}/{}?",
                        state.user_id().unwrap_or("?"),
                        state.device_id().unwrap_or("?")
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Sign out").clicked() {
                            commands.push(AppCommand::SignOut);
                            keep_open = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep_open = false;
                        }
                    });
                });
        }
    }

    if keep_open {
        state.ui.dialog = Some(dialog);
    } else {
        state.close_dialog();
    }
}

fn submit_video(
    input: &str,
    error: &mut Option<String>,
    commands: &mut Vec<AppCommand>,
    keep_open: &mut bool,
) {
    match VideoSource::parse(input) {
        Ok(source) => {
            commands.push(AppCommand::OpenVideo(source));
            *keep_open = false;
        }
        Err(parse_error) => *error = Some(parse_error.to_string()),
    }
}
