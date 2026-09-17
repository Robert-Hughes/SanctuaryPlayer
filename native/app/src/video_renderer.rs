use ::oxideav::core::arena::sync::Frame as ArenaFrame;
use ::oxideav::core::{
    FrameLease, PixelFormat, VideoColorInfo, VideoColorRange, VideoFrame, VideoMatrixCoefficients,
};

use crate::playback::DecodeMode;
#[cfg(target_os = "freebsd")]
use crate::vdpau_vulkan_bridge::VdpauVulkanBridge;
#[cfg(target_os = "freebsd")]
use ::oxideav::core::HardwareVideoFrameStorage;
#[cfg(target_os = "freebsd")]
use oxideav_vdpau::VdpauVideoFrameStorage;

#[cfg(target_os = "freebsd")]
const VDPAU_BRIDGE_SLOTS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presentation {
    None,
    Yuv,
    #[cfg(target_os = "freebsd")]
    VdpauDirect(usize),
}

pub struct VideoRenderer {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    #[cfg(target_os = "freebsd")]
    rgba_pipeline: wgpu::RenderPipeline,
    #[cfg(target_os = "freebsd")]
    rgba_bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buffer: wgpu::Buffer,
    textures: Option<YuvTextures>,
    bind_group: Option<wgpu::BindGroup>,
    #[cfg(target_os = "freebsd")]
    vdpau_bridges: Vec<VdpauVulkanBridge>,
    #[cfg(target_os = "freebsd")]
    vdpau_bind_groups: Vec<wgpu::BindGroup>,
    #[cfg(target_os = "freebsd")]
    vdpau_busy_drops: u64,
    dims: Option<(u32, u32)>,
    presentation: Presentation,
    readback_logged: bool,
    max_texture_dimension_2d: u32,
}

struct YuvTextures {
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
}

#[derive(Clone, Copy, Debug)]
struct YuvConversion {
    range: [f32; 4],
    matrix: [f32; 4],
    mode: f32,
}

impl YuvConversion {
    fn for_stream(color: Option<VideoColorInfo>, width: u32, height: u32) -> Self {
        let range = match color.and_then(|info| info.range) {
            Some(VideoColorRange::Full) => [1.0, 0.0, 1.0, 128.0 / 255.0],
            _ => [255.0 / 219.0, 16.0 / 255.0, 255.0 / 224.0, 128.0 / 255.0],
        };
        let fallback = if width >= 1280 || height > 576 {
            VideoMatrixCoefficients::Bt709
        } else {
            VideoMatrixCoefficients::Smpte170M
        };
        let matrix = color
            .and_then(|info| info.matrix)
            .map(|matrix| match matrix {
                VideoMatrixCoefficients::Unspecified | VideoMatrixCoefficients::Unknown(_) => {
                    fallback
                }
                other => other,
            })
            .unwrap_or(fallback);
        let (matrix, mode) = match matrix {
            VideoMatrixCoefficients::Bt709 => ([1.5748, -0.187324, -0.468124, 1.8556], 0.0),
            VideoMatrixCoefficients::Fcc => ([1.4, -0.343, -0.711, 1.78], 0.0),
            VideoMatrixCoefficients::Bt470Bg | VideoMatrixCoefficients::Smpte170M => {
                ([1.402, -0.344136, -0.714136, 1.772], 0.0)
            }
            VideoMatrixCoefficients::Smpte240M => ([1.576, -0.2253, -0.4767, 1.826], 0.0),
            VideoMatrixCoefficients::Bt2020Ncl => ([1.4746, -0.164553, -0.571353, 1.8814], 0.0),
            VideoMatrixCoefficients::Ycgco => ([0.0; 4], 1.0),
            VideoMatrixCoefficients::Bt2020Cl => ([0.0; 4], 2.0),
            VideoMatrixCoefficients::Identity => ([0.0; 4], 3.0),
            VideoMatrixCoefficients::Unspecified | VideoMatrixCoefficients::Unknown(_) => {
                unreachable!()
            }
        };
        Self {
            range,
            matrix,
            mode,
        }
    }

    fn uniform_words(self) -> [f32; 12] {
        [
            self.range[0],
            self.range[1],
            self.range[2],
            self.range[3],
            self.matrix[0],
            self.matrix[1],
            self.matrix[2],
            self.matrix[3],
            self.mode,
            0.0,
            0.0,
            0.0,
        ]
    }

