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

fn menu_text_width(ui: &egui::Ui, text: egui::RichText) -> f32 {
    egui::WidgetText::from(text)
        .into_galley(
            ui,
            Some(egui::TextWrapMode::Extend),
            f32::INFINITY,
            egui::TextStyle::Body,
        )
        .size()
        .x
}

fn saved_position_rows(state: &AppState) -> Vec<Vec<String>> {
    let current_id = state.source().map(|source| source.id.clone());
    let current_position = state.position();

    state
        .saved_positions()
        .iter()
        .map(|entry| {
            let is_current = current_id.as_deref() == Some(entry.source.id.as_str());
            let mut position_text = format_colon_time(entry.position);
            if is_current {
                position_text.push_str(" (");
                position_text.push_str(&format_relative_position(entry.position, current_position));
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
            vec![
                format_age(entry.modified_age),
                entry.device_id.clone(),
                position_text,
                title,
                release,
            ]
        })
        .collect()
}

fn saved_positions_table_intrinsic_width(
    ui: &egui::Ui,
    rows: &[Vec<String>],
    vmin: f32,
    font_size: f32,
) -> f32 {
    let headings = [
        "Last Watched",
        "Device",
        "Position",
        "Video",
        "Release Date",
    ];
    let cell_padding = (0.25 * vmin).max(1.0);
    let grid_gap = 1.0;
    let mut column_widths = vec![0.0_f32; headings.len()];

    for (column, heading) in headings.iter().enumerate() {
        column_widths[column] = menu_text_width(
            ui,
            egui::RichText::new(*heading)
                .size(font_size)
                .strong()
                .color(egui::Color32::BLACK),
        );
    }
    for row in rows {
        for (column, cell) in row.iter().enumerate() {
            column_widths[column] = column_widths[column].max(menu_text_width(
                ui,
                egui::RichText::new(cell)
                    .size(font_size)
                    .color(egui::Color32::BLACK),
            ));
        }
    }

    column_widths
        .iter()
        .map(|width| width + 2.0 * cell_padding)
        .sum::<f32>()
        + grid_gap * column_widths.len().saturating_sub(1) as f32
}

fn intrinsic_menu_width(
    ui: &egui::Ui,
    state: &AppState,
    vmin: f32,
    font_size: f32,
    max_width: f32,
) -> f32 {
    let row_horizontal_padding = 2.0 * vmin;
    let content_horizontal_padding = 2.0 * f32::from(vmin.round() as i8);
    let mut width = 30.0 * vmin;

    let action_width = |label: &str| {
        menu_text_width(
            ui,
            egui::RichText::new(label)
                .size(font_size)
                .strong()
                .color(theme::PURPLE),
        ) + row_horizontal_padding
    };
    width = width.max(action_width("Change Video…"));

    if state.has_video() {
        let quality_label = menu_text_width(
            ui,
            egui::RichText::new("Quality:")
                .size(font_size)
                .strong()
                .color(theme::PURPLE),
        );
        let selected_quality = state
            .quality()
            .map(|quality| quality.label.as_str())
            .unwrap_or("Unknown");
        let selected_width =
            menu_text_width(ui, egui::RichText::new(selected_quality).size(font_size));
        let spacing = ui.spacing();
        let combo_width = spacing.combo_width.max(
            selected_width
                + spacing.icon_spacing
                + spacing.icon_width
                + 2.0 * spacing.button_padding.x,
        );
        let favourites_width = menu_text_width(
            ui,
            egui::RichText::new("Set favourites…")
                .size(font_size * 0.75)
                .color(theme::PURPLE),
        );
        width = width.max(
            content_horizontal_padding
                + quality_label
                + combo_width
                + favourites_width
                + 2.0 * spacing.item_spacing.x,
        );
    }

    if !state.signed_in() {
        width = width.max(
            content_horizontal_padding
                + menu_text_width(
                    ui,
                    egui::RichText::new("Sign in to show synced positions.").size(font_size),
                ),
        );
    } else {
        let positions = state.saved_positions();
        if state.saved_positions_loading() {
            let loading = if positions.is_empty() {
                "Loading saved positions…"
            } else {
                "Refreshing saved positions…"
            };
            width = width.max(
                content_horizontal_padding
                    + ui.style().spacing.interact_size.y
                    + ui.spacing().item_spacing.x
                    + menu_text_width(ui, egui::RichText::new(loading).size(font_size)),
            );
        }
        if let Some(error) = state.saved_positions_error() {
            width = width.max(
                content_horizontal_padding
                    + menu_text_width(
                        ui,
                        egui::RichText::new(format!("Unable to refresh saved positions: {error}"))
                            .size(font_size),
                    ),
            );
        }
        if positions.is_empty() {
            if !state.saved_positions_loading() {
                width = width.max(
                    content_horizontal_padding
                        + menu_text_width(
                            ui,
                            egui::RichText::new("No saved positions").size(font_size),
                        ),
                );
            }
        } else {
            let rows = saved_position_rows(state);
            width = width.max(
                content_horizontal_padding
                    + saved_positions_table_intrinsic_width(ui, &rows, vmin, font_size),
            );
        }
    }

    let account_label = if state.signed_in() {
        format!(
            "Sign out ({}/{})…",
            state.user_id().unwrap_or("?"),
            state.device_id().unwrap_or("?")
        )
    } else {
        "Sign in…".to_owned()
    };
    width = width.max(action_width(&account_label));

    width.min(max_width)
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
    let font_size = (2.0 * vmin).max(16.0);
    let width = intrinsic_menu_width(ui, state, vmin, font_size, screen.width());
    let top = screen.top() + 12.0 * vmin;
    let pos = egui::pos2(screen.right() - width, top - (1.0 - openness) * 2.0 * vmin);

    let area = egui::Area::new(egui::Id::new("player-menu"))
        .fixed_pos(pos)
        .order(egui::Order::Foreground)
        .show(&ctx, |ui| {
            if !state.ui.menu_open {
                ui.disable();
            }
            ui.style_mut().override_font_id = Some(egui::FontId::proportional(font_size));
            ui.spacing_mut().item_spacing.y = 0.0;
            let frame = egui::Frame::new()
                .fill(theme::WHITE)
                .stroke(egui::Stroke::new((0.1 * vmin).max(1.0), theme::PURPLE))
                .corner_radius(vmin.round() as u8)
                .inner_margin(egui::Margin::same(0));
            frame.show(ui, |ui| {
                ui.set_width(width);
                ui.set_max_height((screen.bottom() - top).max(20.0 * vmin));
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if menu_action_row(ui, "Change Video…", vmin, font_size).clicked() {
                        state.open_change_video_dialog();
                        state.close_menu();
                    }

                    if state.has_video() {
                        menu_content_row(ui, vmin, |ui| {
                            let qualities = state.available_qualities().to_vec();
                            let current = state.quality().map(|quality| quality.id.clone());
                            ui.horizontal_wrapped(|ui| {
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
                                                commands.push(AppCommand::SetQuality(
                                                    quality.id.clone(),
                                                ));
                                            }
                                        }
                                    });
                                if inline_menu_action(ui, "Set favourites…", font_size * 0.75)
                                    .clicked()
                                {
                                    state.open_favourites_dialog();
                                    state.close_menu();
                                }
                            });
                        });
                    }

                    menu_content_row(ui, vmin, |ui| {
                        render_saved_positions(ui, state, commands, vmin, font_size);
                    });

                    if state.signed_in() {
                        let label = format!(
                            "Sign out ({}/{})…",
                            state.user_id().unwrap_or("?"),
                            state.device_id().unwrap_or("?")
                        );
                        if menu_action_row(ui, &label, vmin, font_size).clicked() {
                            state.open_sign_out_dialog();
                            state.close_menu();
                        }
                    } else if menu_action_row(ui, "Sign in…", vmin, font_size).clicked() {
                        state.open_sign_in_dialog();
                        state.close_menu();
                    }
                });
            });
        });
    Some(area.response.rect)
}

