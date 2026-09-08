//! Shared native SanctuaryPlayer application.
//!
//! Platform launchers create the platform-appropriate winit event loop and run
//! [`SanctuaryPlayerApp`]. The application owns the wgpu device/surface and egui
//! renderer so video rendering can later share the same GPU context.

pub mod app;
mod graphics;
mod icon;
mod input;
pub mod model;
pub mod playback;
pub mod services;
pub mod spoilers;
pub mod time_format;
pub mod twitch;
mod ui;
pub mod video;

use std::sync::Arc;
use std::time::{Duration, Instant};

use app::{AppEffect, AppState};
use graphics::{Graphics, RenderStatus};
use input::command_for_key;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::PhysicalKey;
use winit::window::{Fullscreen, Window, WindowAttributes, WindowId};

const ANIMATION_FRAME_INTERVAL: Duration = Duration::from_millis(16);

pub struct SanctuaryPlayerApp {
    window: Option<Arc<Window>>,
    graphics: Option<Graphics>,
    state: AppState,
    last_update: Instant,
    next_animation_frame: Instant,
    next_egui_repaint: Option<Instant>,
}

impl SanctuaryPlayerApp {
    pub fn new() -> Self {
        Self {
            window: None,
            graphics: None,
            state: AppState::new(),
            last_update: Instant::now(),
            next_animation_frame: Instant::now(),
            next_egui_repaint: None,
        }
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
        window.request_redraw();
        self.window = Some(window);
        self.graphics = Some(graphics);
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