    #[cfg(test)]
    fn apply(self, raw_y: f32, raw_u: f32, raw_v: f32) -> [f32; 3] {
        let y = (raw_y - self.range[1]) * self.range[0];
        let cb = (raw_u - self.range[3]) * self.range[2];
        let cr = (raw_v - self.range[3]) * self.range[2];
        match self.mode as u32 {
            0 => [
                y + self.matrix[0] * cr,
                y + self.matrix[1] * cb + self.matrix[2] * cr,
                y + self.matrix[3] * cb,
            ],
            1 => [y - cb + cr, y + cb, y - cb - cr],
            2 => {
                let r = y + if cr >= 0.0 { 0.9936 } else { 1.7184 } * cr;
                let b = y + if cb >= 0.0 { 1.5816 } else { 1.9404 } * cb;
                let g = (y - 0.2627 * r - 0.0593 * b) / 0.6780;
                [r, g, b]
            }
            3 => [
                (raw_v - self.range[1]) * self.range[0],
                y,
                (raw_u - self.range[1]) * self.range[0],
            ],
            _ => unreachable!(),
        }
    }
}
impl VideoRenderer {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        surface_format: wgpu::TextureFormat,
        max_texture_dimension_2d: u32,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sanctuary-yuv-to-rgb"),
            source: wgpu::ShaderSource::Wgsl(include_str!("yuv_to_rgb.wgsl").into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sanctuary-yuv-bgl"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                texture_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sanctuary-yuv-pl"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sanctuary-yuv-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        #[cfg(target_os = "freebsd")]
        let rgba_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sanctuary-rgba-to-screen"),
            source: wgpu::ShaderSource::Wgsl(include_str!("rgba_to_screen.wgsl").into()),
        });
        #[cfg(target_os = "freebsd")]
        let rgba_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("sanctuary-rgba-bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
        #[cfg(target_os = "freebsd")]
        let rgba_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sanctuary-rgba-pl"),
            bind_group_layouts: &[Some(&rgba_bind_group_layout)],
            immediate_size: 0,
        });
        #[cfg(target_os = "freebsd")]
        let rgba_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sanctuary-rgba-pipeline"),
            layout: Some(&rgba_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &rgba_shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &rgba_shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sanctuary-video-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sanctuary-video-uniform"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let default_conversion = YuvConversion::for_stream(None, 1280, 720);
        let mut uniforms = [0.0_f32; 16];
        uniforms[0..4].copy_from_slice(&[1.0, 1.0, 0.0, 0.0]);
        uniforms[4..16].copy_from_slice(&default_conversion.uniform_words());
        queue.write_buffer(&uniform_buffer, 0, bytemuck::cast_slice(&uniforms));

        Self {
            pipeline,
            bind_group_layout,
            #[cfg(target_os = "freebsd")]
            rgba_pipeline,
            #[cfg(target_os = "freebsd")]
            rgba_bind_group_layout,
            sampler,
            uniform_buffer,
            textures: None,
            bind_group: None,
            #[cfg(target_os = "freebsd")]
            vdpau_bridges: Vec::new(),
            #[cfg(target_os = "freebsd")]
            vdpau_bind_groups: Vec::new(),
            #[cfg(target_os = "freebsd")]
            vdpau_busy_drops: 0,
            dims: None,
            presentation: Presentation::None,
            readback_logged: false,
            max_texture_dimension_2d,
        }
    }

    pub fn reset(&mut self) {
        self.presentation = Presentation::None;
    }

    pub fn upload_lease(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        lease: &FrameLease,
        decode_mode: DecodeMode,
        color: Option<VideoColorInfo>,
    ) -> Result<(), String> {
        match decode_mode {
            DecodeMode::Cpu => self.upload_cpu_lease(device, queue, lease, color),
            DecodeMode::MediaCodecReadback => {
                self.upload_mediacodec_readback(device, queue, lease, color)
            }
            DecodeMode::VdpauReadback => self.upload_vdpau_readback(device, queue, lease, color),
            DecodeMode::VdpauDirect => {
                #[cfg(target_os = "freebsd")]
                {
                    self.upload_vdpau_direct(device, queue, lease)
                }
                #[cfg(not(target_os = "freebsd"))]
                {
                    let _ = (device, queue, lease, color);
                    Err("vdpau-direct presentation is only available on FreeBSD".into())
                }
            }
        }
    }

    fn upload_cpu_lease(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        lease: &FrameLease,
        color: Option<VideoColorInfo>,
    ) -> Result<(), String> {
        let arena = lease.as_arena_video().ok_or_else(|| {
            "CPU decode produced a non-arena video lease; refusing materialisation".to_owned()
        })?;
        let view = arena_yuv420p_view(arena)
            .ok_or_else(|| "unsupported arena video layout (expected native YUV420P)".to_owned())?;
        self.upload_yuv420p(device, queue, &view, color)
    }

    fn upload_mediacodec_readback(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        lease: &FrameLease,
        color: Option<VideoColorInfo>,
    ) -> Result<(), String> {
        let hardware = lease.as_hardware_video().ok_or_else(|| {
            "mediacodec-readback mode received a non-hardware video lease".to_owned()
        })?;
        if hardware.backend() != "mediacodec" {
            return Err(format!(
                "mediacodec-readback mode received hardware backend {:?}",
                hardware.backend()
            ));
        }
        if hardware.pixel_format() != PixelFormat::Yuv420P {
            return Err(format!(
                "mediacodec-readback mode received unsupported materialisation format {:?}",
                hardware.pixel_format()
            ));
        }
        let width = hardware.width();
        let height = hardware.height();
        let frame = hardware
            .materialize()
            .map_err(|error| format!("MediaCodec CPU readback failed: {error}"))?;
        let view = video_frame_yuv420p_view(&frame, width, height)
            .ok_or_else(|| "MediaCodec readback produced invalid YUV420P planes".to_owned())?;
        self.upload_yuv420p(device, queue, &view, color)?;
        if !self.readback_logged {
            log::info!(
                "SanctuaryPlayer: MediaCodec readback presentation active ({}x{}, hardware decode -> AImage CPU I420 -> wgpu)",
                width,
                height
            );
            self.readback_logged = true;
        }
        Ok(())
    }

    fn upload_vdpau_readback(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        lease: &FrameLease,
        color: Option<VideoColorInfo>,
    ) -> Result<(), String> {
        let hardware = lease
            .as_hardware_video()
            .ok_or_else(|| "vdpau-readback mode received a non-hardware video lease".to_owned())?;
        if hardware.backend() != "vdpau" {
            return Err(format!(
                "vdpau-readback mode received hardware backend {:?}",
                hardware.backend()
            ));
        }
        if hardware.pixel_format() != PixelFormat::Yuv420P {
            return Err(format!(
                "vdpau-readback mode received unsupported format {:?}",
                hardware.pixel_format()
            ));
        }
        let width = hardware.width();
        let height = hardware.height();
        let frame = hardware
            .materialize()
            .map_err(|error| format!("VDPAU CPU readback failed: {error}"))?;
        let view = video_frame_yuv420p_view(&frame, width, height)
            .ok_or_else(|| "VDPAU readback produced invalid YUV420P planes".to_owned())?;
        self.upload_yuv420p(device, queue, &view, color)?;
        if !self.readback_logged {
            log::info!(
                "SanctuaryPlayer: VDPAU readback presentation active ({}x{}, hardware decode -> CPU I420 -> wgpu)",
                width,
                height
            );
            self.readback_logged = true;
        }
        Ok(())
    }

    fn upload_yuv420p(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &Yuv420pView<'_>,
        color: Option<VideoColorInfo>,
    ) -> Result<(), String> {
        if view.width > self.max_texture_dimension_2d || view.height > self.max_texture_dimension_2d
        {
            return Err(format!(
                "video frame {}x{} exceeds GPU texture limit {}",
                view.width, view.height, self.max_texture_dimension_2d
            ));
        }

        let conversion = YuvConversion::for_stream(color, view.width, view.height);
        queue.write_buffer(
            &self.uniform_buffer,
            16,
            bytemuck::cast_slice(&conversion.uniform_words()),
        );
        self.ensure_textures(device, view.width, view.height);
        self.upload_plane(
            queue,
            PlaneKind::Y,
            view.width,
            view.height,
            view.y_stride,
            view.y,
        );
        self.upload_plane(
            queue,
            PlaneKind::U,
            view.width / 2,
            view.height / 2,
            view.u_stride,
            view.u,
        );
        self.upload_plane(
            queue,
            PlaneKind::V,
            view.width / 2,
            view.height / 2,
            view.v_stride,
            view.v,
        );
        self.presentation = Presentation::Yuv;
        Ok(())
    }

    #[cfg(target_os = "freebsd")]
    fn upload_vdpau_direct(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        lease: &FrameLease,
    ) -> Result<(), String> {
        let hardware = lease
            .as_hardware_video()
            .ok_or_else(|| "vdpau-direct mode received a non-hardware video lease".to_owned())?;
        let storage = hardware
            .downcast_ref::<VdpauVideoFrameStorage>()
            .ok_or_else(|| "vdpau-direct mode received non-VDPAU hardware storage".to_owned())?;
        let width = storage.width();
        let height = storage.height();
        if width == 0
            || height == 0
            || width > self.max_texture_dimension_2d
            || height > self.max_texture_dimension_2d
        {
            return Err(format!("invalid VDPAU frame dimensions {width}x{height}"));
        }

        let rebuild = self
            .vdpau_bridges
            .first()
            .is_none_or(|bridge| bridge.dimensions() != (width, height));
        if rebuild {
            self.vdpau_bridges.clear();
            self.vdpau_bind_groups.clear();
            for _ in 0..VDPAU_BRIDGE_SLOTS {
                let bridge = VdpauVulkanBridge::new(device, queue, width, height)
                    .map_err(|error| format!("VDPAU direct bridge unavailable: {error}"))?;
                let view = bridge
                    .output_texture()
                    .create_view(&wgpu::TextureViewDescriptor::default());
                let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("sanctuary-vdpau-rgba-bg"),
                    layout: &self.rgba_bind_group_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&self.sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: self.uniform_buffer.as_entire_binding(),
                        },
                    ],
                });
                drop(view);
                self.vdpau_bridges.push(bridge);
                self.vdpau_bind_groups.push(bind_group);
            }
            self.vdpau_busy_drops = 0;
            self.presentation = Presentation::None;
            log::info!(
                "SanctuaryPlayer: VDPAU direct presentation active (GLX interop2 -> Vulkan -> wgpu, {}x{}, {} async slots)",
                width,
                height,
                VDPAU_BRIDGE_SLOTS
            );
        }

        let slot = first_ready_slot(self.vdpau_bridges.len(), |index| {
            self.vdpau_bridges[index]
                .is_available()
                .map_err(|error| error.to_string())
        })?;
        let Some(slot) = slot else {
            self.vdpau_busy_drops += 1;
            if self.vdpau_busy_drops == 1 || self.vdpau_busy_drops.is_multiple_of(120) {
                log::info!(
                    "SanctuaryPlayer: all {VDPAU_BRIDGE_SLOTS} VDPAU direct slots are in flight; dropping video frame"
                );
            }
            return Ok(());
        };

        self.vdpau_bridges[slot]
            .copy_from_vdpau(hardware.clone())
            .map_err(|error| format!("VDPAU direct bridge copy failed: {error}"))?;
        self.dims = Some((width, height));
        self.presentation = Presentation::VdpauDirect(slot);
        Ok(())
    }

    pub fn draw(
        &mut self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        let Some((content_width, content_height)) = self.dims else {
            clear_black(encoder, target);
            return;
        };
        if self.presentation == Presentation::None {
            clear_black(encoder, target);
            return;
        }

        write_aspect_uniform(
            queue,
            &self.uniform_buffer,
            content_width,
            content_height,
            surface_width,
            surface_height,
        );

        #[cfg(target_os = "freebsd")]
        let direct_slot = match self.presentation {
            Presentation::VdpauDirect(slot) => Some(slot),
            _ => None,
        };

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("sanctuary-video-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            match self.presentation {
                Presentation::Yuv => {
                    let Some(bind_group) = self.bind_group.as_ref() else {
                        return;
                    };
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, bind_group, &[]);
                    pass.draw(0..3, 0..1);
                }
                #[cfg(target_os = "freebsd")]
                Presentation::VdpauDirect(slot) => {
                    let Some(bind_group) = self.vdpau_bind_groups.get(slot) else {
                        return;
                    };
                    pass.set_pipeline(&self.rgba_pipeline);
                    pass.set_bind_group(0, bind_group, &[]);
                    pass.draw(0..3, 0..1);
                }
                Presentation::None => {}
            }
        }

        #[cfg(target_os = "freebsd")]
        if let Some(slot) = direct_slot
            && let Some(bridge) = self.vdpau_bridges.get_mut(slot)
        {
            bridge.mark_sampled();
        }
    }

    fn ensure_textures(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        if self.dims == Some((width, height)) && self.textures.is_some() {
            return;
        }
        let y = make_plane_texture(device, "sanctuary-y", width, height);
        let u = make_plane_texture(device, "sanctuary-u", width / 2, height / 2);
        let v = make_plane_texture(device, "sanctuary-v", width / 2, height / 2);
        let y_view = y.create_view(&wgpu::TextureViewDescriptor::default());
        let u_view = u.create_view(&wgpu::TextureViewDescriptor::default());
        let v_view = v.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sanctuary-yuv-bg"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&u_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&v_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.uniform_buffer.as_entire_binding(),
                },
            ],
        });
        self.textures = Some(YuvTextures { y, u, v });
        self.bind_group = Some(bind_group);
        self.dims = Some((width, height));
    }

    fn upload_plane(
        &self,
        queue: &wgpu::Queue,
        kind: PlaneKind,
        width: u32,
        height: u32,
        bytes_per_row: u32,
        data: &[u8],
    ) {
        let Some(textures) = self.textures.as_ref() else {
            return;
        };
        let texture = match kind {
            PlaneKind::Y => &textures.y,
            PlaneKind::U => &textures.u,
            PlaneKind::V => &textures.v,
        };
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }
}

