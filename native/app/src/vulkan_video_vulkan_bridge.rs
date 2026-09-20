//! Windows Vulkan Video direct presentation bridge.
//!
//! Decoder-owned NV12 frames remain GPU-resident. This bridge copies the Y and
//! interleaved UV planes into ordinary wgpu-owned R8/RG8 textures on wgpu's
//! own Vulkan graphics queue. The retained hardware-frame lease is held until a
//! non-blocking fence poll proves that copy has completed.

use ::oxideav::core::{Error, HardwareVideoFrame, HardwareVideoFrameStorage, Result};
use ash::vk::{self, Handle};
use oxideav_vulkan_video::decoder::VulkanVideoFrameStorage;
use wgpu::hal::api::Vulkan;

pub(crate) struct VulkanVideoVulkanBridge {
    width: u32,
    height: u32,
    y: wgpu::Texture,
    uv: wgpu::Texture,
    vk_device: ash::Device,
    vk_queue: vk::Queue,
    y_raw: vk::Image,
    uv_raw: vk::Image,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    copy_fence: vk::Fence,
    inflight: Option<HardwareVideoFrame>,
    output_sampled: bool,
}

impl VulkanVideoVulkanBridge {
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let y = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sanctuary-vulkan-video-direct-y"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let uv_width = width.div_ceil(2);
        let uv_height = height.div_ceil(2);
        let uv = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sanctuary-vulkan-video-direct-uv"),
            size: wgpu::Extent3d {
                width: uv_width,
                height: uv_height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        // Give wgpu real initial contents and a known tracker state (COPY_DST).
        // Empty submit flushes the pending queue writes before any raw Vulkan
        // submission can touch these images.
        let y_init = vec![0u8; (width as usize) * (height as usize)];
        let uv_init = vec![128u8; (uv_width as usize) * (uv_height as usize) * 2];
        queue.write_texture(
            y.as_image_copy(),
            &y_init,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        queue.write_texture(
            uv.as_image_copy(),
            &uv_init,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(uv_width * 2),
                rows_per_image: Some(uv_height),
            },
            wgpu::Extent3d {
                width: uv_width,
                height: uv_height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(std::iter::empty());

        // SAFETY: acquire one HAL guard at a time. wgpu-core's resource
        // snatch lock is not re-entrant on Windows, so nested as_hal() guards
        // panic even when they are all read-only.
        let (vk_device, queue_family) = {
            let hal_device = unsafe { device.as_hal::<Vulkan>() }.ok_or_else(|| {
                Error::unsupported("Vulkan Video direct bridge requires wgpu Vulkan")
            })?;
            (
                hal_device.raw_device().clone(),
                hal_device.queue_family_index(),
            )
        };
        let vk_queue = {
            let hal_queue = unsafe { queue.as_hal::<Vulkan>() }.ok_or_else(|| {
                Error::unsupported("Vulkan Video direct bridge requires a Vulkan wgpu queue")
            })?;
            hal_queue.as_raw()
        };
        let y_raw = {
            let hal_y = unsafe { y.as_hal::<Vulkan>() }.ok_or_else(|| {
                Error::unsupported("Vulkan Video direct bridge requires a Vulkan Y texture")
            })?;
            unsafe { hal_y.raw_handle() }
        };
        let uv_raw = {
            let hal_uv = unsafe { uv.as_hal::<Vulkan>() }.ok_or_else(|| {
                Error::unsupported("Vulkan Video direct bridge requires a Vulkan UV texture")
            })?;
            unsafe { hal_uv.raw_handle() }
        };

        let command_pool = unsafe {
            vk_device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(queue_family),
                None,
            )
        }
        .map_err(|error| Error::other(format!("Vulkan direct vkCreateCommandPool: {error}")))?;
        let command_buffer = unsafe {
            vk_device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }
        .map_err(|error| {
            Error::other(format!("Vulkan direct vkAllocateCommandBuffers: {error}"))
        })?[0];
        let copy_fence =
            unsafe { vk_device.create_fence(&vk::FenceCreateInfo::default(), None) }
                .map_err(|error| Error::other(format!("Vulkan direct vkCreateFence: {error}")))?;

        Ok(Self {
            width,
            height,
            y,
            uv,
            vk_device,
            vk_queue,
            y_raw,
            uv_raw,
            command_pool,
            command_buffer,
            copy_fence,
            inflight: None,
            output_sampled: false,
        })
    }

    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub(crate) fn y_texture(&self) -> &wgpu::Texture {
        &self.y
    }

    pub(crate) fn uv_texture(&self) -> &wgpu::Texture {
        &self.uv
    }

    pub(crate) fn mark_sampled(&mut self) {
        self.output_sampled = true;
    }

    pub(crate) fn is_available(&mut self) -> Result<bool> {
        if self.inflight.is_none() {
            return Ok(true);
        }
        match unsafe { self.vk_device.get_fence_status(self.copy_fence) } {
            Ok(true) => {
                self.inflight = None;
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(error) => Err(Error::other(format!(
                "Vulkan direct query copy fence: {error}"
            ))),
        }
    }

    pub(crate) fn copy_from_vulkan(&mut self, frame: HardwareVideoFrame) -> Result<()> {
        if !self.is_available()? {
            return Err(Error::other(
                "Vulkan Video direct bridge slot is still in flight",
            ));
        }
        let storage = frame
            .downcast_ref::<VulkanVideoFrameStorage>()
            .ok_or_else(|| Error::invalid("Vulkan direct bridge received unexpected storage"))?;
        if storage.width() != self.width || storage.height() != self.height {
            return Err(Error::invalid(format!(
                "Vulkan direct bridge size mismatch: frame={}x{} bridge={}x{}",
                storage.width(),
                storage.height(),
                self.width,
                self.height
            )));
        }
        let raw_device = self.vk_device.handle().as_raw();
        let storage_device = storage.device() as usize as u64;
        if raw_device != storage_device {
            return Err(Error::invalid(
                "Vulkan direct frame was produced by a different VkDevice than wgpu",
            ));
        }
        let source = vk::Image::from_raw(storage.image() as usize as u64);

        unsafe {
            self.vk_device
                .reset_command_buffer(self.command_buffer, vk::CommandBufferResetFlags::empty())
                .map_err(|error| {
                    Error::other(format!("Vulkan direct reset command buffer: {error}"))
                })?;
            self.vk_device
                .begin_command_buffer(
                    self.command_buffer,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|error| {
                    Error::other(format!("Vulkan direct begin command buffer: {error}"))
                })?;

            let (old_layout, old_access, old_stage) = if self.output_sampled {
                (
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::SHADER_READ,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                )
            } else {
                (
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::PipelineStageFlags::TRANSFER,
                )
            };

            let to_copy = [
                image_barrier(
                    self.y_raw,
                    old_layout,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    old_access,
                    vk::AccessFlags::TRANSFER_WRITE,
                ),
                image_barrier(
                    self.uv_raw,
                    old_layout,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    old_access,
                    vk::AccessFlags::TRANSFER_WRITE,
                ),
            ];
            self.vk_device.cmd_pipeline_barrier(
                self.command_buffer,
                old_stage,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &to_copy,
            );

            self.vk_device.cmd_copy_image(
                self.command_buffer,
                source,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.y_raw,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::ImageCopy::default()
                    .src_subresource(plane_layers(vk::ImageAspectFlags::PLANE_0))
                    .dst_subresource(color_layers())
                    .extent(vk::Extent3D {
                        width: self.width,
                        height: self.height,
                        depth: 1,
                    })],
            );
            self.vk_device.cmd_copy_image(
                self.command_buffer,
                source,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.uv_raw,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::ImageCopy::default()
                    .src_subresource(plane_layers(vk::ImageAspectFlags::PLANE_1))
                    .dst_subresource(color_layers())
                    .extent(vk::Extent3D {
                        width: self.width.div_ceil(2),
                        height: self.height.div_ceil(2),
                        depth: 1,
                    })],
            );

            // Return the wgpu-owned textures to exactly the state its tracker
            // believes they have. First use remains COPY_DST; after the first
            // draw they remain RESOURCE/SHADER_READ_ONLY between frames.
            let from_copy = [
                image_barrier(
                    self.y_raw,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    old_layout,
                    vk::AccessFlags::TRANSFER_WRITE,
                    old_access,
                ),
                image_barrier(
                    self.uv_raw,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    old_layout,
                    vk::AccessFlags::TRANSFER_WRITE,
                    old_access,
                ),
            ];
            self.vk_device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::TRANSFER,
                old_stage,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &from_copy,
            );

            self.vk_device
                .end_command_buffer(self.command_buffer)
                .map_err(|error| {
                    Error::other(format!("Vulkan direct end command buffer: {error}"))
                })?;
            self.vk_device
                .reset_fences(&[self.copy_fence])
                .map_err(|error| Error::other(format!("Vulkan direct reset fence: {error}")))?;
            let command_buffers = [self.command_buffer];
            let submit = vk::SubmitInfo::default().command_buffers(&command_buffers);
            self.vk_device
                .queue_submit(self.vk_queue, &[submit], self.copy_fence)
                .map_err(|error| Error::other(format!("Vulkan direct submit copy: {error}")))?;
        }
        self.inflight = Some(frame);
        Ok(())
    }
}

impl Drop for VulkanVideoVulkanBridge {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk_device.device_wait_idle();
            self.inflight = None;
            self.vk_device.destroy_fence(self.copy_fence, None);
            self.vk_device.destroy_command_pool(self.command_pool, None);
        }
    }
}

fn color_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .mip_level(0)
        .base_array_layer(0)
        .layer_count(1)
}

fn plane_layers(aspect: vk::ImageAspectFlags) -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(aspect)
        .mip_level(0)
        .base_array_layer(0)
        .layer_count(1)
}

fn image_barrier(
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src_access: vk::AccessFlags,
    dst_access: vk::AccessFlags,
) -> vk::ImageMemoryBarrier<'static> {
    vk::ImageMemoryBarrier::default()
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .base_mip_level(0)
                .level_count(1)
                .base_array_layer(0)
                .layer_count(1),
        )
        .src_access_mask(src_access)
        .dst_access_mask(dst_access)
}
