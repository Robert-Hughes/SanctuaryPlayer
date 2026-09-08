use crate::app::{AppState, DialogState};
use crate::model::AppCommand;
use crate::time_format::parse_friendly_time;
use crate::video::VideoSource;

pub fn render(ui: &mut egui::Ui, state: &mut AppState, commands: &mut Vec<AppCommand>) {
    let Some(mut dialog) = state.ui.dialog.take() else {
        return;
    };
    let ctx = ui.ctx().clone();
    let focus_first_input = std::mem::take(&mut state.ui.focus_first_dialog_input);

    if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        state.close_dialog();
        return;
    }
    let accept_pressed =
        ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
    let mut keep_open = true;

    match &mut dialog {
        DialogState::ChangeVideo { input, error } => {
            egui::Window::new("Change Video")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.label("Enter a YouTube/Twitch video URL or video ID:");
                    let response = ui.add(
                        egui::TextEdit::singleline(input)
                            .id(ui.make_persistent_id("change-video-input")),
                    );
                    if focus_first_input {
                        response.request_focus();
                    }
                    if let Some(error) = error.as_ref() {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Open").clicked() || accept_pressed {
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
                    let response = ui.add(
                        egui::TextEdit::singleline(input)
                            .id(ui.make_persistent_id("seek-to-input")),
                    );
                    if focus_first_input {
                        response.request_focus();
                    }
                    if let Some(error) = error.as_ref() {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Seek").clicked() || accept_pressed {
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
                    let response = ui.add(
                        egui::TextEdit::singleline(input)
                            .id(ui.make_persistent_id("favourite-qualities-input")),
                    );
                    if focus_first_input {
                        response.request_focus();
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() || accept_pressed {
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
                    let response = ui.add(
                        egui::TextEdit::singleline(user_id)
                            .id(ui.make_persistent_id("sign-in-user-id-input")),
                    );
                    if focus_first_input {
                        response.request_focus();
                    }
                    ui.label("Device ID:");
                    ui.add(
                        egui::TextEdit::singleline(device_id)
                            .id(ui.make_persistent_id("sign-in-device-id-input")),
                    );
                    ui.small("Dummy/local only for now; no network requests are made.");
                    ui.horizontal(|ui| {
                        let valid = !user_id.trim().is_empty() && !device_id.trim().is_empty();
                        if ui
                            .add_enabled(valid, egui::Button::new("Sign in"))
                            .clicked()
                            || (accept_pressed && valid)
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
                        if ui.button("Sign out").clicked() || accept_pressed {
                            commands.push(AppCommand::SignOut);
                            keep_open = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep_open = false;
                        }
                    });
                });
        }
        DialogState::TwitchResolving { video_id } => {
            egui::Window::new("Opening Twitch video")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("Resolving Twitch VOD {video_id}…"));
                    });
                });
        }
        DialogState::TwitchResolved { url } => {
            egui::Window::new("Twitch HLS URL")
                .collapsible(false)
                .resizable(true)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .default_width(640.0)
                .show(&ctx, |ui| {
                    ui.label("Resolved master playlist URL:");
                    ui.add(
                        egui::TextEdit::multiline(url)
                            .desired_width(f32::INFINITY)
                            .desired_rows(4),
                    );
                    if ui.button("OK").clicked() || accept_pressed {
                        keep_open = false;
                    }
                });
        }
        DialogState::Message { title, message } => {
            egui::Window::new(title.as_str())
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(&ctx, |ui| {
                    ui.label(message.as_str());
                    if ui.button("OK").clicked() || accept_pressed {
                        keep_open = false;
                    }
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
