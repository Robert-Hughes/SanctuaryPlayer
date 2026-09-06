use winit::keyboard::KeyCode;

use crate::app::AppState;
use crate::model::AppCommand;

pub(crate) fn command_for_key(code: KeyCode, state: &AppState) -> Option<AppCommand> {
    match code {
        KeyCode::Space => Some(AppCommand::TogglePlayback),
        KeyCode::ArrowLeft => Some(AppCommand::SeekRelative(-5)),
        KeyCode::ArrowRight => Some(AppCommand::SeekRelative(5)),
        KeyCode::ArrowUp => state
            .adjacent_playback_rate(1)
            .map(AppCommand::SetPlaybackRate),
        KeyCode::ArrowDown => state
            .adjacent_playback_rate(-1)
            .map(AppCommand::SetPlaybackRate),
        KeyCode::KeyF => Some(AppCommand::ToggleFullscreen),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AppCommand;
    use crate::video::VideoSource;

    fn state() -> AppState {
        let mut state = AppState::new();
        state.apply(AppCommand::OpenVideo(
            VideoSource::parse("2386400830").unwrap(),
        ));
        state
    }

    #[test]
    fn keyboard_shortcuts_map_to_shared_commands() {
        let state = state();
        assert_eq!(
            command_for_key(KeyCode::Space, &state),
            Some(AppCommand::TogglePlayback)
        );
        assert_eq!(
            command_for_key(KeyCode::ArrowLeft, &state),
            Some(AppCommand::SeekRelative(-5))
        );
        assert_eq!(
            command_for_key(KeyCode::ArrowRight, &state),
            Some(AppCommand::SeekRelative(5))
        );
        assert_eq!(
            command_for_key(KeyCode::ArrowUp, &state),
            Some(AppCommand::SetPlaybackRate(1.5))
        );
        assert_eq!(
            command_for_key(KeyCode::ArrowDown, &state),
            Some(AppCommand::SetPlaybackRate(0.5))
        );
        assert_eq!(
            command_for_key(KeyCode::KeyF, &state),
            Some(AppCommand::ToggleFullscreen)
        );
        assert_eq!(command_for_key(KeyCode::KeyA, &state), None);
    }
}
