//! Android MediaCodec PRIVATE/AHardwareBuffer -> Vulkan -> wgpu bridge.
//!
//! Pixel data remains GPU/hardware resident throughout this path. MediaCodec
//! writes a PRIVATE AImageReader buffer, Vulkan imports its AHardwareBuffer and
//! samples it with sampler-YCbCr conversion directly into an ordinary
//! wgpu-owned RGBA texture. The renderer then samples that texture normally.

use std::ffi::c_void;
use std::io::Cursor;

use ::oxideav::core::{
    Error, HardwareVideoFrame, HardwareVideoFrameStorage, Result, VideoColorInfo, VideoColorRange,
    VideoMatrixCoefficients,
};
use ash::vk;
use oxideav_mediacodec::{MediaCodecOutputMode, MediaCodecVideoFrameStorage};
use wgpu::hal::api::Vulkan;

const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

struct ImportedFrame {
    _frame: HardwareVideoFrame,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

struct Slot {
    output: wgpu::Texture,
    output_raw: vk::Image,
    output_view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    command_buffer: vk::CommandBuffer,
    fence: vk::Fence,
    descriptor_set: vk::DescriptorSet,
    output_sampled: bool,
    inflight: Option<ImportedFrame>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExternalFormatKey {
    format: vk::Format,
    external_format: u64,
}

pub struct MediaCodecVulkanBridge {
    width: u32,
    height: u32,
    vk_device: ash::Device,
    vk_queue: vk::Queue,
    queue_family: u32,
    ahb: ash::android::external_memory_android_hardware_buffer::Device,
    format_key: ExternalFormatKey,
    conversion: vk::SamplerYcbcrConversion,
    sampler: vk::Sampler,
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    pipeline_layout: vk::PipelineLayout,
    render_pass: vk::RenderPass,
    pipeline: vk::Pipeline,
    command_pool: vk::CommandPool,
    slots: Vec<Slot>,
}

impl MediaCodecVulkanBridge {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        first_frame: &HardwareVideoFrame,
        color: Option<VideoColorInfo>,
        slot_count: usize,
    ) -> Result<Self> {
        let storage = direct_storage(first_frame)?;
        let width = storage.width();
        let height = storage.height();
        if width == 0 || height == 0 || slot_count == 0 {
            return Err(Error::invalid(
                "MediaCodec Vulkan bridge requires non-zero dimensions and slots",
            ));
        }
        if !device
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_NV12)
        {
            return Err(Error::unsupported(
                "wgpu Vulkan device lacks enabled sampler-YCbCr conversion support",
            ));
        }

        // SAFETY: the bridge retains public wgpu resources for every raw handle
        // it borrows, and performs raw work on wgpu's own Vulkan queue.
        let hal_device = unsafe { device.as_hal::<Vulkan>() }.ok_or_else(|| {
            Error::unsupported("MediaCodec direct presentation requires the wgpu Vulkan backend")
        })?;
        let hal_queue = unsafe { queue.as_hal::<Vulkan>() }.ok_or_else(|| {
            Error::unsupported("MediaCodec direct presentation requires a Vulkan wgpu queue")
        })?;
        if !hal_device
            .enabled_device_extensions()
            .contains(&ash::android::external_memory_android_hardware_buffer::NAME)
        {
            return Err(Error::unsupported(
                "wgpu Vulkan device did not enable VK_ANDROID_external_memory_android_hardware_buffer",
            ));
        }
        if !hal_device
            .enabled_device_extensions()
            .contains(&ash::ext::queue_family_foreign::NAME)
        {
            return Err(Error::unsupported(
                "wgpu Vulkan device did not enable VK_EXT_queue_family_foreign",
            ));
        }

        let vk_device = hal_device.raw_device().clone();
        let vk_queue = hal_queue.as_raw();
        let queue_family = hal_device.queue_family_index();
        let instance = hal_device.shared_instance().raw_instance().clone();
        let ahb = ash::android::external_memory_android_hardware_buffer::Device::new(
            &instance, &vk_device,
        );

