use crate::app::AppState;
use crate::model::AppCommand;

use super::menu;

pub fn render(ui: &mut egui::Ui, state: &mut AppState, adapter_summary: &str) -> Vec<AppCommand> {
    let mut commands = Vec::new();
    let rect = ui.max_rect();
    ui.painter()
        .rect_filled(rect, 0.0, egui::Color32::from_rgb(10, 9, 18));

    ui.vertical_centered(|ui| {
        ui.add_space((rect.height() * 0.15).max(40.0));
        ui.heading(egui::RichText::new("Sanctuary Player").size(42.0).strong());
        ui.add_space(30.0);
        paint_shield(ui);
        ui.add_space(24.0);
        ui.label(
            egui::RichText::new("Please select a video from the Menu\n(top-right corner)")
                .size(22.0),
        );
        ui.add_space(20.0);
        ui.small(format!("GPU: {adapter_summary}"));
    });

    menu::render_button(ui, state);
    menu::render(ui, state, &mut commands);
    commands
}

fn paint_shield(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(170.0, 190.0), egui::Sense::hover());
    let c = rect.center();
    let points = vec![
        egui::pos2(c.x, rect.top()),
        egui::pos2(rect.right() - 8.0, rect.top() + 42.0),
        egui::pos2(rect.right() - 16.0, rect.bottom() - 58.0),
        egui::pos2(c.x, rect.bottom()),
        egui::pos2(rect.left() + 16.0, rect.bottom() - 58.0),
        egui::pos2(rect.left() + 8.0, rect.top() + 42.0),
    ];
    ui.painter().add(egui::Shape::convex_polygon(
        points,
        egui::Color32::from_rgb(108, 99, 255),
        egui::Stroke::new(3.0_f32, egui::Color32::from_rgb(181, 23, 158)),
    ));
    let play = vec![
        egui::pos2(c.x - 24.0, c.y - 38.0),
        egui::pos2(c.x - 24.0, c.y + 38.0),
        egui::pos2(c.x + 42.0, c.y),
    ];
    ui.painter().add(egui::Shape::convex_polygon(
        play,
        egui::Color32::WHITE,
        egui::Stroke::NONE,
    ));
}
