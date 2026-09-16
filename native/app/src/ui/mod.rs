mod dialogs;
mod menu;
mod player;
pub mod theme;
mod welcome;

use crate::app::AppState;
use crate::model::AppCommand;

const SPINNER_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

fn animated_spinner(ui: &mut egui::Ui) -> egui::Response {
    let size = ui.style().spacing.interact_size.y;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::new(egui::WidgetType::ProgressIndicator));

    if ui.is_rect_visible(rect) {
        ui.ctx().request_repaint_after(SPINNER_FRAME_INTERVAL);
        let radius = (rect.height().min(rect.width()) / 2.0) - 2.0;
        let point_count = (radius.round() as usize).clamp(8, 128);
        let time = ui.input(|input| input.time);
        let start_angle = time * std::f64::consts::TAU;
        let end_angle = start_angle + 240_f64.to_radians() * time.sin();
        let points = (0..point_count)
            .map(|index| {
                let t = index as f64 / point_count as f64;
                let angle = start_angle + (end_angle - start_angle) * t;
                let (sin, cos) = angle.sin_cos();
                rect.center() + radius * egui::vec2(cos as f32, sin as f32)
            })
            .collect::<Vec<_>>();
        ui.painter().add(egui::Shape::line(
            points,
            egui::Stroke::new(3.0_f32, ui.visuals().strong_text_color()),
        ));
    }

    response
}

pub fn render(ui: &mut egui::Ui, state: &mut AppState) -> Vec<AppCommand> {
    #[cfg(target_os = "android")]
    state.begin_android_ui_frame();
    let mut commands = if state.has_video() {
        player::render(ui, state)
    } else {
        welcome::render(ui, state)
    };
    dialogs::render(ui, state, &mut commands);
    commands
}
