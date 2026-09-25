use std::ptr::NonNull;

use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVImageBuffer, CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture,
};
use objc2_metal::{MTLPixelFormat, MTLTextureType};
use oxideav_videotoolbox::decoder::VideoToolboxVideoFrameStorage;

use ::oxideav::core::FrameLease;

pub(crate) struct VideoToolboxMetalBridge {
    cache: CFRetained<CVMetalTextureCache>,
}

pub(crate) struct VideoToolboxMetalFrame {
    pub(crate) bind_group: wgpu::BindGroup,
    _lease: FrameLease,
    _y_cv_texture: CFRetained<CVMetalTexture>,
    _uv_cv_texture: CFRetained<CVMetalTexture>,
    _y_texture: wgpu::Texture,
    _uv_texture: wgpu::Texture,
}

unsafe impl Send for VideoToolboxMetalFrame {}

impl VideoToolboxMetalBridge {
    pub(crate) fn new(device: &wgpu::Device) -> Result<Self, String> {
        let metal = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
            .ok_or_else(|| "wgpu device is not using the Metal backend".to_owned())?;
        let mut raw_cache = std::ptr::null_mut();
        let status = unsafe {
            CVMetalTextureCache::create(
                None,
                None,
                metal.raw_device(),
                None,
                NonNull::from(&mut raw_cache),
            )
        };
        if status != 0 {
            return Err(format!(
                "CVMetalTextureCacheCreate failed with CVReturn {status}"
            ));
        }
        let raw_cache = NonNull::new(raw_cache)
            .ok_or_else(|| "CVMetalTextureCacheCreate returned a null cache".to_owned())?;
        let cache = unsafe { CFRetained::from_raw(raw_cache) };
        Ok(Self { cache })
    }

    pub(crate) fn import(
        &self,
        device: &wgpu::Device,
        lease: &FrameLease,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        uniform_buffer: &wgpu::Buffer,
    ) -> Result<VideoToolboxMetalFrame, String> {
        let hardware = lease
            .as_hardware_video()
            .ok_or_else(|| "VideoToolbox direct mode received a non-hardware frame".to_owned())?;
        if hardware.backend() != "videotoolbox" {
            return Err(format!(
                "VideoToolbox direct mode received hardware backend {:?}",
                hardware.backend()
            ));
        }
        let storage = hardware
            .downcast_ref::<VideoToolboxVideoFrameStorage>()
            .ok_or_else(|| "VideoToolbox frame has unexpected storage".to_owned())?;
        let width = hardware.width();
        let height = hardware.height();
        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);

        let source = unsafe { &*(storage.pixel_buffer_ptr() as *const CVImageBuffer) };
        let (y_cv_texture, y_texture) = self.import_plane(
            device,
            source,
            MTLPixelFormat::R8Unorm,
            wgpu::TextureFormat::R8Unorm,
            width,
            height,
            0,
            "sanctuary-videotoolbox-y",
        )?;
        let (uv_cv_texture, uv_texture) = self.import_plane(
            device,
            source,
            MTLPixelFormat::RG8Unorm,
            wgpu::TextureFormat::Rg8Unorm,
            chroma_width,
            chroma_height,
            1,
            "sanctuary-videotoolbox-uv",
        )?;

