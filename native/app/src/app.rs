use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use url::Url;

use ::oxideav::core::{FrameLease, VideoColorInfo};

use crate::model::{AppCommand, PlaybackState, Quality};
use crate::playback::{
    DecodeMode, DummyPlayback, OxidePlayback, PendingPlaybackWakes, PlaybackBackend, PlaybackWake,
};
use crate::services::{PositionService, RemotePositionService, SavedPosition, VideoMetadata};
use crate::session::{SessionState, SessionStore};
use crate::settings::{Settings, SettingsStore};
use crate::spoilers::sanitise_title;
use crate::twitch::{ResolvedTwitchVod, TwitchVodMetadata, TwitchVodResolveError, resolve_vod};
use crate::video::{VideoPlatform, VideoSource};

const CONTROLS_HIDE_AFTER: Duration = Duration::from_secs(2);
const LOCK_SLIDE_BACK_DURATION: Duration = Duration::from_millis(500);
const POSITION_UPLOAD_DELTA: Duration = Duration::from_secs(10);
const CURRENT_POSITION_ROW_TOLERANCE: Duration = Duration::from_secs(10);
const POSITION_SAVE_RETRY_DELAY: Duration = Duration::from_secs(5);
const PAUSE_POSITION_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
const SESSION_SAVE_INTERVAL: Duration = Duration::from_secs(3);
const DEFAULT_WINDOW_TITLE: &str = "Sanctuary Player";

type TwitchResolver = fn(&str) -> Result<ResolvedTwitchVod, TwitchVodResolveError>;
type PlaybackFactory = fn(
    VideoSource,
    Url,
    String,
    DecodeMode,
    bool,
    PlaybackWake,
) -> Result<Box<dyn PlaybackBackend>, String>;

fn open_oxide_playback(
    source: VideoSource,
    url: Url,
    initial_qualities: String,
    decode_mode: DecodeMode,
    muted: bool,
    wake: PlaybackWake,
) -> Result<Box<dyn PlaybackBackend>, String> {
    OxidePlayback::open(source, url, &initial_qualities, decode_mode, muted, wake)
        .map(|playback| Box::new(playback) as Box<dyn PlaybackBackend>)
}

fn video_metadata_from_twitch(metadata: TwitchVodMetadata) -> VideoMetadata {
    let published_millis = i128::from(metadata.published_at.timestamp_millis());
    let now_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i128)
        .unwrap_or(published_millis);
    let age_millis = now_millis.saturating_sub(published_millis).max(0);
    VideoMetadata {
        title: metadata.title,
        release_age: Duration::from_millis(age_millis.min(i128::from(u64::MAX)) as u64),
    }
}

struct OpenedVideo {
    playback: Box<dyn PlaybackBackend>,
    metadata: Option<VideoMetadata>,
}

struct PendingVideoOpen {
    receiver: Receiver<Result<OpenedVideo, String>>,
}

struct PendingPositionsFetch {
    user_id: String,
    receiver: Receiver<Result<Vec<SavedPosition>, String>>,
}

#[derive(Debug, Clone)]
struct PositionSaveRequest {
    user_id: String,
    device_id: String,
    source: VideoSource,
    position: Duration,
}

struct PendingPositionSave {
    request: PositionSaveRequest,
    receiver: Receiver<Result<(), String>>,
}

#[derive(Debug, Clone)]
struct LastUploadedPosition {
    user_id: String,
    device_id: String,
    video_id: String,
    position: Duration,
}

fn same_position_key(left: &PositionSaveRequest, right: &PositionSaveRequest) -> bool {
    left.user_id == right.user_id
        && left.device_id == right.device_id
        && left.source.id == right.source.id
}

