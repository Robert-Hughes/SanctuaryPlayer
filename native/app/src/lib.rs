//! Shared native SanctuaryPlayer application.
//!
//! Platform launchers create the platform-appropriate winit event loop and run
//! [`SanctuaryPlayerApp`]. The application owns the wgpu device/surface and egui
//! renderer so video rendering can later share the same GPU context.

pub mod model;
pub mod spoilers;
pub mod time_format;
pub mod video;

use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowAttributes, WindowId};

/// Shared native SanctuaryPlayer application state.
pub struct SanctuaryPlayerApp {
    window: Option<Arc<Window>>,
    graphics: Option<Graphics>,
    ui: UiState,
}

#[derive(Default)]
struct UiState {
    button_clicks: u64,
}

impl SanctuaryPlayerApp {
    pub fn new() -> Self {
        Self {
            window: None,
            graphics: None,
            ui: UiState::default(),
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

        window.request_redraw();
        self.window = Some(window);
        self.graphics = Some(graphics);
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        // On Android the native surface is only valid between Resumed and
        // Suspended. Dropping it here also makes desktop suspend/resume safe.
        self.graphics = None;
        self.window = None;
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
                if let Some(graphics) = self.graphics.as_mut() {
                    match graphics.render(window, &mut self.ui) {
                        Ok(RenderStatus::Presented) => {}
                        Ok(RenderStatus::Reconfigure) => {
                            graphics.reconfigure();
                            window.request_redraw();
                        }
                        Ok(RenderStatus::Skip) => {}
                        Err(error) => {
                            eprintln!("SanctuaryPlayer: GPU surface error: {error}");
                            event_loop.exit();
                        }
                    }
                }
            }
            _ => {
                // Egui is event-driven for now. Media playback will later request
                // redraws when a new video frame is due.
                window.request_redraw();
            }
        }
    }
}

struct Graphics {
    _instance: wgpu::Instance,
    _adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    max_texture_dimension_2d: u32,
    egui_context: egui::Context,
    egui_winit: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    adapter_summary: String,
}

impl Graphics {
    fn new(window: Arc<Window>) -> Result<Self, String> {
        pollster::block_on(Self::new_async(window))
    }

    async fn new_async(window: Arc<Window>) -> Result<Self, String> {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            backend_options: wgpu::BackendOptions::default(),
            display: None,
        });
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| format!("create_surface: {error}"))?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .map_err(|error| format!("request_adapter: {error}"))?;
        let adapter_info = adapter.get_info();
        let adapter_limits = adapter.limits();
        let max_texture_dimension_2d = adapter_limits.max_texture_dimension_2d;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("sanctuary-player-device"),
                required_features: wgpu::Features::empty(),
                required_limits: adapter_limits,
                memory_hints: wgpu::MemoryHints::default(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(|error| format!("request_device: {error}"))?;

        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(|format| {
                matches!(
                    format,
                    wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
                )
            })
            .unwrap_or(capabilities.formats[0]);
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.clamp(1, max_texture_dimension_2d),
            height: size.height.clamp(1, max_texture_dimension_2d),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: if capabilities
                .alpha_modes
                .contains(&wgpu::CompositeAlphaMode::Opaque)
            {
                wgpu::CompositeAlphaMode::Opaque
            } else {
                capabilities.alpha_modes[0]
            },
            view_formats: vec![],
        };
        surface.configure(&device, &surface_config);

        let egui_context = egui::Context::default();
        egui_context.set_visuals(egui::Visuals::dark());
        let egui_winit = egui_winit::State::new(
            egui_context.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let egui_renderer =
            egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());

        let adapter_summary = format!(
            "{} ({:?}, {:?})",
            adapter_info.name, adapter_info.device_type, adapter_info.backend
        );
        eprintln!("SanctuaryPlayer: GPU {adapter_summary}");

        Ok(Self {
            _instance: instance,
            _adapter: adapter,
            device,
            queue,
            surface,
            surface_config,
            max_texture_dimension_2d,
            egui_context,
            egui_winit,
            egui_renderer,
            adapter_summary,
        })
    }

    fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        self.egui_winit.on_window_event(window, event).consumed
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.surface_config.width = width.clamp(1, self.max_texture_dimension_2d);
        self.surface_config.height = height.clamp(1, self.max_texture_dimension_2d);
        self.reconfigure();
    }

    fn reconfigure(&self) {
        self.surface.configure(&self.device, &self.surface_config);
    }

    fn render(&mut self, window: &Window, ui_state: &mut UiState) -> Result<RenderStatus, String> {
        let (output, reconfigure_after_present) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output) => (output, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(output) => (output, true),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                return Ok(RenderStatus::Reconfigure);
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(RenderStatus::Skip);
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err("surface acquisition failed validation".to_owned());
            }
        };
        let target = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sanctuary-player-frame"),
            });

        let raw_input = self.egui_winit.take_egui_input(window);
        let adapter_summary = self.adapter_summary.clone();
        let full_output = self.egui_context.run_ui(raw_input, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.heading("Sanctuary Player");
                ui.add_space(12.0);
                ui.label("Native winit + wgpu + egui application skeleton");
                ui.add_space(8.0);
                ui.monospace(format!("GPU: {adapter_summary}"));
                ui.add_space(24.0);
                if ui.button("Test egui input").clicked() {
                    ui_state.button_clicks += 1;
                }
                ui.label(format!("Button clicks: {}", ui_state.button_clicks));
            });
        });

        self.egui_winit
            .handle_platform_output(window, full_output.platform_output);

        for (texture_id, image_delta) in &full_output.textures_delta.set {
            self.egui_renderer
                .update_texture(&self.device, &self.queue, *texture_id, image_delta);
        }

        let paint_jobs = self
            .egui_context
            .tessellate(full_output.shapes, full_output.pixels_per_point);
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.surface_config.width, self.surface_config.height],
            pixels_per_point: full_output.pixels_per_point,
        };
        self.egui_renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &paint_jobs,
            &screen_descriptor,
        );

        {
            let mut render_pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("sanctuary-player-egui-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &target,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.015,
                                g: 0.018,
                                b: 0.025,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            self.egui_renderer
                .render(&mut render_pass, &paint_jobs, &screen_descriptor);
        }

        for texture_id in &full_output.textures_delta.free {
            self.egui_renderer.free_texture(texture_id);
        }

        self.queue.submit([encoder.finish()]);
        output.present();
        Ok(if reconfigure_after_present {
            RenderStatus::Reconfigure
        } else {
            RenderStatus::Presented
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderStatus {
    Presented,
    Reconfigure,
    Skip,
}