        let format_props = storage
            .with_hardware_buffer_ptr(|raw| unsafe { query_format_properties(&ahb, raw) })??;
        if !format_props
            .format_features
            .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
        {
            return Err(Error::unsupported(
                "MediaCodec AHardwareBuffer is not Vulkan-sampleable",
            ));
        }
        let format_key = ExternalFormatKey {
            format: format_props.format,
            external_format: format_props.external_format,
        };
        if format_key.format == vk::Format::UNDEFINED && format_key.external_format == 0 {
            return Err(Error::unsupported(
                "MediaCodec AHardwareBuffer exposes neither a Vulkan nor external format",
            ));
        }

        let chroma_filter = if format_props
            .format_features
            .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE_YCBCR_CONVERSION_LINEAR_FILTER)
        {
            vk::Filter::LINEAR
        } else {
            vk::Filter::NEAREST
        };
        let mut external_format =
            vk::ExternalFormatANDROID::default().external_format(format_props.external_format);
        let conversion_info = vk::SamplerYcbcrConversionCreateInfo::default()
            .format(format_props.format)
            .ycbcr_model(ycbcr_model(color, format_props.suggested_ycbcr_model))
            .ycbcr_range(ycbcr_range(color, format_props.suggested_ycbcr_range))
            .components(format_props.sampler_ycbcr_conversion_components)
            .x_chroma_offset(format_props.suggested_x_chroma_offset)
            .y_chroma_offset(format_props.suggested_y_chroma_offset)
            .chroma_filter(chroma_filter);
        let conversion_info = if format_props.format == vk::Format::UNDEFINED {
            conversion_info.push_next(&mut external_format)
        } else {
            conversion_info
        };
        let conversion =
            unsafe { vk_device.create_sampler_ycbcr_conversion(&conversion_info, None) }.map_err(
                |error| Error::other(format!("create MediaCodec YCbCr conversion: {error}")),
            )?;

        let mut conversion_for_sampler =
            vk::SamplerYcbcrConversionInfo::default().conversion(conversion);
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(chroma_filter)
            .min_filter(chroma_filter)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .push_next(&mut conversion_for_sampler);
        let sampler = unsafe { vk_device.create_sampler(&sampler_info, None) }
            .map_err(|error| Error::other(format!("create MediaCodec Vulkan sampler: {error}")))?;

