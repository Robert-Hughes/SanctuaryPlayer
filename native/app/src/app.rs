use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use url::Url;

use ::oxideav::core::FrameLease;

use crate::model::{AppCommand, PlaybackState, Quality};
use crate::playback::{DecodeMode, DummyPlayback, OxidePlayback, PlaybackBackend};
use crate::services::{PositionService, RemotePositionService, SavedPosition, VideoMetadata};
use crate::settings::{Settings, SettingsStore};
use crate::spoilers::sanitise_title;
use crate::twitch::{TwitchVodResolveError, resolve_vod_m3u8};
use crate::video::{VideoPlatform, VideoSource};

const CONTROLS_HIDE_AFTER: Duration = Duration::from_secs(2);
const LOCK_SLIDE_BACK_DURATION: Duration = Duration::from_millis(500);
const POSITION_UPLOAD_DELTA: Duration = Duration::from_secs(10);
const POSITION_SAVE_RETRY_DELAY: Duration = Duration::from_secs(5);

type TwitchResolver = fn(&str) -> Result<Url, TwitchVodResolveError>;
type PlaybackFactory = fn(VideoSource, Url, DecodeMode) -> Result<Box<dyn PlaybackBackend>, String>;

fn open_oxide_playback(
    source: VideoSource,
    url: Url,
    decode_mode: DecodeMode,
) -> Result<Box<dyn PlaybackBackend>, String> {
    OxidePlayback::open(source, url, decode_mode)
        .map(|playback| Box::new(playback) as Box<dyn PlaybackBackend>)
}

struct PendingVideoOpen {
    receiver: Receiver<Result<Box<dyn PlaybackBackend>, String>>,
}

struct PendingPositionsFetch {
    user_id: String,
    receiver: Receiver<Result<Vec<SavedPosition>, String>>,
}

struct PendingPositionSave {
    user_id: String,
    device_id: String,
    source: VideoSource,
    position: Duration,
    receiver: Receiver<Result<(), String>>,
}

#[derive(Debug, Clone)]
struct LastUploadedPosition {
    user_id: String,
    device_id: String,
    video_id: String,
    position: Duration,
}

pub struct AppState {
    playback: Box<dyn PlaybackBackend>,
    positions_service: Arc<dyn PositionService>,
    saved_positions: Vec<SavedPosition>,
    positions_error: Option<String>,
    positions_refresh_requested: bool,
    pending_positions_fetch: Option<PendingPositionsFetch>,
    pending_position_save: Option<PendingPositionSave>,
    last_uploaded_position: Option<LastUploadedPosition>,
    next_position_save_allowed: Instant,
    settings_store: Option<SettingsStore>,
    metadata: Option<VideoMetadata>,
    account: AccountState,
    preferences: Preferences,
    twitch_resolver: TwitchResolver,
    playback_factory: PlaybackFactory,
    decode_mode: DecodeMode,
    pending_video_open: Option<PendingVideoOpen>,
    play_when_opened: bool,
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
    TwitchResolving {
        video_id: String,
    },
    Message {
        title: String,
        message: String,
    },
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            playback: Box::new(DummyPlayback::new()),
            positions_service: Arc::new(RemotePositionService::new()),
            saved_positions: Vec::new(),
            positions_error: None,
            positions_refresh_requested: false,
            pending_positions_fetch: None,
            pending_position_save: None,
            last_uploaded_position: None,
            next_position_save_allowed: Instant::now(),
            settings_store: None,
            metadata: None,
            account: AccountState::default(),
            preferences: Preferences::default(),
            twitch_resolver: resolve_vod_m3u8,
            playback_factory: open_oxide_playback,
            decode_mode: DecodeMode::Cpu,
            pending_video_open: None,
            play_when_opened: false,
            ui: UiState::default(),
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_decode_mode(decode_mode: DecodeMode) -> Self {
        Self {
            decode_mode,
            ..Self::default()
        }
    }

