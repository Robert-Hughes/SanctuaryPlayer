//! Shared native SanctuaryPlayer application.
//!
//! Platform launchers create the platform-appropriate winit event loop and run
//! [`SanctuaryPlayerApp`]. The application owns the wgpu device/surface and egui
//! renderer so video rendering can later share the same GPU context.

pub mod app;
mod audio_output;
mod audio_timeline;
#[cfg(not(target_os = "android"))]
mod graphics;
#[cfg(not(target_os = "android"))]
mod icon;
#[cfg(not(target_os = "android"))]
mod input;
pub mod logging;
#[cfg(target_os = "android")]
mod mediacodec_vulkan_bridge;
pub mod model;
mod persistence;
pub mod playback;
pub mod services;
mod session;
mod settings;
pub mod spoilers;
pub mod time_format;
pub mod twitch;
pub mod ui;
#[cfg(target_os = "freebsd")]
mod vdpau_vulkan_bridge;
pub mod video;
pub mod video_renderer;
#[cfg(target_os = "windows")]
mod vulkan_video_decoder;
#[cfg(target_os = "windows")]
mod vulkan_video_vulkan_bridge;
pub mod youtube;

#[cfg(not(target_os = "android"))]
use std::path::PathBuf;
#[cfg(not(target_os = "android"))]
use std::sync::Arc;
#[cfg(not(target_os = "android"))]
use std::time::{Duration, Instant};

#[cfg(not(target_os = "android"))]
use app::{AppEffect, AppState};
#[cfg(not(target_os = "android"))]
use graphics::{Graphics, RenderStatus};
#[cfg(not(target_os = "android"))]
use input::command_for_key;
#[cfg(not(target_os = "android"))]
use playback::{DecodeMode, PlaybackWake, PlaybackWakeKind};
#[cfg(not(target_os = "android"))]
use winit::application::ApplicationHandler;
#[cfg(not(target_os = "android"))]
use winit::event::{ElementState, WindowEvent};
#[cfg(not(target_os = "android"))]
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
#[cfg(not(target_os = "android"))]
use winit::keyboard::PhysicalKey;
#[cfg(not(target_os = "android"))]
use winit::window::{Fullscreen, Window, WindowAttributes, WindowId};

#[cfg(not(target_os = "android"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEvent {
    PlaybackWake,
}

#[cfg(not(target_os = "android"))]
const ANIMATION_FRAME_INTERVAL: Duration = Duration::from_millis(16);
#[cfg(not(target_os = "android"))]
const RENDER_DIAGNOSTIC_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(not(target_os = "android"))]
const SHUTDOWN_POSITION_FLUSH_BUDGET: Duration = Duration::from_secs(2);

#[cfg(not(target_os = "android"))]
struct RenderDiagnostics {
    last_redraw: Option<Instant>,
    last_report: Instant,
    redraws: u64,
    gaps_over_20ms: u64,
    gaps_over_33ms: u64,
    max_gap: Duration,
    redraw_work_total: Duration,
    max_redraw_work: Duration,
}

#[cfg(not(target_os = "android"))]
impl RenderDiagnostics {
    fn new() -> Self {
        Self {
            last_redraw: None,
            last_report: Instant::now(),
            redraws: 0,
            gaps_over_20ms: 0,
            gaps_over_33ms: 0,
            max_gap: Duration::ZERO,
            redraw_work_total: Duration::ZERO,
            max_redraw_work: Duration::ZERO,
        }
    }

    fn begin_redraw(&mut self, now: Instant) {
        if let Some(last_redraw) = self.last_redraw {
            let gap = now.saturating_duration_since(last_redraw);
            if gap > Duration::from_millis(20) {
                self.gaps_over_20ms = self.gaps_over_20ms.saturating_add(1);
            }
            if gap > Duration::from_millis(33) {
                self.gaps_over_33ms = self.gaps_over_33ms.saturating_add(1);
            }
            self.max_gap = self.max_gap.max(gap);
        }
        self.last_redraw = Some(now);
        self.redraws = self.redraws.saturating_add(1);
    }

