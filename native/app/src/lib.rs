//! Shared native SanctuaryPlayer application.
//!
//! Platform launchers create the platform-appropriate winit event loop and run
//! [`SanctuaryPlayerApp`]. The application owns the wgpu device/surface and egui
//! renderer so video rendering can later share the same GPU context.

pub mod app;
mod graphics;
pub mod model;
pub mod playback;
pub mod services;
pub mod spoilers;
pub mod time_format;
mod ui;
pub mod video;

use std::sync::Arc;
use std::time::Instant;

use app::{AppEffect, AppState};
use graphics::{Graphics, RenderStatus};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Fullscreen, Window, WindowAttributes, WindowId};

pub struct SanctuaryPlayerApp {
    window: Option<Arc<Window>>,
    graphics: Option<Graphics>,
    state: AppState,
    last_update: Instant,
}

impl SanctuaryPlayerApp {
    pub fn new() -> Self {
        Self {
            window: None,
            graphics: None,
            state: AppState::new(),
            last_update: Instant::now(),
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
        window.request_redraw();
        self.window = Some(window);
        self.graphics = Some(graphics);
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        self.graphics = None;
        self.window = None;
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if self.state.needs_animation() {
            if let Some(window) = self.window.as_ref() {
                window.request_redraw();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        if window.id() != window_id {
            return;
        }
        if let Some(graphics) = self.graphics.as_mut() {
            graphics.on_window_event(window, &event);
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
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
                    match graphics.render(window, &self.state) {
                        Ok(frame) => {
                            if frame.status == RenderStatus::Reconfigure {
                                graphics.reconfigure();
                            }
                            for command in frame.commands {
                                if let Some(effect) = self.state.apply(command) {
                                    Self::apply_effect(window, effect);
                                }
                            }
                        }
                        Err(error) => {
                            eprintln!("SanctuaryPlayer: GPU surface error: {error}");
                            event_loop.exit();
                        }
                    }
                }
            }
            _ => window.request_redraw(),
        }
    }
}