        let immutable_samplers = [sampler];
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .immutable_samplers(&immutable_samplers)];
        let descriptor_set_layout = unsafe {
            vk_device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }
        .map_err(|error| Error::other(format!("create MediaCodec descriptor layout: {error}")))?;

        let pool_sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
            descriptor_count: slot_count as u32,
        }];
        let descriptor_pool = unsafe {
            vk_device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(slot_count as u32)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }
        .map_err(|error| Error::other(format!("create MediaCodec descriptor pool: {error}")))?;
        let set_layouts = vec![descriptor_set_layout; slot_count];
        let descriptor_sets = unsafe {
            vk_device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(&set_layouts),
            )
        }
        .map_err(|error| Error::other(format!("allocate MediaCodec descriptors: {error}")))?;

        let set_layouts = [descriptor_set_layout];
        let pipeline_layout = unsafe {
            vk_device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts),
                None,
            )
        }
        .map_err(|error| Error::other(format!("create MediaCodec pipeline layout: {error}")))?;

        let attachments = [vk::AttachmentDescription::default()
            .format(vk::Format::R8G8B8A8_UNORM)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::DONT_CARE)
            .store_op(vk::AttachmentStoreOp::STORE)
            .initial_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .final_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let color_refs = [vk::AttachmentReference {
            attachment: 0,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        }];
        let subpasses = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color_refs)];
        let render_pass = unsafe {
            vk_device.create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&attachments)
                    .subpasses(&subpasses),
                None,
            )
        }
        .map_err(|error| Error::other(format!("create MediaCodec render pass: {error}")))?;

        let vert_words = spv_words(include_bytes!("mediacodec_direct.vert.spv"))?;
        let frag_words = spv_words(include_bytes!("mediacodec_direct.frag.spv"))?;
        let vert = unsafe {
            vk_device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&vert_words),
                None,
            )
        }
        .map_err(|error| Error::other(format!("create MediaCodec vertex shader: {error}")))?;
        let frag = unsafe {
            vk_device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&frag_words),
                None,
            )
        }
        .map_err(|error| Error::other(format!("create MediaCodec fragment shader: {error}")))?;
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vert)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(frag)
                .name(c"main"),
        ];
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewports = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissors = [vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D { width, height },
        }];
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewports(&viewports)
            .scissors(&scissors);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let pipeline_info = [vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .layout(pipeline_layout)
            .render_pass(render_pass)
            .subpass(0)];
        let pipeline = unsafe {
            vk_device.create_graphics_pipelines(vk::PipelineCache::null(), &pipeline_info, None)
        }
        .map_err(|(_, error)| Error::other(format!("create MediaCodec pipeline: {error}")))?[0];
        unsafe {
            vk_device.destroy_shader_module(vert, None);
            vk_device.destroy_shader_module(frag, None);
        }

        let command_pool = unsafe {
            vk_device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(queue_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
        }
        .map_err(|error| Error::other(format!("create MediaCodec command pool: {error}")))?;
        let command_buffers = unsafe {
            vk_device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(slot_count as u32),
            )
        }
        .map_err(|error| Error::other(format!("allocate MediaCodec command buffers: {error}")))?;

        drop(hal_queue);
        drop(hal_device);

        let mut slots = Vec::with_capacity(slot_count);
        for (index, (command_buffer, descriptor_set)) in
            command_buffers.into_iter().zip(descriptor_sets).enumerate()
        {
            let output = make_output_texture(device, queue, width, height, index);
            let hal_output = unsafe { output.as_hal::<Vulkan>() }.ok_or_else(|| {
                Error::unsupported("MediaCodec direct output texture is not Vulkan-backed")
            })?;
            let output_raw = unsafe { hal_output.raw_handle() };
            drop(hal_output);
            let output_view = unsafe {
                vk_device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(output_raw)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(vk::Format::R8G8B8A8_UNORM)
                        .subresource_range(color_range()),
                    None,
                )
            }
            .map_err(|error| Error::other(format!("create MediaCodec output view: {error}")))?;
            let attachments = [output_view];
            let framebuffer = unsafe {
                vk_device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(render_pass)
                        .attachments(&attachments)
                        .width(width)
                        .height(height)
                        .layers(1),
                    None,
                )
            }
            .map_err(|error| Error::other(format!("create MediaCodec framebuffer: {error}")))?;
            let fence = unsafe { vk_device.create_fence(&vk::FenceCreateInfo::default(), None) }
                .map_err(|error| Error::other(format!("create MediaCodec fence: {error}")))?;
            slots.push(Slot {
                output,
                output_raw,
                output_view,
                framebuffer,
                command_buffer,
                fence,
                descriptor_set,
                output_sampled: false,
                inflight: None,
            });
        }

        Ok(Self {
            width,
            height,
            vk_device,
            vk_queue,
            queue_family,
            ahb,
            format_key,
            conversion,
            sampler,
            descriptor_set_layout,
            descriptor_pool,
            pipeline_layout,
            render_pass,
            pipeline,
            command_pool,
            slots,
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    pub fn output_texture(&self, slot: usize) -> Option<&wgpu::Texture> {
        self.slots.get(slot).map(|slot| &slot.output)
    }

    pub fn mark_sampled(&mut self, slot: usize) {
        if let Some(slot) = self.slots.get_mut(slot) {
            slot.output_sampled = true;
        }
    }

    pub fn first_available_slot(&mut self) -> Result<Option<usize>> {
        for index in 0..self.slots.len() {
            if self.is_available(index)? {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }

    pub fn is_available(&mut self, index: usize) -> Result<bool> {
        let slot = self
            .slots
            .get_mut(index)
            .ok_or_else(|| Error::invalid("MediaCodec Vulkan bridge slot out of range"))?;
        if slot.inflight.is_none() {
            return Ok(true);
        }
        match unsafe { self.vk_device.get_fence_status(slot.fence) } {
            Ok(true) => {
                destroy_imported(&self.vk_device, slot.inflight.take());
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(error) => Err(Error::other(format!(
                "query MediaCodec direct fence: {error}"
            ))),
        }
    }

    pub fn render_frame(&mut self, slot_index: usize, frame: HardwareVideoFrame) -> Result<()> {
        if !self.is_available(slot_index)? {
            return Err(Error::other("MediaCodec Vulkan bridge slot is busy"));
        }
        let storage = direct_storage(&frame)?;
        if (storage.width(), storage.height()) != (self.width, self.height) {
            return Err(Error::invalid("MediaCodec direct frame dimensions changed"));
        }

        let retained_frame = frame.clone();
        let imported =
            storage.with_hardware_buffer_ptr(|raw| self.import_frame(raw, retained_frame))??;
        let slot = &mut self.slots[slot_index];
        let image_info = [vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(imported.view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let writes = [vk::WriteDescriptorSet::default()
            .dst_set(slot.descriptor_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&image_info)];
        unsafe { self.vk_device.update_descriptor_sets(&writes, &[]) };

        unsafe {
            self.vk_device
                .reset_fences(&[slot.fence])
                .map_err(|error| Error::other(format!("reset MediaCodec direct fence: {error}")))?;
            self.vk_device
                .reset_command_buffer(slot.command_buffer, vk::CommandBufferResetFlags::empty())
                .map_err(|error| {
                    Error::other(format!("reset MediaCodec command buffer: {error}"))
                })?;
            self.vk_device
                .begin_command_buffer(
                    slot.command_buffer,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|error| {
                    Error::other(format!("begin MediaCodec command buffer: {error}"))
                })?;

            let input_acquire = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .dst_queue_family_index(self.queue_family)
                .image(imported.image)
                .subresource_range(color_range())
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_READ);
            let (old_layout, old_access, old_stage) = if slot.output_sampled {
                (
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::SHADER_READ,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                )
            } else {
                (
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                )
            };
            let output_to_render = vk::ImageMemoryBarrier::default()
                .old_layout(old_layout)
                .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(slot.output_raw)
                .subresource_range(color_range())
                .src_access_mask(old_access)
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);
            self.vk_device.cmd_pipeline_barrier(
                slot.command_buffer,
                old_stage | vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::FRAGMENT_SHADER
                    | vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[input_acquire, output_to_render],
            );

            let clear_values = [vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: [0.0, 0.0, 0.0, 1.0],
                },
            }];
            self.vk_device.cmd_begin_render_pass(
                slot.command_buffer,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(self.render_pass)
                    .framebuffer(slot.framebuffer)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: vk::Extent2D {
                            width: self.width,
                            height: self.height,
                        },
                    })
                    .clear_values(&clear_values),
                vk::SubpassContents::INLINE,
            );
            self.vk_device.cmd_bind_pipeline(
                slot.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline,
            );
            self.vk_device.cmd_bind_descriptor_sets(
                slot.command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline_layout,
                0,
                &[slot.descriptor_set],
                &[],
            );
            self.vk_device.cmd_draw(slot.command_buffer, 3, 1, 0, 0);
            self.vk_device.cmd_end_render_pass(slot.command_buffer);

            // Raw Vulkan work is invisible to wgpu's resource tracker. Restore
            // the output image to exactly the state wgpu believes it has: on
            // the first frame that is COLOR_ATTACHMENT_OPTIMAL, and after wgpu
            // has sampled the slot it is SHADER_READ_ONLY_OPTIMAL. Queue order
            // on wgpu's own Vulkan queue then provides the execution dependency.
            let output_from_render = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .new_layout(old_layout)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(slot.output_raw)
                .subresource_range(color_range())
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_access_mask(old_access);
            self.vk_device.cmd_pipeline_barrier(
                slot.command_buffer,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                old_stage,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[output_from_render],
            );

            let input_release = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(self.queue_family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .image(imported.image)
                .subresource_range(color_range())
                .src_access_mask(vk::AccessFlags::SHADER_READ)
                .dst_access_mask(vk::AccessFlags::empty());
            self.vk_device.cmd_pipeline_barrier(
                slot.command_buffer,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[input_release],
            );
            self.vk_device
                .end_command_buffer(slot.command_buffer)
                .map_err(|error| Error::other(format!("end MediaCodec command buffer: {error}")))?;
            let command_buffers = [slot.command_buffer];
            self.vk_device
                .queue_submit(
                    self.vk_queue,
                    &[vk::SubmitInfo::default().command_buffers(&command_buffers)],
                    slot.fence,
                )
                .map_err(|error| {
                    Error::other(format!("submit MediaCodec direct render: {error}"))
                })?;
        }
        slot.inflight = Some(imported);
        Ok(())
    }

    fn import_frame(
        &self,
        raw_buffer: *mut c_void,
        frame: HardwareVideoFrame,
    ) -> Result<ImportedFrame> {
        let mut format_props = vk::AndroidHardwareBufferFormatPropertiesANDROID::default();
        let mut props =
            vk::AndroidHardwareBufferPropertiesANDROID::default().push_next(&mut format_props);
        unsafe {
            self.ahb
                .get_android_hardware_buffer_properties(raw_buffer.cast(), &mut props)
        }
        .map_err(|error| Error::other(format!("query decoded AHardwareBuffer: {error}")))?;
        let allocation_size = props.allocation_size;
        let memory_type_bits = props.memory_type_bits;
        // End the temporary pNext borrow before reading format_props below.
        let _ = props;
        let key = ExternalFormatKey {
            format: format_props.format,
            external_format: format_props.external_format,
        };
        if key != self.format_key {
            return Err(Error::unsupported(format!(
                "MediaCodec AHardwareBuffer format changed from {:?} to {:?}",
                self.format_key, key
            )));
        }

        let mut external_memory = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::ANDROID_HARDWARE_BUFFER_ANDROID);
        let mut external_format =
            vk::ExternalFormatANDROID::default().external_format(key.external_format);
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(key.format)
            .extent(vk::Extent3D {
                width: self.width,
                height: self.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_memory);
        let image_info = if key.format == vk::Format::UNDEFINED {
            image_info.push_next(&mut external_format)
        } else {
            image_info
        };
        let image = unsafe { self.vk_device.create_image(&image_info, None) }
            .map_err(|error| Error::other(format!("create imported MediaCodec image: {error}")))?;

        let memory_type_index = memory_type_bits.trailing_zeros();
        if memory_type_index >= 32 {
            unsafe { self.vk_device.destroy_image(image, None) };
            return Err(Error::unsupported(
                "MediaCodec AHardwareBuffer exposes no Vulkan memory type",
            ));
        }
        let mut import_info =
            vk::ImportAndroidHardwareBufferInfoANDROID::default().buffer(raw_buffer.cast());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(allocation_size)
            .memory_type_index(memory_type_index)
            .push_next(&mut import_info)
            .push_next(&mut dedicated);
        let memory =
            unsafe { self.vk_device.allocate_memory(&alloc_info, None) }.map_err(|error| {
                unsafe { self.vk_device.destroy_image(image, None) };
                Error::other(format!("import MediaCodec AHardwareBuffer memory: {error}"))
            })?;
        if let Err(error) = unsafe { self.vk_device.bind_image_memory(image, memory, 0) } {
            unsafe {
                self.vk_device.free_memory(memory, None);
                self.vk_device.destroy_image(image, None);
            }
            return Err(Error::other(format!(
                "bind imported MediaCodec image memory: {error}"
            )));
        }

        let mut conversion_info =
            vk::SamplerYcbcrConversionInfo::default().conversion(self.conversion);
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(key.format)
            .subresource_range(color_range())
            .push_next(&mut conversion_info);
        let view =
            unsafe { self.vk_device.create_image_view(&view_info, None) }.map_err(|error| {
                unsafe {
                    self.vk_device.free_memory(memory, None);
                    self.vk_device.destroy_image(image, None);
                }
                Error::other(format!("create MediaCodec imported image view: {error}"))
            })?;

        Ok(ImportedFrame {
            _frame: frame,
            image,
            memory,
            view,
        })
    }
}

impl Drop for MediaCodecVulkanBridge {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk_device.queue_wait_idle(self.vk_queue);
            for slot in &mut self.slots {
                destroy_imported(&self.vk_device, slot.inflight.take());
                self.vk_device.destroy_fence(slot.fence, None);
                self.vk_device.destroy_framebuffer(slot.framebuffer, None);
                self.vk_device.destroy_image_view(slot.output_view, None);
            }
            self.vk_device.destroy_command_pool(self.command_pool, None);
            self.vk_device.destroy_pipeline(self.pipeline, None);
            self.vk_device.destroy_render_pass(self.render_pass, None);
            self.vk_device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.vk_device
                .destroy_descriptor_pool(self.descriptor_pool, None);
            self.vk_device
                .destroy_descriptor_set_layout(self.descriptor_set_layout, None);
            self.vk_device.destroy_sampler(self.sampler, None);
            self.vk_device
                .destroy_sampler_ycbcr_conversion(self.conversion, None);
        }
    }
}