        let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_view = uv_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sanctuary-videotoolbox-direct-bg"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&uv_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: uniform_buffer.as_entire_binding(),
                },
            ],
        });

        Ok(VideoToolboxMetalFrame {
            bind_group,
            _lease: lease.clone(),
            _y_cv_texture: y_cv_texture,
            _uv_cv_texture: uv_cv_texture,
            _y_texture: y_texture,
            _uv_texture: uv_texture,
        })
    }

    fn import_plane(
        &self,
        device: &wgpu::Device,
        source: &CVImageBuffer,
        metal_format: MTLPixelFormat,
        wgpu_format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        plane: usize,
        label: &'static str,
    ) -> Result<(CFRetained<CVMetalTexture>, wgpu::Texture), String> {
        let mut raw_cv_texture = std::ptr::null_mut();
        let status = unsafe {
            CVMetalTextureCache::create_texture_from_image(
                None,
                &self.cache,
                source,
                None,
                metal_format,
                width as usize,
                height as usize,
                plane,
                NonNull::from(&mut raw_cv_texture),
            )
        };
        if status != 0 {
            return Err(format!(
                "CVMetalTextureCacheCreateTextureFromImage plane {plane} failed with CVReturn {status}"
            ));
        }
        let raw_cv_texture = NonNull::new(raw_cv_texture).ok_or_else(|| {
            format!("CoreVideo returned null Metal texture wrapper for plane {plane}")
        })?;
        let cv_texture = unsafe { CFRetained::from_raw(raw_cv_texture) };
        let metal_texture = CVMetalTextureGetTexture(&cv_texture)
            .ok_or_else(|| format!("CVMetalTextureGetTexture returned null for plane {plane}"))?;

        let hal_texture = unsafe {
            wgpu::hal::metal::Device::texture_from_raw(
                metal_texture,
                wgpu_format,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu::hal::CopyExtent {
                    width,
                    height,
                    depth: 1,
                },
            )
        };
        let descriptor = wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu_format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        let texture = unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Metal>(hal_texture, &descriptor)
        };
        Ok((cv_texture, texture))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::oxideav::core::{
        CodecId, CodecParameters, Error, Frame, PixelFormat, VideoFrame, VideoPlane,
    };

    fn synthetic_frame(width: usize, height: usize) -> VideoFrame {
        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);
        VideoFrame {
            pts: Some(0),
            planes: vec![
                VideoPlane {
                    stride: width,
                    data: vec![96; width * height],
                },
                VideoPlane {
                    stride: chroma_width,
                    data: vec![128; chroma_width * chroma_height],
                },
                VideoPlane {
                    stride: chroma_width,
                    data: vec![128; chroma_width * chroma_height],
                },
            ],
        }
    }

    #[test]
    fn imports_real_videotoolbox_buffer_into_wgpu_metal() {
        let width = 320u32;
        let height = 240u32;
        let mut params = CodecParameters::video(CodecId::new("h264"));
        params.width = Some(width);
        params.height = Some(height);
        params.pixel_format = Some(PixelFormat::Yuv420P);

        let mut encoder = oxideav_videotoolbox::encoder::make_h264_encoder(&params)
            .expect("create VideoToolbox H.264 encoder");
        encoder
            .send_frame(&Frame::Video(synthetic_frame(
                width as usize,
                height as usize,
            )))
            .expect("encode source frame");
        encoder.flush().expect("flush encoder");

        let mut packets = Vec::new();
        loop {
            match encoder.receive_packet() {
                Ok(packet) => packets.push(packet),
                Err(Error::NeedMore | Error::Eof) => break,
                Err(error) => panic!("receive encoded packet: {error}"),
            }
        }
        assert!(
            !packets.is_empty(),
            "VideoToolbox encoder produced no packets"
        );

        let mut decoder = oxideav_videotoolbox::decoder::H264VtDecoder::make(&params)
            .expect("create VideoToolbox H.264 decoder");
        let mut decoded = None;
        for packet in &packets {
            decoder.send_packet(packet).expect("decode H.264 packet");
            loop {
                match decoder.receive_frame_lease() {
                    Ok(frame) => {
                        decoded = Some(frame);
                        break;
                    }
                    Err(Error::NeedMore) => break,
                    Err(error) => panic!("receive decoded frame: {error}"),
                }
            }
            if decoded.is_some() {
                break;
            }
        }
        if decoded.is_none() {
            decoder.flush().expect("flush decoder");
            decoded = decoder.receive_frame_lease().ok();
        }
        let decoded = decoded.expect("VideoToolbox decoder produced no hardware frame");
        assert_eq!(
            decoded.as_hardware_video().map(|frame| frame.backend()),
            Some("videotoolbox")
        );

        let (device, _queue) = pollster::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
                backends: wgpu::Backends::METAL,
                flags: wgpu::InstanceFlags::default(),
                memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
                backend_options: wgpu::BackendOptions::default(),
                display: None,
            });
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: None,
                    force_fallback_adapter: false,
                })
                .await
                .expect("request Metal adapter");
            let limits = adapter.limits();
            adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("videotoolbox-metal-test"),
                    required_features: wgpu::Features::empty(),
                    required_limits: limits,
                    memory_hints: wgpu::MemoryHints::default(),
                    experimental_features: wgpu::ExperimentalFeatures::disabled(),
                    trace: wgpu::Trace::Off,
                })
                .await
                .expect("request Metal device")
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("videotoolbox-metal-test-bgl"),
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
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
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
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor::default());
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("videotoolbox-metal-test-uniform"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: false,
        });

        let bridge = VideoToolboxMetalBridge::new(&device).expect("create CoreVideo Metal bridge");
        let _imported = bridge
            .import(&device, &decoded, &layout, &sampler, &uniform)
            .expect("import CVPixelBuffer planes into wgpu Metal textures");
    }
}
