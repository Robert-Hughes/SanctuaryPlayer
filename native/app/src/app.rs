use std::time::Duration;

use crate::model::{AppCommand, PlaybackState, Quality};
use crate::playback::{DummyPlayback, PlaybackBackend};
use crate::services::{
    DummyMetadataService, DummyPositionService, MetadataService, PositionService, SavedPosition,
    VideoMetadata,
};
use crate::spoilers::sanitise_title;
use crate::video::VideoSource;

const CONTROLS_HIDE_AFTER: Duration = Duration::from_secs(2);
const LOCK_SLIDE_BACK_DURATION: Duration = Duration::from_millis(500);

pub struct AppState {
    playback: DummyPlayback,
    metadata_service: DummyMetadataService,
    positions_service: DummyPositionService,
    metadata: Option<VideoMetadata>,
    account: AccountState,
    preferences: Preferences,
    pub(crate) ui: UiState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEffect {
    ToggleFullscreen,
}

#[derive(Debug, Default)]
struct AccountState {
    user_id: Option<String>,
    device_id: Option<String>,
}

#[derive(Debug, Default)]
struct Preferences {
    favourite_qualities: String,
    manually_selected_quality: bool,
}

#[derive(Debug)]
pub(crate) struct UiState {
    pub(crate) menu_open: bool,
    pub(crate) controls_visible: bool,
    pub(crate) controls_locked: bool,
    pub(crate) controls_idle: Duration,
    pub(crate) lock_drag_fraction: f32,
    pub(crate) lock_dragging: bool,
    pub(crate) lock_drag_origin_fraction: f32,
    pub(crate) lock_return_from: Option<f32>,
    pub(crate) lock_return_elapsed: Duration,
    pub(crate) dialog: Option<DialogState>,
    pub(crate) focus_first_dialog_input: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            menu_open: false,
            controls_visible: true,
            controls_locked: false,
            controls_idle: Duration::ZERO,
            lock_drag_fraction: 0.0,
            lock_dragging: false,
            lock_drag_origin_fraction: 0.0,
            lock_return_from: None,
            lock_return_elapsed: Duration::ZERO,
            dialog: None,
            focus_first_dialog_input: false,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum DialogState {
    ChangeVideo {
        input: String,
        error: Option<String>,
    },
    SeekTo {
        input: String,
        error: Option<String>,
    },
    FavouriteQualities {
        input: String,
    },
    SignIn {
        user_id: String,
        device_id: String,
    },
    ConfirmSignOut,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            playback: DummyPlayback::new(),
            metadata_service: DummyMetadataService,
            positions_service: DummyPositionService::new(),
            metadata: None,
            account: AccountState::default(),
            preferences: Preferences::default(),
            ui: UiState::default(),
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, elapsed: Duration) {
        self.playback.update(elapsed);
        self.update_lock_slider_return(elapsed);

        if !self.has_video() {
            self.ui.controls_visible = true;
            self.ui.controls_idle = Duration::ZERO;
            return;
        }
        if !matches!(self.playback.state(), PlaybackState::Playing) {
            self.ui.controls_idle = Duration::ZERO;
            return;
        }
        if self.ui.menu_open || self.ui.dialog.is_some() || self.ui.lock_dragging {
            self.ui.controls_visible = true;
            self.ui.controls_idle = Duration::ZERO;
            return;
        }
        self.ui.controls_idle = self.ui.controls_idle.saturating_add(elapsed);
        if self.ui.controls_idle >= CONTROLS_HIDE_AFTER {
            self.ui.controls_visible = false;
        }
    }

    fn update_lock_slider_return(&mut self, elapsed: Duration) {
        let Some(from) = self.ui.lock_return_from else {
            return;
        };

        self.ui.lock_return_elapsed = self.ui.lock_return_elapsed.saturating_add(elapsed);
        let progress = (self.ui.lock_return_elapsed.as_secs_f32()
            / LOCK_SLIDE_BACK_DURATION.as_secs_f32())
        .clamp(0.0, 1.0);
        self.ui.lock_drag_fraction = from * (1.0 - progress);

        if self.ui.lock_return_elapsed >= LOCK_SLIDE_BACK_DURATION {
            self.ui.lock_drag_fraction = 0.0;
            self.ui.lock_return_from = None;
            self.ui.lock_return_elapsed = Duration::ZERO;
        }
    }

