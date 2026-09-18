use crate::app::{AndroidTextField, AppState, DialogState};
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
            show_modal(&ctx, "change-video-dialog", "Change Video", |ui| {
                ui.label("Enter a YouTube/Twitch video URL or video ID:");
                singleline_text_edit(
                    ui,
                    state,
                    AndroidTextField::ChangeVideo,
                    input,
                    "change-video-input",
                    focus_first_input,
                );
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
            show_modal(&ctx, "seek-to-dialog", "Go to time", |ui| {
                ui.label("Enter a time (e.g. 1h23m45s or 1:23:45):");
                singleline_text_edit(
                    ui,
                    state,
                    AndroidTextField::SeekTo,
                    input,
                    "seek-to-input",
                    focus_first_input,
                );
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
            show_modal(
                &ctx,
                "favourite-qualities-dialog",
                "Favourite qualities",
                |ui| {
                    ui.label("Comma- or semicolon-separated quality names, in preference order:");
                    singleline_text_edit(
                        ui,
                        state,
                        AndroidTextField::FavouriteQualities,
                        input,
                        "favourite-qualities-input",
                        focus_first_input,
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() || accept_pressed {
                            commands.push(AppCommand::SetFavouriteQualities(input.clone()));
                            keep_open = false;
                        }
                        if ui.button("Cancel").clicked() {
                            keep_open = false;
                        }
                    });
                },
            );
        }
        DialogState::SignIn { user_id, device_id } => {
            show_modal(&ctx, "sign-in-dialog", "Sign in", |ui| {
                ui.label("User ID (same ID on another device to sync):");
                singleline_text_edit(
                    ui,
                    state,
                    AndroidTextField::SignInUser,
                    user_id,
                    "sign-in-user-id-input",
                    focus_first_input,
                );
                ui.label("Device ID:");
                singleline_text_edit(
                    ui,
                    state,
                    AndroidTextField::SignInDevice,
                    device_id,
                    "sign-in-device-id-input",
                    false,
                );
                ui.small(
                        "Saved positions sync through sanctuaryplayer.robdh.uk. The User ID is not authenticated; anyone who knows it can access the same synced positions.",
                    );
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
            show_modal(&ctx, "sign-out-dialog", "Sign out", |ui| {
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
            show_modal(
                &ctx,
                "twitch-resolving-dialog",
                "Opening Twitch video",
                |ui| {
                    ui.horizontal(|ui| {
                        super::animated_spinner(ui);
                        ui.label(format!("Resolving Twitch VOD {video_id}…"));
                    });
                },
            );
        }
        DialogState::Message { title, message } => {
            show_modal(&ctx, "message-dialog", title.as_str(), |ui| {
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

fn show_modal<R>(
    ctx: &egui::Context,
    id: &'static str,
    title: &str,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    egui::Modal::new(egui::Id::new(id))
        .show(ctx, |ui| {
            ui.heading(title);
            ui.separator();
            add_contents(ui)
        })
        .inner
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

fn singleline_text_edit(
    ui: &mut egui::Ui,
    state: &mut AppState,
    field: AndroidTextField,
    text: &mut String,
    id_salt: &str,
    focus: bool,
) -> egui::Response {
    let output = egui::TextEdit::singleline(text)
        .id(ui.make_persistent_id(id_salt))
        .show(ui);
    if focus {
        output.response.request_focus();
    }
    #[cfg(target_os = "android")]
    {
        let mut output = output;
        state.capture_android_text_edit(ui.ctx(), field, &mut output, text);
        output.response.response
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (state, field);
        output.response.response
    }
}
