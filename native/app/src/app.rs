use std::time::Duration;

use crate::model::{AppCommand, PlaybackState, Quality};
use crate::playback::{DummyPlayback, PlaybackBackend};
use crate::services::{DummyMetadataService, MetadataService, VideoMetadata};
use crate::spoilers::sanitise_title;
use crate::video::VideoSource;

pub struct AppState {
    playback: DummyPlayback,
    metadata_service: DummyMetadataService,
    metadata: Option<VideoMetadata>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEffect {
    ToggleFullscreen,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            playback: DummyPlayback::new(),
            metadata_service: DummyMetadataService,
            metadata: None,
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, elapsed: Duration) {
        self.playback.update(elapsed);
    }

    pub fn apply(&mut self, command: AppCommand) -> Option<AppEffect> {
        match command {
            AppCommand::OpenVideo(source) => {
                if self.playback.open(&source).is_ok() {
                    self.metadata = Some(self.metadata_service.metadata_for(&source));
                }
            }
            AppCommand::TogglePlayback => match self.playback.state() {
                PlaybackState::Playing => self.playback.pause(),
                PlaybackState::Paused => self.playback.play(),
                _ => {}
            },
            AppCommand::Play => self.playback.play(),
            AppCommand::Pause => self.playback.pause(),
            AppCommand::SeekAbsolute(position) => self.playback.seek(position),
            AppCommand::SeekRelative(offset) => {
                let current = self.playback.position();
                let target = if offset >= 0 {
                    current.saturating_add(Duration::from_secs(offset as u64))
                } else {
                    current.saturating_sub(Duration::from_secs(offset.unsigned_abs()))
                };
                self.playback.seek(target);
            }
            AppCommand::SetPlaybackRate(rate) => self.playback.set_playback_rate(rate),
            AppCommand::SetQuality(quality) => self.playback.set_quality(&quality),
            AppCommand::ToggleFullscreen => return Some(AppEffect::ToggleFullscreen),
            AppCommand::ToggleControlsLock => {}
        }
        None
    }

    pub fn has_video(&self) -> bool {
        self.playback.source().is_some()
    }

    pub fn source(&self) -> Option<&VideoSource> {
        self.playback.source()
    }

    pub fn playback_state(&self) -> &PlaybackState {
        self.playback.state()
    }

    pub fn position(&self) -> Duration {
        self.playback.position()
    }

    pub fn duration(&self) -> Option<Duration> {
        self.playback.duration()
    }

    pub fn playback_rate(&self) -> f32 {
        self.playback.playback_rate()
    }

    pub fn available_rates(&self) -> &[f32] {
        self.playback.available_rates()
    }

    pub fn available_qualities(&self) -> &[Quality] {
        self.playback.available_qualities()
    }

    pub fn quality(&self) -> Option<&Quality> {
        self.playback.quality()
    }

    pub fn safe_title(&self) -> Option<String> {
        self.metadata
            .as_ref()
            .map(|metadata| sanitise_title(&metadata.title))
    }

    pub fn release_age(&self) -> Option<Duration> {
        self.metadata.as_ref().map(|metadata| metadata.release_age)
    }

    pub fn needs_animation(&self) -> bool {
        matches!(
            self.playback.state(),
            PlaybackState::Playing | PlaybackState::Seeking
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded_state() -> AppState {
        let mut state = AppState::new();
        state.apply(AppCommand::OpenVideo(
            VideoSource::parse("2386400830").unwrap(),
        ));
        state
    }

    #[test]
    fn opening_video_populates_metadata_and_starts_paused() {
        let state = loaded_state();
        assert!(state.has_video());
        assert_eq!(state.playback_state(), &PlaybackState::Paused);
        assert!(state.safe_title().unwrap().contains("Game _"));
    }

    #[test]
    fn commands_drive_dummy_backend() {
        let mut state = loaded_state();
        state.apply(AppCommand::TogglePlayback);
        state.update(Duration::from_secs(2));
        assert_eq!(state.position(), Duration::from_secs(2));
        state.apply(AppCommand::SeekRelative(60));
        assert_eq!(state.position(), Duration::from_secs(62));
        assert_eq!(state.playback_state(), &PlaybackState::Seeking);
    }

    #[test]
    fn fullscreen_command_is_returned_as_platform_effect() {
        let mut state = AppState::new();
        assert_eq!(
            state.apply(AppCommand::ToggleFullscreen),
            Some(AppEffect::ToggleFullscreen)
        );
    }
}
