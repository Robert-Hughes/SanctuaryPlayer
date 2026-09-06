mod dialogs;
mod menu;
mod player;
pub(crate) mod theme;
mod welcome;

use crate::app::AppState;
use crate::model::AppCommand;

pub fn render(ui: &mut egui::Ui, state: &mut AppState) -> Vec<AppCommand> {
    let pointer_activity = ui
        .ctx()
        .input(|input| input.pointer.any_pressed() || input.pointer.delta() != egui::Vec2::ZERO);
    if pointer_activity && state.has_video() {
        state.note_interaction();
    }

    let mut commands = if state.has_video() {
        player::render(ui, state)
    } else {
        welcome::render(ui, state)
    };
    dialogs::render(ui, state, &mut commands);
    commands
}