#[cfg(target_os = "freebsd")]
fn first_ready_slot<E>(
    slot_count: usize,
    mut poll: impl FnMut(usize) -> Result<bool, E>,
) -> Result<Option<usize>, E> {
    for index in 0..slot_count {
        if poll(index)? {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

fn write_aspect_uniform(
    queue: &wgpu::Queue,
    uniform_buffer: &wgpu::Buffer,
    content_width: u32,
    content_height: u32,
    surface_width: u32,
    surface_height: u32,
) {
    let surface_aspect = surface_width as f32 / surface_height.max(1) as f32;
    let content_aspect = content_width as f32 / content_height.max(1) as f32;
    let (sx, sy, ox, oy) = if content_aspect > surface_aspect {
        let height_fraction = surface_aspect / content_aspect;
        (
            1.0,
            1.0 / height_fraction,
            0.0,
            (1.0 - height_fraction) * 0.5,
        )
    } else {
        let width_fraction = content_aspect / surface_aspect;
        (1.0 / width_fraction, 1.0, (1.0 - width_fraction) * 0.5, 0.0)
    };
    queue.write_buffer(uniform_buffer, 0, bytemuck::cast_slice(&[sx, sy, ox, oy]));
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn make_plane_texture(
    device: &wgpu::Device,
    label: &str,
    width: u32,
    height: u32,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn clear_black(encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView) {
    let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("sanctuary-video-clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
}

enum PlaneKind {
    Y,
    U,
    V,
}

struct Yuv420pView<'a> {
    width: u32,
    height: u32,
    y: &'a [u8],
    u: &'a [u8],
    v: &'a [u8],
    y_stride: u32,
    u_stride: u32,
    v_stride: u32,
}

fn arena_yuv420p_view(frame: &ArenaFrame) -> Option<Yuv420pView<'_>> {
    let header = frame.header();
    let width = header.width;
    let height = header.height;
    if header.pixel_format != PixelFormat::Yuv420P
        || width == 0
        || height == 0
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
        || frame.plane_count() < 3
    {
        return None;
    }

    let y = frame.plane(0)?;
    let u = frame.plane(1)?;
    let v = frame.plane(2)?;
    yuv420p_view(
        width,
        height,
        y,
        frame.plane_stride(0)?,
        u,
        frame.plane_stride(1)?,
        v,
        frame.plane_stride(2)?,
    )
}

fn video_frame_yuv420p_view(
    frame: &VideoFrame,
    width: u32,
    height: u32,
) -> Option<Yuv420pView<'_>> {
    if frame.planes.len() < 3 {
        return None;
    }
    yuv420p_view(
        width,
        height,
        &frame.planes[0].data,
        frame.planes[0].stride,
        &frame.planes[1].data,
        frame.planes[1].stride,
        &frame.planes[2].data,
        frame.planes[2].stride,
    )
}

#[allow(clippy::too_many_arguments)]
fn yuv420p_view<'a>(
    width: u32,
    height: u32,
    y: &'a [u8],
    y_stride: usize,
    u: &'a [u8],
    u_stride: usize,
    v: &'a [u8],
    v_stride: usize,
) -> Option<Yuv420pView<'a>> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return None;
    }
    let chroma_width = (width / 2) as usize;
    let chroma_height = (height / 2) as usize;
    if !plane_covers_image(y, y_stride, width as usize, height as usize)
        || !plane_covers_image(u, u_stride, chroma_width, chroma_height)
        || !plane_covers_image(v, v_stride, chroma_width, chroma_height)
    {
        return None;
    }
    Some(Yuv420pView {
        width,
        height,
        y,
        u,
        v,
        y_stride: u32::try_from(y_stride).ok()?,
        u_stride: u32::try_from(u_stride).ok()?,
        v_stride: u32::try_from(v_stride).ok()?,
    })
}

