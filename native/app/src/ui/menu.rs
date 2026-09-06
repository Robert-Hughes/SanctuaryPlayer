use crate::app::AppState;
use crate::model::AppCommand;
use crate::spoilers::sanitise_title;
use crate::time_format::{format_age, format_colon_time, format_relative_position};

pub fn render_button(ui: &mut egui::Ui, state: &mut AppState) {
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::new("menu-button"))
        .fixed_pos(egui::pos2(screen.right() - 92.0, 10.0))
        .show(&ctx, |ui| {
            if ui
                .add_enabled(!state.ui.controls_locked, egui::Button::new("Menu"))
                .clicked()
            {
                state.toggle_menu();
            }
        });
}

pub fn render(ui: &mut egui::Ui, state: &mut AppState, commands: &mut Vec<AppCommand>) {
    if !state.ui.menu_open {
        return;
    }

    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    let pos = egui::pos2((screen.right() - 590.0).max(8.0), 52.0);
    egui::Area::new(egui::Id::new("player-menu"))
        .fixed_pos(pos)
        .order(egui::Order::Foreground)
        .show(&ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(570.0);
                ui.set_max_height((screen.height() - 70.0).max(180.0));
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if ui.button("Change Video…").clicked() {
                        state.open_change_video_dialog();
                        state.close_menu();
                    }

                    if state.has_video() {
                        ui.separator();
                        let qualities = state.available_qualities().to_vec();
                        let current = state.quality().map(|quality| quality.id.clone());
                        ui.horizontal(|ui| {
                            ui.label("Quality:");
                            let mut selected = current.clone().unwrap_or_default();
                            egui::ComboBox::from_id_salt("quality-select")
                                .selected_text(
                                    state
                                        .quality()
                                        .map(|quality| quality.label.as_str())
                                        .unwrap_or("Unknown"),
                                )
                                .show_ui(ui, |ui| {
                                    for quality in &qualities {
                                        if ui
                                            .selectable_value(
                                                &mut selected,
                                                quality.id.clone(),
                                                &quality.label,
                                            )
                                            .changed()
                                        {
                                            commands
                                                .push(AppCommand::SetQuality(quality.id.clone()));
                                        }
                                    }
                                });
                            if ui.small_button("Set favourites…").clicked() {
                                state.open_favourites_dialog();
                                state.close_menu();
                            }
                        });
                    }

                    ui.separator();
                    render_saved_positions(ui, state, commands);
                    ui.separator();

                    if state.signed_in() {
                        let label = format!(
                            "Sign out ({}/{})…",
                            state.user_id().unwrap_or("?"),
                            state.device_id().unwrap_or("?")
                        );
                        if ui.button(label).clicked() {
                            state.open_sign_out_dialog();
                            state.close_menu();
                        }
                    } else if ui.button("Sign in…").clicked() {
                        state.open_sign_in_dialog();
                        state.close_menu();
                    }
                });
            });
        });
}

fn render_saved_positions(ui: &mut egui::Ui, state: &mut AppState, commands: &mut Vec<AppCommand>) {
    ui.strong("Saved Positions");
    if !state.signed_in() {
        ui.label("Sign in to show synced positions (dummy data for now). ");
        return;
    }

    let positions = state.saved_positions();
    if positions.is_empty() {
        ui.label("No saved positions");
        return;
    }
    let current_id = state.source().map(|source| source.id.clone());
    let current_position = state.position();
    let highlight = positions
        .iter()
        .enumerate()
        .filter(|(_, entry)| Some(&entry.source.id) == current_id.as_ref())
        .max_by_key(|(_, entry)| entry.position)
        .map(|(index, _)| index);

    egui::ScrollArea::horizontal().show(ui, |ui| {
        egui::Grid::new("saved-positions-grid")
            .striped(true)
            .spacing(egui::vec2(12.0, 5.0))
            .show(ui, |ui| {
                for heading in [
                    "Last Watched",
                    "Device",
                    "Position",
                    "Video",
                    "Release Date",
                ] {
                    ui.strong(heading);
                }
                ui.end_row();

                for (index, entry) in positions.iter().enumerate() {
                    let is_current = current_id.as_deref() == Some(entry.source.id.as_str());
                    let mut position_text = format_colon_time(entry.position);
                    if is_current {
                        position_text.push_str(" (");
                        position_text
                            .push_str(&format_relative_position(entry.position, current_position));
                        position_text.push(')');
                    }
                    let title = entry
                        .title
                        .as_deref()
                        .map(sanitise_title)
                        .unwrap_or_else(|| entry.source.id.clone());
                    let release = entry
                        .release_age
                        .map(format_age)
                        .unwrap_or_else(|| "?".into());
                    let cells = [
                        format_age(entry.modified_age),
                        entry.device_id.clone(),
                        position_text,
                        title,
                        release,
                    ];
                    let fill =
                        (highlight == Some(index)).then_some(egui::Color32::from_rgb(80, 35, 72));
                    let mut clicked = false;
                    for cell in cells {
                        let button = egui::Button::new(cell).frame(false);
                        let response = if let Some(fill) = fill {
                            ui.add(button.fill(fill))
                        } else {
                            ui.add(button)
                        };
                        clicked |= response.clicked();
                    }
                    ui.end_row();
                    if clicked {
                        if is_current {
                            commands.push(AppCommand::SeekAbsolute(entry.position));
                        } else {
                            let mut source = entry.source.clone();
                            source.start_time = Some(entry.position);
                            commands.push(AppCommand::OpenVideo(source));
                        }
                        state.close_menu();
                    }
                }
            });
    });
}