pub struct AppState {
    playback: Box<dyn PlaybackBackend>,
    positions_service: Arc<dyn PositionService>,
    saved_positions: Vec<SavedPosition>,
    positions_error: Option<String>,
    positions_refresh_requested: bool,
    pending_positions_fetch: Option<PendingPositionsFetch>,
    pending_position_save: Option<PendingPositionSave>,
    position_save_queue: VecDeque<PositionSaveRequest>,
    pause_position_save_due: Option<Instant>,
    last_uploaded_position: Option<LastUploadedPosition>,
    next_position_save_allowed: Instant,
    settings_store: Option<SettingsStore>,
    session_store: Option<SessionStore>,
    startup_session: Option<SessionState>,
    last_safe_session: Option<SessionState>,
    last_persisted_session: Option<SessionState>,
    next_session_save_allowed: Instant,
    metadata: Option<VideoMetadata>,
    account: AccountState,
    preferences: Preferences,
    twitch_resolver: TwitchResolver,
    playback_factory: PlaybackFactory,
    playback_wake: PlaybackWake,
    decode_mode: DecodeMode,
    pending_video_open: Option<PendingVideoOpen>,
    play_when_opened: bool,
    muted: bool,
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
    #[cfg(target_os = "android")]
    pub(crate) android_text_input: Option<AndroidTextInputSnapshot>,
    #[cfg(target_os = "android")]
    pub(crate) android_text_selection_override: Option<(AndroidTextField, usize, usize)>,
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
            #[cfg(target_os = "android")]
            android_text_input: None,
            #[cfg(target_os = "android")]
            android_text_selection_override: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AndroidTextField {
    ChangeVideo,
    SeekTo,
    FavouriteQualities,
    SignInUser,
    SignInDevice,
}

#[cfg(target_os = "android")]
#[derive(Clone, Debug)]
pub struct AndroidTextInputSnapshot {
    pub field: AndroidTextField,
    pub text: String,
    pub selection_start: usize,
    pub selection_end: usize,
    pub clicked: bool,
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
            position_save_queue: VecDeque::new(),
            pause_position_save_due: None,
            last_uploaded_position: None,
            next_position_save_allowed: Instant::now(),
            settings_store: None,
            session_store: None,
            startup_session: None,
            last_safe_session: None,
            last_persisted_session: None,
            next_session_save_allowed: Instant::now(),
            metadata: None,
            account: AccountState::default(),
            preferences: Preferences::default(),
            twitch_resolver: resolve_vod,
            playback_factory: open_oxide_playback,
            playback_wake: PlaybackWake::default(),
            decode_mode: DecodeMode::Cpu,
            pending_video_open: None,
            play_when_opened: false,
            muted: false,
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

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    pub fn set_playback_wake(&mut self, wake: PlaybackWake) {
        self.playback_wake = wake;
    }

    pub fn take_playback_wakes(&self) -> PendingPlaybackWakes {
        self.playback_wake.take_pending()
    }

    #[cfg(target_os = "android")]
    pub fn begin_android_ui_frame(&mut self) {
        self.ui.android_text_input = None;
    }

    #[cfg(target_os = "android")]
    pub(crate) fn capture_android_text_edit(
        &mut self,
        ctx: &egui::Context,
        field: AndroidTextField,
        output: &mut egui::widgets::text_edit::TextEditOutput,
        text: &str,
    ) {
        if let Some((override_field, start, end)) = self.ui.android_text_selection_override.take() {
            if override_field == field {
                let range = egui::text::CCursorRange::two(
                    egui::text::CCursor::new(start),
                    egui::text::CCursor::new(end),
                );
                output.state.cursor.set_char_range(Some(range));
                output.state.clone().store(ctx, output.response.id);
                output.cursor_range = Some(range);
            } else {
                self.ui.android_text_selection_override = Some((override_field, start, end));
            }
        }

        if !output.response.has_focus() {
            return;
        }

        let fallback = text.chars().count();
        let (selection_start, selection_end) = output
            .cursor_range
            .map(|range| (range.primary.index, range.secondary.index))
            .unwrap_or((fallback, fallback));
        self.ui.android_text_input = Some(AndroidTextInputSnapshot {
            field,
            text: text.to_owned(),
            selection_start,
            selection_end,
            clicked: output.response.clicked(),
        });
    }

    #[cfg(target_os = "android")]
    pub fn android_text_input(&self) -> Option<&AndroidTextInputSnapshot> {
        self.ui.android_text_input.as_ref()
    }

    #[cfg(target_os = "android")]
    pub fn apply_android_text_input_state(
        &mut self,
        text: String,
        selection_start: usize,
        selection_end: usize,
    ) {
        let Some(snapshot) = self.ui.android_text_input.clone() else {
            return;
        };
        let updated = match (snapshot.field, self.ui.dialog.as_mut()) {
            (AndroidTextField::ChangeVideo, Some(DialogState::ChangeVideo { input, .. }))
            | (AndroidTextField::SeekTo, Some(DialogState::SeekTo { input, .. }))
            | (
                AndroidTextField::FavouriteQualities,
                Some(DialogState::FavouriteQualities { input }),
            ) => {
                *input = text;
                true
            }
            (AndroidTextField::SignInUser, Some(DialogState::SignIn { user_id, .. })) => {
                *user_id = text;
                true
            }
            (AndroidTextField::SignInDevice, Some(DialogState::SignIn { device_id, .. })) => {
                *device_id = text;
                true
            }
            _ => false,
        };
        if updated {
            self.ui.android_text_selection_override =
                Some((snapshot.field, selection_start, selection_end));
        }
    }

