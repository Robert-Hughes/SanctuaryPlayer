use std::time::Duration;

use crate::video::VideoSource;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackState {
    Loading,
    Paused,
    Playing,
    Buffering,
    Seeking,
    Ended,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quality {
    pub id: String,
    pub label: String,
}

impl Quality {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppCommand {
    OpenVideo(VideoSource),
    TogglePlayback,
    Play,
    Pause,
    SeekAbsolute(Duration),
    SeekRelative(i64),
    SetPlaybackRate(f32),
    SetQuality(String),
    SetFavouriteQualities(String),
    SignIn { user_id: String, device_id: String },
    SignOut,
    ToggleFullscreen,
    ToggleControlsLock,
    ToggleControlsVisibility,
}