fn menu_row_stroke(vmin: f32) -> egui::Stroke {
    egui::Stroke::new((0.1 * vmin).max(1.0), theme::PURPLE)
}

fn paint_menu_row_borders(ui: &egui::Ui, rect: egui::Rect, vmin: f32) {
    let stroke = menu_row_stroke(vmin);
    ui.painter().hline(rect.x_range(), rect.top(), stroke);
    ui.painter().hline(rect.x_range(), rect.bottom(), stroke);
}

fn menu_action_row(ui: &mut egui::Ui, label: &str, vmin: f32, font_size: f32) -> egui::Response {
    let text = egui::WidgetText::from(
        egui::RichText::new(label)
            .size(font_size)
            .strong()
            .color(theme::PURPLE),
    );
    let galley = text.into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Body,
    );
    let horizontal_padding = vmin;
    let vertical_padding = 0.5 * vmin;
    let desired_size = egui::vec2(
        ui.available_width(),
        galley.size().y + 2.0 * vertical_padding,
    );
    let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::click());
    let hovered = !cfg!(target_os = "android") && response.hovered();
    ui.painter().rect_filled(
        rect,
        0.0,
        if hovered {
            theme::LIGHT_PURPLE
        } else {
            theme::WHITE
        },
    );
    paint_menu_row_borders(ui, rect, vmin);
    ui.painter().galley(
        egui::pos2(
            rect.left() + horizontal_padding,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        theme::PURPLE,
    );
    response
}

