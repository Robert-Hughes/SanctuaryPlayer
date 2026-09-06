mod dialogs;
mod menu;
mod player;
pub(crate) mod theme;
mod welcome;

use crate::app::AppState;
use crate::model::AppCommand;

pub fn render(ui: &mut egui::Ui, state: &mut AppState) -> Vec<AppCommand> {
    let mut commands = if state.has_video() {
        player::render(ui, state)
    } else {
        welcome::render(ui, state)
    };
    dialogs::render(ui, state, &mut commands);
    commands
}
