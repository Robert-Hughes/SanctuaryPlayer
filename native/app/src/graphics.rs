use std::sync::Arc;
use std::time::Duration;

use winit::event::{ElementState, WindowEvent};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
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
    modifiers: ModifiersState,
    pending_egui_events: Vec<egui::Event>,
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
        let mut style = (*egui_context.global_style()).clone();
        style.interaction.selectable_labels = false;
        let mut visuals = egui::Visuals::light();
        visuals.override_text_color = Some(crate::ui::theme::PURPLE);
        visuals.panel_fill = crate::ui::theme::WHITE;
        visuals.window_fill = crate::ui::theme::WHITE;
        visuals.faint_bg_color = crate::ui::theme::LIGHT_PURPLE;
        visuals.widgets.noninteractive.bg_fill = crate::ui::theme::WHITE;
        visuals.widgets.inactive.bg_fill = crate::ui::theme::WHITE;
        visuals.widgets.hovered.bg_fill = crate::ui::theme::LIGHT_PURPLE;
        visuals.widgets.active.bg_fill = crate::ui::theme::LIGHT_PURPLE;
        visuals.widgets.open.bg_fill = crate::ui::theme::LIGHT_PURPLE;
        // Keep button geometry stable across interaction states. Egui derives
        // button padding from the state's border width, so its default 0px
        // inactive / 1px hovered strokes make buttons contract on hover.
        visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
        visuals.widgets.hovered.bg_stroke = egui::Stroke::NONE;
        visuals.widgets.active.bg_stroke = egui::Stroke::NONE;
        visuals.widgets.open.bg_stroke = egui::Stroke::NONE;
        style.visuals = visuals;
        egui_context.set_global_style(style);
        egui_extras::install_image_loaders(&egui_context);
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
        eprintln!(
            "SanctuaryPlayer: GPU {} ({:?}, {:?})",
            adapter_info.name, adapter_info.device_type, adapter_info.backend
        );

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
            modifiers: ModifiersState::empty(),
            pending_egui_events: Vec::new(),
        })
    }

    pub(crate) fn on_window_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        let response = self.egui_winit.on_window_event(window, event);

        match event {
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::KeyboardInput { event, .. }
                if legacy_shift_insert_paste(self.modifiers, event.state, event.physical_key) =>
            {
                if let Some(contents) = self.egui_winit.clipboard_text() {
                    let contents = contents.replace("\r\n", "\n");
                    if !contents.is_empty() {
                        self.pending_egui_events.push(egui::Event::Paste(contents));
                    }
                }
                return true;
            }
            _ => {}
        }

        response.consumed
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
        state: &mut AppState,
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

        let mut raw_input = self.egui_winit.take_egui_input(window);
        raw_input.events.append(&mut self.pending_egui_events);
        let mut commands = Vec::new();
        let full_output = self.egui_context.run_ui(raw_input, |root_ui| {
            commands = ui::render(root_ui, state);
        });
        let repaint_after = full_output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.repaint_delay)
            .filter(|delay| *delay != Duration::MAX);
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
            repaint_after,
        })
    }
}

pub(crate) struct RenderFrame {
    pub(crate) status: RenderStatus,
    pub(crate) commands: Vec<AppCommand>,
    pub(crate) repaint_after: Option<Duration>,
}

impl RenderFrame {
    fn status(status: RenderStatus) -> Self {
        Self {
            status,
            commands: Vec::new(),
            repaint_after: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenderStatus {
    Presented,
    Reconfigure,
    Skip,
}

fn legacy_shift_insert_paste(
    modifiers: ModifiersState,
    state: ElementState,
    physical_key: PhysicalKey,
) -> bool {
    cfg!(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    )) && state == ElementState::Pressed
        && modifiers.shift_key()
        && matches!(physical_key, PhysicalKey::Code(KeyCode::Insert))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_insert_is_legacy_paste_on_supported_unix_desktops() {
        let supported = cfg!(any(
            target_os = "linux",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd"
        ));
        assert_eq!(
            legacy_shift_insert_paste(
                ModifiersState::SHIFT,
                ElementState::Pressed,
                PhysicalKey::Code(KeyCode::Insert),
            ),
            supported
        );
        assert!(!legacy_shift_insert_paste(
            ModifiersState::empty(),
            ElementState::Pressed,
            PhysicalKey::Code(KeyCode::Insert),
        ));
        assert!(!legacy_shift_insert_paste(
            ModifiersState::SHIFT,
            ElementState::Released,
            PhysicalKey::Code(KeyCode::Insert),
        ));
        assert!(!legacy_shift_insert_paste(
            ModifiersState::SHIFT,
            ElementState::Pressed,
            PhysicalKey::Code(KeyCode::Delete),
        ));
    }
}
