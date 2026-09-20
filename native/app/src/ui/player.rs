use crate::app::AppState;
use crate::model::{AppCommand, DebugEdgeKind, DebugGraphLane, DebugNode, PlaybackState};
use crate::time_format::{format_age, format_colon_time};
use std::collections::HashMap;

use super::{menu, theme};

pub fn render(ui: &mut egui::Ui, state: &mut AppState) -> Vec<AppCommand> {
    let mut commands = Vec::new();
    let rect = ui.max_rect();
    let background = ui.interact(
        rect,
        egui::Id::new("player-background"),
        egui::Sense::click(),
    );

    if state.debug_info_visible() {
        paint_debug_info(ui, state, &mut commands);
    }

    if state.ui.controls_visible {
        paint_top_info(ui, state);
        menu::render_button(ui, state);
        let centre_controls = paint_centre_controls(ui, state, &mut commands);
        paint_centre_status(ui, state, centre_controls);
        paint_bottom_controls(ui, state, &mut commands);
        paint_lock_slider(ui, state, &mut commands);
        menu::render(ui, state, &mut commands);
    }

    if background.clicked() && state.ui.dialog.is_none() {
        commands.push(AppCommand::ToggleControlsVisibility);
    }

    commands
}

fn paint_debug_info(ui: &egui::Ui, state: &AppState, commands: &mut Vec<AppCommand>) {
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    let vmin = theme::vmin(ui);
    let width = (screen.width() - 2.0 * vmin).max(30.0 * vmin);
    let max_height = (screen.height() - 14.0 * vmin).max(20.0 * vmin);
    let graph = state.debug_info_graph();

    egui::Area::new(egui::Id::new("playback-debug-info"))
        .fixed_pos(egui::pos2(screen.left() + vmin, screen.top() + 10.0 * vmin))
        .order(egui::Order::Foreground)
        .show(&ctx, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_black_alpha(220))
                .stroke(egui::Stroke::new(1.0_f32, theme::TOP_INFO))
                .corner_radius((0.8 * vmin).round() as u8)
                .inner_margin(egui::Margin::same((0.8 * vmin).round() as i8))
                .show(ui, |ui| {
                    ui.set_width(width);
                    ui.set_max_height(max_height);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Debug graph")
                                .monospace()
                                .strong()
                                .size((2.2 * vmin).max(13.0))
                                .color(egui::Color32::WHITE),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let close = ui.add(
                                egui::Button::new(
                                    egui::RichText::new("×")
                                        .strong()
                                        .size((2.5 * vmin).max(16.0))
                                        .color(egui::Color32::WHITE),
                                )
                                .frame(false),
                            );
                            if close.clicked() {
                                commands.push(AppCommand::ToggleDebugInfo);
                            }
                        });
                    });
                    ui.separator();
                    egui::ScrollArea::both()
                        .id_salt("playback-debug-graph-scroll")
                        .max_height(max_height - 4.0 * vmin)
                        .show(ui, |ui| {
                            let card_width = (22.0 * vmin).clamp(210.0, 320.0);
                            let column_gap = (2.0 * vmin).max(14.0);
                            let mut node_rects = HashMap::new();
                            let edge_shape_index = ui.painter().add(egui::Shape::Noop);
                            paint_debug_lane(
                                ui,
                                &graph.nodes,
                                DebugGraphLane::Shared,
                                "SHARED / CONTROL",
                                card_width,
                                column_gap,
                                vmin,
                                &mut node_rects,
                            );
                            paint_debug_lane(
                                ui,
                                &graph.nodes,
                                DebugGraphLane::Video,
                                "VIDEO PATH",
                                card_width,
                                column_gap,
                                vmin,
                                &mut node_rects,
                            );
                            paint_debug_lane(
                                ui,
                                &graph.nodes,
                                DebugGraphLane::Audio,
                                "AUDIO PATH",
                                card_width,
                                column_gap,
                                vmin,
                                &mut node_rects,
                            );
                            paint_debug_edges(
                                ui,
                                &graph.edges,
                                &node_rects,
                                vmin,
                                edge_shape_index,
                            );
                        });
                });
        });
}

