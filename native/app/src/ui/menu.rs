use crate::app::AppState;
use crate::model::AppCommand;
use crate::spoilers::sanitise_title;
use crate::time_format::{format_age, format_colon_time, format_relative_position};

use super::theme;

pub fn render_button(ui: &mut egui::Ui, state: &mut AppState) -> egui::Rect {
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    let vmin = theme::vmin(ui);
    // CSS: 9vmin content + 1.5vmin padding on each side.
    let size = 12.0 * vmin;
    let area = egui::Area::new(egui::Id::new("menu-button"))
        .fixed_pos(egui::pos2(screen.right() - size, screen.top()))
        .order(egui::Order::Foreground)
        .show(&ctx, |ui| {
            let enabled = !state.ui.controls_locked;
            let sense = if enabled {
                egui::Sense::click()
            } else {
                egui::Sense::hover()
            };
            let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), sense);
            let fill = if state.ui.menu_open || (enabled && response.hovered()) {
                theme::LIGHT_PURPLE
            } else {
                theme::WHITE
            };
            ui.painter().rect_filled(rect, vmin, fill);
            let icon_rect = rect.shrink(1.5 * vmin);
            let icon_colour = if enabled {
                theme::ICON_PURPLE
            } else {
                egui::Color32::from_rgba_unmultiplied(
                    theme::ICON_PURPLE.r(),
                    theme::ICON_PURPLE.g(),
                    theme::ICON_PURPLE.b(),
                    128,
                )
            };
            let stroke = egui::Stroke::new((0.9 * vmin).max(2.0), icon_colour);
            for y in [0.15_f32, 0.5, 0.85] {
                let yy = egui::lerp(icon_rect.top()..=icon_rect.bottom(), y);
                ui.painter().line_segment(
                    [
                        egui::pos2(icon_rect.left(), yy),
                        egui::pos2(icon_rect.right(), yy),
                    ],
                    stroke,
                );
            }
            if response.clicked() {
                state.toggle_menu();
                ctx.request_repaint();
            }
        });
    area.response.rect
}

pub fn render(
    ui: &mut egui::Ui,
    state: &mut AppState,
    commands: &mut Vec<AppCommand>,
) -> Option<egui::Rect> {
    let ctx = ui.ctx().clone();
    let openness = ctx.animate_bool(egui::Id::new("player-menu-open"), state.ui.menu_open);
    if openness <= 0.001 {
        return None;
    }

    let screen = ctx.content_rect();
    let vmin = theme::vmin(ui);
    let width = (72.0 * vmin)
        .min(screen.width() - 2.0 * vmin)
        .max(30.0 * vmin);
    let top = screen.top() + 12.0 * vmin;
    let pos = egui::pos2(
        screen.right() - width - vmin,
        top - (1.0 - openness) * 2.0 * vmin,
    );
    let font_size = (2.0 * vmin).max(16.0);

    let area = egui::Area::new(egui::Id::new("player-menu"))
        .fixed_pos(pos)
        .order(egui::Order::Foreground)
        .show(&ctx, |ui| {
            if !state.ui.menu_open {
                ui.disable();
            }
            ui.style_mut().override_font_id = Some(egui::FontId::proportional(font_size));
            ui.spacing_mut().item_spacing.y = 0.5 * vmin;
            let frame = egui::Frame::new()
                .fill(theme::WHITE)
                .stroke(egui::Stroke::new((0.1 * vmin).max(1.0), theme::PURPLE))
                .corner_radius(vmin.round() as u8)
                .inner_margin(egui::Margin::same(vmin.round() as i8));
            frame.show(ui, |ui| {
                ui.set_width(width - 2.0 * vmin);
                ui.set_max_height((screen.bottom() - top - vmin).max(20.0 * vmin));
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if ui
                        .add(theme::rounded_button(
                            egui::RichText::new("Change Video…").color(theme::PURPLE),
                            vmin,
                        ))
                        .clicked()
                    {
                        state.open_change_video_dialog();
                        state.close_menu();
                    }

                    if state.has_video() {
                        ui.separator();
                        let qualities = state.available_qualities().to_vec();
                        let current = state.quality().map(|quality| quality.id.clone());
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Quality:")
                                    .strong()
                                    .color(theme::PURPLE),
                            );
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
                    render_saved_positions(ui, state, commands, vmin);
                    ui.separator();

                    if state.signed_in() {
                        let label = format!(
                            "Sign out ({}/{})…",
                            state.user_id().unwrap_or("?"),
                            state.device_id().unwrap_or("?")
                        );
                        if ui
                            .add(theme::rounded_button(
                                egui::RichText::new(label).color(theme::PURPLE),
                                vmin,
                            ))
                            .clicked()
                        {
                            state.open_sign_out_dialog();
                            state.close_menu();
                        }
                    } else if ui
                        .add(theme::rounded_button(
                            egui::RichText::new("Sign in…").color(theme::PURPLE),
                            vmin,
                        ))
                        .clicked()
                    {
                        state.open_sign_in_dialog();
                        state.close_menu();
                    }
                });
            });
        });
    Some(area.response.rect)
}

fn render_saved_positions(
    ui: &mut egui::Ui,
    state: &mut AppState,
    commands: &mut Vec<AppCommand>,
    vmin: f32,
) {
    ui.label(
        egui::RichText::new("Saved Positions")
            .strong()
            .color(theme::PURPLE),
    );
    if !state.signed_in() {
        ui.label("Sign in to show synced positions.");
        return;
    }

    let positions = state.saved_positions();
    if state.saved_positions_loading() {
        ui.horizontal(|ui| {
            super::animated_spinner(ui);
            ui.label(if positions.is_empty() {
                "Loading saved positions…"
            } else {
                "Refreshing saved positions…"
            });
        });
    }
    if let Some(error) = state.saved_positions_error() {
        ui.label(
            egui::RichText::new(format!("Unable to refresh saved positions: {error}"))
                .color(egui::Color32::DARK_RED),
        );
    }
    if positions.is_empty() {
        if !state.saved_positions_loading() {
            ui.label("No saved positions");
        }
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
            .spacing(egui::vec2(1.5 * vmin, 0.5 * vmin))
            .show(ui, |ui| {
                for heading in [
                    "Last Watched",
                    "Device",
                    "Position",
                    "Video",
                    "Release Date",
                ] {
                    ui.label(
                        egui::RichText::new(heading)
                            .strong()
                            .color(egui::Color32::BLACK),
                    );
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
                    let fill = if highlight == Some(index) {
                        egui::Color32::from_rgb(247, 161, 218)
                    } else {
                        theme::WHITE
                    };
                    let mut clicked = false;
                    for cell in cells {
                        let response = ui.add(
                            egui::Button::new(
                                egui::RichText::new(cell).color(egui::Color32::BLACK),
                            )
                            .fill(fill)
                            .frame(true),
                        );
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
