use std::sync::Arc;

use winit::event::WindowEvent;
use winit::window::Window;

use crate::app::AppState;
use crate::model::AppCommand;
use crate::ui;

pub(crate) struct Graphics {
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
    pub(crate) fn new(window: Arc<Window>) -> Result<Self, String> {
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

    pub(crate) fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        self.egui_winit.on_window_event(window, event).consumed
    }

    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        self.surface_config.width = width.clamp(1, self.max_texture_dimension_2d);
        self.surface_config.height = height.clamp(1, self.max_texture_dimension_2d);
        self.reconfigure();
    }

    pub(crate) fn reconfigure(&self) {
        self.surface.configure(&self.device, &self.surface_config);
    }

    pub(crate) fn render(
        &mut self,
        window: &Window,
        state: &AppState,
    ) -> Result<RenderFrame, String> {
        let (output, reconfigure_after_present) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output) => (output, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(output) => (output, true),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                return Ok(RenderFrame::status(RenderStatus::Reconfigure));
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(RenderFrame::status(RenderStatus::Skip));
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
        let mut commands = Vec::new();
        let full_output = self.egui_context.run_ui(raw_input, |root_ui| {
            commands = ui::render(root_ui, state, &adapter_summary);
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

        Ok(RenderFrame {
            status: if reconfigure_after_present {
                RenderStatus::Reconfigure
            } else {
                RenderStatus::Presented
            },
            commands,
        })
    }
}

pub(crate) struct RenderFrame {
    pub(crate) status: RenderStatus,
    pub(crate) commands: Vec<AppCommand>,
}

impl RenderFrame {
    fn status(status: RenderStatus) -> Self {
        Self {
            status,
            commands: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenderStatus {
    Presented,
    Reconfigure,
    Skip,
}
