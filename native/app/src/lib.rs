//! Shared native SanctuaryPlayer application.
//!
//! Platform launchers create the platform-appropriate winit event loop and run
//! [`SanctuaryPlayerApp`]. The application owns the wgpu device/surface and egui
//! renderer so video rendering can later share the same GPU context.

pub mod app;
mod audio_output;
mod audio_timeline;
mod graphics;
mod icon;
mod input;
pub mod model;
pub mod playback;
pub mod services;
mod settings;
pub mod spoilers;
pub mod time_format;
pub mod twitch;
mod ui;
#[cfg(target_os = "freebsd")]
mod vdpau_vulkan_bridge;
pub mod video;
mod video_renderer;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use app::{AppEffect, AppState};
use graphics::{Graphics, RenderStatus};
use input::command_for_key;
use playback::DecodeMode;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::PhysicalKey;
use winit::window::{Fullscreen, Window, WindowAttributes, WindowId};

const ANIMATION_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const RENDER_DIAGNOSTIC_INTERVAL: Duration = Duration::from_secs(1);

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
        eprintln!(
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
}

impl SanctuaryPlayerApp {
    pub fn new() -> Self {
        Self {
            window: None,
            graphics: None,
            state: AppState::new(),
            initial_video: None,
            initial_autoplay: false,
            last_update: Instant::now(),
            next_animation_frame: Instant::now(),
            next_egui_repaint: None,
            render_diagnostics: RenderDiagnostics::new(),
        }
    }

    pub fn set_settings_path(&mut self, path: PathBuf) {
        self.state.set_settings_path(path);
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.state.set_muted(muted);
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
        Self::with_initial_video_decode_options(source, autoplay, DecodeMode::Cpu)
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
        window.request_redraw();
    }

    fn shutdown(&mut self, event_loop: &ActiveEventLoop) {
        self.next_egui_repaint = None;
        if let Some(graphics) = self.graphics.take() {
            drop(graphics);
        }
        self.window = None;
        event_loop.exit();
    }
}

impl Default for SanctuaryPlayerApp {
    fn default() -> Self {
        Self::new()
    }
}

impl ApplicationHandler for SanctuaryPlayerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = WindowAttributes::default()
            .with_title("Sanctuary Player")
            .with_window_icon(icon::app_icon())
            .with_inner_size(winit::dpi::PhysicalSize::new(1280, 720));
        let window = match event_loop.create_window(attrs) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                eprintln!("SanctuaryPlayer: failed to create window: {error}");
                event_loop.exit();
                return;
            }
        };
        let graphics = match Graphics::new(window.clone()) {
            Ok(graphics) => graphics,
            Err(error) => {
                eprintln!("SanctuaryPlayer: failed to initialise GPU rendering: {error}");
                event_loop.exit();
                return;
            }
        };
        self.last_update = Instant::now();
        self.next_animation_frame = self.last_update;
        self.next_egui_repaint = None;
        self.window = Some(window.clone());
        self.graphics = Some(graphics);
        if let Some(source) = self.initial_video.take() {
            if self.initial_autoplay {
                self.state.play_when_opened();
            }
            self.apply_command(window.as_ref(), crate::model::AppCommand::OpenVideo(source));
        } else {
            window.request_redraw();
        }
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        self.graphics = None;
        self.window = None;
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        let mut requested_redraw = false;

        if self.state.needs_animation() && now >= self.next_animation_frame {
            requested_redraw = true;
            self.next_animation_frame = now + ANIMATION_FRAME_INTERVAL;
        }

        if self.next_egui_repaint.is_some_and(|when| now >= when) {
            requested_redraw = true;
            self.next_egui_repaint = None;
        }

        if requested_redraw && let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }

        let playback_deadline = self
            .state
            .needs_animation()
            .then_some(self.next_animation_frame);
        let next_deadline = match (playback_deadline, self.next_egui_repaint) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
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
                let diagnose_render = self.state.needs_animation();
                if diagnose_render {
                    self.render_diagnostics.begin_redraw(now);
                } else {
                    self.render_diagnostics.reset(now);
                }

                self.state
                    .update(now.saturating_duration_since(self.last_update));
                self.last_update = now;

                if let Some(graphics) = self.graphics.as_mut() {
                    match graphics.render(window, &mut self.state) {
                        Ok(frame) => {
                            if frame.status == RenderStatus::Reconfigure {
                                graphics.reconfigure();
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
                            eprintln!("SanctuaryPlayer: GPU surface error: {error}");
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
