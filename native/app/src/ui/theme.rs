pub const PURPLE: egui::Color32 = egui::Color32::from_rgb(108, 99, 255); // #6C63FF
pub const PINK: egui::Color32 = egui::Color32::from_rgb(181, 23, 158); // #B5179E
pub const LIGHT_PURPLE: egui::Color32 = egui::Color32::from_rgb(207, 204, 255); // hsl(243,100%,90%)
pub const TOP_INFO: egui::Color32 = egui::Color32::from_rgb(182, 178, 255); // hsl(243,100%,85%)
pub const ICON_PURPLE: egui::Color32 = egui::Color32::from_rgb(110, 102, 255); // hsl(243,100%,70%)
pub const WHITE: egui::Color32 = egui::Color32::WHITE;

/// CSS `1vmin`: one percent of the smaller viewport dimension.
pub fn vmin(ui: &egui::Ui) -> f32 {
    let rect = ui.ctx().content_rect();
    rect.width().min(rect.height()) / 100.0
}

pub fn rounded_button<'a>(text: impl Into<egui::WidgetText>, vmin: f32) -> egui::Button<'a> {
    egui::Button::new(text)
        .fill(WHITE)
        .stroke(egui::Stroke::new((0.1 * vmin).max(1.0), PURPLE))
        .corner_radius((1.0 * vmin).round() as u8)
}