    pub fn set_settings_path(&mut self, path: PathBuf) {
        let store = SettingsStore::new(path);
        log::info!("SanctuaryPlayer: settings path={}", store.path().display());
        match store.load() {
            Ok(settings) => {
                self.account.user_id = settings.user_id;
                self.account.device_id = settings.device_id;
                self.preferences.favourite_qualities = settings.favourite_qualities;
                self.positions_refresh_requested = self.signed_in();
            }
            Err(error) => {
                log::warn!("SanctuaryPlayer: unable to load settings: {error}");
            }
        }
        self.settings_store = Some(store);
    }

    pub fn set_session_path(&mut self, path: PathBuf) {
        let store = SessionStore::new(path);
        log::info!("SanctuaryPlayer: session path={}", store.path().display());
        match store.load() {
            Ok(session) => {
                self.startup_session = session.clone();
                self.last_persisted_session = session;
            }
            Err(error) => {
                log::warn!("SanctuaryPlayer: unable to load session: {error}");
            }
        }
        self.session_store = Some(store);
    }

    pub fn take_startup_session_source(&mut self) -> Option<VideoSource> {
        self.startup_session
            .take()
            .map(|session| session.restore_source())
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
            log::warn!("SanctuaryPlayer: unable to save settings: {error}");
        }
    }

    pub fn decode_mode(&self) -> DecodeMode {
        self.decode_mode
    }

    pub fn play_when_opened(&mut self) {
        self.play_when_opened = true;
    }

    pub fn update(&mut self, elapsed: Duration) {
        self.poll_video_open();
        let was_seeking = matches!(self.playback.state(), PlaybackState::Seeking);
        self.playback.update(elapsed);
        let seek_completed =
            was_seeking && !matches!(self.playback.state(), PlaybackState::Seeking);
        self.refresh_safe_session();
        self.persist_session(seek_completed);
        self.age_saved_positions(elapsed);
        if let Some(metadata) = self.metadata.as_mut() {
            metadata.release_age = metadata.release_age.saturating_add(elapsed);
        }
        self.poll_positions_fetch();
        self.poll_position_save();
        self.start_positions_fetch_if_requested();
        self.maybe_queue_paused_position();
        self.maybe_save_position();
        self.start_next_position_save();
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
            Ok(opened) => {
                self.playback = opened.playback;
                self.metadata = opened.metadata;
                self.preferences.manually_selected_quality = false;
                let start_time = self
                    .playback
                    .source()
                    .and_then(|source| source.start_time)
                    .filter(|position| !position.is_zero());
                if let Some(position) = start_time {
                    self.playback.seek(position);
                } else {
                    if self.play_when_opened {
                        self.playback.play();
                        self.play_when_opened = false;
                    }
                    self.refresh_safe_session();
                    self.persist_session(true);
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
        let playback_wake = self.playback_wake.clone();
        let initial_qualities = self.preferences.favourite_qualities.clone();
        let decode_mode = self.decode_mode;
        let muted = self.muted;
        let video_id = source.id.clone();
        let worker_video_id = video_id.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = resolver(&worker_video_id)
                .map_err(|error| error.to_string())
                .and_then(|resolved| {
                    let metadata = resolved.metadata.map(video_metadata_from_twitch);
                    playback_factory(
                        source,
                        resolved.hls_url,
                        initial_qualities,
                        decode_mode,
                        muted,
                        playback_wake,
                    )
                    .map(|playback| OpenedVideo { playback, metadata })
                });
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

    fn refresh_safe_session(&mut self) {
        if !matches!(
            self.playback.state(),
            PlaybackState::Playing | PlaybackState::Paused | PlaybackState::Ended
        ) {
            return;
        }
        let Some(mut source) = self.playback.source().cloned() else {
            return;
        };
        source.start_time = None;
        self.last_safe_session = Some(SessionState {
            source,
            position: self.playback.position(),
        });
    }

    fn persist_session(&mut self, force: bool) {
        let Some(store) = self.session_store.as_ref() else {
            return;
        };
        let Some(session) = self.last_safe_session.as_ref() else {
            return;
        };
        let now = Instant::now();
        if !force && now < self.next_session_save_allowed {
            return;
        }
        if self.last_persisted_session.as_ref() == Some(session) {
            self.next_session_save_allowed = now + SESSION_SAVE_INTERVAL;
            return;
        }

        match store.save(session) {
            Ok(()) => {
                self.last_persisted_session = Some(session.clone());
            }
            Err(error) => {
                log::warn!("SanctuaryPlayer: unable to save session: {error}");
            }
        }
        self.next_session_save_allowed = now + SESSION_SAVE_INTERVAL;
    }

    pub fn flush_local_session(&mut self) {
        self.refresh_safe_session();
        self.persist_session(true);
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
                log::info!(
                    "SanctuaryPlayer: loaded {} saved positions",
                    positions.len()
                );
                self.saved_positions = positions;
                self.positions_error = None;
            }
            Err(error) => {
                log::warn!("SanctuaryPlayer: saved-position fetch failed: {error}");
                self.positions_error = Some(error);
            }
        }
    }

    fn maybe_save_position(&mut self) {
        if !matches!(
            self.playback.state(),
            PlaybackState::Playing | PlaybackState::Paused | PlaybackState::Ended
        ) {
            return;
        }
        self.refresh_safe_session();
        let Some(session) = self.last_safe_session.clone() else {
            return;
        };
        self.queue_position_save(&session, false);
    }

    fn schedule_paused_position_save(&mut self) {
        self.pause_position_save_due = Some(Instant::now() + PAUSE_POSITION_SAVE_DEBOUNCE);
    }

    fn cancel_paused_position_save(&mut self) {
        self.pause_position_save_due = None;
    }

    fn maybe_queue_paused_position(&mut self) {
        if !self
            .pause_position_save_due
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return;
        }
        self.pause_position_save_due = None;
        self.refresh_safe_session();
        if let Some(session) = self.last_safe_session.clone() {
            self.queue_position_save(&session, true);
        }
    }