fn direct_storage(frame: &HardwareVideoFrame) -> Result<&MediaCodecVideoFrameStorage> {
    let storage = frame
        .downcast_ref::<MediaCodecVideoFrameStorage>()
        .ok_or_else(|| {
            Error::invalid("MediaCodec direct bridge received foreign hardware storage")
        })?;
    if storage.output_mode() != MediaCodecOutputMode::Direct {
        return Err(Error::invalid(
            "MediaCodec direct bridge received a readback image",
        ));
    }
    Ok(storage)
}

unsafe fn query_format_properties(
    ahb: &ash::android::external_memory_android_hardware_buffer::Device,
    raw: *mut c_void,
) -> Result<vk::AndroidHardwareBufferFormatPropertiesANDROID<'static>> {
    let mut format = vk::AndroidHardwareBufferFormatPropertiesANDROID::default();
    let mut props = vk::AndroidHardwareBufferPropertiesANDROID::default().push_next(&mut format);
    unsafe {
        ahb.get_android_hardware_buffer_properties(raw.cast(), &mut props)
            .map_err(|error| Error::other(format!("query MediaCodec AHardwareBuffer: {error}")))?;
    }
    // Remove the borrowed pNext marker before returning the plain value.
    format.p_next = std::ptr::null_mut();
    Ok(unsafe {
        std::mem::transmute::<
            vk::AndroidHardwareBufferFormatPropertiesANDROID<'_>,
            vk::AndroidHardwareBufferFormatPropertiesANDROID<'static>,
        >(format)
    })
}

