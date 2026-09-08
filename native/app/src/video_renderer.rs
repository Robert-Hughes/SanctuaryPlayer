use ::oxideav::core::arena::sync::Frame as ArenaFrame;
use ::oxideav::core::{FrameLease, PixelFormat};

pub(crate) struct VideoRenderer {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform_buffer: wgpu::Buffer,
    textures: Option<YuvTextures>,
    bind_group: Option<wgpu::BindGroup>,
    dims: Option<(u32, u32)>,
    has_frame: bool,
    max_texture_dimension_2d: u32,
}

struct YuvTextures {
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
}

impl VideoRenderer {
    pub(crate) fn new(
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
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sanctuary-yuv-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sanctuary-yuv-uniform"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(
            &uniform_buffer,
            0,
            bytemuck::cast_slice(&[1.0_f32, 1.0, 0.0, 0.0]),
        );

        Self {
            pipeline,
            bind_group_layout,
            sampler,
            uniform_buffer,
            textures: None,
            bind_group: None,
            dims: None,
            has_frame: false,
            max_texture_dimension_2d,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.has_frame = false;
    }

    pub(crate) fn upload_lease(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        lease: &FrameLease,
    ) -> Result<(), String> {
        let arena = lease.as_arena_video().ok_or_else(|| {
            "software playback produced a non-arena video lease; refusing CPU materialisation"
                .to_owned()
        })?;
        let view = arena_yuv420p_view(arena)
            .ok_or_else(|| "unsupported arena video layout (expected native YUV420P)".to_owned())?;
        if view.width > self.max_texture_dimension_2d || view.height > self.max_texture_dimension_2d
        {
            return Err(format!(
                "video frame {}x{} exceeds GPU texture limit {}",
                view.width, view.height, self.max_texture_dimension_2d
            ));
        }

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
        self.has_frame = true;
        Ok(())
    }

    pub(crate) fn draw(
        &self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        let Some((content_width, content_height)) = self.dims.filter(|_| self.has_frame) else {
            clear_black(encoder, target);
            return;
        };
        let Some(bind_group) = self.bind_group.as_ref() else {
            clear_black(encoder, target);
            return;
        };

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
        queue.write_buffer(
            &self.uniform_buffer,
            0,
            bytemuck::cast_slice(&[sx, sy, ox, oy]),
        );

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
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..3, 0..1);
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

struct ArenaYuv420pView<'a> {
    width: u32,
    height: u32,
    y: &'a [u8],
    u: &'a [u8],
    v: &'a [u8],
    y_stride: u32,
    u_stride: u32,
    v_stride: u32,
}

fn arena_yuv420p_view(frame: &ArenaFrame) -> Option<ArenaYuv420pView<'_>> {
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
    let y_stride = frame.plane_stride(0)?;
    let u_stride = frame.plane_stride(1)?;
    let v_stride = frame.plane_stride(2)?;
    let chroma_width = (width / 2) as usize;
    let chroma_height = (height / 2) as usize;

    if !plane_covers_image(y, y_stride, width as usize, height as usize)
        || !plane_covers_image(u, u_stride, chroma_width, chroma_height)
        || !plane_covers_image(v, v_stride, chroma_width, chroma_height)
    {
        return None;
    }

    Some(ArenaYuv420pView {
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
}