fn plane_covers_image(data: &[u8], stride: usize, row_bytes: usize, rows: usize) -> bool {
    if stride < row_bytes || rows == 0 {
        return false;
    }
    stride
        .checked_mul(rows.saturating_sub(1))
        .and_then(|prefix| prefix.checked_add(row_bytes))
        .is_some_and(|required| data.len() >= required)
}

#[cfg(test)]
mod tests {
    use ::oxideav::core::arena::sync::{ArenaPool, FrameHeader, VideoFrameBuilder};

    use super::*;

    #[test]
    fn arena_view_preserves_native_plane_addresses_and_strides() {
        let pool = ArenaPool::new(1, 32);
        let arena = pool.lease().unwrap();
        let mut builder = VideoFrameBuilder::<u8>::new(arena, &[8, 2, 2], &[4, 2, 2]).unwrap();
        builder.plane_mut(0).unwrap().copy_from_slice(&[16; 8]);
        builder.plane_mut(1).unwrap().copy_from_slice(&[128; 2]);
        builder.plane_mut(2).unwrap().copy_from_slice(&[128; 2]);
        let frame = builder
            .freeze(FrameHeader::new(4, 2, PixelFormat::Yuv420P, Some(0)))
            .unwrap();
        let y_ptr = frame.plane(0).unwrap().as_ptr();

        let view = arena_yuv420p_view(&frame).unwrap();
        assert_eq!(view.y.as_ptr(), y_ptr);
        assert_eq!(view.y_stride, 4);
        assert_eq!(view.u_stride, 2);
        assert_eq!(view.v_stride, 2);
    }