    fn force_remote_position_save(&mut self) {
        self.pause_position_save_due = None;
        self.refresh_safe_session();
        if let Some(session) = self.last_safe_session.clone() {
            self.queue_position_save(&session, true);
        }
        self.start_next_position_save();
    }

    fn queue_position_save(&mut self, session: &SessionState, force: bool) {
        let (Some(user_id), Some(device_id)) =
            (self.account.user_id.clone(), self.account.device_id.clone())
        else {
            return;
        };
        let rounded_seconds = session
            .position
            .as_secs_f64()
            .round()
            .clamp(0.0, u64::MAX as f64) as u64;
        let position = Duration::from_secs(rounded_seconds);
        if position.is_zero() {
            return;
        }

        let mut source = session.source.clone();
        source.start_time = None;
        let baseline = self.latest_requested_position(&user_id, &device_id, &source.id);
        let should_upload = baseline.is_none_or(|baseline| {
            if force {
                baseline != position
            } else {
                position.abs_diff(baseline) > POSITION_UPLOAD_DELTA
            }
        });
        if !should_upload {
            return;
        }

        let request = PositionSaveRequest {
            user_id,
            device_id,
            source,
            position,
        };
        if let Some(index) = self
            .position_save_queue
            .iter()
            .position(|queued| same_position_key(queued, &request))
        {
            self.position_save_queue[index] = request;
        } else {
            self.position_save_queue.push_back(request);
        }
    }

    fn latest_requested_position(
        &self,
        user_id: &str,
        device_id: &str,
        video_id: &str,
    ) -> Option<Duration> {
        self.position_save_queue
            .iter()
            .rev()
            .find(|request| {
                request.user_id == user_id
                    && request.device_id == device_id
                    && request.source.id == video_id
            })
            .map(|request| request.position)
            .or_else(|| {
                self.pending_position_save
                    .as_ref()
                    .filter(|pending| {
                        pending.request.user_id == user_id
                            && pending.request.device_id == device_id
                            && pending.request.source.id == video_id
                    })
                    .map(|pending| pending.request.position)
            })
            .or_else(|| {
                self.last_uploaded_position
                    .as_ref()
                    .filter(|last| {
                        last.user_id == user_id
                            && last.device_id == device_id
                            && last.video_id == video_id
                    })
                    .map(|last| last.position)
            })
    }

