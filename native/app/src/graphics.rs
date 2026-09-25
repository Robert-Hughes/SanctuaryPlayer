use std::sync::Arc;
use std::time::Duration;

use winit::event::{ElementState, WindowEvent};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::Window;

use crate::app::AppState;
use crate::model::{AppCommand, DebugEdge, DebugGraph, DebugGraphLane, DebugNode};
use crate::playback::DecodeMode;
use crate::ui;
use crate::video_renderer::VideoRenderer;

const MAX_SURFACE_VALIDATION_RECOVERY_ATTEMPTS: u8 = 3;

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
    video_renderer: VideoRenderer,
    modifiers: ModifiersState,
    pending_egui_events: Vec<egui::Event>,
    surface_validation_failures: u8,
}

impl Graphics {
    pub(crate) fn new(window: Arc<Window>, decode_mode: DecodeMode) -> Result<Self, String> {
        pollster::block_on(Self::new_async(window, decode_mode))
    }

    async fn new_async(window: Arc<Window>, decode_mode: DecodeMode) -> Result<Self, String> {
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
        let device_desc = wgpu::DeviceDescriptor {
            label: Some("sanctuary-player-device"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter_limits,
            memory_hints: wgpu::MemoryHints::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        };
        #[cfg(target_os = "windows")]
        let (device, queue) = if decode_mode.prefers_windows_shared_vulkan_device() {
            match crate::vulkan_video_decoder::request_shared_wgpu_device(&adapter, &device_desc) {
                Ok(device_and_queue) => device_and_queue,
                Err(error) if !decode_mode.requires_windows_shared_vulkan_device() => {
                    log::warn!(
                        "SanctuaryPlayer: shared Vulkan Video device unavailable in auto mode ({error}); falling back to ordinary wgpu device"
                    );
                    crate::vulkan_video_decoder::clear_direct_device();
                    adapter
                        .request_device(&device_desc)
                        .await
                        .map_err(|fallback_error| {
                            format!(
                                "request shared Vulkan Video device: {error}; fallback request_device: {fallback_error}"
                            )
                        })?
                }
                Err(error) => return Err(error),
            }
        } else {
            crate::vulkan_video_decoder::clear_direct_device();
            adapter
                .request_device(&device_desc)
                .await
                .map_err(|error| format!("request_device: {error}"))?
        };
        #[cfg(not(target_os = "windows"))]
        let (device, queue) = {
            let _ = decode_mode;
            adapter
                .request_device(&device_desc)
                .await
                .map_err(|error| format!("request_device: {error}"))?
        };

        device.on_uncaptured_error(Arc::new(|error| {
            log::error!("SanctuaryPlayer: uncaptured wgpu error: {error}");
        }));
        device.set_device_lost_callback(|reason, message| {
            log::error!("SanctuaryPlayer: wgpu device lost reason={reason:?} message={message}");
        });

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
        ui::theme::configure_context(&egui_context);
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
        let video_renderer = VideoRenderer::new(&device, &queue, format, max_texture_dimension_2d);
        log::info!(
            "SanctuaryPlayer: GPU {} ({:?}, {:?})",
            adapter_info.name,
            adapter_info.device_type,
            adapter_info.backend
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
            video_renderer,
            modifiers: ModifiersState::empty(),
            pending_egui_events: Vec::new(),
            surface_validation_failures: 0,
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

    fn debug_graph(&self) -> DebugGraph {
        let adapter = self._adapter.get_info();
        let mut graph = self.video_renderer.debug_graph();
        let device_rows = vec![
            ("adapter".into(), adapter.name),
            ("backend".into(), format!("{:?}", adapter.backend)),
            ("device type".into(), format!("{:?}", adapter.device_type)),
            (
                "max texture dimension".into(),
                self.max_texture_dimension_2d.to_string(),
            ),
        ];

        #[cfg(target_os = "windows")]
        let mut device_rows = device_rows;

        #[cfg(target_os = "windows")]
        if let Some(info) = crate::vulkan_video_decoder::direct_device_debug_info() {
            device_rows.extend([
                ("shared Vulkan Video device".into(), "true".into()),
                (
                    "graphics queue family".into(),
                    info.graphics_queue_family_index.to_string(),
                ),
                (
                    "video decode queue family".into(),
                    info.video_queue_family_index.to_string(),
                ),
                (
                    "video decode queue index".into(),
                    info.video_queue_index.to_string(),
                ),
            ]);
            graph.edges.push(DebugEdge::relationship(
                "oxideav-video-stage-0",
                "wgpu-device",
                "same VkDevice",
            ));
            graph.edges.push(DebugEdge::relationship(
                "video-direct-copy",
                "wgpu-device",
                "wgpu graphics queue",
            ));
        }

        graph.nodes.push(DebugNode::new(
            "wgpu-device",
            "wgpu device / graphics queue",
            format!("{:?}", adapter.backend),
            DebugGraphLane::Shared,
            5,
            device_rows,
        ));
        graph.nodes.push(DebugNode::new(
            "presentation-surface",
            "wgpu presentation surface",
            format!(
                "{}x{} {:?}",
                self.surface_config.width, self.surface_config.height, self.surface_config.format
            ),
            DebugGraphLane::Video,
            9,
            vec![
                (
                    "size / format".into(),
                    format!(
                        "{}x{} {:?}",
                        self.surface_config.width,
                        self.surface_config.height,
                        self.surface_config.format
                    ),
                ),
                (
                    "present mode".into(),
                    format!("{:?}", self.surface_config.present_mode),
                ),
                (
                    "alpha mode".into(),
                    format!("{:?}", self.surface_config.alpha_mode),
                ),
                (
                    "frame latency".into(),
                    self.surface_config
                        .desired_maximum_frame_latency
                        .to_string(),
                ),
            ],
        ));
        let final_video_node = if graph.nodes.iter().any(|node| node.id == "video-shader") {
            "video-shader"
        } else {
            "video-presentation"
        };
        graph.edges.push(DebugEdge::flow(
            final_video_node,
            "presentation-surface",
            "render pass",
        ));
        graph.edges.push(DebugEdge::relationship(
            "wgpu-device",
            "presentation-surface",
            "owns surface/render queue",
        ));
        graph
    }
    pub(crate) fn render(
        &mut self,
        window: &Window,
        state: &mut AppState,
    ) -> Result<RenderFrame, String> {
        let (output, reconfigure_after_present) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output) => {
                self.surface_validation_failures = 0;
                (output, false)
            }
            wgpu::CurrentSurfaceTexture::Suboptimal(output) => {
                self.surface_validation_failures = 0;
                (output, true)
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                return Ok(RenderFrame::status(RenderStatus::Reconfigure));
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(RenderFrame::status(RenderStatus::Skip));
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                match next_surface_validation_recovery_attempt(self.surface_validation_failures) {
                    Some(attempt) => {
                        self.surface_validation_failures = attempt;
                        log::error!(
                            "SanctuaryPlayer: GPU surface acquisition validation failure; recovery_attempt={attempt}/{MAX_SURFACE_VALIDATION_RECOVERY_ATTEMPTS}; reconfiguring surface"
                        );
                        return Ok(RenderFrame::status(RenderStatus::Reconfigure));
                    }
                    None => {
                        return Err(format!(
                            "surface acquisition validation persisted after {MAX_SURFACE_VALIDATION_RECOVERY_ATTEMPTS} recovery attempts"
                        ));
                    }
                }
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

        if !state.has_video() {
            self.video_renderer.reset();
        }
        let decode_mode = state.decode_mode();
        let color = state.video_color_info();
        if let Some(frame) = state.take_video_frame_lease() {
            self.video_renderer.upload_lease(
                &self.device,
                &self.queue,
                &frame,
                decode_mode,
                color,
            )?;
        }
        self.video_renderer.draw(
            &self.queue,
            &mut encoder,
            &target,
            self.surface_config.width,
            self.surface_config.height,
        );

        if state.debug_info_visible() {
            state.set_graphics_debug_graph(self.debug_graph());
        } else {
            state.set_graphics_debug_graph(DebugGraph::default());
        }

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
                            load: wgpu::LoadOp::Load,
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
        self.video_renderer.after_submit(&self.device, &self.queue);
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

fn next_surface_validation_recovery_attempt(previous_failures: u8) -> Option<u8> {
    let attempt = previous_failures.saturating_add(1);
    (attempt <= MAX_SURFACE_VALIDATION_RECOVERY_ATTEMPTS).then_some(attempt)
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
    fn surface_validation_recovery_is_bounded() {
        assert_eq!(next_surface_validation_recovery_attempt(0), Some(1));
        assert_eq!(next_surface_validation_recovery_attempt(1), Some(2));
        assert_eq!(next_surface_validation_recovery_attempt(2), Some(3));
        assert_eq!(next_surface_validation_recovery_attempt(3), None);
        assert_eq!(next_surface_validation_recovery_attempt(u8::MAX), None);
    }

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