#[allow(clippy::too_many_arguments)]
fn paint_debug_lane(
    ui: &mut egui::Ui,
    all_nodes: &[DebugNode],
    lane: DebugGraphLane,
    label: &str,
    card_width: f32,
    column_gap: f32,
    vmin: f32,
    node_rects: &mut HashMap<String, egui::Rect>,
) {
    let mut nodes = all_nodes
        .iter()
        .filter(|node| node.lane == lane)
        .collect::<Vec<_>>();
    if nodes.is_empty() {
        return;
    }
    nodes.sort_by_key(|node| node.column);

    ui.add_space((0.7 * vmin).max(5.0));
    ui.label(
        egui::RichText::new(label)
            .monospace()
            .strong()
            .size((1.7 * vmin).max(11.0))
            .color(theme::TOP_INFO),
    );
    ui.add_space((0.3 * vmin).max(2.0));

    ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
        let mut next_column = 0_u8;
        for node in nodes {
            if node.column > next_column {
                let missing = f32::from(node.column - next_column);
                ui.add_space(missing * (card_width + column_gap));
            }
            let rect = paint_debug_node(ui, node, card_width, vmin);
            node_rects.insert(node.id.clone(), rect);
            ui.add_space(column_gap);
            next_column = node.column.saturating_add(1);
        }
    });
}

fn paint_debug_node(ui: &mut egui::Ui, node: &DebugNode, card_width: f32, vmin: f32) -> egui::Rect {
    let expansion_id = egui::Id::new(("debug-graph-node-expanded", node.id.as_str()));
    let expanded = ui
        .ctx()
        .data(|data| data.get_temp::<bool>(expansion_id).unwrap_or(false));
    let arrow = if expanded { "▾" } else { "▸" };

    let frame = egui::Frame::new()
        .fill(egui::Color32::from_rgb(28, 25, 58))
        .stroke(egui::Stroke::new(
            1.0_f32,
            egui::Color32::from_rgb(88, 81, 155),
        ))
        .corner_radius((0.6 * vmin).round() as u8)
        .inner_margin(egui::Margin::same((0.7 * vmin).round() as i8))
        .show(ui, |ui| {
            ui.set_width(card_width);
            ui.set_max_width(card_width);
            let header = ui.add(
                egui::Button::new(
                    egui::RichText::new(format!("{arrow} {}", node.title))
                        .monospace()
                        .strong()
                        .size((1.65 * vmin).max(11.0))
                        .color(egui::Color32::WHITE),
                )
                .frame(false),
            );
            if header.clicked() {
                ui.ctx()
                    .data_mut(|data| data.insert_temp(expansion_id, !expanded));
            }

            if !node.summary.is_empty() {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&node.summary)
                            .monospace()
                            .size((1.45 * vmin).max(10.0))
                            .color(egui::Color32::LIGHT_GRAY),
                    )
                    .truncate(),
                )
                .on_hover_text(&node.summary);
            }

            if expanded {
                ui.separator();
                ui.scope(|ui| {
                    ui.visuals_mut().faint_bg_color = egui::Color32::from_rgb(48, 42, 105);
                    egui::ScrollArea::both()
                        .id_salt(("debug-node-properties", node.id.as_str()))
                        .max_height((17.0 * vmin).clamp(110.0, 220.0))
                        .show(ui, |ui| {
                            egui::Grid::new(("debug-node-grid", node.id.as_str()))
                                .num_columns(2)
                                .spacing(egui::vec2((0.8 * vmin).max(6.0), 0.2 * vmin))
                                .striped(true)
                                .show(ui, |ui| {
                                    for (key, value) in &node.rows {
                                        ui.label(
                                            egui::RichText::new(key)
                                                .monospace()
                                                .size((1.35 * vmin).max(10.0))
                                                .color(egui::Color32::LIGHT_GRAY),
                                        );
                                        ui.label(
                                            egui::RichText::new(value)
                                                .monospace()
                                                .size((1.35 * vmin).max(10.0))
                                                .color(egui::Color32::WHITE),
                                        );
                                        ui.end_row();
                                    }
                                });
                        });
                });
            }
        });
    frame.response.rect
}