    pub fn set_settings_path(&mut self, path: PathBuf) {
        let store = SettingsStore::new(path);
        eprintln!("SanctuaryPlayer: settings path={}", store.path().display());
        match store.load() {
            Ok(settings) => {
                self.account.user_id = settings.user_id;
                self.account.device_id = settings.device_id;
                self.preferences.favourite_qualities = settings.favourite_qualities;
                self.positions_refresh_requested = self.signed_in();
            }
            Err(error) => {
                eprintln!("SanctuaryPlayer: unable to load settings: {error}");
            }
        }
        self.settings_store = Some(store);
    }

    fn persist_settings(&self) {
        let Some(store) = self.settings_store.as_ref() else {
            return;
        };
        let settings = Settings {
            user_id: self.account.user_id.clone(),
            device_id: self.account.device_id.clone(),
            favourite_qualities: self.preferences.favourite_qualities.clone(),
        };
        if let Err(error) = store.save(&settings) {
            eprintln!("SanctuaryPlayer: unable to save settings: {error}");
        }
    }

    pub(crate) fn decode_mode(&self) -> DecodeMode {
        self.decode_mode
    }

    pub fn play_when_opened(&mut self) {
        self.play_when_opened = true;
    }

    pub fn update(&mut self, elapsed: Duration) {
        self.poll_video_open();
        self.playback.update(elapsed);
        self.age_saved_positions(elapsed);
        self.poll_positions_fetch();
        self.poll_position_save();
        self.start_positions_fetch_if_requested();
        self.maybe_save_position();
        self.resume_deferred_autoplay();
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

    fn poll_video_open(&mut self) {
        let Some(pending) = self.pending_video_open.as_ref() else {
            return;
        };

        let result = match pending.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Err("video-open worker stopped unexpectedly".into()))
            }
        };
        let Some(result) = result else {
            return;
        };

        self.pending_video_open = None;
        match result {
            Ok(playback) => {
                self.playback = playback;
                self.metadata = None;
                self.preferences.manually_selected_quality = false;
                self.apply_favourite_quality();
                let start_time = self
                    .playback
                    .source()
                    .and_then(|source| source.start_time)
                    .filter(|position| !position.is_zero());
                if let Some(position) = start_time {
                    self.playback.seek(position);
                } else if self.play_when_opened {
                    self.playback.play();
                    self.play_when_opened = false;
                }
                self.ui.dialog = None;
            }
            Err(message) => {
                self.play_when_opened = false;
                self.ui.dialog = Some(DialogState::Message {
                    title: "Unable to open Twitch video".into(),
                    message,
                });
            }
        }
        self.note_interaction();
    }

    fn begin_twitch_resolution(&mut self, source: VideoSource) {
        let resolver = self.twitch_resolver;
        let playback_factory = self.playback_factory;
        let decode_mode = self.decode_mode;
        let video_id = source.id.clone();
        let worker_video_id = video_id.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = resolver(&worker_video_id)
                .map_err(|error| error.to_string())
                .and_then(|url| playback_factory(source, url, decode_mode));
            let _ = sender.send(result);
        });

        self.playback = Box::new(DummyPlayback::new());
        self.metadata = None;
        self.pending_video_open = Some(PendingVideoOpen { receiver });
        self.ui.menu_open = false;
        self.ui.dialog = Some(DialogState::TwitchResolving { video_id });
        self.note_interaction();
    }

    fn show_message(&mut self, title: impl Into<String>, message: impl Into<String>) {
        self.pending_video_open = None;
        self.ui.menu_open = false;
        self.ui.dialog = Some(DialogState::Message {
            title: title.into(),
            message: message.into(),
        });
        self.note_interaction();
    }

    fn age_saved_positions(&mut self, elapsed: Duration) {
        for position in &mut self.saved_positions {
            position.modified_age = position.modified_age.saturating_add(elapsed);
            if let Some(age) = position.release_age.as_mut() {
                *age = age.saturating_add(elapsed);
            }
        }
    }

    fn resume_deferred_autoplay(&mut self) {
        if !self.play_when_opened || self.pending_video_open.is_some() || !self.has_video() {
            return;
        }
        match self.playback.state() {
            PlaybackState::Paused => {
                self.playback.play();
                self.play_when_opened = false;
            }
            PlaybackState::Ended | PlaybackState::Error(_) => {
                self.play_when_opened = false;
            }
            _ => {}
        }
    }

    fn start_positions_fetch_if_requested(&mut self) {
        if !self.positions_refresh_requested || self.pending_positions_fetch.is_some() {
            return;
        }
        let Some(user_id) = self.account.user_id.clone() else {
            self.positions_refresh_requested = false;
            return;
        };
        let service = Arc::clone(&self.positions_service);
        let worker_user_id = user_id.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = service.positions(&worker_user_id);
            let _ = sender.send(result);
        });
        self.positions_refresh_requested = false;
        self.positions_error = None;
        self.pending_positions_fetch = Some(PendingPositionsFetch { user_id, receiver });
    }

    fn poll_positions_fetch(&mut self) {
        let Some(pending) = self.pending_positions_fetch.as_ref() else {
            return;
        };
        let result = match pending.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(
                "saved-position fetch worker stopped unexpectedly".into(),
            )),
        };
        let Some(result) = result else {
            return;
        };
        let user_id = pending.user_id.clone();
        self.pending_positions_fetch = None;
        if self.account.user_id.as_deref() != Some(user_id.as_str()) {
            return;
        }
        match result {
            Ok(positions) => {
                eprintln!(
                    "SanctuaryPlayer: loaded {} saved positions",
                    positions.len()
                );
                self.saved_positions = positions;
                self.positions_error = None;
            }
            Err(error) => {
                eprintln!("SanctuaryPlayer: saved-position fetch failed: {error}");
                self.positions_error = Some(error);
            }
        }
    }

    fn maybe_save_position(&mut self) {
        if self.pending_position_save.is_some() || Instant::now() < self.next_position_save_allowed
        {
            return;
        }
        if !matches!(
            self.playback.state(),
            PlaybackState::Playing | PlaybackState::Paused | PlaybackState::Ended
        ) {
            return;
        }
        let (Some(user_id), Some(device_id), Some(source)) = (
            self.account.user_id.clone(),
            self.account.device_id.clone(),
            self.playback.source().cloned(),
        ) else {
            return;
        };
        let position = self.playback.position();
        if position.is_zero() {
            return;
        }
        let rounded_seconds = position.as_secs_f64().round().clamp(0.0, u64::MAX as f64) as u64;
        let rounded_position = Duration::from_secs(rounded_seconds);
        if rounded_position.is_zero() {
            return;
        }
        let should_upload = self.last_uploaded_position.as_ref().is_none_or(|last| {
            last.user_id != user_id
                || last.device_id != device_id
                || last.video_id != source.id
                || position.abs_diff(last.position) > POSITION_UPLOAD_DELTA
        });
        if !should_upload {
            return;
        }

        let service = Arc::clone(&self.positions_service);
        let worker_user_id = user_id.clone();
        let worker_device_id = device_id.clone();
        let worker_source = source.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = service.save_position(
                &worker_user_id,
                &worker_device_id,
                &worker_source,
                rounded_position,
            );
            let _ = sender.send(result);
        });
        self.pending_position_save = Some(PendingPositionSave {
            user_id,
            device_id,
            source,
            position: rounded_position,
            receiver,
        });
    }

    fn poll_position_save(&mut self) {
        let Some(pending) = self.pending_position_save.as_ref() else {
            return;
        };
        let result = match pending.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(
                "saved-position upload worker stopped unexpectedly".into(),
            )),
        };
        let Some(result) = result else {
            return;
        };
        let user_id = pending.user_id.clone();
        let device_id = pending.device_id.clone();
        let source = pending.source.clone();
        let position = pending.position;
        self.pending_position_save = None;
        match result {
            Ok(()) => {
                eprintln!(
                    "SanctuaryPlayer: saved position video={} position={}s",
                    source.id,
                    position.as_secs()
                );
                self.last_uploaded_position = Some(LastUploadedPosition {
                    user_id: user_id.clone(),
                    device_id: device_id.clone(),
                    video_id: source.id.clone(),
                    position,
                });
                if self.account.user_id.as_deref() == Some(user_id.as_str())
                    && self.account.device_id.as_deref() == Some(device_id.as_str())
                {
                    self.update_cached_saved_position(source, device_id, position);
                }
            }
            Err(error) => {
                eprintln!("SanctuaryPlayer: saved-position upload failed: {error}");
                self.next_position_save_allowed = Instant::now() + POSITION_SAVE_RETRY_DELAY;
            }
        }
    }

    fn update_cached_saved_position(
        &mut self,
        source: VideoSource,
        device_id: String,
        position: Duration,
    ) {
        let existing_index = self
            .saved_positions
            .iter()
            .position(|entry| entry.source.id == source.id && entry.device_id == device_id);
        let (title, release_age) = existing_index
            .map(|index| {
                let existing = self.saved_positions.remove(index);
                (existing.title, existing.release_age)
            })
            .unwrap_or((None, None));
        self.saved_positions.insert(
            0,
            SavedPosition {
                source,
                device_id,
                position,
                modified_age: Duration::ZERO,
                title,
                release_age,
            },
        );
        self.saved_positions.truncate(10);
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
            AppCommand::OpenVideo(source) => match source.platform {
                VideoPlatform::Twitch => self.begin_twitch_resolution(source),
                VideoPlatform::YouTube => self.show_message(
                    "YouTube is not supported yet",
                    "SanctuaryPlayer recognises YouTube video IDs and URLs, but YouTube playback is currently unsupported.",
                ),
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
                self.persist_settings();
                if !self.preferences.manually_selected_quality {
                    self.apply_favourite_quality();
                }
            }
            AppCommand::SignIn { user_id, device_id } => {
                self.account.user_id = Some(user_id);
                self.account.device_id = Some(device_id);
                self.persist_settings();
                self.saved_positions.clear();
                self.positions_error = None;
                self.positions_refresh_requested = true;
                self.last_uploaded_position = None;
                self.next_position_save_allowed = Instant::now();
            }
            AppCommand::SignOut => {
                self.account = AccountState::default();
                self.persist_settings();
                self.saved_positions.clear();
                self.positions_error = None;
                self.positions_refresh_requested = false;
                self.last_uploaded_position = None;
            }
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
        if self.ui.menu_open {
            if self.signed_in() {
                self.positions_refresh_requested = true;
            }
            if self.has_video() {
                self.playback.pause();
            }
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

    pub(crate) fn take_video_frame_lease(&mut self) -> Option<FrameLease> {
        self.playback.take_video_frame_lease()
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
        if self.signed_in() {
            self.saved_positions.clone()
        } else {
            Vec::new()
        }
    }

    pub fn saved_positions_loading(&self) -> bool {
        self.signed_in()
            && (self.positions_refresh_requested || self.pending_positions_fetch.is_some())
    }

    pub fn saved_positions_error(&self) -> Option<&str> {
        self.positions_error.as_deref()
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
        self.playback.needs_animation()
            || self.ui.lock_return_from.is_some()
            || self.pending_video_open.is_some()
            || self.positions_refresh_requested
            || self.pending_positions_fetch.is_some()
            || self.pending_position_save.is_some()
    }
}

#[cfg(test)]
mod tests {
    use crate::services::{DummyMetadataService, MetadataService};

    use super::*;

    fn temporary_settings_path(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "sanctuary-player-app-settings-test-{}-{unique}-{name}.json",
            std::process::id()
        ))
    }

    fn loaded_state() -> AppState {
        let mut state = AppState::new();
        let source = VideoSource::parse("2386400830").unwrap();
        state.playback.open(&source).unwrap();
        state.metadata = Some(DummyMetadataService.metadata_for(&source));
        state
    }

    fn test_twitch_resolver(_video_id: &str) -> Result<Url, TwitchVodResolveError> {
        Ok(Url::parse("https://usher.ttvnw.net/vod/2386400830.m3u8?sig=test").unwrap())
    }

    fn test_playback_factory(
        source: VideoSource,
        _url: Url,
        _decode_mode: DecodeMode,
    ) -> Result<Box<dyn PlaybackBackend>, String> {
        let mut playback = DummyPlayback::new();
        playback.open(&source)?;
        Ok(Box::new(playback))
    }

    fn test_playback_factory_requires_app_start_seek(
        source: VideoSource,
        _url: Url,
        _decode_mode: DecodeMode,
    ) -> Result<Box<dyn PlaybackBackend>, String> {
        let mut playback = DummyPlayback::new();
        playback.open(&source)?;
        // Preserve the source/start_time metadata but force the backend itself
        // back to zero. This models OxidePlayback, which needs AppState to
        // dispatch the actual HLS seek after opening.
        playback.seek(Duration::ZERO);
        playback.update(Duration::from_secs(1));
        Ok(Box::new(playback))
    }

    #[test]
    fn video_start_time_is_applied_after_async_backend_open() {
        let mut state = AppState::new();
        state.twitch_resolver = test_twitch_resolver;
        state.playback_factory = test_playback_factory_requires_app_start_seek;
        state.apply(AppCommand::OpenVideo(
            VideoSource::parse("https://www.twitch.tv/videos/2386400830?t=5m").unwrap(),
        ));

        for _ in 0..100 {
            state.update(Duration::from_millis(200));
            if state.pending_video_open.is_none()
                && !matches!(state.playback_state(), PlaybackState::Seeking)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(state.pending_video_open.is_none());
        assert_eq!(state.playback_state(), &PlaybackState::Paused);
        assert_eq!(state.position(), Duration::from_secs(300));
    }

    #[test]
    fn youtube_open_reports_currently_unsupported() {
        let mut state = AppState::new();
        state.apply(AppCommand::OpenVideo(
            VideoSource::parse("3fgD9k8Hkbc").unwrap(),
        ));

        assert!(matches!(
            state.ui.dialog,
            Some(DialogState::Message { ref title, ref message })
                if title.contains("YouTube") && message.contains("unsupported")
        ));
        assert!(state.pending_video_open.is_none());
        assert!(!state.has_video());
    }

    #[test]
    fn twitch_open_resolves_hls_url_without_blocking_the_command() {
        let mut state = AppState::new();
        state.twitch_resolver = test_twitch_resolver;
        state.playback_factory = test_playback_factory;
        state.apply(AppCommand::OpenVideo(
            VideoSource::parse("2386400830").unwrap(),
        ));

        assert!(state.pending_video_open.is_some());
        assert!(state.needs_animation());
        assert!(matches!(
            state.ui.dialog,
            Some(DialogState::TwitchResolving { .. })
        ));

        for _ in 0..100 {
            state.update(Duration::ZERO);
            if state.pending_video_open.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        assert!(state.pending_video_open.is_none());
        assert!(state.ui.dialog.is_none());
        assert!(state.has_video());
    }

    #[test]
    fn autoplay_starts_after_async_video_open_completes() {
        let mut state = AppState::new();
        state.twitch_resolver = test_twitch_resolver;
        state.playback_factory = test_playback_factory;
        state.play_when_opened();
        state.apply(AppCommand::OpenVideo(
            VideoSource::parse("2386400830").unwrap(),
        ));

        for _ in 0..100 {
            state.update(Duration::ZERO);
            if state.pending_video_open.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        assert!(state.pending_video_open.is_none());
        assert_eq!(state.playback_state(), &PlaybackState::Playing);
        assert!(!state.play_when_opened);
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
    fn persisted_account_and_favourites_are_restored_on_restart() {
        let path = temporary_settings_path("restore");
        let mut first = AppState::new();
        first.set_settings_path(path.clone());
        first.apply(AppCommand::SignIn {
            user_id: "test-user".into(),
            device_id: "test-device".into(),
        });
        first.apply(AppCommand::SetFavouriteQualities("1080p60,720p60".into()));

        let mut second = AppState::new();
        second.set_settings_path(path.clone());
        assert!(second.signed_in());
        assert_eq!(second.user_id(), Some("test-user"));
        assert_eq!(second.device_id(), Some("test-device"));
        assert_eq!(second.favourite_qualities(), "1080p60,720p60");
        assert!(second.positions_refresh_requested);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sign_out_clears_persisted_account_but_keeps_preferences() {
        let path = temporary_settings_path("signout");
        let mut first = AppState::new();
        first.set_settings_path(path.clone());
        first.apply(AppCommand::SignIn {
            user_id: "test-user".into(),
            device_id: "test-device".into(),
        });
        first.apply(AppCommand::SetFavouriteQualities("720p60".into()));
        first.apply(AppCommand::SignOut);

        let mut second = AppState::new();
        second.set_settings_path(path.clone());
        assert!(!second.signed_in());
        assert_eq!(second.user_id(), None);
        assert_eq!(second.device_id(), None);
        assert_eq!(second.favourite_qualities(), "720p60");
        assert!(!second.positions_refresh_requested);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sign_in_fetches_saved_positions_without_blocking() {
        let service = Arc::new(crate::services::DummyPositionService::new());
        let mut state = loaded_state();
        state.positions_service = service;
        assert!(state.saved_positions().is_empty());
        state.apply(AppCommand::SignIn {
            user_id: "test-user".into(),
            device_id: "Desktop".into(),
        });
        assert!(state.saved_positions_loading());

        for _ in 0..100 {
            state.update(Duration::ZERO);
            if !state.saved_positions_loading() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!state.saved_positions_loading());
        assert_eq!(state.saved_positions().len(), 3);
    }

    #[test]
    fn progress_uploads_after_more_than_ten_seconds_from_last_success() {
        let service = Arc::new(crate::services::DummyPositionService::new());
        let mut state = loaded_state();
        state.positions_service = service.clone();
        state.apply(AppCommand::SignIn {
            user_id: "test-user".into(),
            device_id: "Native".into(),
        });

        fn settle(state: &mut AppState) {
            for _ in 0..200 {
                state.update(Duration::from_millis(200));
                if state.pending_position_save.is_none()
                    && !matches!(state.playback_state(), PlaybackState::Seeking)
                {
                    // Give a just-spawned upload one extra poll opportunity.
                    state.update(Duration::ZERO);
                    if state.pending_position_save.is_none() {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("position worker did not settle");
        }

        state.apply(AppCommand::SeekAbsolute(Duration::from_secs(100)));
        settle(&mut state);
        let current = || {
            service
                .positions("test-user")
                .unwrap()
                .into_iter()
                .find(|entry| entry.device_id == "Native" && entry.source.id == "2386400830")
                .map(|entry| entry.position)
        };
        assert_eq!(current(), Some(Duration::from_secs(100)));

        state.apply(AppCommand::SeekAbsolute(Duration::from_secs(105)));
        settle(&mut state);
        assert_eq!(current(), Some(Duration::from_secs(100)));

        state.apply(AppCommand::SeekAbsolute(Duration::from_secs(112)));
        settle(&mut state);
        assert_eq!(current(), Some(Duration::from_secs(112)));
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