    fn finish_redraw(&mut self, started: Instant, finished: Instant) {
        let redraw_work = finished.saturating_duration_since(started);
        self.redraw_work_total += redraw_work;
        self.max_redraw_work = self.max_redraw_work.max(redraw_work);

        let interval = finished.saturating_duration_since(self.last_report);
        if interval < RENDER_DIAGNOSTIC_INTERVAL {
            return;
        }

        let rate = self.redraws as f64 / interval.as_secs_f64();
        let average_work_ms = if self.redraws == 0 {
            0.0
        } else {
            self.redraw_work_total.as_secs_f64() * 1000.0 / self.redraws as f64
        };
        log::info!(
            "SanctuaryPlayer: render cadence redraws={} rate={rate:.1}/s gap_gt20ms={} gap_gt33ms={} max_gap={:.2}ms redraw_work_avg={average_work_ms:.2}ms redraw_work_max={:.2}ms",
            self.redraws,
            self.gaps_over_20ms,
            self.gaps_over_33ms,
            self.max_gap.as_secs_f64() * 1000.0,
            self.max_redraw_work.as_secs_f64() * 1000.0,
        );

        self.last_report = finished;
        self.redraws = 0;
        self.gaps_over_20ms = 0;
        self.gaps_over_33ms = 0;
        self.max_gap = Duration::ZERO;
        self.redraw_work_total = Duration::ZERO;
        self.max_redraw_work = Duration::ZERO;
    }

    fn reset(&mut self, now: Instant) {
        self.last_redraw = None;
        self.last_report = now;
        self.redraws = 0;
        self.gaps_over_20ms = 0;
        self.gaps_over_33ms = 0;
        self.max_gap = Duration::ZERO;
        self.redraw_work_total = Duration::ZERO;
        self.max_redraw_work = Duration::ZERO;
    }
}

#[cfg(not(target_os = "android"))]
pub struct SanctuaryPlayerApp {
    window: Option<Arc<Window>>,
    graphics: Option<Graphics>,
    state: AppState,
    initial_video: Option<video::VideoSource>,
    initial_autoplay: bool,
    last_update: Instant,
    next_animation_frame: Instant,
    next_egui_repaint: Option<Instant>,
    render_diagnostics: RenderDiagnostics,
    window_title: String,
}

#[cfg(not(target_os = "android"))]
impl SanctuaryPlayerApp {
    pub fn new() -> Self {
        Self {
            window: None,
            graphics: None,
            state: AppState::with_decode_mode(DecodeMode::platform_default()),
            initial_video: None,
            initial_autoplay: false,
            last_update: Instant::now(),
            next_animation_frame: Instant::now(),
            next_egui_repaint: None,
            render_diagnostics: RenderDiagnostics::new(),
            window_title: String::new(),
        }
    }

    pub fn set_settings_path(&mut self, path: PathBuf) {
        self.state.set_settings_path(path);
    }

    pub fn set_session_path(&mut self, path: PathBuf) {
        self.state.set_session_path(path);
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.state.set_muted(muted);
    }

    pub fn flush_persistence_for_shutdown(&mut self) {
        self.state
            .flush_persistence_for_shutdown(SHUTDOWN_POSITION_FLUSH_BUDGET);
    }

    pub fn set_event_proxy(&mut self, proxy: EventLoopProxy<AppEvent>) {
        self.state.set_playback_wake(PlaybackWake::new(move || {
            let _ = proxy.send_event(AppEvent::PlaybackWake);
        }));
    }

    pub fn with_initial_video(source: video::VideoSource) -> Self {
        Self::with_initial_video_options(source, false)
    }

    pub fn with_decode_mode(decode_mode: DecodeMode) -> Self {
        let mut app = Self::new();
        app.state = AppState::with_decode_mode(decode_mode);
        app
    }

    pub fn with_initial_video_options(source: video::VideoSource, autoplay: bool) -> Self {
        Self::with_initial_video_decode_options(source, autoplay, DecodeMode::platform_default())
    }

    pub fn with_initial_video_decode_options(
        source: video::VideoSource,
        autoplay: bool,
        decode_mode: DecodeMode,
    ) -> Self {
        let mut app = Self::with_decode_mode(decode_mode);
        app.initial_video = Some(source);
        app.initial_autoplay = autoplay;
        app
    }

    fn take_startup_video(&mut self) -> Option<(video::VideoSource, bool)> {
        let restored = self.state.take_startup_session_source();
        self.initial_video
            .take()
            .map(|source| (source, self.initial_autoplay))
            .or_else(|| restored.map(|source| (source, false)))
    }