fn paint_debug_edges(
    ui: &egui::Ui,
    edges: &[crate::model::DebugEdge],
    node_rects: &HashMap<String, egui::Rect>,
    vmin: f32,
    edge_shape_index: egui::layers::ShapeIdx,
) {
    let painter = ui.painter();
    let mut background_shapes = Vec::new();
    for edge in edges {
        let (Some(from), Some(to)) = (node_rects.get(&edge.from), node_rects.get(&edge.to)) else {
            continue;
        };
        let horizontal =
            (to.center().x - from.center().x).abs() >= (to.center().y - from.center().y).abs();
        let (start, end, path) = if horizontal {
            let start = if to.center().x >= from.center().x {
                egui::pos2(from.right(), from.center().y)
            } else {
                egui::pos2(from.left(), from.center().y)
            };
            let end = if to.center().x >= from.center().x {
                egui::pos2(to.left(), to.center().y)
            } else {
                egui::pos2(to.right(), to.center().y)
            };
            let elbow_x = (start.x + end.x) * 0.5;
            (
                start,
                end,
                vec![
                    start,
                    egui::pos2(elbow_x, start.y),
                    egui::pos2(elbow_x, end.y),
                    end,
                ],
            )
        } else {
            let start = if to.center().y >= from.center().y {
                egui::pos2(from.center().x, from.bottom())
            } else {
                egui::pos2(from.center().x, from.top())
            };
            let end = if to.center().y >= from.center().y {
                egui::pos2(to.center().x, to.top())
            } else {
                egui::pos2(to.center().x, to.bottom())
            };
            let elbow_y = (start.y + end.y) * 0.5;
            (
                start,
                end,
                vec![
                    start,
                    egui::pos2(start.x, elbow_y),
                    egui::pos2(end.x, elbow_y),
                    end,
                ],
            )
        };

        let (stroke, label_color) = match edge.kind {
            DebugEdgeKind::Flow => (
                egui::Stroke::new(1.5_f32, theme::TOP_INFO),
                egui::Color32::WHITE,
            ),
            DebugEdgeKind::Relationship => (
                egui::Stroke::new(1.0_f32, egui::Color32::GRAY),
                egui::Color32::LIGHT_GRAY,
            ),
        };
        match edge.kind {
            DebugEdgeKind::Flow => {
                background_shapes.push(egui::Shape::line(path.clone(), stroke));
            }
            DebugEdgeKind::Relationship => {
                background_shapes.extend(egui::Shape::dashed_line(
                    &path,
                    stroke,
                    (0.7 * vmin).max(4.0),
                    (0.5 * vmin).max(3.0),
                ));
            }
        }

        if !edge.label.is_empty() {
            painter.text(
                egui::pos2((start.x + end.x) * 0.5, (start.y + end.y) * 0.5),
                egui::Align2::CENTER_CENTER,
                &edge.label,
                egui::FontId::monospace((1.2 * vmin).max(9.0)),
                label_color,
            );
        }
    }
    painter.set(edge_shape_index, egui::Shape::Vec(background_shapes));
}