    #[test]
    fn materialised_view_uses_existing_plane_storage() {
        let frame = VideoFrame {
            pts: Some(7),
            planes: vec![
                ::oxideav::core::VideoPlane {
                    stride: 4,
                    data: vec![16; 8],
                },
                ::oxideav::core::VideoPlane {
                    stride: 2,
                    data: vec![128; 2],
                },
                ::oxideav::core::VideoPlane {
                    stride: 2,
                    data: vec![128; 2],
                },
            ],
        };
        let y_ptr = frame.planes[0].data.as_ptr();
        let view = video_frame_yuv420p_view(&frame, 4, 2).unwrap();
        assert_eq!(view.y.as_ptr(), y_ptr);
        assert_eq!(view.y_stride, 4);
    }

    fn assert_rgb_close(actual: [f32; 3], expected: [f32; 3]) {
        for (actual, expected) in actual.into_iter().zip(expected) {
            assert!((actual - expected).abs() < 1.0e-5, "{actual} != {expected}");
        }
    }

    #[test]
    fn bt709_full_range_keeps_black_and_white_levels() {
        let conversion = YuvConversion::for_stream(
            Some(VideoColorInfo {
                range: Some(VideoColorRange::Full),
                matrix: Some(VideoMatrixCoefficients::Bt709),
                ..Default::default()
            }),
            1280,
            720,
        );
        let neutral = 128.0 / 255.0;
        assert_rgb_close(conversion.apply(0.0, neutral, neutral), [0.0, 0.0, 0.0]);
        assert_rgb_close(conversion.apply(1.0, neutral, neutral), [1.0, 1.0, 1.0]);
    }

    #[test]
    fn bt709_limited_range_expands_nominal_black_and_white() {
        let conversion = YuvConversion::for_stream(
            Some(VideoColorInfo {
                range: Some(VideoColorRange::Limited),
                matrix: Some(VideoMatrixCoefficients::Bt709),
                ..Default::default()
            }),
            1280,
            720,
        );
        let neutral = 128.0 / 255.0;
        assert_rgb_close(
            conversion.apply(16.0 / 255.0, neutral, neutral),
            [0.0, 0.0, 0.0],
        );
        assert_rgb_close(
            conversion.apply(235.0 / 255.0, neutral, neutral),
            [1.0, 1.0, 1.0],
        );
    }
}
