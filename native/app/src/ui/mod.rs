mod player;
mod welcome;

use crate::app::AppState;
use crate::model::AppCommand;

pub fn render(ui: &mut egui::Ui, state: &AppState, adapter_summary: &str) -> Vec<AppCommand> {
    if state.has_video() {
        player::render(ui, state)
    } else {
        welcome::render(ui, adapter_summary)
    }
}