fn paint_top_info(ui: &mut egui::Ui, state: &AppState) -> egui::Rect {
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    let vmin = theme::vmin(ui);
    let area = egui::Area::new(egui::Id::new("player-top-info"))
        .fixed_pos(egui::pos2(screen.left() + vmin, screen.top() + 0.5 * vmin))
        .show(&ctx, |ui| {
            ui.set_max_width((screen.width() - 14.0 * vmin).max(10.0 * vmin));
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlayerIcon {
    Play,
    Pause,
    Refresh,
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
        PlayerIcon::Refresh => egui::Image::new(egui::include_image!(
            "../../assets/player-icons/refresh.svg"
        )),
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
                let (icon, command) = primary_playback_control(
                    state.playback_state(),
                    state.playback_intends_playing(),
                );
                if icon_button(
                    ui,
                    icon,
                    size,
                    radius,
                    command.is_some() && !state.ui.controls_locked,
                )
                .clicked()
                    && let Some(command) = command
                {
                    commands.push(command);
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

fn primary_playback_control(
    state: &PlaybackState,
    intends_playing: bool,
) -> (PlayerIcon, Option<AppCommand>) {
    match state {
        PlaybackState::Playing | PlaybackState::Buffering => {
            (PlayerIcon::Pause, Some(AppCommand::TogglePlayback))
        }
        PlaybackState::Paused => (PlayerIcon::Play, Some(AppCommand::TogglePlayback)),
        PlaybackState::Error(_) => (PlayerIcon::Refresh, Some(AppCommand::RefreshPlayback)),
        PlaybackState::Seeking if intends_playing => {
            (PlayerIcon::Pause, Some(AppCommand::TogglePlayback))
        }
        PlaybackState::Seeking => (PlayerIcon::Play, Some(AppCommand::TogglePlayback)),
        PlaybackState::Ended => (PlayerIcon::Play, None),
        PlaybackState::Loading => (PlayerIcon::Play, None),
    }
}

fn playback_status_label(state: &PlaybackState) -> Option<&'static str> {
    match state {
        PlaybackState::Playing | PlaybackState::Paused => None,
        PlaybackState::Loading => Some("Loading…"),
        PlaybackState::Buffering => Some("Buffering…"),
        PlaybackState::Seeking => Some("Seeking…"),
        PlaybackState::Ended => Some("Ended"),
        PlaybackState::Error(_) => Some("Error"),
    }
}

fn paint_centre_status(ui: &egui::Ui, state: &AppState, controls_rect: egui::Rect) {
    let Some(label) = playback_status_label(state.playback_state()) else {
        return;
    };
    let ctx = ui.ctx().clone();
    let vmin = theme::vmin(ui);
    egui::Area::new(egui::Id::new("centre-playback-status"))
        .fixed_pos(egui::pos2(
            controls_rect.center().x,
            controls_rect.bottom() + 1.5 * vmin,
        ))
        .pivot(egui::Align2::CENTER_TOP)
        .order(egui::Order::Foreground)
        .show(&ctx, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(label)
                        .size(4.0 * vmin)
                        .color(theme::TOP_INFO)
                        .strong(),
                )
                .wrap_mode(egui::TextWrapMode::Extend),
            );
        });
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

fn bottom_control_row_origins(
    centre_x: f32,
    middle_width: f32,
    gap: f32,
    left_widths: &[f32],
) -> (f32, f32, f32) {
    let middle_x = centre_x - middle_width * 0.5;
    let left_width =
        left_widths.iter().sum::<f32>() + gap * left_widths.len().saturating_sub(1) as f32;
    let left_x = middle_x - gap - left_width;
    let right_x = middle_x + middle_width + gap;
    (left_x, middle_x, right_x)
}

fn bottom_control_top(bottom: f32, height: f32) -> f32 {
    bottom - height
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
    let time_text = format_colon_time(state.position());
    let time_size = text_control_size(ui, &time_text, font_size, padding);
    let left_widths: Vec<_> = left_sizes.iter().map(|size| size.x).collect();
    let (mut x, middle_x, right_x) =
        bottom_control_row_origins(screen.center().x, middle_width, gap, &left_widths);
    let middle_centre_x = middle_x + middle_width * 0.5;

    for ((label, offset), size) in left.into_iter().zip(left_sizes) {
        let y = bottom_control_top(bottom, size.y);
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

    let time_fill = matches!(state.playback_state(), PlaybackState::Seeking).then_some(theme::PINK);
    let mut time_clicked = false;
    egui::Area::new(egui::Id::new("bottom-time"))
        .fixed_pos(egui::pos2(
            middle_centre_x - time_size.x * 0.5,
            bottom_control_top(bottom, time_size.y),
        ))
        .order(egui::Order::Foreground)
        .show(&ctx, |ui| {
            time_clicked = text_control_button(
                ui, &time_text, font_size, padding, radius, enabled, time_fill,
            )
            .clicked();
        });
    if time_clicked {
        state.open_seek_dialog();
    }

    let rates = state.available_rates().to_vec();
    let mut selected_rate = state.playback_rate();
    let speed_width = 14.0 * vmin;
    let speed_height = 5.0 * vmin;
    let speed_y = bottom_control_top(bottom, time_size.y) - gap - speed_height;
    egui::Area::new(egui::Id::new("bottom-speed"))
        .fixed_pos(egui::pos2(middle_centre_x - speed_width * 0.5, speed_y))
        .order(egui::Order::Foreground)
        .show(&ctx, |ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(speed_width, speed_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.style_mut().override_font_id = Some(egui::FontId::proportional(3.5 * vmin));
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
        });
    x = right_x;

    for ((label, offset), size) in right.into_iter().zip(right_sizes) {
        let y = bottom_control_top(bottom, size.y);
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
    let x = screen.left() + state.ui.lock_drag_fraction * travel;

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

#[cfg(test)]
mod tests {
    use super::{
        PlayerIcon, bottom_control_row_origins, bottom_control_top, playback_status_label,
        primary_playback_control,
    };
    use crate::model::{AppCommand, PlaybackState};

    #[test]
    fn error_state_uses_shared_refresh_control() {
        let (icon, command) =
            primary_playback_control(&PlaybackState::Error("network failed".into()), false);
        assert_eq!(icon, PlayerIcon::Refresh);
        assert_eq!(command, Some(AppCommand::RefreshPlayback));
    }

    #[test]
    fn ended_state_keeps_disabled_play_control_and_status_text() {
        let (icon, command) = primary_playback_control(&PlaybackState::Ended, false);
        assert_eq!(icon, PlayerIcon::Play);
        assert_eq!(command, None);
        assert_eq!(playback_status_label(&PlaybackState::Ended), Some("Ended"));
    }

    #[test]
    fn seeking_control_reflects_and_toggles_resume_intent() {
        let (icon, command) = primary_playback_control(&PlaybackState::Seeking, true);
        assert_eq!(icon, PlayerIcon::Pause);
        assert_eq!(command, Some(AppCommand::TogglePlayback));

        let (icon, command) = primary_playback_control(&PlaybackState::Seeking, false);
        assert_eq!(icon, PlayerIcon::Play);
        assert_eq!(command, Some(AppCommand::TogglePlayback));
    }

    #[test]
    fn centre_status_hides_obvious_states_and_labels_transitional_states() {
        assert_eq!(playback_status_label(&PlaybackState::Playing), None);
        assert_eq!(playback_status_label(&PlaybackState::Paused), None);
        assert_eq!(
            playback_status_label(&PlaybackState::Buffering),
            Some("Buffering…")
        );
        assert_eq!(
            playback_status_label(&PlaybackState::Seeking),
            Some("Seeking…")
        );
        assert_eq!(playback_status_label(&PlaybackState::Ended), Some("Ended"));
        assert_eq!(
            playback_status_label(&PlaybackState::Error("failed".into())),
            Some("Error")
        );
    }

    #[test]
    fn bottom_middle_controls_stay_centred_with_asymmetric_seek_widths() {
        let centre_x = 591.0;
        let middle_width = 144.0;
        let gap = 7.2;
        let left_widths = [84.6925, 64.41125, 48.755];

        let (left_x, middle_x, right_x) =
            bottom_control_row_origins(centre_x, middle_width, gap, &left_widths);

        assert!((middle_x + middle_width * 0.5 - centre_x).abs() < f32::EPSILON);

        let left_end = left_x
            + left_widths.iter().sum::<f32>()
            + gap * left_widths.len().saturating_sub(1) as f32;
        assert!((middle_x - left_end - gap).abs() < 0.001);
        assert!((right_x - (middle_x + middle_width) - gap).abs() < 0.001);
    }

    #[test]
    fn bottom_middle_uses_safe_content_centre_not_viewport_centre() {
        let viewport_centre_x = 600.0;
        let content_centre_x = 640.0;
        let middle_width = 144.0;
        let gap = 7.2;
        let left_widths = [84.6925, 64.41125, 48.755];

        let (_, middle_x, _) =
            bottom_control_row_origins(content_centre_x, middle_width, gap, &left_widths);

        assert_eq!(middle_x + middle_width * 0.5, content_centre_x);
        assert_ne!(middle_x + middle_width * 0.5, viewport_centre_x);
    }

    #[test]
    fn seek_and_time_controls_share_bottom_edge() {
        let bottom = 340.0;
        for height in [28.0, 32.5, 41.0] {
            let top = bottom_control_top(bottom, height);
            assert!((top + height - bottom).abs() < f32::EPSILON);
        }
    }
}
