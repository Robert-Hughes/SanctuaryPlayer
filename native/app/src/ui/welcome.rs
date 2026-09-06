use crate::app::AppState;
use crate::model::AppCommand;

use super::{menu, theme};

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

    ui.vertical_centered(|ui| {
        // The web version centres the entire flex column, including margins.
        let content_height = title_size * 1.2
            + title_size // title's 0.5em top + bottom margin
            + logo_height
            + subtitle_size * 2.4
            + subtitle_size; // subtitle 0.5em top + bottom margin
        ui.add_space(((rect.height() - content_height) * 0.5).max(0.0));

        ui.label(
            egui::RichText::new("Sanctuary Player")
                .size(title_size)
                .color(theme::PURPLE),
        );
        ui.add_space(0.5 * title_size);

        ui.add(
            egui::Image::new(egui::include_image!("../../assets/sanctuary-logo.svg"))
                .fit_to_exact_size(egui::vec2(logo_width, logo_height)),
        );

        ui.add_space(0.5 * subtitle_size);
        ui.label(
            egui::RichText::new("Please select a video from the Menu\n(top-right corner)")
                .size(subtitle_size)
                .color(theme::PINK),
        );
    });

    menu::render_button(ui, state);
    menu::render(ui, state, &mut commands);
    commands
}