    fn apply_effect(window: &Window, effect: AppEffect) {
        match effect {
            AppEffect::ToggleFullscreen => {
                let next = if window.fullscreen().is_some() {
                    None
                } else {
                    Some(Fullscreen::Borderless(None))
                };
                window.set_fullscreen(next);
            }
        }
    }

    fn apply_command(&mut self, window: &Window, command: crate::model::AppCommand) {
        if let Some(effect) = self.state.apply(command) {
            Self::apply_effect(window, effect);
        }
        self.sync_window_title(window);
        window.request_redraw();
    }

    fn sync_window_title(&mut self, window: &Window) {
        let title = self.state.window_title();
        if title != self.window_title {
            window.set_title(&title);
            self.window_title = title;
        }
    }

    fn update_state_at(&mut self, now: Instant) {
        self.state
            .update(now.saturating_duration_since(self.last_update));
        self.last_update = now;
    }

    fn shutdown(&mut self, event_loop: &ActiveEventLoop) {
        self.flush_persistence_for_shutdown();
        self.next_egui_repaint = None;
        if let Some(graphics) = self.graphics.take() {
            drop(graphics);
        }
        self.window = None;
        event_loop.exit();
    }
}

#[cfg(not(target_os = "android"))]
impl Default for SanctuaryPlayerApp {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(target_os = "android"))]
impl ApplicationHandler<AppEvent> for SanctuaryPlayerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let initial_title = self.state.window_title();
        let attrs = WindowAttributes::default()
            .with_title(initial_title.clone())
            .with_window_icon(icon::app_icon())
            .with_inner_size(winit::dpi::PhysicalSize::new(1280, 720));
        let window = match event_loop.create_window(attrs) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                log::error!("SanctuaryPlayer: failed to create window: {error}");
                event_loop.exit();
                return;
            }
        };
        let graphics = match Graphics::new(window.clone(), self.state.decode_mode()) {
            Ok(graphics) => graphics,
            Err(error) => {
                log::error!("SanctuaryPlayer: failed to initialise GPU rendering: {error}");
                event_loop.exit();
                return;
            }
        };
        self.last_update = Instant::now();
        self.next_animation_frame = self.last_update;
        self.next_egui_repaint = None;
        self.window_title = initial_title;
        self.window = Some(window.clone());
        self.graphics = Some(graphics);
        if let Some((source, autoplay)) = self.take_startup_video() {
            if autoplay {
                self.state.play_when_opened();
            }
            self.apply_command(window.as_ref(), crate::model::AppCommand::OpenVideo(source));
        } else {
            window.request_redraw();
        }
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        self.state.flush_persistence_for_background();
        self.graphics = None;
        self.window = None;
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::PlaybackWake => {
                let pending = self.state.take_playback_wakes();
                if pending.is_empty() {
                    return;
                }
                let Some(window) = self.window.as_ref().cloned() else {
                    return;
                };
                let now = Instant::now();
                self.update_state_at(now);
                self.sync_window_title(window.as_ref());
                let control = pending.contains(PlaybackWakeKind::Control);
                let video_due = pending.contains(PlaybackWakeKind::Video)
                    && self
                        .state
                        .playback_wake_deadline(now)
                        .is_some_and(|deadline| deadline <= now);
                if control || video_due {
                    window.request_redraw();
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let mut requested_redraw = false;

        let animation_deadline = self
            .state
            .needs_animation()
            .then_some(self.next_animation_frame);
        if animation_deadline.is_some_and(|when| now >= when) {
            requested_redraw = true;
            self.next_animation_frame = now + ANIMATION_FRAME_INTERVAL;
        }

        let playback_deadline = self.state.playback_wake_deadline(now);
        if playback_deadline.is_some_and(|when| now >= when) {
            requested_redraw = true;
        }
        let persistence_deadline = self.state.persistence_wake_deadline(now);
        if persistence_deadline.is_some_and(|when| now >= when) {
            requested_redraw = true;
        }

        if self.next_egui_repaint.is_some_and(|when| now >= when) {
            requested_redraw = true;
            self.next_egui_repaint = None;
        }

        if requested_redraw && let Some(window) = self.window.as_ref() {
            window.request_redraw();
            event_loop.set_control_flow(ControlFlow::Poll);
            return;
        }

        let animation_deadline = self
            .state
            .needs_animation()
            .then_some(self.next_animation_frame)
            .filter(|deadline| *deadline > now);
        let playback_deadline = playback_deadline.filter(|deadline| *deadline > now);
        let persistence_deadline = persistence_deadline.filter(|deadline| *deadline > now);
        let next_deadline = [
            animation_deadline,
            playback_deadline,
            persistence_deadline,
            self.next_egui_repaint,
        ]
        .into_iter()
        .flatten()
        .min();
        if let Some(deadline) = next_deadline {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.clone() else {
            return;
        };
        if window.id() != window_id {
            return;
        }

        if matches!(event, WindowEvent::CloseRequested | WindowEvent::Destroyed) {
            drop(window);
            self.shutdown(event_loop);
            return;
        }

        let window = window.as_ref();
        let egui_consumed = self
            .graphics
            .as_mut()
            .map(|graphics| graphics.on_window_event(window, &event))
            .unwrap_or(false);

        match event {
            WindowEvent::Resized(size) => {
                if let Some(graphics) = self.graphics.as_mut() {
                    graphics.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                let diagnose_render = self.state.has_video();
                if diagnose_render {
                    self.render_diagnostics.begin_redraw(now);
                } else {
                    self.render_diagnostics.reset(now);
                }

                self.update_state_at(now);
                self.sync_window_title(window);

                if let Some(graphics) = self.graphics.as_mut() {
                    match graphics.render(window, &mut self.state) {
                        Ok(frame) => {
                            if frame.status == RenderStatus::Reconfigure {
                                graphics.reconfigure();
                                window.request_redraw();
                            }
                            if let Some(delay) = frame.repaint_after {
                                if delay.is_zero() {
                                    window.request_redraw();
                                    self.next_egui_repaint = None;
                                } else {
                                    self.next_egui_repaint = Some(Instant::now() + delay);
                                }
                            } else {
                                self.next_egui_repaint = None;
                            }
                            for command in frame.commands {
                                self.apply_command(window, command);
                            }
                        }
                        Err(error) => {
                            log::error!("SanctuaryPlayer: GPU surface error: {error}");
                            self.flush_persistence_for_shutdown();
                            event_loop.exit();
                        }
                    }
                }

                if diagnose_render {
                    self.render_diagnostics.finish_redraw(now, Instant::now());
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if !egui_consumed && event.state == ElementState::Pressed && !event.repeat =>
            {
                if let PhysicalKey::Code(code) = event.physical_key
                    && let Some(command) = command_for_key(code, &self.state)
                {
                    self.apply_command(window, command);
                }
            }
            _ => window.request_redraw(),
        }
    }
}

#[cfg(all(test, not(target_os = "android")))]
mod startup_tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::session::{SessionState, SessionStore};

    fn temporary_session_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "sanctuary-player-startup-test-{}-{unique}",
                std::process::id()
            ))
            .join(name)
    }

    #[test]
    fn explicit_video_wins_and_discards_one_shot_restored_session() {
        let path = temporary_session_path("session.json");
        SessionStore::new(path.clone())
            .save(&SessionState {
                source: video::VideoSource::parse("2386400830").unwrap(),
                position: Duration::from_secs(99),
            })
            .unwrap();

        let explicit = video::VideoSource::parse("2859508682").unwrap();
        let mut app = SanctuaryPlayerApp::with_initial_video_options(explicit.clone(), true);
        app.set_session_path(path.clone());

        assert_eq!(app.take_startup_video(), Some((explicit, true)));
        assert_eq!(app.take_startup_video(), None);

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn restored_session_starts_paused() {
        let path = temporary_session_path("session.json");
        SessionStore::new(path.clone())
            .save(&SessionState {
                source: video::VideoSource::parse("2386400830").unwrap(),
                position: Duration::from_millis(12_345),
            })
            .unwrap();

        let mut app = SanctuaryPlayerApp::new();
        app.set_session_path(path.clone());
        let (source, autoplay) = app.take_startup_video().unwrap();

        assert_eq!(source.id, "2386400830");
        assert_eq!(source.start_time, Some(Duration::from_millis(12_345)));
        assert!(!autoplay);
        assert_eq!(app.take_startup_video(), None);

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