    pub(crate) fn begin_lock_drag(&mut self) {
        self.ui.lock_dragging = true;
        self.ui.lock_drag_origin_fraction = self.ui.lock_drag_fraction;
        self.ui.lock_return_from = None;
        self.ui.lock_return_elapsed = Duration::ZERO;
        self.note_interaction();
    }

    pub(crate) fn set_lock_drag_delta(&mut self, delta_fraction: f32) {
        self.ui.lock_drag_fraction =
            (self.ui.lock_drag_origin_fraction + delta_fraction).clamp(0.0, 1.0);
        self.note_interaction();
    }

    pub(crate) fn end_lock_drag(&mut self) -> bool {
        let toggles_lock = self.ui.lock_drag_fraction >= 1.0;
        self.ui.lock_dragging = false;
        self.ui.lock_return_from =
            (self.ui.lock_drag_fraction > 0.0).then_some(self.ui.lock_drag_fraction);
        self.ui.lock_return_elapsed = Duration::ZERO;
        self.note_interaction();
        toggles_lock
    }

    pub fn apply(&mut self, command: AppCommand) -> Option<AppEffect> {
        if self.ui.controls_locked
            && !matches!(
                command,
                AppCommand::ToggleControlsLock | AppCommand::ToggleControlsVisibility
            )
        {
            return None;
        }

        match command {
            AppCommand::OpenVideo(source) => {
                if self.playback.open(&source).is_ok() {
                    self.metadata = Some(self.metadata_service.metadata_for(&source));
                    self.preferences.manually_selected_quality = false;
                    self.apply_favourite_quality();
                    self.ui.menu_open = false;
                    self.ui.dialog = None;
                    self.note_interaction();
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
            AppCommand::SetQuality(quality) => {
                self.preferences.manually_selected_quality = true;
                self.playback.set_quality(&quality);
            }
            AppCommand::SetFavouriteQualities(qualities) => {
                self.preferences.favourite_qualities = qualities;
                if !self.preferences.manually_selected_quality {
                    self.apply_favourite_quality();
                }
            }
            AppCommand::SignIn { user_id, device_id } => {
                self.account.user_id = Some(user_id);
                self.account.device_id = Some(device_id);
            }
            AppCommand::SignOut => self.account = AccountState::default(),
            AppCommand::ToggleFullscreen => {
                if self.has_video() {
                    self.note_interaction();
                }
                return Some(AppEffect::ToggleFullscreen);
            }
            AppCommand::ToggleControlsLock => {
                self.ui.controls_locked = !self.ui.controls_locked;
                self.ui.menu_open = false;
                self.note_interaction();
            }
            AppCommand::ToggleControlsVisibility => {
                self.toggle_controls_visibility();
                return None;
            }
        }
        if self.has_video() {
            self.note_interaction();
        }
        None
    }

    fn apply_favourite_quality(&mut self) {
        for wanted in self
            .preferences
            .favourite_qualities
            .split([',', ';'])
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            if let Some(quality_id) = self
                .playback
                .available_qualities()
                .iter()
                .find(|quality| quality.id == wanted || quality.label == wanted)
                .map(|quality| quality.id.clone())
            {
                self.playback.set_quality(&quality_id);
                break;
            }
        }
    }

    pub(crate) fn note_interaction(&mut self) {
        self.ui.controls_visible = true;
        self.ui.controls_idle = Duration::ZERO;
    }

    pub(crate) fn toggle_controls_visibility(&mut self) {
        if self.ui.controls_visible {
            self.ui.controls_visible = false;
            self.ui.menu_open = false;
            self.ui.controls_idle = Duration::ZERO;
        } else {
            self.note_interaction();
        }
    }

    pub(crate) fn toggle_menu(&mut self) {
        if self.ui.controls_locked {
            return;
        }
        self.ui.menu_open = !self.ui.menu_open;
        if self.ui.menu_open && self.has_video() {
            self.playback.pause();
        }
        self.note_interaction();
    }

    pub(crate) fn close_menu(&mut self) {
        self.ui.menu_open = false;
    }

    pub(crate) fn open_change_video_dialog(&mut self) {
        self.ui.dialog = Some(DialogState::ChangeVideo {
            input: String::new(),
            error: None,
        });
        self.ui.focus_first_dialog_input = true;
        self.note_interaction();
    }

    pub(crate) fn open_seek_dialog(&mut self) {
        self.ui.dialog = Some(DialogState::SeekTo {
            input: crate::time_format::format_friendly_time(self.position()),
            error: None,
        });
        self.ui.focus_first_dialog_input = true;
        self.note_interaction();
    }

    pub(crate) fn open_favourites_dialog(&mut self) {
        self.ui.dialog = Some(DialogState::FavouriteQualities {
            input: self.preferences.favourite_qualities.clone(),
        });
        self.ui.focus_first_dialog_input = true;
        self.note_interaction();
    }

    pub(crate) fn open_sign_in_dialog(&mut self) {
        self.ui.dialog = Some(DialogState::SignIn {
            user_id: self.account.user_id.clone().unwrap_or_default(),
            device_id: self
                .account
                .device_id
                .clone()
                .unwrap_or_else(|| "Device 1".into()),
        });
        self.ui.focus_first_dialog_input = true;
        self.note_interaction();
    }

    pub(crate) fn open_sign_out_dialog(&mut self) {
        self.ui.dialog = Some(DialogState::ConfirmSignOut);
        self.ui.focus_first_dialog_input = false;
        self.note_interaction();
    }

    pub(crate) fn close_dialog(&mut self) {
        self.ui.dialog = None;
        self.ui.focus_first_dialog_input = false;
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

    pub fn signed_in(&self) -> bool {
        self.account
            .user_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
    }

    pub fn user_id(&self) -> Option<&str> {
        self.account.user_id.as_deref()
    }

    pub fn device_id(&self) -> Option<&str> {
        self.account.device_id.as_deref()
    }

    pub fn favourite_qualities(&self) -> &str {
        &self.preferences.favourite_qualities
    }

    pub fn saved_positions(&self) -> Vec<SavedPosition> {
        self.account
            .user_id
            .as_deref()
            .map(|user| self.positions_service.positions(user))
            .unwrap_or_default()
    }

    pub fn adjacent_playback_rate(&self, direction: i32) -> Option<f32> {
        let rates = self.available_rates();
        let current = rates
            .iter()
            .position(|rate| (*rate - self.playback_rate()).abs() < f32::EPSILON)?;
        let next = if direction > 0 {
            current.checked_add(1)?
        } else {
            current.checked_sub(1)?
        };
        rates.get(next).copied()
    }

    pub fn needs_animation(&self) -> bool {
        matches!(
            self.playback.state(),
            PlaybackState::Playing | PlaybackState::Seeking
        ) || self.ui.lock_return_from.is_some()
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
    fn controls_auto_hide_only_while_playing() {
        let mut state = loaded_state();
        state.update(Duration::from_secs(10));
        assert!(state.ui.controls_visible);
        state.apply(AppCommand::Play);
        state.update(CONTROLS_HIDE_AFTER);
        assert!(!state.ui.controls_visible);
        state.note_interaction();
        assert!(state.ui.controls_visible);
    }

    #[test]
    fn lock_drag_keeps_controls_visible_while_playing() {
        let mut state = loaded_state();
        state.apply(AppCommand::Play);
        state.begin_lock_drag();
        state.update(Duration::from_secs(10));
        assert!(state.ui.controls_visible);
        state.end_lock_drag();
    }

    #[test]
    fn lock_slider_returns_over_web_transition_duration() {
        let mut state = loaded_state();
        state.begin_lock_drag();
        state.set_lock_drag_delta(0.8);
        assert!(!state.end_lock_drag());
        assert!(state.needs_animation());

        state.update(Duration::from_millis(250));
        assert!((state.ui.lock_drag_fraction - 0.4).abs() < 1e-6);

        state.update(Duration::from_millis(250));
        assert_eq!(state.ui.lock_drag_fraction, 0.0);
        assert!(state.ui.lock_return_from.is_none());
        assert!(!state.needs_animation());
    }

    #[test]
    fn lock_slider_regrab_continues_from_return_position() {
        let mut state = loaded_state();
        state.begin_lock_drag();
        state.set_lock_drag_delta(0.8);
        assert!(!state.end_lock_drag());
        state.update(Duration::from_millis(250));
        assert!((state.ui.lock_drag_fraction - 0.4).abs() < 1e-6);

        state.begin_lock_drag();
        state.set_lock_drag_delta(0.1);
        assert!((state.ui.lock_drag_fraction - 0.5).abs() < 1e-6);
        assert!(state.ui.lock_return_from.is_none());
    }

    #[test]
    fn manual_control_visibility_toggle_survives_paused_updates() {
        let mut state = loaded_state();
        state.ui.menu_open = true;

        state.apply(AppCommand::ToggleControlsVisibility);
        assert!(!state.ui.controls_visible);
        assert!(!state.ui.menu_open);

        state.update(Duration::from_secs(10));
        assert!(!state.ui.controls_visible);

        state.apply(AppCommand::ToggleControlsVisibility);
        assert!(state.ui.controls_visible);

        state.apply(AppCommand::ToggleControlsLock);
        state.apply(AppCommand::ToggleControlsVisibility);
        assert!(!state.ui.controls_visible);
    }

    #[test]
    fn favourite_quality_is_applied_until_user_overrides_it() {
        let mut state = loaded_state();
        state.apply(AppCommand::SetFavouriteQualities("1080p60,720p60".into()));
        assert_eq!(state.quality().unwrap().id, "1080p60");
        state.apply(AppCommand::SetQuality("480p".into()));
        state.apply(AppCommand::SetFavouriteQualities("source".into()));
        assert_eq!(state.quality().unwrap().id, "480p");
    }

    #[test]
    fn sign_in_exposes_dummy_saved_positions() {
        let mut state = loaded_state();
        assert!(state.saved_positions().is_empty());
        state.apply(AppCommand::SignIn {
            user_id: "test-user".into(),
            device_id: "Desktop".into(),
        });
        assert!(!state.saved_positions().is_empty());
    }

    #[test]
    fn ended_video_does_not_restart_when_toggle_is_pressed() {
        let mut state = loaded_state();
        state.apply(AppCommand::SeekAbsolute(state.duration().unwrap()));
        state.update(Duration::from_secs(1));
        assert_eq!(state.playback_state(), &PlaybackState::Ended);
        let ended_position = state.position();
        state.apply(AppCommand::TogglePlayback);
        state.update(Duration::from_secs(1));
        assert_eq!(state.playback_state(), &PlaybackState::Ended);
        assert_eq!(state.position(), ended_position);
    }

    #[test]
    fn locked_controls_block_commands_until_unlocked() {
        let mut state = loaded_state();
        state.apply(AppCommand::ToggleControlsLock);
        assert!(state.ui.controls_locked);
        state.apply(AppCommand::Play);
        state.update(Duration::from_secs(2));
        assert_eq!(state.position(), Duration::ZERO);
        state.apply(AppCommand::SeekRelative(60));
        assert_eq!(state.position(), Duration::ZERO);
        state.apply(AppCommand::ToggleControlsLock);
        state.apply(AppCommand::Play);
        state.update(Duration::from_secs(2));
        assert_eq!(state.position(), Duration::from_secs(2));
    }

    #[test]
    fn adjacent_rate_stops_at_available_rate_boundaries() {
        let mut state = loaded_state();
        assert_eq!(state.adjacent_playback_rate(1), Some(1.5));
        state.apply(AppCommand::SetPlaybackRate(2.0));
        assert_eq!(state.adjacent_playback_rate(1), None);
        assert_eq!(state.adjacent_playback_rate(-1), Some(1.5));
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