fn ycbcr_model(
    color: Option<VideoColorInfo>,
    suggested: vk::SamplerYcbcrModelConversion,
) -> vk::SamplerYcbcrModelConversion {
    match color.and_then(|info| info.matrix) {
        Some(VideoMatrixCoefficients::Bt709) => vk::SamplerYcbcrModelConversion::YCBCR_709,
        Some(VideoMatrixCoefficients::Bt470Bg | VideoMatrixCoefficients::Smpte170M) => {
            vk::SamplerYcbcrModelConversion::YCBCR_601
        }
        Some(VideoMatrixCoefficients::Bt2020Ncl) => vk::SamplerYcbcrModelConversion::YCBCR_2020,
        Some(VideoMatrixCoefficients::Identity) => vk::SamplerYcbcrModelConversion::RGB_IDENTITY,
        _ => suggested,
    }
}

fn ycbcr_range(
    color: Option<VideoColorInfo>,
    suggested: vk::SamplerYcbcrRange,
) -> vk::SamplerYcbcrRange {
    match color.and_then(|info| info.range) {
        Some(VideoColorRange::Full) => vk::SamplerYcbcrRange::ITU_FULL,
        Some(VideoColorRange::Limited) => vk::SamplerYcbcrRange::ITU_NARROW,
        None => suggested,
    }
}

fn make_output_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    width: u32,
    height: u32,
    index: usize,
) -> wgpu::Texture {
    let output = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("mediacodec-direct-wgpu-output"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: OUTPUT_FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = output.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("mediacodec-direct-output-init"),
    });
    {
        let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("mediacodec-direct-output-init-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
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
    queue.submit(Some(encoder.finish()));
    drop(view);
    log::debug!("SanctuaryPlayer: initialised MediaCodec direct output slot {index}");
    output
}

fn destroy_imported(device: &ash::Device, frame: Option<ImportedFrame>) {
    if let Some(frame) = frame {
        unsafe {
            device.destroy_image_view(frame.view, None);
            device.destroy_image(frame.image, None);
            device.free_memory(frame.memory, None);
        }
        drop(frame);
    }
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1)
}

fn spv_words(bytes: &[u8]) -> Result<Vec<u32>> {
    ash::util::read_spv(&mut Cursor::new(bytes))
        .map_err(|error| Error::other(format!("read embedded MediaCodec SPIR-V: {error}")))
}
