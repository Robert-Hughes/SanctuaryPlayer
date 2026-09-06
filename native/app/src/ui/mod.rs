mod dialogs;
mod menu;
mod player;
mod welcome;

use crate::app::AppState;
use crate::model::AppCommand;

pub fn render(ui: &mut egui::Ui, state: &mut AppState, adapter_summary: &str) -> Vec<AppCommand> {
    let pointer_activity = ui
        .ctx()
        .input(|input| input.pointer.any_pressed() || input.pointer.delta() != egui::Vec2::ZERO);
    if pointer_activity && state.has_video() {
        state.note_interaction();
    }

    let mut commands = if state.has_video() {
        player::render(ui, state)
    } else {
        welcome::render(ui, state, adapter_summary)
    };
    dialogs::render(ui, state, &mut commands);
    commands
}