    fn start_next_position_save(&mut self) {
        if self.pending_position_save.is_some()
            || self.position_save_queue.is_empty()
            || Instant::now() < self.next_position_save_allowed
        {
            return;
        }
        let Some(request) = self.position_save_queue.pop_front() else {
            return;
        };
        let service = Arc::clone(&self.positions_service);
        let worker_request = request.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = service.save_position(
                &worker_request.user_id,
                &worker_request.device_id,
                &worker_request.source,
                worker_request.position,
            );
            let _ = sender.send(result);
        });
        self.pending_position_save = Some(PendingPositionSave { request, receiver });
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
        self.complete_position_save(result, true);
    }

    fn complete_position_save(&mut self, result: Result<(), String>, requeue_failure: bool) {
        let Some(pending) = self.pending_position_save.take() else {
            return;
        };
        let request = pending.request;
        match result {
            Ok(()) => {
                log::info!(
                    "SanctuaryPlayer: saved position video={} position={}s",
                    request.source.id,
                    request.position.as_secs()
                );
                self.last_uploaded_position = Some(LastUploadedPosition {
                    user_id: request.user_id.clone(),
                    device_id: request.device_id.clone(),
                    video_id: request.source.id.clone(),
                    position: request.position,
                });
                if self.account.user_id.as_deref() == Some(request.user_id.as_str())
                    && self.account.device_id.as_deref() == Some(request.device_id.as_str())
                {
                    self.update_cached_saved_position(
                        request.source,
                        request.device_id,
                        request.position,
                    );
                }
                self.next_position_save_allowed = Instant::now();
            }
            Err(error) => {
                log::warn!("SanctuaryPlayer: saved-position upload failed: {error}");
                if requeue_failure
                    && !self
                        .position_save_queue
                        .iter()
                        .any(|queued| same_position_key(queued, &request))
                {
                    self.position_save_queue.push_front(request);
                }
                self.next_position_save_allowed = Instant::now() + POSITION_SAVE_RETRY_DELAY;
            }
        }
    }

    pub fn flush_persistence_for_shutdown(&mut self, budget: Duration) {
        self.flush_local_session();
        self.force_remote_position_save();

        let deadline = Instant::now() + budget;
        loop {
            if self.pending_position_save.is_none() {
                if self.position_save_queue.is_empty() {
                    break;
                }
                self.next_position_save_allowed = Instant::now();
                self.start_next_position_save();
            }

            let Some(pending) = self.pending_position_save.as_ref() else {
                break;
            };
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            let result = match pending.receiver.recv_timeout(remaining) {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Err("saved-position upload worker stopped unexpectedly".into())
                }
            };
            self.complete_position_save(result, false);
        }

        if self.pending_position_save.is_some() || !self.position_save_queue.is_empty() {
            log::warn!(
                "SanctuaryPlayer: shutdown position flush timed out with {} in-flight and {} queued save(s)",
                usize::from(self.pending_position_save.is_some()),
                self.position_save_queue.len()
            );
        }
    }

    pub fn flush_persistence_for_background(&mut self) {
        self.flush_local_session();
        self.force_remote_position_save();
    }

    pub fn persistence_wake_deadline(&self, now: Instant) -> Option<Instant> {
        let pause_deadline = self.pause_position_save_due;
        let retry_deadline = (self.pending_position_save.is_none()
            && !self.position_save_queue.is_empty())
        .then_some(self.next_position_save_allowed);
        [pause_deadline, retry_deadline]
            .into_iter()
            .flatten()
            .map(|deadline| deadline.max(now))
            .min()
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
            AppCommand::OpenVideo(source) => {
                self.flush_local_session();
                self.force_remote_position_save();
                match source.platform {
                    VideoPlatform::Twitch => self.begin_twitch_resolution(source),
                    VideoPlatform::YouTube => self.show_message(
                        "YouTube is not supported yet",
                        "SanctuaryPlayer recognises YouTube video IDs and URLs, but YouTube playback is currently unsupported.",
                    ),
                }
            }
            AppCommand::TogglePlayback => match self.playback.state() {
                PlaybackState::Playing => {
                    self.playback.pause();
                    self.refresh_safe_session();
                    self.persist_session(true);
                    self.schedule_paused_position_save();
                }
                PlaybackState::Paused => {
                    self.cancel_paused_position_save();
                    self.playback.play();
                }
                _ => {}
            },
            AppCommand::Play => {
                self.cancel_paused_position_save();
                self.playback.play();
            }
            AppCommand::Pause => {
                self.playback.pause();
                self.refresh_safe_session();
                self.persist_session(true);
                self.schedule_paused_position_save();
            }
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
                self.position_save_queue.clear();
                self.pause_position_save_due = None;
                self.next_position_save_allowed = Instant::now();
            }
            AppCommand::SignOut => {
                self.account = AccountState::default();
                self.persist_settings();
                self.saved_positions.clear();
                self.positions_error = None;
                self.positions_refresh_requested = false;
                self.last_uploaded_position = None;
                self.position_save_queue.clear();
                self.pause_position_save_due = None;
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

    pub fn take_video_frame_lease(&mut self) -> Option<FrameLease> {
        self.playback.take_video_frame_lease()
    }

    pub fn video_color_info(&self) -> Option<VideoColorInfo> {
        self.playback.video_color_info()
    }
    pub fn safe_title(&self) -> Option<String> {
        self.metadata
            .as_ref()
            .map(|metadata| sanitise_title(&metadata.title))
    }

    pub fn window_title(&self) -> String {
        self.safe_title()
            .filter(|title| !title.trim().is_empty())
            .map_or_else(
                || DEFAULT_WINDOW_TITLE.to_owned(),
                |title| format!("{title} - {DEFAULT_WINDOW_TITLE}"),
            )
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
        if !self.signed_in() {
            return Vec::new();
        }

        let current_id = self.source().map(|source| source.id.as_str());
        let current_device = self.device_id();
        let current_position = self.position();
        self.saved_positions
            .iter()
            .filter(|entry| {
                !(Some(entry.source.id.as_str()) == current_id
                    && Some(entry.device_id.as_str()) == current_device
                    && entry.position.abs_diff(current_position) < CURRENT_POSITION_ROW_TOLERANCE)
            })
            .cloned()
            .collect()
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
        self.ui.lock_return_from.is_some()
            || self.pending_video_open.is_some()
            || self.positions_refresh_requested
            || self.pending_positions_fetch.is_some()
            || self.pending_position_save.is_some()
    }

    pub fn playback_wake_deadline(&self, now: Instant) -> Option<Instant> {
        self.playback.next_wake_deadline(now)
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

    fn settle_position_saves(state: &mut AppState) {
        for _ in 0..500 {
            state.update(Duration::from_millis(200));
            if state.pending_position_save.is_none()
                && state.position_save_queue.is_empty()
                && !matches!(state.playback_state(), PlaybackState::Seeking)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("position save did not settle");
    }

    struct BlockingPositionService {
        calls: std::sync::Mutex<Vec<Duration>>,
        released: std::sync::Mutex<bool>,
        release_signal: std::sync::Condvar,
    }

    impl BlockingPositionService {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                released: std::sync::Mutex::new(false),
                release_signal: std::sync::Condvar::new(),
            }
        }

        fn wait_for_calls(&self, count: usize) {
            for _ in 0..500 {
                if self.calls.lock().unwrap().len() >= count {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("timed out waiting for {count} position save call(s)");
        }

        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.release_signal.notify_all();
        }

        fn calls(&self) -> Vec<Duration> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl PositionService for BlockingPositionService {
        fn positions(&self, _user_id: &str) -> Result<Vec<SavedPosition>, String> {
            Ok(Vec::new())
        }

        fn save_position(
            &self,
            _user_id: &str,
            _device_id: &str,
            _source: &VideoSource,
            position: Duration,
        ) -> Result<(), String> {
            self.calls.lock().unwrap().push(position);
            let released = self.released.lock().unwrap();
            let _released = self
                .release_signal
                .wait_while(released, |released| !*released)
                .unwrap();
            Ok(())
        }
    }

    fn test_twitch_resolver(_video_id: &str) -> Result<ResolvedTwitchVod, TwitchVodResolveError> {
        Ok(ResolvedTwitchVod {
            hls_url: Url::parse("https://usher.ttvnw.net/vod/2386400830.m3u8?sig=test").unwrap(),
            metadata: Some(TwitchVodMetadata {
                title: "HLE vs CFO - Game 5".into(),
                published_at: chrono::DateTime::parse_from_rfc3339("2026-09-08T12:34:56Z").unwrap(),
            }),
        })
    }

    fn test_twitch_resolver_without_metadata(
        _video_id: &str,
    ) -> Result<ResolvedTwitchVod, TwitchVodResolveError> {
        Ok(ResolvedTwitchVod {
            hls_url: Url::parse("https://usher.ttvnw.net/vod/2386400830.m3u8?sig=test").unwrap(),
            metadata: None,
        })
    }

    fn test_playback_factory(
        source: VideoSource,
        _url: Url,
        _initial_qualities: String,
        _decode_mode: DecodeMode,
        _muted: bool,
        _wake: PlaybackWake,
    ) -> Result<Box<dyn PlaybackBackend>, String> {
        let mut playback = DummyPlayback::new();
        playback.open(&source)?;
        Ok(Box::new(playback))
    }

    fn test_playback_factory_requires_initial_quality_and_app_start_seek(
        source: VideoSource,
        _url: Url,
        initial_qualities: String,
        _decode_mode: DecodeMode,
        _muted: bool,
        _wake: PlaybackWake,
    ) -> Result<Box<dyn PlaybackBackend>, String> {
        if initial_qualities != "480p" {
            return Err(format!(
                "expected initial favourite quality 480p, got {initial_qualities:?}"
            ));
        }
        let mut playback = DummyPlayback::new();
        playback.open(&source)?;
        playback.set_quality("480p");
        // Preserve the source/start_time metadata but force the backend itself
        // back to zero. This models OxidePlayback, which needs AppState to
        // dispatch the actual HLS seek after opening.
        playback.seek(Duration::ZERO);
        playback.update(Duration::from_secs(1));
        Ok(Box::new(playback))
    }

    fn test_playback_factory_requires_muted(
        source: VideoSource,
        _url: Url,
        _initial_qualities: String,
        _decode_mode: DecodeMode,
        muted: bool,
        _wake: PlaybackWake,
    ) -> Result<Box<dyn PlaybackBackend>, String> {
        if !muted {
            return Err("expected muted playback factory invocation".into());
        }
        let mut playback = DummyPlayback::new();
        playback.open(&source)?;
        Ok(Box::new(playback))
    }

    #[test]
    fn video_start_time_survives_initial_favourite_quality_selection() {
        let mut state = AppState::new();
        state.twitch_resolver = test_twitch_resolver;
        state.playback_factory = test_playback_factory_requires_initial_quality_and_app_start_seek;
        state.preferences.favourite_qualities = "480p".into();
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
        assert_eq!(state.quality().unwrap().id, "480p");
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
        assert_eq!(state.safe_title().as_deref(), Some("_ vs _ - Game _"));
        assert!(
            state
                .release_age()
                .is_some_and(|age| age > Duration::from_secs(24 * 3600))
        );
        assert_eq!(state.window_title(), "_ vs _ - Game _ - Sanctuary Player");
    }

    #[test]
    fn window_title_falls_back_without_usable_metadata() {
        let mut state = AppState::new();
        assert_eq!(state.window_title(), "Sanctuary Player");

        state.metadata = Some(VideoMetadata {
            title: "   ".into(),
            release_age: Duration::ZERO,
        });
        assert_eq!(state.window_title(), "Sanctuary Player");
    }

    #[test]
    fn opening_new_video_clears_previous_window_title_until_metadata_arrives() {
        let mut state = AppState::new();
        state.metadata = Some(VideoMetadata {
            title: "Alpha vs Beta - Game 3".into(),
            release_age: Duration::ZERO,
        });
        assert_eq!(state.window_title(), "_ vs _ - Game _ - Sanctuary Player");

        state.twitch_resolver = test_twitch_resolver;
        state.playback_factory = test_playback_factory;
        state.apply(AppCommand::OpenVideo(
            VideoSource::parse("2386400830").unwrap(),
        ));

        assert!(state.pending_video_open.is_some());
        assert_eq!(state.window_title(), "Sanctuary Player");
    }

    #[test]
    fn mute_option_is_forwarded_to_playback_factory() {
        let mut state = AppState::new();
        state.twitch_resolver = test_twitch_resolver_without_metadata;
        state.playback_factory = test_playback_factory_requires_muted;
        state.set_muted(true);
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
        assert!(state.has_video());
    }
    #[test]
    fn missing_twitch_metadata_does_not_block_video_open() {
        let mut state = AppState::new();
        state.twitch_resolver = test_twitch_resolver_without_metadata;
        state.playback_factory = test_playback_factory;
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

        assert!(state.has_video());
        assert_eq!(state.safe_title(), None);
        assert_eq!(state.window_title(), "Sanctuary Player");
        assert_eq!(state.release_age(), None);
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
    fn playback_does_not_enable_polling_animation() {
        let mut state = loaded_state();
        state.apply(AppCommand::Play);
        assert!(matches!(state.playback_state(), PlaybackState::Playing));
        assert!(!state.needs_animation());
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
    fn local_session_restores_position_with_initial_favourite_quality_without_account() {
        let path = temporary_settings_path("session-restore");
        let store = SessionStore::new(path.clone());
        store
            .save(&SessionState {
                source: VideoSource::parse("2386400830").unwrap(),
                position: Duration::from_millis(12_345),
            })
            .unwrap();

        let mut state = AppState::new();
        state.twitch_resolver = test_twitch_resolver;
        state.playback_factory = test_playback_factory_requires_initial_quality_and_app_start_seek;
        state.preferences.favourite_qualities = "480p".into();
        state.set_session_path(path.clone());
        let restored = state.take_startup_session_source().unwrap();
        assert_eq!(restored.id, "2386400830");
        assert_eq!(restored.start_time, Some(Duration::from_millis(12_345)));
        assert!(!state.signed_in());

        state.apply(AppCommand::OpenVideo(restored));
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
        assert_eq!(state.position(), Duration::from_millis(12_345));
        assert_eq!(state.quality().unwrap().id, "480p");
        assert_eq!(
            store.load().unwrap().unwrap().position,
            Duration::from_millis(12_345)
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pausing_flushes_precise_local_session_position() {
        let path = temporary_settings_path("session-pause");
        let mut state = loaded_state();
        state.set_session_path(path.clone());

        state.apply(AppCommand::SeekAbsolute(Duration::from_millis(12_345)));
        state.update(Duration::from_secs(1));
        state.apply(AppCommand::Play);
        state.update(Duration::from_millis(321));
        let expected = state.position();
        state.apply(AppCommand::Pause);

        let saved = SessionStore::new(path.clone()).load().unwrap().unwrap();
        assert_eq!(saved.source.id, "2386400830");
        assert_eq!(saved.source.start_time, None);
        assert_eq!(saved.position, expected);

        let _ = std::fs::remove_file(path);
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
    fn saved_positions_hide_only_redundant_current_device_row() {
        let mut state = loaded_state();
        state.account.user_id = Some("test-user".into());
        state.account.device_id = Some("Native".into());
        state.apply(AppCommand::SeekAbsolute(Duration::from_secs(100)));
        state.update(Duration::from_secs(1));

        let current_source = VideoSource::parse("2386400830").unwrap();
        state.saved_positions = vec![
            SavedPosition {
                source: current_source.clone(),
                device_id: "Native".into(),
                position: Duration::from_secs(95),
                modified_age: Duration::ZERO,
                title: None,
                release_age: None,
            },
            SavedPosition {
                source: current_source.clone(),
                device_id: "Phone".into(),
                position: Duration::from_secs(95),
                modified_age: Duration::ZERO,
                title: None,
                release_age: None,
            },
            SavedPosition {
                source: current_source,
                device_id: "Native".into(),
                position: Duration::from_secs(80),
                modified_age: Duration::ZERO,
                title: None,
                release_age: None,
            },
        ];

        let visible = state.saved_positions();
        assert_eq!(visible.len(), 2);
        assert!(visible.iter().any(|entry| entry.device_id == "Phone"));
        assert!(visible.iter().any(|entry| {
            entry.device_id == "Native" && entry.position == Duration::from_secs(80)
        }));
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
    fn paused_position_uploads_even_below_normal_ten_second_delta() {
        let service = Arc::new(crate::services::DummyPositionService::new());
        let mut state = loaded_state();
        state.positions_service = service.clone();
        state.apply(AppCommand::SignIn {
            user_id: "test-user".into(),
            device_id: "Native".into(),
        });

        state.apply(AppCommand::SeekAbsolute(Duration::from_secs(100)));
        settle_position_saves(&mut state);

        state.apply(AppCommand::SeekAbsolute(Duration::from_secs(105)));
        settle_position_saves(&mut state);
        let current = || {
            service
                .positions("test-user")
                .unwrap()
                .into_iter()
                .find(|entry| entry.device_id == "Native" && entry.source.id == "2386400830")
                .map(|entry| entry.position)
        };
        assert_eq!(current(), Some(Duration::from_secs(100)));

        state.apply(AppCommand::Pause);
        state.pause_position_save_due = Some(Instant::now());
        settle_position_saves(&mut state);
        assert_eq!(current(), Some(Duration::from_secs(105)));
    }

    #[test]
    fn forced_position_coalesces_behind_in_flight_upload() {
        let service = Arc::new(BlockingPositionService::new());
        let mut state = loaded_state();
        state.positions_service = service.clone();
        state.apply(AppCommand::SignIn {
            user_id: "test-user".into(),
            device_id: "Native".into(),
        });

        state.apply(AppCommand::SeekAbsolute(Duration::from_secs(100)));
        state.update(Duration::from_secs(1));
        state.update(Duration::ZERO);
        service.wait_for_calls(1);
        assert!(state.pending_position_save.is_some());

        state.apply(AppCommand::SeekAbsolute(Duration::from_secs(105)));
        state.update(Duration::from_secs(1));
        state.force_remote_position_save();
        state.apply(AppCommand::SeekAbsolute(Duration::from_secs(106)));
        state.update(Duration::from_secs(1));
        state.force_remote_position_save();

        assert_eq!(state.position_save_queue.len(), 1);
        assert_eq!(
            state.position_save_queue.front().unwrap().position,
            Duration::from_secs(106)
        );

        service.release();
        settle_position_saves(&mut state);
        assert_eq!(
            service.calls(),
            vec![Duration::from_secs(100), Duration::from_secs(106)]
        );
    }

    #[test]
    fn shutdown_flushes_latest_position_below_normal_delta() {
        let service = Arc::new(crate::services::DummyPositionService::new());
        let mut state = loaded_state();
        state.positions_service = service.clone();
        state.apply(AppCommand::SignIn {
            user_id: "test-user".into(),
            device_id: "Native".into(),
        });

        state.apply(AppCommand::SeekAbsolute(Duration::from_secs(100)));
        settle_position_saves(&mut state);
        state.apply(AppCommand::SeekAbsolute(Duration::from_secs(105)));
        settle_position_saves(&mut state);

        state.flush_persistence_for_shutdown(Duration::from_secs(1));

        let saved = service
            .positions("test-user")
            .unwrap()
            .into_iter()
            .find(|entry| entry.device_id == "Native" && entry.source.id == "2386400830")
            .unwrap();
        assert_eq!(saved.position, Duration::from_secs(105));
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