fn menu_content_row<R>(
    ui: &mut egui::Ui,
    vmin: f32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let horizontal_margin = vmin.round() as i8;
    let available_width = ui.available_width();
    let row = egui::Frame::new()
        .fill(theme::WHITE)
        .inner_margin(egui::Margin::symmetric(
            horizontal_margin,
            (0.5 * vmin).round() as i8,
        ))
        .show(ui, |ui| {
            ui.set_min_width((available_width - 2.0 * f32::from(horizontal_margin)).max(0.0));
            add_contents(ui)
        });
    paint_menu_row_borders(ui, row.response.rect, vmin);
    row
}

fn inline_menu_action(ui: &mut egui::Ui, label: &str, font_size: f32) -> egui::Response {
    let background = ui.painter().add(egui::Shape::Noop);
    let response = ui.add(
        egui::Label::new(
            egui::RichText::new(label)
                .size(font_size)
                .color(theme::PURPLE),
        )
        .sense(egui::Sense::click()),
    );
    if !cfg!(target_os = "android") && response.hovered() {
        ui.painter().set(
            background,
            egui::Shape::rect_filled(response.rect, 0.0, theme::LIGHT_PURPLE),
        );
    }
    response
}

fn render_saved_positions(
    ui: &mut egui::Ui,
    state: &mut AppState,
    commands: &mut Vec<AppCommand>,
    vmin: f32,
    font_size: f32,
) {
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
    let highlight = positions
        .iter()
        .enumerate()
        .filter(|(_, entry)| Some(&entry.source.id) == current_id.as_ref())
        .max_by_key(|(_, entry)| entry.position)
        .map(|(index, _)| index);

    let rows = saved_position_rows(state);

    let clicked = egui::ScrollArea::horizontal()
        .show(ui, |ui| {
            render_saved_positions_table(ui, &rows, highlight, vmin, font_size)
        })
        .inner;

    if let Some(index) = clicked {
        let entry = &positions[index];
        let is_current = current_id.as_deref() == Some(entry.source.id.as_str());
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

fn render_saved_positions_table(
    ui: &mut egui::Ui,
    rows: &[Vec<String>],
    highlight: Option<usize>,
    vmin: f32,
    font_size: f32,
) -> Option<usize> {
    let headings = [
        "Last Watched",
        "Device",
        "Position",
        "Video",
        "Release Date",
    ];
    let heading_galleys: Vec<_> = headings
        .iter()
        .map(|heading| {
            egui::WidgetText::from(
                egui::RichText::new(*heading)
                    .size(font_size)
                    .strong()
                    .color(egui::Color32::BLACK),
            )
            .into_galley(
                ui,
                Some(egui::TextWrapMode::Extend),
                f32::INFINITY,
                egui::TextStyle::Body,
            )
        })
        .collect();
    let row_galleys: Vec<Vec<_>> = rows
        .iter()
        .map(|cells| {
            cells
                .iter()
                .map(|cell| {
                    egui::WidgetText::from(
                        egui::RichText::new(cell)
                            .size(font_size)
                            .color(egui::Color32::BLACK),
                    )
                    .into_galley(
                        ui,
                        Some(egui::TextWrapMode::Extend),
                        f32::INFINITY,
                        egui::TextStyle::Body,
                    )
                })
                .collect()
        })
        .collect();

    let cell_padding = (0.25 * vmin).max(1.0);
    let grid_gap = 1.0;
    let mut column_widths = vec![0.0_f32; headings.len()];
    let mut text_height = 0.0_f32;
    for (column, galley) in heading_galleys.iter().enumerate() {
        column_widths[column] = column_widths[column].max(galley.size().x);
        text_height = text_height.max(galley.size().y);
    }
    for row in &row_galleys {
        for (column, galley) in row.iter().enumerate() {
            column_widths[column] = column_widths[column].max(galley.size().x);
            text_height = text_height.max(galley.size().y);
        }
    }
    for width in &mut column_widths {
        *width += 2.0 * cell_padding;
    }

    let row_height = text_height + 2.0 * cell_padding;
    let table_width =
        column_widths.iter().sum::<f32>() + grid_gap * column_widths.len().saturating_sub(1) as f32;
    let table_rows = rows.len() + 1;
    let table_height =
        row_height * table_rows as f32 + grid_gap * table_rows.saturating_sub(1) as f32;
    let (table_rect, _) =
        ui.allocate_exact_size(egui::vec2(table_width, table_height), egui::Sense::hover());
    let painter = ui.painter().clone();
    painter.rect_filled(table_rect, 0.0, theme::LIGHT_PURPLE);

    let paint_row = |painter: &egui::Painter,
                     y: f32,
                     galleys: &[std::sync::Arc<egui::Galley>],
                     fill: egui::Color32| {
        let mut x = table_rect.left();
        for (column, galley) in galleys.iter().enumerate() {
            let cell_rect = egui::Rect::from_min_size(
                egui::pos2(x, y),
                egui::vec2(column_widths[column], row_height),
            );
            painter.rect_filled(cell_rect, 0.0, fill);
            painter.galley(
                egui::pos2(
                    cell_rect.left() + cell_padding,
                    cell_rect.center().y - galley.size().y * 0.5,
                ),
                galley.clone(),
                egui::Color32::BLACK,
            );
            x += column_widths[column] + grid_gap;
        }
    };

    paint_row(&painter, table_rect.top(), &heading_galleys, theme::WHITE);

    let mut clicked = None;
    for (index, galleys) in row_galleys.iter().enumerate() {
        let y = table_rect.top() + (index + 1) as f32 * (row_height + grid_gap);
        let row_rect = egui::Rect::from_min_size(
            egui::pos2(table_rect.left(), y),
            egui::vec2(table_width, row_height),
        );
        let response = ui.interact(
            row_rect,
            ui.id().with(("saved-position-row", index)),
            egui::Sense::click(),
        );
        let fill = if !cfg!(target_os = "android") && response.hovered() {
            theme::LIGHT_PURPLE
        } else if highlight == Some(index) {
            egui::Color32::from_rgb(247, 161, 218)
        } else {
            theme::WHITE
        };
        paint_row(&painter, y, galleys, fill);
        if response.clicked() {
            clicked = Some(index);
        }
    }

    clicked
}
