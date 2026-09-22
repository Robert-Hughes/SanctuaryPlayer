use crate::app::AppState;
use crate::model::AppCommand;

use super::{animated_spinner, menu, theme};

pub fn render(ui: &mut egui::Ui, state: &mut AppState) -> Vec<AppCommand> {
    let mut commands = Vec::new();
    let rect = ui.max_rect();
    let vmin = theme::vmin(ui);

    // Match the original web welcome page: white page, purple title, exact SVG
    // logo at 50vmin wide, and pink subtitle at 5vmin.
    ui.painter().rect_filled(rect, 0.0, theme::WHITE);

    let title_size = 10.0 * vmin;
    let subtitle_size = 5.0 * vmin;
    let logo_width = 50.0 * vmin;
    let logo_height = logo_width * 45.0 / 40.0;

    // CSS flex centring includes each element's actual rendered height plus
    // the title/subtitle 0.5em margins. Use egui's real glyph metrics instead
    // of estimating line-height so the block is vertically centred exactly.
    let title_galley = ui.painter().layout_no_wrap(
        "Sanctuary Player".to_owned(),
        egui::FontId::proportional(title_size),
        theme::PURPLE,
    );
    let subtitle_font = egui::FontId::proportional(subtitle_size);
    let subtitle_line_one = ui.painter().layout_no_wrap(
        "Please select a video from the Menu".to_owned(),
        subtitle_font.clone(),
        theme::PINK,
    );
    let subtitle_line_two =
        ui.painter()
            .layout_no_wrap("(top-right corner)".to_owned(), subtitle_font, theme::PINK);
    let subtitle_height = subtitle_line_one.size().y + subtitle_line_two.size().y;
    let content_height = 0.5 * title_size
        + title_galley.size().y
        + 0.5 * title_size
        + logo_height
        + 0.5 * subtitle_size
        + subtitle_height
        + 0.5 * subtitle_size;
    let mut y = rect.center().y - content_height * 0.5;

    y += 0.5 * title_size;
    let title_pos = egui::pos2(rect.center().x - title_galley.size().x * 0.5, y);
    ui.painter()
        .galley(title_pos, title_galley.clone(), theme::PURPLE);
    y += title_galley.size().y + 0.5 * title_size;

    let logo_rect = egui::Rect::from_min_size(
        egui::pos2(rect.center().x - logo_width * 0.5, y),
        egui::vec2(logo_width, logo_height),
    );
    egui::Image::new(egui::include_image!("../../assets/sanctuary-logo.svg"))
        .paint_at(ui, logo_rect);
    y += logo_height + 0.5 * subtitle_size;

    let subtitle_one_pos = egui::pos2(rect.center().x - subtitle_line_one.size().x * 0.5, y);
    ui.painter()
        .galley(subtitle_one_pos, subtitle_line_one.clone(), theme::PINK);
    y += subtitle_line_one.size().y;
    let subtitle_two_pos = egui::pos2(rect.center().x - subtitle_line_two.size().x * 0.5, y);
    ui.painter()
        .galley(subtitle_two_pos, subtitle_line_two.clone(), theme::PINK);

    if state.opening_video() {
        let status_font = egui::FontId::proportional(0.7 * subtitle_size);
        let status_galley =
            ui.painter()
                .layout_no_wrap("Loading video…".to_owned(), status_font, theme::PINK);
        let spinner_size = ui.style().spacing.interact_size.y;
        let gap = 0.35 * subtitle_size;
        let total_width = spinner_size + gap + status_galley.size().x;
        let status_y = y + subtitle_line_two.size().y + 0.5 * subtitle_size;
        let spinner_rect = egui::Rect::from_min_size(
            egui::pos2(rect.center().x - total_width * 0.5, status_y),
            egui::vec2(spinner_size, spinner_size),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(spinner_rect), |ui| {
            animated_spinner(ui);
        });
        let text_pos = egui::pos2(
            spinner_rect.right() + gap,
            status_y + (spinner_size - status_galley.size().y) * 0.5,
        );
        ui.painter().galley(text_pos, status_galley, theme::PINK);
    }

    menu::render_button(ui, state);
    menu::render(ui, state, &mut commands);
    commands
}
