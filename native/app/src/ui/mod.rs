mod dialogs;
mod menu;
mod player;
pub mod theme;
mod welcome;

use crate::app::AppState;
use crate::model::AppCommand;

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
